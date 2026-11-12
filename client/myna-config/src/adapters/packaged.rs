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
/// The packaged client, which [`is_packaged_install`] looks for.
pub const PACKAGED_CLIENT: &str = "/usr/bin/myna-desktop";
const LAUNCHER_PREFIX: &str = "myna-server-";
const CLIENT_UNIT: &str = "myna-desktop.service";
const SERVER_UNIT: &str = "myna-server.service";
/// Packages own model selection; this names it in every refusal.
const PACKAGED_ENGINES: &str =
    r#"{"active-engine":"cpu","engines":[{"name":"cpu","compatible":true}]}"#;
const UNSUPPORTED: &str = "a packaged install changes models through the package manager";

/// True when Myna was installed from packages and snapd is not running:
/// the client binary is there and snapd's socket is not.
pub fn is_packaged_install() -> bool {
    Path::new(PACKAGED_CLIENT).exists() && !Path::new("/run/snapd.socket").exists()
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
        _cancellation: CancellationToken,
    ) -> Result<Vec<InstalledSnap>, BackendSurfaceError> {
        let mut installed = vec![InstalledSnap {
            name: crate::onboarding::MYNA_SNAP.to_owned(),
            version: "packaged".to_owned(),
        }];
        installed.extend(installed_families(&self.libexec).into_iter().map(|family| {
            InstalledSnap {
                name: family.snap_name().to_owned(),
                version: "packaged".to_owned(),
            }
        }));
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

fn unsupported() -> SystemConfiguratorError {
    SystemConfiguratorError::Execution {
        step: crate::ports::FailedStep::Command {
            executable: "apt".to_owned(),
            arguments: Vec::new(),
            exit_status: None,
            stderr: String::new(),
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
        Err(SystemConfiguratorFailure::new(Vec::new(), unsupported()))
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
        Err(SystemConfiguratorFailure::new(Vec::new(), unsupported()))
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
        Err(unsupported())
    }

    async fn install_snap(
        &self,
        _snap: &str,
        _cancellation: CancellationToken,
    ) -> Result<Option<String>, SystemConfiguratorError> {
        Err(unsupported())
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
