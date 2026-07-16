//! Append-only session JSONL tree manager.
//!
//! Port of `packages/coding-agent/src/core/session-manager.ts`.

use crate::config::{get_agent_dir, get_default_session_dir_path, normalize_path, resolve_path};
use crate::serde_util::NullOr;
use crate::session_types::{
    CURRENT_SESSION_VERSION, FileEntry, SessionEntry, SessionHeader, SessionHeaderType,
    parse_session_entries, parse_session_entry_line, serialize_file_entry_line,
    serialize_session_jsonl,
};
use serde_json::Value;
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use thiserror::Error;
use uuid::Uuid;

/// Result alias for session operations.
pub type Result<T, E = SessionError> = std::result::Result<T, E>;

#[derive(Debug, Error)]
pub enum SessionError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("{0}")]
    Message(String),
}

impl SessionError {
    fn msg(s: impl Into<String>) -> Self {
        Self::Message(s.into())
    }
}

/// Options for creating a new session.
#[derive(Clone, Debug, Default)]
pub struct NewSessionOptions {
    pub id: Option<String>,
    pub parent_session: Option<String>,
}

/// Listing metadata for a session file.
#[derive(Clone, Debug)]
pub struct SessionInfo {
    pub path: PathBuf,
    pub id: String,
    pub cwd: String,
    pub name: Option<String>,
    pub parent_session_path: Option<String>,
    pub created: String,
    pub modified_ms: i64,
    pub message_count: usize,
    pub first_message: String,
    pub all_messages_text: String,
}

/// Tree node returned by [`SessionManager::get_tree`].
#[derive(Clone, Debug)]
pub struct SessionTreeNode {
    pub entry: SessionEntry,
    pub children: Vec<SessionTreeNode>,
    pub label: Option<String>,
    pub label_timestamp: Option<String>,
}

