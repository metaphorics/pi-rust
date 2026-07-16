//! Settings manager against `~/.pi/agent/settings.json` and project `.pi/settings.json`.
//!
//! Port of `packages/coding-agent/src/core/settings-manager.ts` (load/migrate/merge surface).

use crate::config::{CONFIG_DIR_NAME, get_agent_dir, resolve_path};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;
use thiserror::Error;

/// Result alias for settings operations.
pub type Result<T, E = SettingsError> = std::result::Result<T, E>;

#[derive(Debug, Error)]
pub enum SettingsError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("{0}")]
    Message(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettingsScope {
    Global,
    Project,
}

/// Settings document — open object so unknown keys and field order round-trip.
///
/// Known fields are documented on the TypeScript `Settings` interface; we store
/// the document as an ordered map for byte-compat and expose typed getters.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Settings(pub Map<String, Value>);

impl Settings {
    pub fn new() -> Self {
        Self(Map::new())
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        self.0.get(key)
    }

    pub fn get_str(&self, key: &str) -> Option<&str> {
        self.0.get(key).and_then(|v| v.as_str())
    }

    pub fn get_bool(&self, key: &str) -> Option<bool> {
        self.0.get(key).and_then(|v| v.as_bool())
    }

    pub fn insert(&mut self, key: impl Into<String>, value: Value) {
        self.0.insert(key.into(), value);
    }

    pub fn as_map(&self) -> &Map<String, Value> {
        &self.0
    }

    pub fn as_map_mut(&mut self) -> &mut Map<String, Value> {
        &mut self.0
    }
}

/// Deep merge: overrides win; nested objects merge one level (oracle `deepMergeSettings`).
pub fn deep_merge_settings(base: &Settings, overrides: &Settings) -> Settings {
    let mut result = base.clone();
    for (key, override_value) in &overrides.0 {
        if override_value.is_null() {
            // still apply null? TS skips undefined only; null is an object-ish — apply.
        }
        let base_value = result.0.get(key);
        if override_value.is_object()
            && !override_value
                .as_object()
                .map(|o| o.is_empty())
                .unwrap_or(true)
            && base_value.map(|b| b.is_object()).unwrap_or(false)
        {
            let mut merged = base_value
                .and_then(|b| b.as_object())
                .cloned()
                .unwrap_or_default();
            if let Some(ov) = override_value.as_object() {
                for (nk, nv) in ov {
                    merged.insert(nk.clone(), nv.clone());
                }
            }
            result.0.insert(key.clone(), Value::Object(merged));
        } else {
            result.0.insert(key.clone(), override_value.clone());
        }
    }
    result
}

/// Migrate old settings shape to current (oracle `migrateSettings`). Idempotent.
pub fn migrate_settings(mut settings: Map<String, Value>) -> Settings {
    // queueMode → steeringMode only when the current key is absent.
    if settings.contains_key("queueMode")
        && !settings.contains_key("steeringMode")
        && let Some(value) = settings.remove("queueMode")
    {
        settings.insert("steeringMode".into(), value);
    }

    // websockets boolean → transport enum only when the current key is absent.
    if !settings.contains_key("transport")
        && settings.get("websockets").is_some_and(Value::is_boolean)
        && let Some(Value::Bool(websockets)) = settings.remove("websockets")
    {
        settings.insert(
            "transport".into(),
            Value::String(if websockets { "websocket" } else { "sse" }.into()),
        );
    }

    // skills object → array + enableSkillCommands
    if let Some(Value::Object(skills_obj)) = settings.get("skills").cloned()
        && !skills_obj.contains_key("0")
    {
        // treat as object form (not array — arrays deserialize as Array)
    }
    if let Some(Value::Object(skills_obj)) = settings.get("skills").cloned() {
        // Only migrate object form
        if let Some(enable) = skills_obj.get("enableSkillCommands")
            && !settings.contains_key("enableSkillCommands")
        {
            settings.insert("enableSkillCommands".into(), enable.clone());
        }
        if let Some(Value::Array(dirs)) = skills_obj.get("customDirectories") {
            if !dirs.is_empty() {
                settings.insert("skills".into(), Value::Array(dirs.clone()));
            } else {
                settings.remove("skills");
            }
        } else {
            settings.remove("skills");
        }
    }

    // retry.maxDelayMs → retry.provider.maxRetryDelayMs
    if let Some(Value::Object(mut retry)) = settings.get("retry").cloned() {
        let provider = retry
            .get("provider")
            .and_then(|p| p.as_object())
            .cloned()
            .unwrap_or_default();
        if let Some(max_delay) = retry.get("maxDelayMs").cloned() {
            let needs = provider
                .get("maxRetryDelayMs")
                .map(|v| v.is_null())
                .unwrap_or(true);
            if needs && max_delay.is_number() {
                let mut provider = provider;
                provider.insert("maxRetryDelayMs".into(), max_delay);
                retry.insert("provider".into(), Value::Object(provider));
            }
        }
        retry.remove("maxDelayMs");
        settings.insert("retry".into(), Value::Object(retry));
    }

    Settings(settings)
}

fn parse_settings_text(content: &str) -> Result<Settings> {
    let value: Value = serde_json::from_str(content)?;
    let map = match value {
        Value::Object(m) => m,
        _ => Map::new(),
    };
    Ok(migrate_settings(map))
}

const LOCK_ATTEMPTS: usize = 10;
const LOCK_RETRY_DELAY: Duration = Duration::from_millis(20);
const LOCK_STALE_AFTER: Duration = Duration::from_secs(10);

struct SettingsFileLock {
    path: Option<PathBuf>,
}

impl SettingsFileLock {
    fn acquire(settings_path: &Path) -> std::io::Result<Self> {
        let mut lock_name = settings_path.as_os_str().to_owned();
        lock_name.push(".lock");
        let path = PathBuf::from(lock_name);

        for attempt in 0..LOCK_ATTEMPTS {
            match fs::create_dir(&path) {
                Ok(()) => return Ok(Self { path: Some(path) }),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    let stale = fs::metadata(&path)
                        .and_then(|metadata| metadata.modified())
                        .and_then(|modified| modified.elapsed().map_err(std::io::Error::other))
                        .is_ok_and(|age| age >= LOCK_STALE_AFTER);
                    if stale {
                        match fs::remove_dir(&path) {
                            Ok(()) => continue,
                            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                            Err(_) => {}
                        }
                    }
                    if attempt + 1 == LOCK_ATTEMPTS {
                        return Err(error);
                    }
                    thread::sleep(LOCK_RETRY_DELAY);
                }
                Err(error) => return Err(error),
            }
        }
        unreachable!("lock acquisition loop always returns")
    }

    fn release(mut self) -> std::io::Result<()> {
        let path = self
            .path
            .take()
            .expect("lock path is present until release");
        fs::remove_dir(path)
    }
}

