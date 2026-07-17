//! CLI session selection — port of `main.ts:143-355`
//! (`createSessionManager`, `resolveSessionPath` consumers, confirm prompt).
//!
//! This is a printer module of the `pi` binary. All user-facing output here
//! goes to STDERR: stdout may already belong to a wire mode (json/print
//! piping), matching the oracle's takeOverStdout redirection.

use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::Arc;

use parking_lot::Mutex;

use crate::session_manager::{ResolvedSession, SessionManager, resolve_session_path};
use crate::settings_manager::SettingsManager;

use super::args::Args;
use super::session_picker::{SessionPick, select_session};

fn exit_error(message: &str) -> ! {
    eprintln!("\x1b[31m{message}\x1b[39m");
    std::process::exit(1);
}

/// Oracle `promptConfirm`: `[y/N]` readline confirmation on stdin.
fn prompt_confirm(message: &str) -> bool {
    eprint!("{message} [y/N] ");
    let _ = std::io::stderr().flush();
    let mut answer = String::new();
    if std::io::stdin().lock().read_line(&mut answer).is_err() {
        return false;
    }
    let answer = answer.trim().to_lowercase();
    answer == "y" || answer == "yes"
}

fn open_session_or_exit(path: &PathBuf, session_dir: Option<PathBuf>) -> SessionManager {
    match SessionManager::open(path, session_dir, None) {
        Ok(manager) => manager,
        Err(error) => exit_error(&format!("Error: {error}")),
    }
}

fn fork_session_or_exit(
    source_path: &PathBuf,
    cwd: &str,
    session_dir: Option<PathBuf>,
    session_id: Option<&str>,
) -> SessionManager {
    let options = session_id.map(|id| crate::session_manager::NewSessionOptions {
        id: Some(id.to_string()),
        ..Default::default()
    });
    match SessionManager::fork_from(source_path, cwd, session_dir, options) {
        Ok(manager) => manager,
        Err(error) => exit_error(&format!("Error: {error}")),
    }
}

fn find_local_session_by_exact_id(
    session_id: &str,
    cwd: &str,
    session_dir: Option<PathBuf>,
) -> Option<PathBuf> {
    SessionManager::list(cwd, session_dir, None)
        .ok()?
        .into_iter()
        .find(|s| s.id == session_id)
        .map(|s| s.path)
}

fn resolve_or_exit(arg: &str, cwd: &str, session_dir: Option<PathBuf>) -> ResolvedSession {
    match resolve_session_path(arg, cwd, session_dir) {
        Ok(resolved) => resolved,
        Err(error) => exit_error(&format!("Error: {error}")),
    }
}

fn new_session_options(
    session_id: Option<&str>,
) -> Option<crate::session_manager::NewSessionOptions> {
    session_id.map(|id| crate::session_manager::NewSessionOptions {
        id: Some(id.to_string()),
        ..Default::default()
    })
}

/// Run the `--resume` interactive picker over a startup Tui.
/// Returns the chosen session path; `None` = user cancelled (exit 0 upstream).
fn pick_session_or_exit(
    cwd: &str,
    session_dir: Option<PathBuf>,
    agent_dir: &std::path::Path,
    settings_manager: &Arc<Mutex<SettingsManager>>,
) -> PathBuf {
    let terminal = pi_tui::terminal::ProcessTerminal::new();
    let mut ui = super::startup_ui::create_startup_tui(agent_dir, settings_manager, terminal);
    let pick = select_session(&mut ui, cwd, session_dir);
    ui.stop();
    crate::modes::interactive::theme::watcher::stop_theme_watcher();
    match pick {
        SessionPick::Selected(path) => path,
        SessionPick::Cancelled | SessionPick::Quit => {
            eprintln!("\x1b[2mNo session selected\x1b[22m");
            std::process::exit(0);
        }
    }
}

/// Resolve the session manager from CLI flags (oracle `createSessionManager`,
/// main.ts:264-355). Precedence: noSession/help/listModels → in-memory;
/// fork; --session; --resume picker; --continue; --session-id; new session.
///
/// Error paths print the oracle strings and exit.
pub fn create_session_manager(
    parsed: &Args,
    cwd: &str,
    session_dir: Option<PathBuf>,
    agent_dir: &std::path::Path,
    settings_manager: &Arc<Mutex<SettingsManager>>,
) -> SessionManager {
    let in_memory = || {
        SessionManager::in_memory(Some(cwd), new_session_options(parsed.session_id.as_deref()))
            .unwrap_or_else(|error| exit_error(&format!("Error: {error}")))
    };
    if parsed.no_session || parsed.help || parsed.list_models.is_some() {
        return in_memory();
    }

    if let Some(fork_arg) = &parsed.fork {
        if let Some(session_id) = &parsed.session_id
            && find_local_session_by_exact_id(session_id, cwd, session_dir.clone()).is_some()
        {
            exit_error(&format!("Session already exists with id '{session_id}'"));
        }
        return match resolve_or_exit(fork_arg, cwd, session_dir.clone()) {
            ResolvedSession::Path(path)
            | ResolvedSession::Local(path)
            | ResolvedSession::Global { path, .. } => {
                fork_session_or_exit(&path, cwd, session_dir, parsed.session_id.as_deref())
            }
            ResolvedSession::NotFound(arg) => {
                exit_error(&format!("No session found matching '{arg}'"))
            }
        };
    }

    if let Some(session_arg) = &parsed.session {
        return match resolve_or_exit(session_arg, cwd, session_dir.clone()) {
            ResolvedSession::Path(path) | ResolvedSession::Local(path) => {
                open_session_or_exit(&path, session_dir)
            }
            ResolvedSession::Global {
                path,
                cwd: other_cwd,
            } => {
                eprintln!("\x1b[33mSession found in different project: {other_cwd}\x1b[39m");
                if !prompt_confirm("Fork this session into current directory?") {
                    eprintln!("\x1b[2mAborted.\x1b[22m");
                    std::process::exit(0);
                }
                fork_session_or_exit(&path, cwd, session_dir, None)
            }
            ResolvedSession::NotFound(arg) => {
                exit_error(&format!("No session found matching '{arg}'"))
            }
        };
    }

    if parsed.resume {
        let path = pick_session_or_exit(cwd, session_dir.clone(), agent_dir, settings_manager);
        return open_session_or_exit(&path, session_dir);
    }

    if parsed.r#continue {
        return SessionManager::continue_recent(cwd, session_dir)
            .unwrap_or_else(|error| exit_error(&format!("Error: {error}")));
    }

    if let Some(session_id) = &parsed.session_id {
        if let Some(path) = find_local_session_by_exact_id(session_id, cwd, session_dir.clone()) {
            return open_session_or_exit(&path, session_dir);
        }
        eprintln!(
            "\x1b[33mWarning: No project session found with id '{session_id}'; creating a new session with that id.\x1b[39m"
        );
    }

    SessionManager::create(
        cwd,
        session_dir,
        new_session_options(parsed.session_id.as_deref()),
    )
    .unwrap_or_else(|error| exit_error(&format!("Error: {error}")))
}
