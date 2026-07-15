//! Open an authorization URL in the system browser.
//!
//! Spawn failures are soft: callers already surface the URL via `on_auth`
//! (printed-URL fallback). No library-side console I/O.

use std::process::Command;

/// Attempt to open `url` with the platform browser opener.
///
/// Returns `true` when the spawn succeeded (the process may still fail later).
/// Returns `false` when no opener could be started — caller should rely on the
/// already-delivered auth URL (printed-URL fallback).
pub fn open_url(url: &str) -> bool {
    open_url_with(url, default_open_command)
}

fn default_open_command(url: &str) -> Option<Command> {
    #[cfg(target_os = "macos")]
    {
        let mut cmd = Command::new("open");
        cmd.arg(url);
        return Some(cmd);
    }
    #[cfg(target_os = "windows")]
    {
        let mut cmd = Command::new("cmd");
        cmd.args(["/C", "start", "", url]);
        return Some(cmd);
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        for program in ["xdg-open", "gio", "gnome-open", "kde-open"] {
            if which_exists(program) {
                let mut cmd = Command::new(program);
                if program == "gio" {
                    cmd.args(["open", url]);
                } else {
                    cmd.arg(url);
                }
                return Some(cmd);
            }
        }
        None
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn which_exists(program: &str) -> bool {
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|paths| std::env::split_paths(&paths).collect::<Vec<_>>())
        .any(|dir| dir.join(program).is_file())
}

/// Test seam: inject a custom command builder.
pub fn open_url_with<F>(url: &str, mut build: F) -> bool
where
    F: FnMut(&str) -> Option<Command>,
{
    let Some(mut cmd) = build(url) else {
        return false;
    };
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .is_ok()
}
