//! App / config path resolution.
//!
//! Port of `packages/coding-agent/src/config.ts` path + naming surface
//! (`APP_NAME`, `CONFIG_DIR_NAME`, agent/session dirs, env overrides).

use std::env;
use std::path::{Path, PathBuf};

/// Compile-time app name (default `"pi"`). Override via `PI_APP_NAME` at build time.
pub const APP_NAME: &str = match option_env!("PI_APP_NAME") {
    Some(v) => v,
    None => "pi",
};

/// Compile-time config dir name under `$HOME` (default `".pi"`).
/// Override via `PI_CONFIG_DIR` at build time.
pub const CONFIG_DIR_NAME: &str = match option_env!("PI_CONFIG_DIR") {
    Some(v) => v,
    None => ".pi",
};

/// Package display name fallback (oracle reads package.json; we keep the published default).
pub const PACKAGE_NAME: &str = "@earendil-works/pi-coding-agent";

/// Runtime env key for agent dir override (e.g. `PI_CODING_AGENT_DIR`).
pub fn env_agent_dir_key() -> String {
    format!("{}_CODING_AGENT_DIR", APP_NAME.to_ascii_uppercase())
}

/// Runtime env key for session dir override (e.g. `PI_CODING_AGENT_SESSION_DIR`).
pub fn env_session_dir_key() -> String {
    format!("{}_CODING_AGENT_SESSION_DIR", APP_NAME.to_ascii_uppercase())
}

/// Expand a leading `~/` to the home directory. Other paths are returned as-is.
pub fn expand_tilde_path(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    } else if path == "~"
        && let Some(home) = dirs::home_dir()
    {
        return home;
    }
    PathBuf::from(path)
}

/// Normalize a path string the way pi does for config paths (tilde expand + absolute if possible).
pub fn normalize_path(path: &str) -> PathBuf {
    let expanded = expand_tilde_path(path);
    if expanded.is_absolute() {
        return expanded;
    }
    env::current_dir()
        .map(|cwd| cwd.join(&expanded))
        .unwrap_or(expanded)
}

/// Resolve a path: absolute as-is (after tilde), relative against `base` (default cwd).
pub fn resolve_path(path: &str, base: Option<&Path>) -> PathBuf {
    let expanded = expand_tilde_path(path);
    if expanded.is_absolute() {
        return expanded;
    }
    match base {
        Some(b) => b.join(expanded),
        None => env::current_dir()
            .map(|cwd| cwd.join(&expanded))
            .unwrap_or(expanded),
    }
}

/// `~/.pi/agent` (or `PI_CODING_AGENT_DIR` / rebranded env).
pub fn get_agent_dir() -> PathBuf {
    if let Ok(env_dir) = env::var(env_agent_dir_key())
        && !env_dir.is_empty()
    {
        return expand_tilde_path(&env_dir);
    }
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    home.join(CONFIG_DIR_NAME).join("agent")
}

/// Sessions root: `~/.pi/agent/sessions`.
pub fn get_sessions_dir() -> PathBuf {
    get_agent_dir().join("sessions")
}

/// `~/.pi/agent/settings.json`
pub fn get_settings_path() -> PathBuf {
    get_agent_dir().join("settings.json")
}

/// `~/.pi/agent/auth.json`
pub fn get_auth_path() -> PathBuf {
    get_agent_dir().join("auth.json")
}

/// `~/.pi/agent/models.json`
pub fn get_models_path() -> PathBuf {
    get_agent_dir().join("models.json")
}

/// `~/.pi/agent/themes`
pub fn get_custom_themes_dir() -> PathBuf {
    get_agent_dir().join("themes")
}

/// `~/.pi/agent/tools`
pub fn get_tools_dir() -> PathBuf {
    get_agent_dir().join("tools")
}

/// `~/.pi/agent/bin`
pub fn get_bin_dir() -> PathBuf {
    get_agent_dir().join("bin")
}

/// `~/.pi/agent/prompts`
pub fn get_prompts_dir() -> PathBuf {
    get_agent_dir().join("prompts")
}

/// Debug log path under agent dir.
pub fn get_debug_log_path() -> PathBuf {
    get_agent_dir().join(format!("{APP_NAME}-debug.log"))
}

/// Package asset root. Honors `PI_PACKAGE_DIR`; otherwise directory of current executable.
pub fn get_package_dir() -> PathBuf {
    if let Ok(env_dir) = env::var("PI_PACKAGE_DIR")
        && !env_dir.is_empty()
    {
        return normalize_path(&env_dir);
    }
    env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Encode a cwd into the session subdirectory name used under `sessions/`.
///
/// Matches session-manager.ts: `` `--${cwd.replace(/^[/\\]/, "").replace(/[/\\:]/g, "-")}--` ``.
pub fn encode_session_cwd(cwd: &str) -> String {
    let stripped = cwd.trim_start_matches(['/', '\\']);
    let safe: String = stripped
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' => '-',
            other => other,
        })
        .collect();
    format!("--{safe}--")
}

/// Default session directory for a working directory: `~/.pi/agent/sessions/<encoded-cwd>/`.
pub fn get_default_session_dir_path(cwd: &str, agent_dir: Option<&Path>) -> PathBuf {
    let agent = agent_dir
        .map(Path::to_path_buf)
        .unwrap_or_else(get_agent_dir);
    let resolved_cwd = resolve_path(cwd, None);
    let cwd_str = resolved_cwd.to_string_lossy();
    agent.join("sessions").join(encode_session_cwd(&cwd_str))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_session_cwd_matches_oracle() {
        assert_eq!(
            encode_session_cwd("/Users/badlogic/workspaces/pi-mono"),
            "--Users-badlogic-workspaces-pi-mono--"
        );
        // "C:\\foo\\bar" → replace ':' and '\\' → "C--foo-bar"
        assert_eq!(encode_session_cwd("C:\\foo\\bar"), "--C--foo-bar--");
    }

    #[test]
    fn app_name_defaults() {
        assert_eq!(APP_NAME, "pi");
        assert_eq!(CONFIG_DIR_NAME, ".pi");
        assert_eq!(env_agent_dir_key(), "PI_CODING_AGENT_DIR");
        assert_eq!(env_session_dir_key(), "PI_CODING_AGENT_SESSION_DIR");
    }
}
