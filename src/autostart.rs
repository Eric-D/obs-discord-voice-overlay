// On Linux dev VMs we don't link the tray subsystem (no GTK), so the
// enable/disable surface goes unused. The functions remain compiled +
// tested for parity with the Windows / macOS build.
#![cfg_attr(
    not(any(target_os = "windows", target_os = "macos")),
    allow(dead_code)
)]

//! Autostart wrapper around the `auto-launch` crate.
//!
//! Errors are non-fatal: the caller logs them and leaves UI state unchanged.
//! No local persistence — every check goes through the OS (registry on
//! Windows, Login Items on macOS, autostart `.desktop` on Linux).

use std::env;
use std::path::PathBuf;

use auto_launch::{AutoLaunch, AutoLaunchBuilder};
use thiserror::Error;

/// Distinct app name used for the OS autostart entry.
const APP_NAME: &str = "obs-discord-voice-overlay";

#[derive(Debug, Error)]
pub enum AutostartError {
    #[error("could not determine current executable path: {0}")]
    CurrentExe(#[source] std::io::Error),
    #[error("auto-launch builder failed: {0}")]
    Builder(String),
    #[error("auto-launch operation failed: {0}")]
    Op(String),
}

/// Strip a leading Windows extended-length path prefix (`\\?\`) if present.
///
/// Some autostart consumers (notably the Windows registry path used by
/// `auto-launch`) reject the `\\?\` form even though `current_exe()` may
/// produce it. The check is a no-op on platforms whose paths never carry
/// that prefix (Linux, macOS).
fn strip_extended_prefix(path: &str) -> &str {
    if let Some(rest) = path.strip_prefix(r"\\?\") {
        rest
    } else {
        path
    }
}

fn build() -> Result<AutoLaunch, AutostartError> {
    let exe: PathBuf = env::current_exe().map_err(AutostartError::CurrentExe)?;
    let exe_str = exe.to_string_lossy().to_string();
    let exe_str = strip_extended_prefix(&exe_str);
    AutoLaunchBuilder::new()
        .set_app_name(APP_NAME)
        .set_app_path(exe_str)
        .build()
        .map_err(|e| AutostartError::Builder(e.to_string()))
}

/// Returns whether autostart is currently enabled in the OS.
///
/// Errors propagate to the caller so genuine OS failures are visible
/// (and don't masquerade as a clean "off" state, which would otherwise
/// invite an infinite re-enable loop on a flaky query).
pub fn is_enabled() -> Result<bool, AutostartError> {
    let al = build()?;
    al.is_enabled().map_err(|e| AutostartError::Op(e.to_string()))
}

/// Creates the OS autostart entry pointing at the current binary.
pub fn enable() -> Result<(), AutostartError> {
    let al = build()?;
    al.enable().map_err(|e| AutostartError::Op(e.to_string()))
}

/// Removes the OS autostart entry.
pub fn disable() -> Result<(), AutostartError> {
    let al = build()?;
    al.disable().map_err(|e| AutostartError::Op(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_enabled_returns_a_value() {
        // Smoke test: must not panic. The OS-side state is environment-
        // dependent (CI vs. dev box vs. user box) so we assert nothing
        // about the boolean — only that the call returns *something*.
        let _ = is_enabled();
    }

    #[test]
    fn strip_extended_prefix_removes_windows_prefix() {
        assert_eq!(
            strip_extended_prefix(r"\\?\C:\Program Files\app.exe"),
            r"C:\Program Files\app.exe"
        );
    }

    #[test]
    fn strip_extended_prefix_is_noop_without_prefix() {
        assert_eq!(
            strip_extended_prefix(r"C:\Program Files\app.exe"),
            r"C:\Program Files\app.exe"
        );
        assert_eq!(strip_extended_prefix("/usr/bin/app"), "/usr/bin/app");
    }
}
