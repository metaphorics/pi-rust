//! Byte-compatible `trust.json` persistence for the interactive `/trust` flow.
//!
//! Port of `core/trust-manager.ts` `ProjectTrustStore`. Keys are canonical
//! absolute paths, values are booleans, and output is sorted/pretty JSON with a
//! trailing newline.

use std::collections::BTreeMap;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::components::trust_selector::{ProjectTrustStoreEntry, ProjectTrustUpdate};
/// Oracle `hasTrustRequiringProjectResources` (trust-manager.ts:184-206):
/// the cwd carries project resources whose loading requires trust —
/// `.pi/{settings.json,extensions,skills,prompts,themes}` or a non-HOME
/// ancestor `.agents/skills`.
#[must_use]
pub fn has_trust_requiring_project_resources(cwd: &Path) -> bool {
    const TRUST_REQUIRING: [&str; 5] =
        ["settings.json", "extensions", "skills", "prompts", "themes"];
    let config_dir = cwd.join(crate::config::CONFIG_DIR_NAME);
    if TRUST_REQUIRING
        .iter()
        .any(|entry| config_dir.join(entry).exists())
    {
        return true;
    }
    let user_agents_skills = dirs::home_dir().map(|home| home.join(".agents").join("skills"));
    let mut current = cwd.to_path_buf();
    loop {
        let agents_skills = current.join(".agents").join("skills");
        if Some(&agents_skills) != user_agents_skills.as_ref() && agents_skills.exists() {
            return true;
        }
        if !current.pop() {
            return false;
        }
    }
}

pub struct ProjectTrustStore {
    path: PathBuf,
}

impl ProjectTrustStore {
    pub fn new(agent_dir: &Path) -> Self {
        Self {
            path: agent_dir.join("trust.json"),
        }
    }

    pub fn get_entry(&self, cwd: &Path) -> Result<Option<ProjectTrustStoreEntry>, String> {
        let _lock = TrustLock::acquire(&self.path)?;
        let data = read_store(&self.path)?;
        let mut current = normalize(cwd);
        loop {
            if let Some(decision) = data.get(&current) {
                return Ok(Some(ProjectTrustStoreEntry {
                    path: current.display().to_string(),
                    decision: *decision,
                }));
            }
            let Some(parent) = current.parent() else {
                return Ok(None);
            };
            if parent == current {
                return Ok(None);
            }
            current = parent.to_path_buf();
        }
    }

    pub fn set_many(&self, updates: &[ProjectTrustUpdate]) -> Result<(), String> {
        let _lock = TrustLock::acquire(&self.path)?;
        let mut data = read_store(&self.path)?;
        for update in updates {
            let key = normalize(Path::new(&update.path));
            if let Some(decision) = update.decision {
                data.insert(key, decision);
            } else {
                data.remove(&key);
            }
        }
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
        let wire: BTreeMap<String, bool> = data
            .into_iter()
            .map(|(path, decision)| (path.display().to_string(), decision))
            .collect();
        let mut text = serde_json::to_string_pretty(&wire).map_err(|error| error.to_string())?;
        text.push('\n');
        fs::write(&self.path, text).map_err(|error| error.to_string())
    }
}

fn normalize(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    };
    fs::canonicalize(&absolute).unwrap_or(absolute)
}

fn read_store(path: &Path) -> Result<BTreeMap<PathBuf, bool>, String> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(error) => {
            return Err(format!(
                "Failed to read trust store {}: {error}",
                path.display()
            ));
        }
    };
    let value: serde_json::Value = serde_json::from_str(&text)
        .map_err(|error| format!("Failed to read trust store {}: {error}", path.display()))?;
    let object = value
        .as_object()
        .ok_or_else(|| format!("Invalid trust store {}: expected an object", path.display()))?;
    let mut data = BTreeMap::new();
    for (key, value) in object {
        if value.is_null() {
            continue;
        }
        let decision = value.as_bool().ok_or_else(|| {
            format!(
                "Invalid trust store {}: value for {} must be true, false, or null",
                path.display(),
                serde_json::to_string(key).unwrap_or_else(|_| format!("\"{key}\""))
            )
        })?;
        data.insert(PathBuf::from(key), decision);
    }
    Ok(data)
}

struct TrustLock {
    path: PathBuf,
}

impl TrustLock {
    fn acquire(store_path: &Path) -> Result<Self, String> {
        if let Some(parent) = store_path.parent() {
            fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
        let mut lock_path = store_path.as_os_str().to_os_string();
        lock_path.push(".lock");
        let path = PathBuf::from(lock_path);
        for attempt in 0..10 {
            match fs::create_dir(&path) {
                Ok(()) => return Ok(Self { path }),
                Err(error) if error.kind() == ErrorKind::AlreadyExists && attempt < 9 => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(error) => return Err(error.to_string()),
            }
        }
        Err("Failed to acquire trust store lock".to_owned())
    }
}

impl Drop for TrustLock {
    fn drop(&mut self) {
        let _ = fs::remove_dir(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nearest_entry_and_sorted_wire_format_match_oracle() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("parent/project");
        fs::create_dir_all(&project).unwrap();
        let store = ProjectTrustStore::new(dir.path());
        store
            .set_many(&[
                ProjectTrustUpdate {
                    path: project.display().to_string(),
                    decision: Some(false),
                },
                ProjectTrustUpdate {
                    path: project.parent().unwrap().display().to_string(),
                    decision: Some(true),
                },
            ])
            .unwrap();
        let entry = store.get_entry(&project).unwrap().unwrap();
        assert_eq!(entry.path, project.display().to_string());
        assert!(!entry.decision);
        let wire = fs::read_to_string(dir.path().join("trust.json")).unwrap();
        assert!(wire.ends_with('\n'));
        let parent_key =
            serde_json::to_string(&project.parent().unwrap().display().to_string()).unwrap();
        let project_key = serde_json::to_string(&project.display().to_string()).unwrap();
        assert!(wire.find(&parent_key).unwrap() < wire.find(&project_key).unwrap());
    }
}