/// Resolved context for the LLM.
#[derive(Clone, Debug)]
pub struct SessionContext {
    pub messages: Vec<Value>,
    pub thinking_level: String,
    pub model: Option<SessionModelRef>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionModelRef {
    pub provider: String,
    pub model_id: String,
}

/// Validate session id characters (oracle `assertValidSessionId`).
pub fn assert_valid_session_id(id: &str) -> Result<()> {
    let re_ok = !id.is_empty()
        && id.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
        && id.chars().last().is_some_and(|c| c.is_ascii_alphanumeric())
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.');
    if !re_ok {
        return Err(SessionError::msg(
            "Session id must be non-empty, contain only alphanumeric characters, '-', '_', and '.', and start and end with an alphanumeric character",
        ));
    }
    Ok(())
}

fn create_session_id() -> String {
    Uuid::now_v7().to_string()
}

fn generate_entry_id(by_id: &HashMap<String, SessionEntry>) -> String {
    for _ in 0..100 {
        let id = Uuid::new_v4().to_string();
        let short = &id[..8];
        if !by_id.contains_key(short) {
            return short.to_string();
        }
    }
    Uuid::new_v4().to_string()
}

fn now_iso() -> String {
    jiff::Timestamp::now().to_string()
}

/// Ensure default session dir exists and return it.
pub fn get_default_session_dir(cwd: &str, agent_dir: Option<&Path>) -> Result<PathBuf> {
    let dir = get_default_session_dir_path(cwd, agent_dir);
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Load entries from a session file (empty / invalid header → empty vec).
pub fn load_entries_from_file(file_path: &Path) -> Result<Vec<FileEntry>> {
    let resolved = normalize_path(&file_path.to_string_lossy());
    if !resolved.exists() {
        return Ok(Vec::new());
    }
    let mut file = File::open(&resolved)?;
    let mut content = String::new();
    file.read_to_string(&mut content)?;
    let entries = parse_session_entries(&content);
    if entries.is_empty() {
        return Ok(entries);
    }
    match entries.first() {
        Some(FileEntry::Header(h)) if !h.id.is_empty() => Ok(entries),
        _ => Ok(Vec::new()),
    }
}

/// Migrate v1 → v2: assign id/parentId chain. Mutates in place. Returns true if changed.
fn migrate_v1_to_v2(entries: &mut [FileEntry]) -> bool {
    let mut ids: HashMap<String, ()> = HashMap::new();
    let mut prev_id: Option<String> = None;
    let mut changed = false;

    for entry in entries.iter_mut() {
        match entry {
            FileEntry::Header(h) => {
                if h.version != Some(2) && h.version != Some(3) {
                    h.version = Some(2);
                    changed = true;
                }
            }
            FileEntry::Entry(e) => {
                if e.id().is_none() {
                    let id = {
                        let mut id = String::new();
                        for _ in 0..100 {
                            let full = Uuid::new_v4().to_string();
                            let short = full[..8].to_string();
                            if !ids.contains_key(&short) {
                                id = short;
                                break;
                            }
                        }
                        if id.is_empty() {
                            id = Uuid::new_v4().to_string();
                        }
                        ids.insert(id.clone(), ());
                        id
                    };
                    e.set_id(id.clone());
                    e.set_parent_id(NullOr::from_option(prev_id.clone()));
                    prev_id = Some(id);
                    changed = true;
                } else if let Some(id) = e.id() {
                    ids.insert(id.to_string(), ());
                    prev_id = Some(id.to_string());
                }
            }
        }
    }

    // Second pass: resolve compaction indices after all ids assigned.
    let id_at: Vec<Option<String>> = entries
        .iter()
        .map(|e| match e {
            FileEntry::Header(_) => None,
            FileEntry::Entry(en) => en.id().map(str::to_string),
        })
        .collect();
    for entry in entries.iter_mut() {
        if let FileEntry::Entry(SessionEntry::Compaction {
            first_kept_entry_index,
            first_kept_entry_id,
            ..
        }) = entry
        {
            if first_kept_entry_id.is_none() {
                if let Some(idx) = first_kept_entry_index.take() {
                    let idx = idx as usize;
                    if let Some(Some(tid)) = id_at.get(idx) {
                        *first_kept_entry_id = Some(tid.clone());
                        changed = true;
                    }
                }
            } else {
                *first_kept_entry_index = None;
            }
        }
    }

    changed
}

/// Migrate v2 → v3: hookMessage → custom role.
fn migrate_v2_to_v3(entries: &mut [FileEntry]) -> bool {
    let mut changed = false;
    for entry in entries.iter_mut() {
        match entry {
            FileEntry::Header(h) => {
                if h.version != Some(3) {
                    h.version = Some(3);
                    changed = true;
                }
            }
            FileEntry::Entry(SessionEntry::Message { message, .. }) => {
                if let Some(role) = message.get("role").and_then(|v| v.as_str())
                    && role == "hookMessage"
                    && let Some(obj) = message.as_object_mut()
                {
                    obj.insert("role".into(), Value::String("custom".into()));
                    changed = true;
                }
            }
            _ => {}
        }
    }
    changed
}

/// Run migrations to current version. Returns true if any migration applied.
pub fn migrate_to_current_version(entries: &mut [FileEntry]) -> bool {
    let version = entries
        .iter()
        .find_map(|e| e.as_header())
        .and_then(|h| h.version)
        .unwrap_or(1);
    if version >= CURRENT_SESSION_VERSION {
        return false;
    }
    let mut changed = false;
    if version < 2 {
        changed |= migrate_v1_to_v2(entries);
    }
    if version < 3 {
        // Re-read version after v1 migration may have set 2.
        changed |= migrate_v2_to_v3(entries);
    }
    changed
}

/// Exported for tests (oracle name).
pub fn migrate_session_entries(entries: &mut [FileEntry]) {
    let _ = migrate_to_current_version(entries);
}

fn read_session_header(file_path: &Path) -> Option<SessionHeader> {
    let file = File::open(file_path).ok()?;
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    let entry = parse_session_entry_line(line.trim_end_matches(['\r', '\n']))?;
    entry.as_header().cloned()
}

/// Find most recently modified session in a directory, optionally filtered by cwd.
pub fn find_most_recent_session(session_dir: &Path, cwd: Option<&str>) -> Option<PathBuf> {
    let resolved_cwd = cwd.map(|c| resolve_path(c, None));
    let entries = fs::read_dir(session_dir).ok()?;
    let mut best: Option<(PathBuf, std::time::SystemTime)> = None;
    for ent in entries.flatten() {
        let path = ent.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let header = match read_session_header(&path) {
            Some(h) => h,
            None => continue,
        };
        if let Some(ref rcwd) = resolved_cwd {
            let hcwd = resolve_path(&header.cwd, None);
            if hcwd != *rcwd {
                continue;
            }
        }
        let mtime = ent.metadata().ok().and_then(|m| m.modified().ok())?;
        match &best {
            Some((_, t)) if *t >= mtime => {}
            _ => best = Some((path, mtime)),
        }
    }
    best.map(|(p, _)| p)
}

/// Session manager: append-only JSONL tree.
pub struct SessionManager {
    session_id: String,
    session_file: Option<PathBuf>,
    session_dir: PathBuf,
    cwd: PathBuf,
    persist: bool,
    flushed: bool,
    file_entries: Vec<FileEntry>,
    by_id: HashMap<String, SessionEntry>,
    labels_by_id: HashMap<String, String>,
    label_timestamps_by_id: HashMap<String, String>,
    leaf_id: Option<String>,
}

impl SessionManager {
    fn new(
        cwd: impl AsRef<Path>,
        session_dir: impl Into<PathBuf>,
        session_file: Option<PathBuf>,
        persist: bool,
        new_session_options: Option<NewSessionOptions>,
    ) -> Result<Self> {
        let cwd = resolve_path(&cwd.as_ref().to_string_lossy(), None);
        let session_dir = normalize_path(&session_dir.into().to_string_lossy());
        if persist && !session_dir.as_os_str().is_empty() {
            fs::create_dir_all(&session_dir)?;
        }
        let mut sm = Self {
            session_id: String::new(),
            session_file: None,
            session_dir,
            cwd,
            persist,
            flushed: false,
            file_entries: Vec::new(),
            by_id: HashMap::new(),
            labels_by_id: HashMap::new(),
            label_timestamps_by_id: HashMap::new(),
            leaf_id: None,
        };
        if let Some(file) = session_file {
            sm.set_session_file(file)?;
        } else {
            sm.new_session(new_session_options.unwrap_or_default())?;
        }
        Ok(sm)
    }

    /// Create a persisted session under the default or provided session dir.
    pub fn create(
        cwd: impl AsRef<Path>,
        session_dir: Option<PathBuf>,
        options: Option<NewSessionOptions>,
    ) -> Result<Self> {
        let cwd_str = cwd.as_ref().to_string_lossy();
        let dir = match session_dir {
            Some(d) => normalize_path(&d.to_string_lossy()),
            None => get_default_session_dir(&cwd_str, None)?,
        };
        Self::new(cwd, dir, None, true, options)
    }

    /// Open a session file.
    pub fn open(
        path: impl AsRef<Path>,
        session_dir: Option<PathBuf>,
        cwd_override: Option<&str>,
    ) -> Result<Self> {
        let resolved = resolve_path(&path.as_ref().to_string_lossy(), None);
        let entries = load_entries_from_file(&resolved)?;
        let header = entries.iter().find_map(|e| e.as_header());
        let cwd = cwd_override
            .map(|s| s.to_string())
            .or_else(|| header.map(|h| h.cwd.clone()))
            .unwrap_or_else(|| {
                std::env::current_dir()
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_else(|_| ".".into())
            });
        let dir = match session_dir {
            Some(d) => normalize_path(&d.to_string_lossy()),
            None => resolved
                .parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| PathBuf::from(".")),
        };
        Self::new(cwd, dir, Some(resolved), true, None)
    }

    /// Continue most recent session or create new.
    pub fn continue_recent(cwd: impl AsRef<Path>, session_dir: Option<PathBuf>) -> Result<Self> {
        let cwd_str = cwd.as_ref().to_string_lossy();
        let default_path = get_default_session_dir_path(&cwd_str, None);
        let dir = match session_dir {
            Some(d) => normalize_path(&d.to_string_lossy()),
            None => get_default_session_dir(&cwd_str, None)?,
        };
        let filter_cwd = dir != default_path;
        let most_recent = find_most_recent_session(
            &dir,
            if filter_cwd {
                Some(cwd_str.as_ref())
            } else {
                None
            },
        );
        match most_recent {
            Some(file) => Self::new(cwd, dir, Some(file), true, None),
            None => Self::new(cwd, dir, None, true, None),
        }
    }

    /// In-memory session (no disk).
    pub fn in_memory(cwd: Option<&str>, options: Option<NewSessionOptions>) -> Result<Self> {
        let cwd = cwd.unwrap_or(".");
        Self::new(cwd, PathBuf::new(), None, false, options)
    }

    /// Fork another session file into `target_cwd`.
    pub fn fork_from(
        source_path: impl AsRef<Path>,
        target_cwd: impl AsRef<Path>,
        session_dir: Option<PathBuf>,
        options: Option<NewSessionOptions>,
    ) -> Result<Self> {
        let resolved_source = resolve_path(&source_path.as_ref().to_string_lossy(), None);
        let resolved_target = resolve_path(&target_cwd.as_ref().to_string_lossy(), None);
        let source_entries = load_entries_from_file(&resolved_source)?;
        if source_entries.is_empty() {
            return Err(SessionError::msg(format!(
                "Cannot fork: source session file is empty or invalid: {}",
                resolved_source.display()
            )));
        }
        if source_entries.iter().find_map(|e| e.as_header()).is_none() {
            return Err(SessionError::msg(format!(
                "Cannot fork: source session has no header: {}",
                resolved_source.display()
            )));
        }
        let dir = match session_dir {
            Some(d) => normalize_path(&d.to_string_lossy()),
            None => get_default_session_dir(&resolved_target.to_string_lossy(), None)?,
        };
        fs::create_dir_all(&dir)?;
        let options = options.unwrap_or_default();
        if let Some(ref id) = options.id {
            assert_valid_session_id(id)?;
        }
        let new_session_id = options.id.unwrap_or_else(create_session_id);
        let timestamp = now_iso();
        let file_timestamp = timestamp.replace([':', '.'], "-");
        let new_session_file = dir.join(format!("{file_timestamp}_{new_session_id}.jsonl"));
        let new_header = SessionHeader {
            entry_type: SessionHeaderType::Session,
            version: Some(CURRENT_SESSION_VERSION),
            id: new_session_id,
            timestamp,
            cwd: resolved_target.to_string_lossy().into_owned(),
            parent_session: Some(resolved_source.to_string_lossy().into_owned()),
            extra: serde_json::Map::new(),
        };
        let mut body = serialize_file_entry_line(&FileEntry::Header(new_header))?;
        body.push('\n');
        for entry in &source_entries {
            if entry.is_header() {
                continue;
            }
            body.push_str(&serialize_file_entry_line(entry)?);
            body.push('\n');
        }
        // Exclusive create like flag "wx"
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&new_session_file)?;
        f.write_all(body.as_bytes())?;
        Self::new(resolved_target, dir, Some(new_session_file), true, None)
    }

