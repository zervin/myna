//! Myna installed from distribution packages instead of snaps.
//!
//! A packaged install has no snapd: the client is `/usr/bin/myna-desktop`
//! under the `myna-desktop` user unit, and each model is a package that ships
//! a launcher `/usr/libexec/myna/myna-server-<family>` started by the
//! `myna-server` user unit. Nothing here installs, switches or configures a
//! model; the package manager owns that. These adapters report what is
//! installed so Settings opens on its settings pages, and restart the
//! client's user unit.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;

use crate::active_backend::SwitchPlan;
use crate::backend_apply::ApplyPreview;
use crate::command::{CancellationToken, CommandError, CommandRequest, CommandRunner};
use crate::diagnostics::InstalledSnap;
use crate::domain::{
    parse_engine_options, parse_status, ActiveBackendState, BackendIdentity, BackendSnapshot,
    BackendSurface, BackendSurfaceError, CommandResult, ConnectionSnapshot,
};
use crate::ports::{
    BackendRepository, SetUpPlan, SystemConfigurator, SystemConfiguratorError,
    SystemConfiguratorFailure,
};
use crate::snap_changes::ChangeInProgress;
use myna_core::language::ModelFamily;

/// Where model packages put their launchers.
pub const PACKAGED_LIBEXEC: &str = "/usr/libexec/myna";
const LAUNCHER_PREFIX: &str = "myna-server-";
const CLIENT_UNIT: &str = "myna-desktop.service";
const SERVER_UNIT: &str = "myna-server.service";
const PACKAGED_ENGINES: &str =
    r#"{"active-engine":"cpu","engines":[{"name":"cpu","compatible":true}]}"#;
/// Packages own model selection; this names it in every refusal.
const UNSUPPORTED: &str = "a packaged install changes models through the package manager";

/// True for a build made for distribution packages (`MYNA_PACKAGING=deb`
/// at build time). Decided when building, not by probing: every Ubuntu has
/// snapd's socket, and a deb install there still has no Myna snap.
pub fn is_packaged_install() -> bool {
    myna_core::distro_packaged()
}

/// The model families whose launcher is installed under `libexec`, sorted.
pub fn installed_families(libexec: &Path) -> Vec<ModelFamily> {
    let mut families: Vec<ModelFamily> = std::fs::read_dir(libexec)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            let family = name.strip_prefix(LAUNCHER_PREFIX)?;
            ModelFamily::from_snap_name(&format!("myna-{family}"))
        })
        .collect();
    families.sort_by_key(|family| family.snap_name());
    families.dedup();
    families
}

/// A packaged model as the rest of Settings names a backend: by the snap
/// name of its family, so labels and recommendations follow the family.
fn identity(family: ModelFamily) -> BackendIdentity {
    BackendIdentity::new(family.snap_name(), "provider")
}

/// One model installed is the one serving; with more, packages do not say
/// which, so none reads as connected rather than guessing.
pub fn connection_snapshot(families: &[ModelFamily]) -> ConnectionSnapshot {
    let backends: Vec<BackendIdentity> = families.iter().copied().map(identity).collect();
    let active = match backends.as_slice() {
        [only] => ActiveBackendState::Connected(only.clone()),
        _ => ActiveBackendState::Disconnected,
    };
    ConnectionSnapshot::new(backends, active)
}

fn unit_state_request(unit: &str) -> CommandRequest {
    CommandRequest::new(
        "systemctl".to_owned(),
        vec!["--user".to_owned(), "is-active".to_owned(), unit.to_owned()],
    )
}

const CLIENT_PACKAGE: &str = "myna-desktop";

/// A model family's package, named after its launcher.
fn server_package(family: ModelFamily) -> String {
    format!(
        "myna-server-{}",
        family.snap_name().trim_start_matches("myna-")
    )
}

fn version_request(package: &str) -> CommandRequest {
    CommandRequest::new(
        "dpkg-query".to_owned(),
        vec![
            "--show".to_owned(),
            "--showformat=${Version}".to_owned(),
            package.to_owned(),
        ],
    )
}

fn restart_request() -> CommandRequest {
    CommandRequest::new(
        "systemctl".to_owned(),
        vec![
            "--user".to_owned(),
            "restart".to_owned(),
            CLIENT_UNIT.to_owned(),
        ],
    )
}

/// `systemctl is-active` prints the state and exits non-zero for every
/// state but active, so the state is read from stdout either way.
fn unit_state(result: Result<String, CommandError>) -> Option<String> {
    let text = match result {
        Ok(stdout) => stdout,
        Err(CommandError::NonZero { stdout, .. }) => stdout,
        Err(_) => return None,
    };
    let state = text.trim();
    (!state.is_empty()).then(|| state.to_owned())
}