impl Drop for SettingsFileLock {
    fn drop(&mut self) {
        if let Some(path) = self.path.take() {
            let _ = fs::remove_dir(path);
        }
    }
}

/// Storage backend for settings scopes.
pub trait SettingsStorage {
    fn with_lock(
        &mut self,
        scope: SettingsScope,
        f: &mut dyn FnMut(Option<&str>) -> Option<String>,
    ) -> Result<()>;
}

/// File-backed settings storage.
pub struct FileSettingsStorage {
    global_settings_path: PathBuf,
    project_settings_path: PathBuf,
}

impl FileSettingsStorage {
    pub fn new(cwd: impl AsRef<Path>, agent_dir: impl AsRef<Path>) -> Self {
        let cwd = resolve_path(&cwd.as_ref().to_string_lossy(), None);
        let agent_dir = resolve_path(&agent_dir.as_ref().to_string_lossy(), None);
        Self {
            global_settings_path: agent_dir.join("settings.json"),
            project_settings_path: cwd.join(CONFIG_DIR_NAME).join("settings.json"),
        }
    }

    fn path(&self, scope: SettingsScope) -> &Path {
        match scope {
            SettingsScope::Global => &self.global_settings_path,
            SettingsScope::Project => &self.project_settings_path,
        }
    }
}

impl SettingsStorage for FileSettingsStorage {
    fn with_lock(
        &mut self,
        scope: SettingsScope,
        f: &mut dyn FnMut(Option<&str>) -> Option<String>,
    ) -> Result<()> {
        let path = self.path(scope).to_path_buf();
        let file_exists = path.exists();
        let mut lock = file_exists
            .then(|| SettingsFileLock::acquire(&path))
            .transpose()?;
        let current = file_exists.then(|| fs::read_to_string(&path)).transpose()?;
        let mut next = f(current.as_deref());

        if next.is_some() {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            if lock.is_none() {
                lock = Some(SettingsFileLock::acquire(&path)?);
                if path.exists() {
                    let current = fs::read_to_string(&path)?;
                    next = f(Some(&current));
                }
            }
            if let Some(content) = next {
                fs::write(&path, content)?;
            }
        }
        if let Some(lock) = lock {
            lock.release()?;
        }
        Ok(())
    }
}

/// In-memory settings storage (tests / ephemeral).
#[derive(Default)]
pub struct InMemorySettingsStorage {
    global: Option<String>,
    project: Option<String>,
}

impl SettingsStorage for InMemorySettingsStorage {
    fn with_lock(
        &mut self,
        scope: SettingsScope,
        f: &mut dyn FnMut(Option<&str>) -> Option<String>,
    ) -> Result<()> {
        let current = match scope {
            SettingsScope::Global => self.global.as_deref(),
            SettingsScope::Project => self.project.as_deref(),
        };
        let next = f(current);
        if let Some(content) = next {
            match scope {
                SettingsScope::Global => self.global = Some(content),
                SettingsScope::Project => self.project = Some(content),
            }
        }
        Ok(())
    }
}

/// Loaded settings with global/project merge.
pub struct SettingsManager {
    storage: Box<dyn SettingsStorage + Send>,
    global_settings: Settings,
    project_settings: Settings,
    settings: Settings,
    project_trusted: bool,
    modified_fields: HashSet<String>,
    modified_nested_fields: HashMap<String, HashSet<String>>,
    modified_project_fields: HashSet<String>,
    modified_project_nested_fields: HashMap<String, HashSet<String>>,
    global_settings_load_error: Option<String>,
    project_settings_load_error: Option<String>,
}

impl SettingsManager {
    fn from_parts(
        storage: Box<dyn SettingsStorage + Send>,
        global: Settings,
        project: Settings,
        global_err: Option<String>,
        project_err: Option<String>,
        project_trusted: bool,
    ) -> Self {
        let settings = deep_merge_settings(&global, &project);
        Self {
            storage,
            global_settings: global,
            project_settings: project,
            settings,
            project_trusted,
            modified_fields: HashSet::new(),
            modified_nested_fields: HashMap::new(),
            modified_project_fields: HashSet::new(),
            modified_project_nested_fields: HashMap::new(),
            global_settings_load_error: global_err,
            project_settings_load_error: project_err,
        }
    }