    pub fn set_session_file(&mut self, session_file: impl AsRef<Path>) -> Result<()> {
        let resolved = resolve_path(&session_file.as_ref().to_string_lossy(), None);
        self.session_file = Some(resolved.clone());
        if resolved.exists() {
            let mut entries = load_entries_from_file(&resolved)?;
            if entries.is_empty() {
                let size = fs::metadata(&resolved)?.len();
                if size > 0 {
                    return Err(SessionError::msg(format!(
                        "Session file is not a valid pi session: {}",
                        resolved.display()
                    )));
                }
                self.new_session(NewSessionOptions::default())?;
                self.session_file = Some(resolved);
                self.rewrite_file()?;
                self.flushed = true;
                return Ok(());
            }
            let header_id = entries
                .iter()
                .find_map(|e| e.as_header())
                .map(|h| h.id.clone())
                .unwrap_or_else(create_session_id);
            self.session_id = header_id;
            if migrate_to_current_version(&mut entries) {
                self.file_entries = entries;
                self.rewrite_file()?;
            } else {
                self.file_entries = entries;
            }
            self.build_index();
            self.flushed = true;
        } else {
            let explicit = resolved;
            self.new_session(NewSessionOptions::default())?;
            self.session_file = Some(explicit);
        }
        Ok(())
    }

