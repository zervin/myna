//! How to start the HUD: `$MYNA_HUD_BINARY` if set (a build tree's binary,
//! which a packaged snap must not shadow), else a distribution package's
//! `/usr/libexec/myna/myna-hud`, else the snap's `myna.hud` app, hosting
//! itself as the session's display protocol needs. The order is the shell
//! extension's (`extensions/myna-shell/resolve.js`).

use myna_platform::SessionKind;

pub const OVERRIDE_ENV: &str = "MYNA_HUD_BINARY";
/// The renderer a distribution package (a deb, not the snap) installs. It
/// exists only when that package was installed on purpose, while `snap` may
/// be on a host that never installed Myna's snap, so it is tried first; it
/// is in libexec, off `PATH`, so a stray build is never picked up.
pub const PACKAGED_HUD: &str = "/usr/libexec/myna/myna-hud";

/// `myna-hud --host` for a session: a layer surface on Wayland, else an X11
/// window, which is also what a session naming no display gets.
pub fn host(kind: SessionKind) -> &'static str {
    match kind {
        SessionKind::Wayland => "layer-shell",
        SessionKind::X11 | SessionKind::Unknown => "x11",
    }
}

/// What to run to host the HUD in a `kind` session, or why nothing can.
pub fn argv(
    kind: SessionKind,
    getenv: impl Fn(&str) -> Option<String>,
    is_executable: impl Fn(&str) -> bool,
) -> Result<Vec<String>, String> {
    let host = ["--host", host(kind)].map(str::to_owned);
    let mut argv = match getenv(OVERRIDE_ENV).filter(|path| !path.is_empty()) {
        // A bad override is a configuration error, never a silent fallback.
        Some(path) if is_executable(&path) => vec![path],
        Some(path) => return Err(format!("{OVERRIDE_ENV} is not executable: {path}")),
        None if is_executable(PACKAGED_HUD) => vec![PACKAGED_HUD.to_owned()],
        None if is_executable("snap") || is_executable("/usr/bin/snap") => {
            ["snap", "run", "myna.hud"].map(str::to_owned).to_vec()
        }
        None => return Err(format!("neither {PACKAGED_HUD} nor snap is installed")),
    };
    argv.extend(host);
    Ok(argv)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &'static [(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        move |name| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| (*value).to_owned())
        }
    }

    #[test]
    fn without_a_packaged_hud_the_snap_app_runs() {
        let got = argv(SessionKind::X11, env(&[]), |path| path == "/usr/bin/snap").unwrap();
        assert_eq!(got, ["snap", "run", "myna.hud", "--host", "x11"]);
    }

    #[test]
    fn a_packaged_hud_wins_over_the_snap() {
        let got = argv(SessionKind::Wayland, env(&[]), |_| true).unwrap();
        assert_eq!(got, [PACKAGED_HUD, "--host", "layer-shell"]);
    }

    #[test]
    fn the_override_wins_over_a_packaged_hud() {
        let got = argv(
            SessionKind::X11,
            env(&[(OVERRIDE_ENV, "/t/myna-hud")]),
            |_| true,
        )
        .unwrap();
        assert_eq!(got, ["/t/myna-hud", "--host", "x11"]);
    }

    #[test]
    fn the_override_wins_over_the_snap() {
        let got = argv(
            SessionKind::X11,
            env(&[(OVERRIDE_ENV, "/t/myna-hud")]),
            |_| true,
        )
        .unwrap();
        assert_eq!(got, ["/t/myna-hud", "--host", "x11"]);
    }

    #[test]
    fn a_missing_override_is_an_error_not_a_fallback() {
        let error = argv(
            SessionKind::X11,
            env(&[(OVERRIDE_ENV, "/t/none")]),
            |path| path != "/t/none",
        )
        .unwrap_err();
        assert!(error.contains("/t/none"));
    }

    #[test]
    fn an_empty_override_is_unset() {
        let got = argv(SessionKind::X11, env(&[(OVERRIDE_ENV, "")]), |path| {
            path == "snap"
        })
        .unwrap();
        assert_eq!(got[0], "snap");
    }

    #[test]
    fn a_wayland_session_gets_a_layer_surface() {
        let got = argv(SessionKind::Wayland, env(&[]), |_| true).unwrap();
        assert_eq!(got[got.len() - 2..], ["--host", "layer-shell"]);
        assert_eq!(host(SessionKind::Unknown), "x11");
    }

    #[test]
    fn no_snap_and_no_override_is_an_error() {
        assert!(argv(SessionKind::X11, env(&[]), |_| false).is_err());
    }
}