    fn try_load(
        storage: &mut dyn SettingsStorage,
        scope: SettingsScope,
        project_trusted: bool,
    ) -> (Settings, Option<String>) {
        if scope == SettingsScope::Project && !project_trusted {
            return (Settings::new(), None);
        }
        let mut content: Option<String> = None;
        if let Err(error) = storage.with_lock(scope, &mut |current| {
            content = current.map(str::to_string);
            None
        }) {
            return (Settings::new(), Some(error.to_string()));
        }
        match content {
            None => (Settings::new(), None),
            Some(text) => match parse_settings_text(&text) {
                Ok(s) => (s, None),
                Err(e) => (Settings::new(), Some(e.to_string())),
            },
        }
    }

    pub fn create(cwd: impl AsRef<Path>, agent_dir: Option<PathBuf>) -> Self {
        let agent = agent_dir.unwrap_or_else(get_agent_dir);
        let mut storage = FileSettingsStorage::new(cwd, agent);
        let project_trusted = true;
        let (global, gerr) = Self::try_load(&mut storage, SettingsScope::Global, project_trusted);
        let (project, perr) = Self::try_load(&mut storage, SettingsScope::Project, project_trusted);
        Self::from_parts(
            Box::new(storage),
            global,
            project,
            gerr,
            perr,
            project_trusted,
        )
    }

    pub fn from_storage(
        mut storage: Box<dyn SettingsStorage + Send>,
        project_trusted: bool,
    ) -> Self {
        let (global, gerr) =
            Self::try_load(storage.as_mut(), SettingsScope::Global, project_trusted);
        let (project, perr) =
            Self::try_load(storage.as_mut(), SettingsScope::Project, project_trusted);
        Self::from_parts(storage, global, project, gerr, perr, project_trusted)
    }

    pub fn in_memory(settings: Settings, project_trusted: bool) -> Self {
        let mut storage = InMemorySettingsStorage::default();
        let text = serde_json::to_string_pretty(&settings.0).unwrap_or_else(|_| "{}".into());
        storage
            .with_lock(SettingsScope::Global, &mut |_| Some(text.clone()))
            .expect("in-memory settings storage cannot fail");
        Self::from_storage(Box::new(storage), project_trusted)
    }

    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    pub fn global_settings(&self) -> &Settings {
        &self.global_settings
    }

    pub fn project_settings(&self) -> &Settings {
        &self.project_settings
    }

    pub fn is_project_trusted(&self) -> bool {
        self.project_trusted
    }

    pub fn set_project_trusted(&mut self, trusted: bool) {
        if self.project_trusted == trusted {
            return;
        }
        self.project_trusted = trusted;
        self.modified_project_fields.clear();
        self.modified_project_nested_fields.clear();
        if !trusted {
            self.project_settings = Settings::new();
            self.project_settings_load_error = None;
            self.settings = deep_merge_settings(&self.global_settings, &self.project_settings);
            return;
        }
        let (project, perr) = Self::try_load(self.storage.as_mut(), SettingsScope::Project, true);
        self.project_settings = project;
        self.project_settings_load_error = perr;
        self.settings = deep_merge_settings(&self.global_settings, &self.project_settings);
    }

    pub fn reload(&mut self) {
        let (global, gerr) = Self::try_load(self.storage.as_mut(), SettingsScope::Global, true);
        if gerr.is_none() {
            self.global_settings = global;
            self.global_settings_load_error = None;
        } else {
            self.global_settings_load_error = gerr;
        }
        self.modified_fields.clear();
        self.modified_nested_fields.clear();
        self.modified_project_fields.clear();
        self.modified_project_nested_fields.clear();
        let (project, perr) = Self::try_load(
            self.storage.as_mut(),
            SettingsScope::Project,
            self.project_trusted,
        );
        if perr.is_none() {
            self.project_settings = project;
            self.project_settings_load_error = None;
        } else {
            self.project_settings_load_error = perr;
        }
        self.settings = deep_merge_settings(&self.global_settings, &self.project_settings);
    }

    pub fn apply_overrides(&mut self, overrides: &Settings) {
        self.settings = deep_merge_settings(&self.settings, overrides);
    }

    fn mark_modified(&mut self, field: &str, nested: Option<&str>) {
        self.modified_fields.insert(field.to_string());
        if let Some(n) = nested {
            self.modified_nested_fields
                .entry(field.to_string())
                .or_default()
                .insert(n.to_string());
        }
    }

    fn save_global(&mut self) {
        self.settings = deep_merge_settings(&self.global_settings, &self.project_settings);
        if self.global_settings_load_error.is_some() {
            return;
        }
        let snapshot = self.global_settings.clone();
        let modified = self.modified_fields.clone();
        let nested = self.modified_nested_fields.clone();
        Self::persist_scoped(
            self.storage.as_mut(),
            SettingsScope::Global,
            &snapshot,
            &modified,
            &nested,
        )
        .expect("failed to persist global settings");
        self.modified_fields.clear();
        self.modified_nested_fields.clear();
    }