    pub fn new_session(&mut self, options: NewSessionOptions) -> Result<Option<PathBuf>> {
        if let Some(ref id) = options.id {
            assert_valid_session_id(id)?;
        }
        self.session_id = options.id.unwrap_or_else(create_session_id);
        let timestamp = now_iso();
        let header = SessionHeader {
            entry_type: SessionHeaderType::Session,
            version: Some(CURRENT_SESSION_VERSION),
            id: self.session_id.clone(),
            timestamp: timestamp.clone(),
            cwd: self.cwd.to_string_lossy().into_owned(),
            parent_session: options.parent_session,
            extra: serde_json::Map::new(),
        };
        self.file_entries = vec![FileEntry::Header(header)];
        self.by_id.clear();
        self.labels_by_id.clear();
        self.label_timestamps_by_id.clear();
        self.leaf_id = None;
        self.flushed = false;
        if self.persist {
            let file_timestamp = timestamp.replace([':', '.'], "-");
            self.session_file = Some(
                self.session_dir
                    .join(format!("{file_timestamp}_{}.jsonl", self.session_id)),
            );
        }
        Ok(self.session_file.clone())
    }

    fn build_index(&mut self) {
        self.by_id.clear();
        self.labels_by_id.clear();
        self.label_timestamps_by_id.clear();
        self.leaf_id = None;
        for entry in &self.file_entries {
            let Some(e) = entry.as_entry() else {
                continue;
            };
            if let Some(id) = e.id() {
                self.by_id.insert(id.to_string(), e.clone());
                self.leaf_id = Some(id.to_string());
            }
            if let SessionEntry::Label {
                target_id,
                label,
                timestamp,
                ..
            } = e
            {
                if let Some(l) = label {
                    self.labels_by_id.insert(target_id.clone(), l.clone());
                    self.label_timestamps_by_id
                        .insert(target_id.clone(), timestamp.clone());
                } else {
                    self.labels_by_id.remove(target_id);
                    self.label_timestamps_by_id.remove(target_id);
                }
            }
        }
    }

    fn rewrite_file(&self) -> Result<()> {
        if !self.persist {
            return Ok(());
        }
        let Some(ref path) = self.session_file else {
            return Ok(());
        };
        let body = serialize_session_jsonl(&self.file_entries)?;
        fs::write(path, body)?;
        Ok(())
    }

