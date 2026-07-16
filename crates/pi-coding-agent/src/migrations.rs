//! One-time startup migrations (port of `migrations.ts`).
//!
//! All migrations are idempotent: re-running after pi or pi-rust must not
//! destructively re-apply completed moves.

use crate::config::{CONFIG_DIR_NAME, get_agent_dir, get_bin_dir, get_settings_path};
use serde_json::{Map, Value};
use std::fs;
use std::path::Path;

/// Result of [`run_migrations`].
#[derive(Clone, Debug, Default)]
pub struct MigrationResult {
    pub migrated_auth_providers: Vec<String>,
    pub deprecation_warnings: Vec<String>,
}

/// Migrate legacy `oauth.json` + `settings.json` apiKeys → `auth.json`.
///
/// Skips entirely when `auth.json` already exists.
pub fn migrate_auth_to_auth_json(agent_dir: &Path) -> Vec<String> {
    let auth_path = agent_dir.join("auth.json");
    if auth_path.exists() {
        return Vec::new();
    }
    let oauth_path = agent_dir.join("oauth.json");
    let settings_path = agent_dir.join("settings.json");
    let mut migrated: Map<String, Value> = Map::new();
    let mut providers = Vec::new();

    if oauth_path.exists()
        && let Ok(text) = fs::read_to_string(&oauth_path)
        && let Ok(Value::Object(oauth)) = serde_json::from_str::<Value>(&text)
    {
        for (provider, cred) in oauth {
            if let Value::Object(mut c) = cred {
                c.insert("type".into(), Value::String("oauth".into()));
                // ensure type is first-ish: rebuild
                let mut ordered = Map::new();
                ordered.insert("type".into(), Value::String("oauth".into()));
                for (k, v) in c {
                    if k != "type" {
                        ordered.insert(k, v);
                    }
                }
                migrated.insert(provider.clone(), Value::Object(ordered));
                providers.push(provider);
            }
        }
        let _ = fs::rename(&oauth_path, format!("{}.migrated", oauth_path.display()));
    }

    if settings_path.exists()
        && let Ok(text) = fs::read_to_string(&settings_path)
        && let Ok(Value::Object(mut settings)) = serde_json::from_str::<Value>(&text)
        && let Some(Value::Object(api_keys)) = settings.remove("apiKeys")
    {
        for (provider, key) in api_keys {
            if migrated.contains_key(&provider) {
                continue;
            }
            if let Value::String(k) = key {
                let mut obj = Map::new();
                obj.insert("type".into(), Value::String("api_key".into()));
                obj.insert("key".into(), Value::String(k));
                migrated.insert(provider.clone(), Value::Object(obj));
                providers.push(provider);
            }
        }
        let pretty =
            serde_json::to_string_pretty(&Value::Object(settings)).unwrap_or_else(|_| "{}".into());
        let _ = fs::write(&settings_path, pretty);
    }

    if !migrated.is_empty() {
        if let Some(parent) = auth_path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let pretty =
            serde_json::to_string_pretty(&Value::Object(migrated)).unwrap_or_else(|_| "{}".into());
        let _ = fs::write(&auth_path, pretty);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&auth_path, fs::Permissions::from_mode(0o600));
        }
    }
    providers
}

/// Move `~/.pi/agent/*.jsonl` into `sessions/<encoded-cwd>/` based on header cwd.
pub fn migrate_sessions_from_agent_root(agent_dir: &Path) {
    let Ok(rd) = fs::read_dir(agent_dir) else {
        return;
    };
    for ent in rd.flatten() {
        let path = ent.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        // only direct children of agentDir
        if path.parent() != Some(agent_dir) {
            continue;
        }
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        let first = text.lines().next().unwrap_or("");
        let Ok(header) = serde_json::from_str::<Value>(first) else {
            continue;
        };
        if header.get("type").and_then(|t| t.as_str()) != Some("session") {
            continue;
        }
        let Some(cwd) = header.get("cwd").and_then(|c| c.as_str()) else {
            continue;
        };
        let safe = crate::config::encode_session_cwd(cwd);
        let correct_dir = agent_dir.join("sessions").join(safe);
        let _ = fs::create_dir_all(&correct_dir);
        let file_name = match path.file_name() {
            Some(n) => n.to_owned(),
            None => continue,
        };
        let new_path = correct_dir.join(file_name);
        if new_path.exists() {
            continue;
        }
        let _ = fs::rename(&path, &new_path);
    }
}

fn migrate_commands_to_prompts(base_dir: &Path) -> bool {
    let commands = base_dir.join("commands");
    let prompts = base_dir.join("prompts");
    if commands.exists() && !prompts.exists() {
        return fs::rename(&commands, &prompts).is_ok();
    }
    false
}