    fn persist_scoped(
        storage: &mut dyn SettingsStorage,
        scope: SettingsScope,
        snapshot: &Settings,
        modified_fields: &HashSet<String>,
        modified_nested: &HashMap<String, HashSet<String>>,
    ) -> Result<()> {
        storage.with_lock(scope, &mut |current| {
            let mut current_file = match current {
                Some(text) => parse_settings_text(text).unwrap_or_default(),
                None => Settings::new(),
            };
            for field in modified_fields {
                let value = snapshot.get(field);
                if let Some(nested_keys) = modified_nested.get(field)
                    && let Some(Value::Object(in_mem)) = value
                {
                    let mut base_nested = current_file
                        .get(field)
                        .and_then(|v| v.as_object())
                        .cloned()
                        .unwrap_or_default();
                    for nk in nested_keys {
                        if let Some(nv) = in_mem.get(nk) {
                            base_nested.insert(nk.clone(), nv.clone());
                        }
                    }
                    current_file.insert(field.clone(), Value::Object(base_nested));
                    continue;
                }
                if let Some(v) = value {
                    current_file.insert(field.clone(), v.clone());
                }
            }
            // Match TS JSON.stringify(obj, null, 2) — pretty, no trailing newline.
            Some(pretty_json_map(&current_file.0))
        })
    }

    // --- typed getters matching oracle defaults ---

    pub fn get_default_provider(&self) -> Option<&str> {
        self.settings.get_str("defaultProvider")
    }

    pub fn get_default_model(&self) -> Option<&str> {
        self.settings.get_str("defaultModel")
    }

    pub fn set_default_provider(&mut self, provider: impl Into<String>) {
        self.global_settings
            .insert("defaultProvider", Value::String(provider.into()));
        self.mark_modified("defaultProvider", None);
        self.save_global();
    }

    pub fn set_default_model(&mut self, model_id: impl Into<String>) {
        self.global_settings
            .insert("defaultModel", Value::String(model_id.into()));
        self.mark_modified("defaultModel", None);
        self.save_global();
    }

    pub fn get_steering_mode(&self) -> &str {
        self.settings
            .get_str("steeringMode")
            .unwrap_or("one-at-a-time")
    }
    /// Oracle `setSteeringMode` (settings-manager.ts:707).
    pub fn set_steering_mode(&mut self, mode: impl Into<String>) {
        self.global_settings
            .insert("steeringMode", Value::String(mode.into()));
        self.mark_modified("steeringMode", None);
        self.save_global();
    }

    pub fn get_follow_up_mode(&self) -> &str {
        self.settings
            .get_str("followUpMode")
            .unwrap_or("one-at-a-time")
    }
    /// Oracle `setFollowUpMode` (settings-manager.ts:717).
    pub fn set_follow_up_mode(&mut self, mode: impl Into<String>) {
        self.global_settings
            .insert("followUpMode", Value::String(mode.into()));
        self.mark_modified("followUpMode", None);
        self.save_global();
    }

    /// Persist one top-level global setting while preserving unknown keys.
    pub(crate) fn set_global_value(&mut self, key: &str, value: Value) {
        self.global_settings.insert(key.to_owned(), value);
        self.mark_modified(key, None);
        self.save_global();
    }

    /// Persist one nested global setting while preserving sibling keys.
    pub(crate) fn set_global_nested_value(&mut self, section: &str, key: &str, value: Value) {
        let object = self
            .global_settings
            .0
            .entry(section.to_owned())
            .or_insert_with(|| Value::Object(Map::new()));
        if !object.is_object() {
            *object = Value::Object(Map::new());
        }
        object
            .as_object_mut()
            .expect("object initialized above")
            .insert(key.to_owned(), value);
        self.mark_modified(section, Some(key));
        self.save_global();
    }

    pub fn get_theme(&self) -> Option<&str> {
        let theme = self.settings.get_str("theme")?;
        if theme.contains('/') {
            None
        } else {
            Some(theme)
        }
    }

    pub fn set_theme(&mut self, theme: impl Into<String>) {
        self.global_settings
            .insert("theme", Value::String(theme.into()));
        self.mark_modified("theme", None);
        self.save_global();
    }

    pub fn get_default_thinking_level(&self) -> Option<&str> {
        self.settings.get_str("defaultThinkingLevel")
    }
    /// Oracle `setDefaultThinkingLevel` (settings-manager.ts:744).
    pub fn set_default_thinking_level(&mut self, level: impl Into<String>) {
        self.global_settings
            .insert("defaultThinkingLevel", Value::String(level.into()));
        self.mark_modified("defaultThinkingLevel", None);
        self.save_global();
    }

    pub fn get_transport(&self) -> &str {
        self.settings.get_str("transport").unwrap_or("auto")
    }

    pub fn get_compaction_enabled(&self) -> bool {
        self.settings
            .get("compaction")
            .and_then(|c| c.get("enabled"))
            .and_then(|v| v.as_bool())
            .unwrap_or(true)
    }
    /// Oracle `setCompactionEnabled` (settings-manager.ts:764).
    pub fn set_compaction_enabled(&mut self, enabled: bool) {
        let mut compaction = self
            .global_settings
            .get("compaction")
            .and_then(|v| v.as_object())
            .cloned()
            .unwrap_or_default();
        compaction.insert("enabled".into(), Value::Bool(enabled));
        self.global_settings
            .insert("compaction", Value::Object(compaction));
        self.mark_modified("compaction", Some("enabled"));
        self.save_global();
    }

    pub fn get_compaction_reserve_tokens(&self) -> u64 {
        self.settings
            .get("compaction")
            .and_then(|c| c.get("reserveTokens"))
            .and_then(|v| v.as_u64())
            .unwrap_or(16384)
    }