    fn persist_entry(&mut self, entry: &SessionEntry) -> Result<()> {
        if !self.persist {
            return Ok(());
        }
        let Some(ref path) = self.session_file else {
            return Ok(());
        };
        let has_assistant = self.file_entries.iter().any(|e| {
            matches!(
                e.as_entry(),
                Some(SessionEntry::Message { message, .. })
                    if message.get("role").and_then(|r| r.as_str()) == Some("assistant")
            )
        });
        if !has_assistant {
            if self.flushed {
                let mut f = OpenOptions::new().create(true).append(true).open(path)?;
                let line = serialize_file_entry_line(&FileEntry::Entry(entry.clone()))?;
                f.write_all(line.as_bytes())?;
                f.write_all(b"\n")?;
            } else {
                self.flushed = false;
            }
            return Ok(());
        }
        if !self.flushed {
            // Exclusive create of full file
            let body = serialize_session_jsonl(&self.file_entries)?;
            match OpenOptions::new().write(true).create_new(true).open(path) {
                Ok(mut f) => {
                    f.write_all(body.as_bytes())?;
                }
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                    fs::write(path, body)?;
                }
                Err(err) => return Err(err.into()),
            }
            self.flushed = true;
        } else {
            let mut f = OpenOptions::new().create(true).append(true).open(path)?;
            let line = serialize_file_entry_line(&FileEntry::Entry(entry.clone()))?;
            f.write_all(line.as_bytes())?;
            f.write_all(b"\n")?;
        }
        Ok(())
    }

    fn append_entry(&mut self, entry: SessionEntry) -> Result<String> {
        let id = entry
            .id()
            .map(str::to_string)
            .ok_or_else(|| SessionError::msg("entry missing id"))?;
        self.file_entries.push(FileEntry::Entry(entry.clone()));
        self.by_id.insert(id.clone(), entry.clone());
        self.leaf_id = Some(id.clone());
        self.persist_entry(&entry)?;
        Ok(id)
    }

    fn make_base_ids(&self) -> (String, NullOr<String>, String) {
        let id = generate_entry_id(&self.by_id);
        let parent = NullOr::from_option(self.leaf_id.clone());
        let timestamp = now_iso();
        (id, parent, timestamp)
    }

    pub fn append_message(&mut self, message: Value) -> Result<String> {
        let (id, parent_id, timestamp) = self.make_base_ids();
        self.append_entry(SessionEntry::Message {
            id: Some(id),
            parent_id,
            timestamp,
            message,
        })
    }

    pub fn append_thinking_level_change(
        &mut self,
        thinking_level: impl Into<String>,
    ) -> Result<String> {
        let (id, parent_id, timestamp) = self.make_base_ids();
        self.append_entry(SessionEntry::ThinkingLevelChange {
            id: Some(id),
            parent_id,
            timestamp,
            thinking_level: thinking_level.into(),
        })
    }

    pub fn append_model_change(
        &mut self,
        provider: impl Into<String>,
        model_id: impl Into<String>,
    ) -> Result<String> {
        let (id, parent_id, timestamp) = self.make_base_ids();
        self.append_entry(SessionEntry::ModelChange {
            id: Some(id),
            parent_id,
            timestamp,
            provider: provider.into(),
            model_id: model_id.into(),
        })
    }

    pub fn append_compaction(
        &mut self,
        summary: impl Into<String>,
        first_kept_entry_id: impl Into<String>,
        tokens_before: u64,
        details: Option<Value>,
        from_hook: Option<bool>,
    ) -> Result<String> {
        let (id, parent_id, timestamp) = self.make_base_ids();
        self.append_entry(SessionEntry::Compaction {
            id: Some(id),
            parent_id,
            timestamp,
            summary: summary.into(),
            first_kept_entry_id: Some(first_kept_entry_id.into()),
            first_kept_entry_index: None,
            tokens_before,
            details,
            from_hook,
        })
    }

    pub fn append_custom_entry(
        &mut self,
        custom_type: impl Into<String>,
        data: Option<Value>,
    ) -> Result<String> {
        let (id, parent_id, timestamp) = self.make_base_ids();
        self.append_entry(SessionEntry::Custom {
            id: Some(id),
            parent_id,
            timestamp,
            custom_type: custom_type.into(),
            data,
        })
    }

    pub fn append_session_info(&mut self, name: impl Into<String>) -> Result<String> {
        let sanitized = name.into().replace(['\r', '\n'], " ").trim().to_string();
        let (id, parent_id, timestamp) = self.make_base_ids();
        self.append_entry(SessionEntry::SessionInfo {
            id: Some(id),
            parent_id,
            timestamp,
            name: Some(sanitized),
        })
    }

    pub fn append_custom_message_entry(
        &mut self,
        custom_type: impl Into<String>,
        content: Value,
        display: bool,
        details: Option<Value>,
    ) -> Result<String> {
        let (id, parent_id, timestamp) = self.make_base_ids();
        self.append_entry(SessionEntry::CustomMessage {
            id: Some(id),
            parent_id,
            timestamp,
            custom_type: custom_type.into(),
            content,
            details,
            display,
        })
    }

    pub fn append_label_change(
        &mut self,
        target_id: impl Into<String>,
        label: Option<String>,
    ) -> Result<String> {
        let target_id = target_id.into();
        if !self.by_id.contains_key(&target_id) {
            return Err(SessionError::msg(format!("Entry {target_id} not found")));
        }
        let (id, parent_id, timestamp) = self.make_base_ids();
        let entry_id = self.append_entry(SessionEntry::Label {
            id: Some(id),
            parent_id,
            timestamp: timestamp.clone(),
            target_id: target_id.clone(),
            label: label.clone(),
        })?;
        if let Some(l) = label {
            self.labels_by_id.insert(target_id.clone(), l);
            self.label_timestamps_by_id.insert(target_id, timestamp);
        } else {
            self.labels_by_id.remove(&target_id);
            self.label_timestamps_by_id.remove(&target_id);
        }
        Ok(entry_id)
    }

    pub fn branch_with_summary(
        &mut self,
        branch_from_id: Option<String>,
        summary: impl Into<String>,
        details: Option<Value>,
        from_hook: Option<bool>,
    ) -> Result<String> {
        if let Some(ref id) = branch_from_id
            && !self.by_id.contains_key(id)
        {
            return Err(SessionError::msg(format!("Entry {id} not found")));
        }
        self.leaf_id = branch_from_id.clone();
        let (id, _, timestamp) = self.make_base_ids();
        let from_id = branch_from_id.clone().unwrap_or_else(|| "root".into());
        self.append_entry(SessionEntry::BranchSummary {
            id: Some(id),
            parent_id: NullOr::from_option(branch_from_id),
            timestamp,
            from_id,
            summary: summary.into(),
            details,
            from_hook,
        })
    }

    pub fn branch(&mut self, branch_from_id: &str) -> Result<()> {
        if !self.by_id.contains_key(branch_from_id) {
            return Err(SessionError::msg(format!(
                "Entry {branch_from_id} not found"
            )));
        }
        self.leaf_id = Some(branch_from_id.to_string());
        Ok(())
    }

    pub fn reset_leaf(&mut self) {
        self.leaf_id = None;
    }

    pub fn is_persisted(&self) -> bool {
        self.persist
    }

    pub fn get_cwd(&self) -> &Path {
        &self.cwd
    }

    pub fn get_session_dir(&self) -> &Path {
        &self.session_dir
    }

    pub fn uses_default_session_dir(&self) -> bool {
        let default =
            get_default_session_dir_path(&self.cwd.to_string_lossy(), Some(&get_agent_dir()));
        self.session_dir == default
    }

    pub fn get_session_id(&self) -> &str {
        &self.session_id
    }

    pub fn get_session_file(&self) -> Option<&Path> {
        self.session_file.as_deref()
    }

    pub fn get_leaf_id(&self) -> Option<&str> {
        self.leaf_id.as_deref()
    }

    pub fn get_leaf_entry(&self) -> Option<&SessionEntry> {
        self.leaf_id.as_ref().and_then(|id| self.by_id.get(id))
    }

    pub fn get_entry(&self, id: &str) -> Option<&SessionEntry> {
        self.by_id.get(id)
    }

    pub fn get_label(&self, id: &str) -> Option<&str> {
        self.labels_by_id.get(id).map(String::as_str)
    }

    pub fn get_header(&self) -> Option<&SessionHeader> {
        self.file_entries.iter().find_map(|e| e.as_header())
    }

    pub fn get_entries(&self) -> Vec<SessionEntry> {
        self.file_entries
            .iter()
            .filter_map(|e| e.as_entry().cloned())
            .collect()
    }

    pub fn get_session_name(&self) -> Option<String> {
        for entry in self.get_entries().into_iter().rev() {
            if let SessionEntry::SessionInfo { name, .. } = entry {
                let trimmed = name.as_deref().map(str::trim).filter(|s| !s.is_empty());
                return trimmed.map(str::to_string);
            }
        }
        None
    }

    pub fn get_branch(&self, from_id: Option<&str>) -> Vec<SessionEntry> {
        let mut path = Vec::new();
        let mut current = from_id.map(str::to_string).or_else(|| self.leaf_id.clone());
        while let Some(id) = current {
            let Some(entry) = self.by_id.get(&id) else {
                break;
            };
            path.push(entry.clone());
            current = entry.parent_id().as_option().cloned();
        }
        path.reverse();
        path
    }

    pub fn build_context_entries(&self) -> Vec<SessionEntry> {
        build_context_entries(
            &self.get_entries(),
            self.leaf_id.as_deref(),
            Some(&self.by_id),
        )
    }

    pub fn build_session_context(&self) -> SessionContext {
        build_session_context(
            &self.get_entries(),
            self.leaf_id.as_deref(),
            Some(&self.by_id),
        )
    }

    pub fn get_tree(&self) -> Vec<SessionTreeNode> {
        let entries = self.get_entries();
        let mut node_map: HashMap<String, SessionTreeNode> = HashMap::new();
        for entry in &entries {
            let id = match entry.id() {
                Some(id) => id.to_string(),
                None => continue,
            };
            let label = self.labels_by_id.get(&id).cloned();
            let label_timestamp = self.label_timestamps_by_id.get(&id).cloned();
            node_map.insert(
                id,
                SessionTreeNode {
                    entry: entry.clone(),
                    children: Vec::new(),
                    label,
                    label_timestamp,
                },
            );
        }
        let mut roots: Vec<String> = Vec::new();
        let mut child_links: Vec<(String, String)> = Vec::new();
        for entry in &entries {
            let Some(id) = entry.id().map(str::to_string) else {
                continue;
            };
            match entry.parent_id().as_option() {
                Some(pid) if node_map.contains_key(pid) => {
                    child_links.push((pid.clone(), id));
                }
                _ => roots.push(id),
            }
        }
        for (parent, child) in child_links {
            let Some(child_node) = node_map.remove(&child) else {
                continue;
            };
            if let Some(parent_node) = node_map.get_mut(&parent) {
                parent_node.children.push(child_node);
            } else {
                // parent missing mid-build — treat as root
                node_map.insert(child.clone(), child_node);
                roots.push(child);
            }
        }
        // Sort children by timestamp
        fn sort_tree(nodes: &mut [SessionTreeNode]) {
            nodes.sort_by(|a, b| a.entry.timestamp().cmp(b.entry.timestamp()));
            for n in nodes.iter_mut() {
                sort_tree(&mut n.children);
            }
        }
        let mut root_nodes: Vec<SessionTreeNode> = roots
            .into_iter()
            .filter_map(|id| node_map.remove(&id))
            .collect();
        sort_tree(&mut root_nodes);
        root_nodes
    }
}