fn migrate_tools_to_bin(agent_dir: &Path) {
    let tools_dir = agent_dir.join("tools");
    let bin_dir = agent_dir.join("bin");
    if !tools_dir.exists() {
        return;
    }
    for bin in ["fd", "rg", "fd.exe", "rg.exe"] {
        let old_path = tools_dir.join(bin);
        let new_path = bin_dir.join(bin);
        if !old_path.exists() {
            continue;
        }
        let _ = fs::create_dir_all(&bin_dir);
        if !new_path.exists() {
            let _ = fs::rename(&old_path, &new_path);
        } else {
            let _ = fs::remove_file(&old_path);
        }
    }
}

fn check_deprecated_extension_dirs(base_dir: &Path, label: &str) -> Vec<String> {
    let mut warnings = Vec::new();
    if base_dir.join("hooks").exists() {
        warnings.push(format!(
            "{label} hooks/ directory found. Hooks have been renamed to extensions."
        ));
    }
    let tools_dir = base_dir.join("tools");
    if tools_dir.exists()
        && let Ok(rd) = fs::read_dir(&tools_dir)
    {
        let custom: Vec<_> = rd
            .flatten()
            .filter(|e| {
                let name = e.file_name().to_string_lossy().to_ascii_lowercase();
                !matches!(name.as_str(), "fd" | "rg" | "fd.exe" | "rg.exe")
                    && !name.starts_with('.')
            })
            .collect();
        if !custom.is_empty() {
            warnings.push(format!(
                    "{label} tools/ directory contains custom tools. Custom tools have been merged into extensions."
                ));
        }
    }
    warnings
}

fn migrate_extension_system(cwd: &Path, agent_dir: &Path) -> Vec<String> {
    let project_dir = cwd.join(CONFIG_DIR_NAME);
    let _ = migrate_commands_to_prompts(agent_dir);
    let _ = migrate_commands_to_prompts(&project_dir);
    let mut warnings = check_deprecated_extension_dirs(agent_dir, "Global");
    warnings.extend(check_deprecated_extension_dirs(&project_dir, "Project"));
    warnings
}

/// Run all startup migrations (idempotent).
pub fn run_migrations(cwd: impl AsRef<Path>) -> MigrationResult {
    let agent_dir = get_agent_dir();
    let migrated_auth_providers = migrate_auth_to_auth_json(&agent_dir);
    migrate_sessions_from_agent_root(&agent_dir);
    migrate_tools_to_bin(&agent_dir);
    // keybindings migration requires keybindings module — skip body when file absent / leave for later
    let deprecation_warnings = migrate_extension_system(cwd.as_ref(), &agent_dir);
    let _ = get_settings_path();
    let _ = get_bin_dir();
    MigrationResult {
        migrated_auth_providers,
        deprecation_warnings,
    }
}

/// Run migrations with an explicit agent dir (tests).
pub fn run_migrations_with_agent_dir(cwd: &Path, agent_dir: &Path) -> MigrationResult {
    let migrated_auth_providers = migrate_auth_to_auth_json(agent_dir);
    migrate_sessions_from_agent_root(agent_dir);
    migrate_tools_to_bin(agent_dir);
    let deprecation_warnings = migrate_extension_system(cwd, agent_dir);
    MigrationResult {
        migrated_auth_providers,
        deprecation_warnings,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn auth_migration_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let agent = dir.path();
        fs::write(
            agent.join("oauth.json"),
            r#"{"anthropic":{"access":"a","refresh":"r","expires":1}}"#,
        )
        .unwrap();
        let p1 = migrate_auth_to_auth_json(agent);
        assert_eq!(p1, vec!["anthropic".to_string()]);
        assert!(agent.join("auth.json").exists());
        assert!(agent.join("oauth.json.migrated").exists());
        let p2 = migrate_auth_to_auth_json(agent);
        assert!(p2.is_empty());
    }

    #[test]
    fn session_root_migration_moves_file() {
        let dir = tempfile::tempdir().unwrap();
        let agent = dir.path();
        let session = agent.join("old.jsonl");
        let mut f = fs::File::create(&session).unwrap();
        writeln!(
            f,
            r#"{{"type":"session","id":"x","timestamp":"t","cwd":"/tmp/proj"}}"#
        )
        .unwrap();
        migrate_sessions_from_agent_root(agent);
        assert!(!session.exists());
        let dest = agent
            .join("sessions")
            .join("--tmp-proj--")
            .join("old.jsonl");
        assert!(dest.exists());
    }
}