pub struct PackagedBackendRepository {
    runner: Arc<dyn CommandRunner>,
    libexec: PathBuf,
}

impl PackagedBackendRepository {
    pub fn new(runner: Arc<dyn CommandRunner>) -> Self {
        Self::with_libexec(runner, PACKAGED_LIBEXEC)
    }

    pub fn with_libexec(runner: Arc<dyn CommandRunner>, libexec: impl Into<PathBuf>) -> Self {
        Self {
            runner,
            libexec: libexec.into(),
        }
    }

    /// The installed version of `package`, as dpkg records it; the package
    /// name alone when dpkg cannot say, so a report never claims a version.
    async fn package_version(&self, package: &str, cancellation: CancellationToken) -> String {
        match self
            .runner
            .run(version_request(package), cancellation)
            .await
        {
            Ok(output) if !output.stdout().trim().is_empty() => {
                format!("{} (deb {package})", output.stdout().trim())
            }
            _ => format!("unknown (deb {package})"),
        }
    }

    fn snapshot(&self) -> ConnectionSnapshot {
        connection_snapshot(&installed_families(&self.libexec))
    }
}

#[async_trait(?Send)]
impl BackendRepository for PackagedBackendRepository {
    /// The client and each model, named as their snaps would be so the
    /// onboarding assessment and Diagnostics read a packaged install as
    /// installed.
    async fn installed_snaps(
        &self,
        cancellation: CancellationToken,
    ) -> Result<Vec<InstalledSnap>, BackendSurfaceError> {
        let mut installed = vec![InstalledSnap {
            name: crate::onboarding::MYNA_SNAP.to_owned(),
            version: self
                .package_version(CLIENT_PACKAGE, cancellation.clone())
                .await,
        }];
        for family in installed_families(&self.libexec) {
            let package = server_package(family);
            installed.push(InstalledSnap {
                name: family.snap_name().to_owned(),
                version: self.package_version(&package, cancellation.clone()).await,
            });
        }
        Ok(installed)
    }

    async fn discover(
        &self,
        _cancellation: CancellationToken,
    ) -> Result<ConnectionSnapshot, BackendSurfaceError> {
        Ok(self.snapshot())
    }

    /// The server unit's state is the model's only status; packages expose
    /// no model, engine or configuration surface.
    async fn read_snapshot(
        &self,
        backend: &BackendIdentity,
        cancellation: CancellationToken,
    ) -> BackendSnapshot {
        let mut snapshot = BackendSnapshot::empty(backend.clone());
        let request = unit_state_request(SERVER_UNIT);
        let state = unit_state(
            self.runner
                .run(request, cancellation)
                .await
                .map(|output| output.stdout().to_owned()),
        );
        match state.map(|state| {
            parse_status(&serde_json::json!({ "services": { "server": state } }).to_string())
        }) {
            Some(Ok(status)) => {
                snapshot.set_status(status);
                // Packaged models run on the CPU, the one engine they ship.
                if let Ok(engines) = parse_engine_options(PACKAGED_ENGINES) {
                    snapshot.set_engines(engines);
                }
            }
            _ => snapshot.add_error(BackendSurfaceError::new(
                BackendSurface::Status,
                format!("could not read the state of {SERVER_UNIT}"),
                "",
            )),
        }
        snapshot
    }

    async fn refresh(
        &self,
        _cancellation: CancellationToken,
    ) -> Result<ConnectionSnapshot, BackendSurfaceError> {
        Ok(self.snapshot())
    }
}

pub struct PackagedSystemConfigurator {
    runner: Arc<dyn CommandRunner>,
}

impl PackagedSystemConfigurator {
    pub fn new(runner: Arc<dyn CommandRunner>) -> Self {
        Self { runner }
    }
}

/// A refusal names what was asked for and that nothing ran.
fn unsupported(operation: &str) -> SystemConfiguratorError {
    SystemConfiguratorError::Execution {
        step: crate::ports::FailedStep::Unsupported {
            operation: operation.to_owned(),
        },
        message: UNSUPPORTED.to_owned(),
    }
}

#[async_trait(?Send)]
impl SystemConfigurator for PackagedSystemConfigurator {
    fn installs_models(&self) -> bool {
        false
    }

    async fn execute_backend_switch(
        &self,
        _plan: &SwitchPlan,
        _cancellation: CancellationToken,
    ) -> Result<Vec<CommandResult>, SystemConfiguratorFailure> {
        Err(SystemConfiguratorFailure::new(
            Vec::new(),
            unsupported("switch models"),
        ))
    }