fn build_entry_index<'a>(
    entries: &'a [SessionEntry],
    by_id: Option<&'a HashMap<String, SessionEntry>>,
) -> HashMap<String, &'a SessionEntry> {
    if let Some(map) = by_id {
        return map.iter().map(|(k, v)| (k.clone(), v)).collect();
    }
    let mut index = HashMap::new();
    for entry in entries {
        if let Some(id) = entry.id() {
            index.insert(id.to_string(), entry);
        }
    }
    index
}

fn build_session_path(
    entries: &[SessionEntry],
    leaf_id: Option<&str>,
    by_id: Option<&HashMap<String, SessionEntry>>,
) -> Vec<SessionEntry> {
    let index = build_entry_index(entries, by_id);
    let leaf = match leaf_id {
        Some(id) => index.get(id).copied(),
        None => entries.last(),
    };
    let Some(leaf) = leaf else {
        return Vec::new();
    };
    let mut path = Vec::new();
    let mut current = Some(leaf);
    while let Some(entry) = current {
        path.push(entry.clone());
        current = entry
            .parent_id()
            .as_option()
            .and_then(|pid| index.get(pid).copied());
    }
    path.reverse();
    path
}

/// Build compaction-aware context entry list (oracle `buildContextEntries`).
pub fn build_context_entries(
    entries: &[SessionEntry],
    leaf_id: Option<&str>,
    by_id: Option<&HashMap<String, SessionEntry>>,
) -> Vec<SessionEntry> {
    let path = build_session_path(entries, leaf_id, by_id);
    let compaction = path.iter().rev().find_map(|e| match e {
        SessionEntry::Compaction { id, .. } => Some(id.clone().unwrap_or_default()),
        _ => None,
    });
    let Some(comp_id) = compaction else {
        return path;
    };
    let compaction_idx = match path.iter().position(|e| e.id() == Some(comp_id.as_str())) {
        Some(i) => i,
        None => return path,
    };
    let first_kept = match &path[compaction_idx] {
        SessionEntry::Compaction {
            first_kept_entry_id,
            ..
        } => first_kept_entry_id.clone(),
        _ => None,
    };
    let mut context = vec![path[compaction_idx].clone()];
    let mut found_first = false;
    for entry in path.iter().take(compaction_idx) {
        if first_kept
            .as_deref()
            .is_some_and(|fk| entry.id() == Some(fk))
        {
            found_first = true;
        }
        if found_first {
            context.push(entry.clone());
        }
    }
    context.extend(path.iter().skip(compaction_idx + 1).cloned());
    context
}