    pub fn get_compaction_keep_recent_tokens(&self) -> u64 {
        self.settings
            .get("compaction")
            .and_then(|c| c.get("keepRecentTokens"))
            .and_then(|v| v.as_u64())
            .unwrap_or(20000)
    }

    pub fn get_retry_enabled(&self) -> bool {
        self.settings
            .get("retry")
            .and_then(|c| c.get("enabled"))
            .and_then(|v| v.as_bool())
            .unwrap_or(true)
    }
    /// Oracle `setRetryEnabled` (settings-manager.ts:804).
    pub fn set_retry_enabled(&mut self, enabled: bool) {
        let mut retry = self
            .global_settings
            .get("retry")
            .and_then(|v| v.as_object())
            .cloned()
            .unwrap_or_default();
        retry.insert("enabled".into(), Value::Bool(enabled));
        self.global_settings.insert("retry", Value::Object(retry));
        self.mark_modified("retry", Some("enabled"));
        self.save_global();
    }

    pub fn get_extensions(&self) -> Vec<String> {
        self.settings
            .get("extensions")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn get_skills(&self) -> Vec<String> {
        self.settings
            .get("skills")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn get_prompts(&self) -> Vec<String> {
        self.settings
            .get("prompts")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn get_themes(&self) -> Vec<String> {
        self.settings
            .get("themes")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn get_session_dir(&self) -> Option<String> {
        self.settings.get_str("sessionDir").map(|s| {
            crate::config::normalize_path(s)
                .to_string_lossy()
                .into_owned()
        })
    }

    pub fn get_packages(&self) -> Vec<Value> {
        self.settings
            .get("packages")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default()
    }

    fn mark_project_modified(&mut self, field: &str, nested: Option<&str>) {
        self.modified_project_fields.insert(field.to_string());
        if let Some(n) = nested {
            self.modified_project_nested_fields
                .entry(field.to_string())
                .or_default()
                .insert(n.to_string());
        }
    }

    fn save_project(&mut self) -> Result<()> {
        if !self.project_trusted {
            return Err(SettingsError::Message(
                "Project is not trusted; refusing to write project settings".to_string(),
            ));
        }
        self.settings = deep_merge_settings(&self.global_settings, &self.project_settings);
        if self.project_settings_load_error.is_some() {
            return Ok(());
        }
        let snapshot = self.project_settings.clone();
        let modified = self.modified_project_fields.clone();
        let nested = self.modified_project_nested_fields.clone();
        Self::persist_scoped(
            self.storage.as_mut(),
            SettingsScope::Project,
            &snapshot,
            &modified,
            &nested,
        )?;
        self.modified_project_fields.clear();
        self.modified_project_nested_fields.clear();
        Ok(())
    }

    fn update_project_settings<F>(&mut self, field: &str, mut update: F) -> Result<()>
    where
        F: FnMut(&mut Settings),
    {
        if !self.project_trusted {
            return Err(SettingsError::Message(
                "Project is not trusted; refusing to write project settings".to_string(),
            ));
        }
        let mut proj = self.project_settings.clone();
        update(&mut proj);
        self.project_settings = proj;
        self.mark_project_modified(field, None);
        self.save_project()
    }

    // --- resource-list setters ---

    pub fn set_packages(&mut self, packages: Vec<Value>) {
        self.global_settings
            .insert("packages", Value::Array(packages));
        self.mark_modified("packages", None);
        self.save_global();
    }

    pub fn set_project_packages(&mut self, packages: Vec<Value>) -> Result<()> {
        self.update_project_settings("packages", |settings| {
            settings.insert("packages", Value::Array(packages.clone()));
        })
    }

    pub fn set_extension_paths(&mut self, paths: Vec<String>) {
        let arr = paths.into_iter().map(Value::String).collect();
        self.global_settings.insert("extensions", Value::Array(arr));
        self.mark_modified("extensions", None);
        self.save_global();
    }

    pub fn set_project_extension_paths(&mut self, paths: Vec<String>) -> Result<()> {
        self.update_project_settings("extensions", |settings| {
            let arr = paths.iter().map(|p| Value::String(p.clone())).collect();
            settings.insert("extensions", Value::Array(arr));
        })
    }

    pub fn set_skill_paths(&mut self, paths: Vec<String>) {
        let arr = paths.into_iter().map(Value::String).collect();
        self.global_settings.insert("skills", Value::Array(arr));
        self.mark_modified("skills", None);
        self.save_global();
    }

    pub fn set_project_skill_paths(&mut self, paths: Vec<String>) -> Result<()> {
        self.update_project_settings("skills", |settings| {
            let arr = paths.iter().map(|p| Value::String(p.clone())).collect();
            settings.insert("skills", Value::Array(arr));
        })
    }

    pub fn set_prompt_template_paths(&mut self, paths: Vec<String>) {
        let arr = paths.into_iter().map(Value::String).collect();
        self.global_settings.insert("prompts", Value::Array(arr));
        self.mark_modified("prompts", None);
        self.save_global();
    }

    pub fn set_project_prompt_template_paths(&mut self, paths: Vec<String>) -> Result<()> {
        self.update_project_settings("prompts", |settings| {
            let arr = paths.iter().map(|p| Value::String(p.clone())).collect();
            settings.insert("prompts", Value::Array(arr));
        })
    }

    pub fn set_theme_paths(&mut self, paths: Vec<String>) {
        let arr = paths.into_iter().map(Value::String).collect();
        self.global_settings.insert("themes", Value::Array(arr));
        self.mark_modified("themes", None);
        self.save_global();
    }

    pub fn set_project_theme_paths(&mut self, paths: Vec<String>) -> Result<()> {
        self.update_project_settings("themes", |settings| {
            let arr = paths.iter().map(|p| Value::String(p.clone())).collect();
            settings.insert("themes", Value::Array(arr));
        })
    }

    // --- seven typed getters ---

    pub fn get_npm_command(&self) -> Option<Vec<String>> {
        self.settings
            .get("npmCommand")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
    }

    pub fn get_enabled_models(&self) -> Option<Vec<String>> {
        self.settings
            .get("enabledModels")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
    }

    pub fn get_default_project_trust(&self) -> &str {
        match self.global_settings.get_str("defaultProjectTrust") {
            Some("always") => "always",
            Some("never") => "never",
            _ => "ask",
        }
    }

    pub fn get_image_auto_resize(&self) -> bool {
        self.settings
            .get("images")
            .and_then(|i| i.get("autoResize"))
            .and_then(|v| v.as_bool())
            .unwrap_or(true)
    }

    pub fn get_http_idle_timeout_ms(&self) -> u64 {
        if let Some(val) = self.settings.get("httpIdleTimeoutMs")
            && let Some(timeout_ms) = parse_http_idle_timeout_ms(val)
        {
            return timeout_ms;
        }
        300000 // Default 300s (5 minutes)
    }

    pub fn get_quiet_startup(&self) -> bool {
        self.settings.get_bool("quietStartup").unwrap_or(false)
    }

    /// Oracle `getShowTerminalProgress`: `terminal.showTerminalProgress ?? false`.
    pub fn get_show_terminal_progress(&self) -> bool {
        self.settings
            .get("terminal")
            .and_then(|t| t.get("showTerminalProgress"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    }

    /// Oracle `getDoubleEscapeAction`: `doubleEscapeAction ?? "tree"`.
    pub fn get_double_escape_action(&self) -> String {
        self.settings
            .get_str("doubleEscapeAction")
            .unwrap_or("tree")
            .to_owned()
    }

    pub fn http_proxy(&self) -> Option<&str> {
        self.settings.get_str("httpProxy")
    }
}

/// Pretty-print a JSON object like `JSON.stringify(obj, null, 2)`.
pub fn pretty_json_map(map: &Map<String, Value>) -> String {
    serde_json::to_string_pretty(&Value::Object(map.clone())).unwrap_or_else(|_| "{}".into())
}

/// Parse settings JSON text into a migrated [`Settings`] (public for goldens).
pub fn parse_settings_json(text: &str) -> Result<Settings> {
    parse_settings_text(text)
}

/// Serialize settings with the same pretty layout pi uses for settings.json.
pub fn serialize_settings_json(settings: &Settings) -> String {
    pretty_json_map(&settings.0)
}

fn parse_http_idle_timeout_ms(value: &Value) -> Option<u64> {
    match value {
        Value::String(s) => {
            let trimmed = s.trim();
            if trimmed.eq_ignore_ascii_case("disabled") {
                return Some(0);
            }
            if trimmed.is_empty() {
                return None;
            }
            if let Ok(num) = trimmed.parse::<f64>()
                && num.is_finite()
                && num >= 0.0
            {
                return Some(num as u64);
            }
            None
        }
        Value::Number(n) => {
            if let Some(i) = n.as_u64() {
                Some(i)
            } else if let Some(f) = n.as_f64() {
                if f.is_finite() && f >= 0.0 {
                    Some(f as u64)
                } else {
                    None
                }
            } else {
                None
            }
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migration_preserves_legacy_keys_when_current_keys_exist() {
        let settings = migrate_settings(
            serde_json::json!({
                "queueMode": "all",
                "steeringMode": "one-at-a-time",
                "websockets": true,
                "transport": "sse"
            })
            .as_object()
            .expect("object")
            .clone(),
        );
        assert_eq!(settings.get_str("queueMode"), Some("all"));
        assert_eq!(settings.get_str("steeringMode"), Some("one-at-a-time"));
        assert_eq!(settings.get_bool("websockets"), Some(true));
        assert_eq!(settings.get_str("transport"), Some("sse"));
    }
    #[test]
    fn session_runtime_setters_persist_and_preserve_unknown_keys() {
        let temp = tempfile::tempdir().expect("tempdir");
        let agent_dir = temp.path().join("agent");
        let cwd = temp.path().join("project");
        std::fs::create_dir_all(&agent_dir).expect("agent dir");
        // Pre-existing settings.json with unknown top-level and nested keys.
        std::fs::write(
            agent_dir.join("settings.json"),
            r#"{
  "someUnknownKey": {"keep": true},
  "compaction": {"reserveTokens": 32768, "unknownNested": 1},
  "retry": {"maxRetries": 7}
}"#,
        )
        .expect("seed settings");

        let mut manager = SettingsManager::create(&cwd, Some(agent_dir.clone()));
        manager.set_steering_mode("all");
        manager.set_follow_up_mode("all");
        manager.set_default_thinking_level("high");
        manager.set_compaction_enabled(false);
        manager.set_retry_enabled(false);

        // Effective view reflects the changes immediately.
        assert_eq!(manager.get_steering_mode(), "all");
        assert_eq!(manager.get_follow_up_mode(), "all");
        assert_eq!(manager.get_default_thinking_level(), Some("high"));
        assert!(!manager.get_compaction_enabled());
        assert!(!manager.get_retry_enabled());

        // A fresh manager re-reads the persisted file.
        let reloaded = SettingsManager::create(&cwd, Some(agent_dir.clone()));
        assert_eq!(reloaded.get_steering_mode(), "all");
        assert_eq!(reloaded.get_follow_up_mode(), "all");
        assert_eq!(reloaded.get_default_thinking_level(), Some("high"));
        assert!(!reloaded.get_compaction_enabled());
        assert!(!reloaded.get_retry_enabled());

        // Unknown keys and untouched nested fields survive the writes.
        let raw: Value = serde_json::from_str(
            &std::fs::read_to_string(agent_dir.join("settings.json")).expect("read"),
        )
        .expect("json");
        assert_eq!(raw["someUnknownKey"]["keep"], Value::Bool(true));
        assert_eq!(raw["compaction"]["reserveTokens"], 32768);
        assert_eq!(raw["compaction"]["unknownNested"], 1);
        assert_eq!(raw["compaction"]["enabled"], Value::Bool(false));
        assert_eq!(raw["retry"]["maxRetries"], 7);
        assert_eq!(raw["retry"]["enabled"], Value::Bool(false));
    }

    #[test]
    fn concurrent_first_writes_merge_under_the_file_lock() {
        use std::sync::{Arc, Barrier};

        let temp = tempfile::tempdir().expect("tempdir");
        let agent_dir = temp.path().join("agent");
        let cwd = temp.path().join("project");
        let barrier = Arc::new(Barrier::new(2));
        let mut threads = Vec::new();

        for key in ["first", "second"] {
            let agent_dir = agent_dir.clone();
            let cwd = cwd.clone();
            let barrier = Arc::clone(&barrier);
            threads.push(thread::spawn(move || {
                let mut storage = FileSettingsStorage::new(cwd, agent_dir);
                let mut first_call = true;
                storage
                    .with_lock(SettingsScope::Global, &mut |current| {
                        if first_call {
                            first_call = false;
                            barrier.wait();
                        }
                        let mut map = current
                            .and_then(|text| serde_json::from_str::<Value>(text).ok())
                            .and_then(|value| value.as_object().cloned())
                            .unwrap_or_default();
                        map.insert(key.into(), Value::Bool(true));
                        Some(pretty_json_map(&map))
                    })
                    .expect("write settings");
            }));
        }
        for thread in threads {
            thread.join().expect("writer thread");
        }

        let content =
            fs::read_to_string(agent_dir.join("settings.json")).expect("read settings file");
        let value: Value = serde_json::from_str(&content).expect("parse settings file");
        assert_eq!(value["first"], true);
        assert_eq!(value["second"], true);
    }

    #[test]
    fn migrate_queue_mode_idempotent() {
        let mut m = Map::new();
        m.insert("queueMode".into(), Value::String("all".into()));
        let s = migrate_settings(m);
        assert_eq!(s.get_str("steeringMode"), Some("all"));
        assert!(s.get("queueMode").is_none());
        // second migrate
        let s2 = migrate_settings(s.0.clone());
        assert_eq!(s2.get_str("steeringMode"), Some("all"));
    }

    #[test]
    fn migrate_websockets_boolean_to_transport_when_absent() {
        let mut m = Map::new();
        m.insert("websockets".into(), Value::Bool(true));
        assert_eq!(migrate_settings(m).get_str("transport"), Some("websocket"));
        let mut m = Map::new();
        m.insert("websockets".into(), Value::Bool(false));
        let s = migrate_settings(m);
        assert_eq!(s.get_str("transport"), Some("sse"));
        assert!(s.get("websockets").is_none());
    }

    #[test]
    fn migrate_websockets_preserves_non_boolean_when_transport_absent() {
        let mut m = Map::new();
        m.insert("websockets".into(), Value::String("auto".into()));
        let s = migrate_settings(m);
        assert_eq!(s.get_str("websockets"), Some("auto"));
        assert!(s.get("transport").is_none());

        let mut m = Map::new();
        m.insert("websockets".into(), serde_json::json!({"mode":"auto"}));
        let s = migrate_settings(m);
        assert!(s.get("websockets").unwrap().is_object());
        assert!(s.get("transport").is_none());
    }

    #[test]
    fn migrate_retry_numeric_promotes_and_removes_legacy_key() {
        let mut m = Map::new();
        m.insert("retry".into(), serde_json::json!({"maxDelayMs": 5000}));
        let s = migrate_settings(m);
        assert_eq!(s.get("retry").unwrap()["provider"]["maxRetryDelayMs"], 5000);
        assert!(s.get("retry").unwrap().get("maxDelayMs").is_none());
    }

    #[test]
    fn migrate_retry_numeric_promotes_when_provider_max_retry_delay_ms_null() {
        let mut m = Map::new();
        m.insert(
            "retry".into(),
            serde_json::json!({"maxDelayMs": 3000, "provider": {"maxRetryDelayMs": null}}),
        );
        let s = migrate_settings(m);
        assert_eq!(s.get("retry").unwrap()["provider"]["maxRetryDelayMs"], 3000);
        assert!(s.get("retry").unwrap().get("maxDelayMs").is_none());
    }

    #[test]
    fn migrate_retry_non_numeric_removes_key_without_promoting() {
        let mut m = Map::new();
        m.insert("retry".into(), serde_json::json!({"maxDelayMs": "soon"}));
        let s = migrate_settings(m);
        let prov = s.get("retry").unwrap().get("provider");
        assert!(prov.is_none() || prov.unwrap().get("maxRetryDelayMs").is_none());
        assert!(s.get("retry").unwrap().get("maxDelayMs").is_none());
    }

    #[test]
    fn migrate_retry_numeric_skips_overwrite_but_still_removes_legacy_key() {
        let mut m = Map::new();
        m.insert(
            "retry".into(),
            serde_json::json!({"maxDelayMs": 9000, "provider": {"maxRetryDelayMs": 1000}}),
        );
        let s = migrate_settings(m);
        assert_eq!(s.get("retry").unwrap()["provider"]["maxRetryDelayMs"], 1000);
        assert!(s.get("retry").unwrap().get("maxDelayMs").is_none());
    }

    #[test]
    fn deep_merge_nested() {
        let mut base = Settings::new();
        base.insert(
            "compaction",
            serde_json::json!({"enabled": true, "reserveTokens": 1}),
        );
        let mut over = Settings::new();
        over.insert("compaction", serde_json::json!({"reserveTokens": 2}));
        let m = deep_merge_settings(&base, &over);
        assert_eq!(m.get("compaction").unwrap()["enabled"], true);
        assert_eq!(m.get("compaction").unwrap()["reserveTokens"], 2);
    }

    #[test]
    fn test_setters_preserving_concurrent_unknown_nested_fields() {
        let temp = tempfile::tempdir().expect("tempdir");
        let agent_dir = temp.path().join("agent");
        let cwd = temp.path().join("project");

        // Write a base global settings file
        let global_settings_path = agent_dir.join("settings.json");
        fs::create_dir_all(&agent_dir).unwrap();
        fs::write(
            &global_settings_path,
            "{\n  \"some_unknown_key\": \"some_value\",\n  \"compaction\": {\n    \"enabled\": true,\n    \"reserveTokens\": 16384\n  }\n}",
        )
        .unwrap();

        // Write a project settings file
        let project_settings_path = cwd.join(CONFIG_DIR_NAME).join("settings.json");
        fs::create_dir_all(cwd.join(CONFIG_DIR_NAME)).unwrap();
        fs::write(
            &project_settings_path,
            "{\n  \"project_unknown\": 123,\n  \"extensions\": [\"old-proj-ext\"]\n}",
        )
        .unwrap();

        // Load SettingsManager
        let mut sm = SettingsManager::create(cwd.clone(), Some(agent_dir.clone()));

        // Verify initial load
        assert_eq!(
            sm.settings().get_str("some_unknown_key"),
            Some("some_value")
        );

        // Simulating CONCURRENT modification by another process on global file
        fs::write(
            &global_settings_path,
            "{\n  \"some_unknown_key\": \"some_value\",\n  \"concurrent_global_key\": \"concurrent_val\",\n  \"compaction\": {\n    \"enabled\": true,\n    \"reserveTokens\": 12345\n  }\n}",
        )
        .unwrap();

        // Simulating CONCURRENT modification by another process on project file
        fs::write(
            &project_settings_path,
            "{\n  \"project_unknown\": 123,\n  \"concurrent_project_key\": 999,\n  \"extensions\": [\"old-proj-ext\"]\n}",
        )
        .unwrap();

        // 1. Modify global extensions path (rereads under lock + merges modified keys)
        sm.set_extension_paths(vec!["new-ext".to_string()]);

        // Read the written global file
        let global_content = fs::read_to_string(&global_settings_path).unwrap();
        let global_val: Value = serde_json::from_str(&global_content).unwrap();
        // Assert new key is set
        assert_eq!(global_val["extensions"][0], "new-ext");
        // Assert loaded unknown key is preserved
        assert_eq!(global_val["some_unknown_key"], "some_value");
        // Assert concurrent unknown key is preserved!
        assert_eq!(global_val["concurrent_global_key"], "concurrent_val");
        // Assert concurrent nested compaction fields are preserved!
        assert_eq!(global_val["compaction"]["enabled"], true);
        assert_eq!(global_val["compaction"]["reserveTokens"], 12345);

        // 2. Modify project extensions path (trusted is true by default in sm.create)
        sm.set_project_extension_paths(vec!["new-proj-ext".to_string()])
            .unwrap();

        let project_content = fs::read_to_string(&project_settings_path).unwrap();
        let project_val: Value = serde_json::from_str(&project_content).unwrap();
        assert_eq!(project_val["extensions"][0], "new-proj-ext");
        assert_eq!(project_val["project_unknown"], 123); // preserved!
        assert_eq!(project_val["concurrent_project_key"], 999); // concurrent change preserved!

        // 3. Verify untrusted refusal
        sm.set_project_trusted(false);
        let untrusted_res = sm.set_project_extension_paths(vec!["untrusted-ext".to_string()]);
        assert!(untrusted_res.is_err());
        let err_msg = untrusted_res.err().unwrap().to_string();
        assert!(err_msg.contains("Project is not trusted"));

        // Verify file is NOT mutated on untrusted refusal
        let project_content_after = fs::read_to_string(&project_settings_path).unwrap();
        let project_val_after: Value = serde_json::from_str(&project_content_after).unwrap();
        assert_eq!(project_val_after["extensions"][0], "new-proj-ext");
    }
}
