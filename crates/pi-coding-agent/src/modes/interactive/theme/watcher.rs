//! Custom-theme file watcher (oracle `startThemeWatcher`).
//!
//! Watches `<agent_dir>/themes/<current>.json` and hot-reloads the global
//! theme on change (100ms debounce in the oracle; mtime polling here — the
//! Rust binary replaces `fs.watch` with a poll thread to avoid an inotify
//! dependency).

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, SystemTime};

use crate::config::get_custom_themes_dir;

/// Poll interval; the oracle debounces fs.watch events by 100ms.
const POLL_INTERVAL: Duration = Duration::from_millis(100);

struct WatchState {
    /// Generation counter: bumping it invalidates the running watcher thread.
    generation: u64,
    watched: Option<(String, PathBuf)>,
}

static WATCHER: LazyLock<Mutex<WatchState>> = LazyLock::new(|| {
    Mutex::new(WatchState {
        generation: 0,
        watched: None,
    })
});

static GENERATION: AtomicU64 = AtomicU64::new(0);

/// Oracle `startThemeWatcher`: watch the current custom theme (builtin
/// dark/light are embedded and never watched).
pub fn start_theme_watcher() {
    stop_theme_watcher();
    let Some(name) = super::current_theme_name() else {
        return;
    };
    if name == "dark" || name == "light" || name == "<in-memory>" {
        return;
    }
    let theme_file = get_custom_themes_dir().join(format!("{name}.json"));
    if !theme_file.exists() {
        return;
    }
    let generation = GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
    {
        let mut state = WATCHER.lock().unwrap_or_else(|e| e.into_inner());
        state.generation = generation;
        state.watched = Some((name.clone(), theme_file.clone()));
    }
    std::thread::Builder::new()
        .name("pi-theme-watcher".to_owned())
        .spawn(move || {
            let mut last_mtime = mtime(&theme_file);
            loop {
                std::thread::sleep(POLL_INTERVAL);
                {
                    let state = WATCHER.lock().unwrap_or_else(|e| e.into_inner());
                    if state.generation != generation {
                        return; // superseded or stopped
                    }
                }
                // Keep the last successfully loaded theme if the file is
                // temporarily missing (oracle behavior).
                let Some(current) = mtime(&theme_file) else {
                    continue;
                };
                if Some(current) != last_mtime {
                    last_mtime = Some(current);
                    super::reload_watched_theme(&name, &theme_file);
                }
            }
        })
        .ok();
}

/// Oracle `stopThemeWatcher`.
pub fn stop_theme_watcher() {
    let generation = GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
    let mut state = WATCHER.lock().unwrap_or_else(|e| e.into_inner());
    state.generation = generation;
    state.watched = None;
}

fn mtime(path: &std::path::Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}