fn timestamp_millis(timestamp: &str) -> i64 {
    timestamp
        .parse::<jiff::Timestamp>()
        .expect("session entry timestamps are valid ISO-8601")
        .as_millisecond()
}

fn session_entry_to_context_messages(entry: &SessionEntry) -> Vec<Value> {
    match entry {
        SessionEntry::Message { message, .. } => {
            let mut msg = message.clone();
            if let Some(role) = msg.get("role").and_then(|r| r.as_str())
                && matches!(role, "user" | "assistant" | "toolResult")
                && msg.get("content").map(|c| c.is_null()).unwrap_or(true)
                && !msg
                    .get("content")
                    .map(|c| c.is_array() || c.is_string())
                    .unwrap_or(false)
                && let Some(obj) = msg.as_object_mut()
            {
                obj.insert("content".into(), Value::Array(vec![]));
            }
            vec![msg]
        }
        SessionEntry::CustomMessage {
            custom_type,
            content,
            display,
            details,
            timestamp,
            ..
        } => {
            let mut obj = serde_json::Map::new();
            obj.insert("role".into(), Value::String("custom".into()));
            obj.insert("customType".into(), Value::String(custom_type.clone()));
            obj.insert("content".into(), content.clone());
            obj.insert("display".into(), Value::Bool(*display));
            if let Some(d) = details {
                obj.insert("details".into(), d.clone());
            }
            obj.insert("timestamp".into(), Value::from(timestamp_millis(timestamp)));
            vec![Value::Object(obj)]
        }
        SessionEntry::BranchSummary {
            summary,
            from_id,
            timestamp,
            ..
        } if !summary.is_empty() => {
            let mut obj = serde_json::Map::new();
            obj.insert("role".into(), Value::String("branchSummary".into()));
            obj.insert("summary".into(), Value::String(summary.clone()));
            obj.insert("fromId".into(), Value::String(from_id.clone()));
            obj.insert("timestamp".into(), Value::from(timestamp_millis(timestamp)));
            vec![Value::Object(obj)]
        }
        SessionEntry::Compaction {
            summary,
            tokens_before,
            timestamp,
            ..
        } => {
            let mut obj = serde_json::Map::new();
            obj.insert("role".into(), Value::String("compactionSummary".into()));
            obj.insert("summary".into(), Value::String(summary.clone()));
            obj.insert("tokensBefore".into(), Value::from(*tokens_before));
            obj.insert("timestamp".into(), Value::from(timestamp_millis(timestamp)));
            vec![Value::Object(obj)]
        }
        _ => Vec::new(),
    }
}