    async fn restart_myna(
        &self,
        cancellation: CancellationToken,
    ) -> Result<(), SystemConfiguratorError> {
        let request = restart_request();
        let (executable, arguments) = (
            request.executable().to_owned(),
            request.arguments().to_vec(),
        );
        match self.runner.run(request, cancellation).await {
            Ok(_) => Ok(()),
            Err(CommandError::Cancelled) => Err(SystemConfiguratorError::Cancelled),
            Err(CommandError::NonZero {
                exit_status,
                stderr,
                ..
            }) => Err(SystemConfiguratorError::execution(
                executable,
                arguments,
                exit_status,
                stderr.clone(),
                stderr.trim().to_owned(),
            )),
            Err(error) => Err(SystemConfiguratorError::execution(
                executable,
                arguments,
                None,
                "",
                error.to_string(),
            )),
        }
    }

    async fn apply_backend_config(
        &self,
        _preview: &ApplyPreview,
        _cancellation: CancellationToken,
    ) -> Result<Vec<CommandResult>, SystemConfiguratorFailure> {
        Err(SystemConfiguratorFailure::new(
            Vec::new(),
            unsupported("configure a model"),
        ))
    }

    async fn changes_in_progress(
        &self,
        _cancellation: CancellationToken,
    ) -> Result<Vec<ChangeInProgress>, String> {
        Ok(Vec::new())
    }

    /// No snapd, so no flag to set: user services always run.
    async fn user_daemons_enabled(&self, _cancellation: CancellationToken) -> Result<bool, String> {
        Ok(true)
    }

    async fn set_up(
        &self,
        _plan: &SetUpPlan,
        _cancellation: CancellationToken,
    ) -> Result<(), SystemConfiguratorError> {
        Err(unsupported("set up snaps"))
    }

    async fn install_snap(
        &self,
        _snap: &str,
        _cancellation: CancellationToken,
    ) -> Result<Option<String>, SystemConfiguratorError> {
        Err(unsupported("install a model snap"))
    }

    async fn snap_change(
        &self,
        _change_id: &str,
        _cancellation: CancellationToken,
    ) -> Result<ChangeInProgress, String> {
        Err(UNSUPPORTED.to_owned())
    }
}