/// Build LLM session context (oracle `buildSessionContext`).
pub fn build_session_context(
    entries: &[SessionEntry],
    leaf_id: Option<&str>,
    by_id: Option<&HashMap<String, SessionEntry>>,
) -> SessionContext {
    let path = build_session_path(entries, leaf_id, by_id);
    let mut thinking_level = "off".to_string();
    let mut model = None;
    for entry in &path {
        match entry {
            SessionEntry::ThinkingLevelChange {
                thinking_level: t, ..
            } => {
                thinking_level = t.clone();
            }
            SessionEntry::ModelChange {
                provider, model_id, ..
            } => {
                model = Some(SessionModelRef {
                    provider: provider.clone(),
                    model_id: model_id.clone(),
                });
            }
            SessionEntry::Message { message, .. } => {
                if message.get("role").and_then(|r| r.as_str()) == Some("assistant")
                    && let (Some(provider), Some(model_id)) = (
                        message.get("provider").and_then(|v| v.as_str()),
                        message.get("model").and_then(|v| v.as_str()),
                    )
                {
                    model = Some(SessionModelRef {
                        provider: provider.to_string(),
                        model_id: model_id.to_string(),
                    });
                }
            }
            _ => {}
        }
    }
    let messages = build_context_entries(entries, leaf_id, by_id)
        .iter()
        .flat_map(session_entry_to_context_messages)
        .collect();
    SessionContext {
        messages,
        thinking_level,
        model,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn in_memory_append_and_branch() {
        let mut sm = SessionManager::in_memory(Some("/tmp/proj"), None).unwrap();
        assert!(sm.get_header().is_some());
        assert_eq!(sm.get_header().unwrap().version, Some(3));
        let uid = sm
            .append_message(
                json!({"role":"user","content":[{"type":"text","text":"hi"}],"timestamp":1}),
            )
            .unwrap();
        let _ = sm
            .append_message(json!({"role":"assistant","content":[{"type":"text","text":"yo"}],"api":"anthropic-messages","provider":"anthropic","model":"x","usage":{"input":1,"output":1,"cacheRead":0,"cacheWrite":0,"totalTokens":2,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}},"stopReason":"stop","timestamp":2}))
            .unwrap();
        assert_eq!(sm.get_entries().len(), 2);
        sm.branch(&uid).unwrap();
        let _ = sm
            .append_message(json!({"role":"user","content":"retry","timestamp":3}))
            .unwrap();
        assert_eq!(sm.get_branch(None).len(), 2); // user + new user child path from leaf
    }

    #[test]
    fn session_file_name_pattern() {
        let dir = tempfile::tempdir().unwrap();
        let sm = SessionManager::create(
            dir.path(),
            Some(dir.path().to_path_buf()),
            Some(NewSessionOptions {
                id: Some("created-session-id".into()),
                parent_session: None,
            }),
        )
        .unwrap();
        let file = sm.get_session_file().unwrap();
        let name = file.file_name().unwrap().to_string_lossy();
        assert!(name.ends_with("_created-session-id.jsonl"));
        assert!(!file.exists()); // not flushed until assistant
    }
}