/// The repository and configurator for this host: packaged when Myna was
/// installed from packages, snapd otherwise.
pub fn host_ports(
    runner: Arc<dyn CommandRunner>,
) -> (
    std::rc::Rc<dyn BackendRepository>,
    std::rc::Rc<dyn SystemConfigurator>,
) {
    use std::rc::Rc;
    if is_packaged_install() {
        (
            Rc::new(PackagedBackendRepository::new(runner.clone())),
            Rc::new(PackagedSystemConfigurator::new(runner)),
        )
    } else {
        (
            Rc::new(crate::adapters::snap_backend::SnapBackendRepository::new(
                runner.clone(),
                Arc::new(crate::adapters::snapd_client::UnixSocketSnapdClient::new()),
            )),
            Rc::new(crate::adapters::system_configurator::PkexecSystemConfigurator::new(runner)),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_each_installed_model_launcher_and_nothing_else() {
        let dir = std::env::temp_dir().join(format!("myna-packaged-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for name in ["myna-server-parakeet", "myna-hud", "myna-server-nope"] {
            std::fs::write(dir.join(name), "").unwrap();
        }
        let found = installed_families(&dir);
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(found, vec![ModelFamily::Parakeet]);
    }

    #[test]
    fn a_missing_libexec_is_no_models() {
        assert!(installed_families(Path::new("/nonexistent/myna")).is_empty());
    }

    #[test]
    fn one_installed_model_is_the_connected_one() {
        let snapshot = connection_snapshot(&[ModelFamily::Parakeet]);
        assert_eq!(
            snapshot.active_state(),
            ActiveBackendState::Connected(identity(ModelFamily::Parakeet))
        );
    }

    #[test]
    fn several_installed_models_name_none_connected() {
        let snapshot = connection_snapshot(&[ModelFamily::Parakeet, ModelFamily::Whisper]);
        assert_eq!(snapshot.backends().len(), 2);
        assert_eq!(snapshot.active_state(), ActiveBackendState::Disconnected);
    }

    #[test]
    fn a_unit_state_is_read_from_stdout_even_when_inactive() {
        assert_eq!(unit_state(Ok("active\n".into())).as_deref(), Some("active"));
        assert_eq!(
            unit_state(Err(CommandError::NonZero {
                exit_status: Some(3),
                stdout: "inactive\n".into(),
                stderr: String::new(),
            }))
            .as_deref(),
            Some("inactive")
        );
        assert_eq!(unit_state(Err(CommandError::Cancelled)), None);
    }

    #[test]
    fn packaged_models_report_the_cpu_engine_in_use() {
        let engines = parse_engine_options(PACKAGED_ENGINES).unwrap();
        assert_eq!(engines.active(), Some("cpu"));
    }

    #[test]
    fn a_packaged_host_installs_no_models_from_settings() {
        let configurator =
            PackagedSystemConfigurator::new(Arc::new(crate::command::GioCommandRunner));
        assert!(!configurator.installs_models());
    }

    fn ok(stdout: &str) -> Result<crate::command::CommandOutput, CommandError> {
        Ok(crate::command::CommandOutput::new(Some(0), stdout, ""))
    }

    fn libexec_with(names: &[&str]) -> PathBuf {
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("myna-libexec-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for name in names {
            std::fs::write(dir.join(name), "").unwrap();
        }
        dir
    }

    #[test]
    fn installed_packages_report_their_dpkg_versions() {
        let dir = libexec_with(&["myna-server-parakeet"]);
        let runner = crate::command::FakeCommandRunner::scripted([ok("1.2-3"), ok("4.5-6")]);
        let repository = PackagedBackendRepository::with_libexec(Arc::new(runner.clone()), &dir);
        let installed = gtk4::glib::MainContext::new()
            .block_on(repository.installed_snaps(CancellationToken::new()))
            .unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        let versions: Vec<_> = installed
            .iter()
            .map(|s| (s.name.as_str(), s.version.as_str()))
            .collect();
        assert_eq!(
            versions,
            [
                ("myna", "1.2-3 (deb myna-desktop)"),
                ("myna-parakeet", "4.5-6 (deb myna-server-parakeet)")
            ]
        );
        assert_eq!(
            runner.calls()[1].arguments().last().map(String::as_str),
            Some("myna-server-parakeet")
        );
    }

    #[test]
    fn an_unknown_version_never_reads_as_one() {
        let dir = libexec_with(&[]);
        let runner = crate::command::FakeCommandRunner::scripted([Err(CommandError::NonZero {
            exit_status: Some(1),
            stdout: String::new(),
            stderr: "no packages found".into(),
        })]);
        let repository = PackagedBackendRepository::with_libexec(Arc::new(runner), &dir);
        let installed = gtk4::glib::MainContext::new()
            .block_on(repository.installed_snaps(CancellationToken::new()))
            .unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(installed[0].version, "unknown (deb myna-desktop)");
    }

    #[test]
    fn the_server_unit_state_is_the_model_status() {
        let runner = crate::command::FakeCommandRunner::scripted([Err(CommandError::NonZero {
            exit_status: Some(3),
            stdout: "failed\n".into(),
            stderr: String::new(),
        })]);
        let repository =
            PackagedBackendRepository::with_libexec(Arc::new(runner.clone()), "/nonexistent");
        let snapshot = gtk4::glib::MainContext::new().block_on(
            repository.read_snapshot(&identity(ModelFamily::Parakeet), CancellationToken::new()),
        );
        let service = &snapshot.status().unwrap().services()[0];
        assert_eq!(service.name(), "server");
        assert_eq!(service.state(), &crate::domain::ServiceState::Failed);
        assert_eq!(
            runner.calls()[0].arguments(),
            ["--user", "is-active", "myna-server.service"]
        );
    }

    #[test]
    fn a_failed_restart_reports_the_command_that_failed() {
        let runner = crate::command::FakeCommandRunner::scripted([Err(CommandError::NonZero {
            exit_status: Some(5),
            stdout: String::new(),
            stderr: "Unit myna-desktop.service not found.".into(),
        })]);
        let configurator = PackagedSystemConfigurator::new(Arc::new(runner));
        let error = gtk4::glib::MainContext::new()
            .block_on(configurator.restart_myna(CancellationToken::new()))
            .unwrap_err();
        match error {
            SystemConfiguratorError::Execution {
                step:
                    crate::ports::FailedStep::Command {
                        executable,
                        exit_status,
                        ..
                    },
                message,
            } => {
                assert_eq!(executable, "systemctl");
                assert_eq!(exit_status, Some(5));
                assert_eq!(message, "Unit myna-desktop.service not found.");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn refusals_name_the_operation_and_that_nothing_ran() {
        let configurator =
            PackagedSystemConfigurator::new(Arc::new(crate::command::FakeCommandRunner::default()));
        let error = gtk4::glib::MainContext::new()
            .block_on(configurator.install_snap("myna-whisper", CancellationToken::new()))
            .unwrap_err();
        assert!(matches!(
            error,
            SystemConfiguratorError::Execution { step: crate::ports::FailedStep::Unsupported { ref operation }, .. }
                if operation == "install a model snap"
        ));
    }

    #[test]
    fn restart_targets_the_packaged_client_unit() {
        let request = restart_request();
        assert_eq!(request.executable(), "systemctl");
        assert_eq!(
            request.arguments(),
            ["--user", "restart", "myna-desktop.service"]
        );
    }
}
