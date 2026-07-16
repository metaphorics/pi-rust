//! Session JSONL wire types.
//!
//! Field order and optional-field emission match pi 0.80.7 `session-manager.ts`.
//! Message payloads stay as `serde_json::Value` so nested tool-arg / details
//! key order is preserved for byte-identical round-trips.

use crate::serde_util::{NullOr, is_absent, serialize_null_or};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Current on-disk session schema version (v3).
pub const CURRENT_SESSION_VERSION: u32 = 3;

/// First line of every session file.
///
/// Field order matches pi's writer: `type`, `version?`, `id`, `timestamp`, `cwd`,
/// `parentSession?`. Legacy v1 headers may also carry `provider` / `modelId` /
/// `thinkingLevel` / `branchedFrom` — those are kept in `extra` only when needed
/// via flattened unknown keys on a Value-backed path; for typed round-trip of
/// real fixtures we use a dedicated header that preserves unknown fields.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionHeader {
    #[serde(rename = "type")]
    pub entry_type: SessionHeaderType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<u32>,
    pub id: String,
    pub timestamp: String,
    pub cwd: String,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "parentSession"
    )]
    pub parent_session: Option<String>,
    /// Legacy / extension header fields (provider, modelId, thinkingLevel, branchedFrom, …).
    /// Flattened so unknown keys round-trip with order preserved (serde_json Map).
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionHeaderType {
    Session,
}

/// Shared tree fields present on v2+/v3 entries. Absent on raw v1 lines.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionEntryBase {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// `null` is written for roots in v2/v3; key omitted in v1.
    #[serde(
        default,
        skip_serializing_if = "is_absent",
        serialize_with = "serialize_null_or",
        rename = "parentId"
    )]
    pub parent_id: NullOr<String>,
    pub timestamp: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionMessageEntry {
    #[serde(rename = "type")]
    pub entry_type: MessageEntryType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "is_absent",
        serialize_with = "serialize_null_or",
        rename = "parentId"
    )]
    pub parent_id: NullOr<String>,
    pub timestamp: String,
    /// Opaque wire message (user / assistant / toolResult / bashExecution / custom).
    pub message: Value,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MessageEntryType {
    Message,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ThinkingLevelChangeEntry {
    #[serde(rename = "type")]
    pub entry_type: ThinkingLevelChangeType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "is_absent",
        serialize_with = "serialize_null_or",
        rename = "parentId"
    )]
    pub parent_id: NullOr<String>,
    pub timestamp: String,
    #[serde(rename = "thinkingLevel")]
    pub thinking_level: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ThinkingLevelChangeType {
    #[serde(rename = "thinking_level_change")]
    ThinkingLevelChange,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModelChangeEntry {
    #[serde(rename = "type")]
    pub entry_type: ModelChangeType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "is_absent",
        serialize_with = "serialize_null_or",
        rename = "parentId"
    )]
    pub parent_id: NullOr<String>,
    pub timestamp: String,
    pub provider: String,
    #[serde(rename = "modelId")]
    pub model_id: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ModelChangeType {
    #[serde(rename = "model_change")]
    ModelChange,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CompactionEntry {
    #[serde(rename = "type")]
    pub entry_type: CompactionType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "is_absent",
        serialize_with = "serialize_null_or",
        rename = "parentId"
    )]
    pub parent_id: NullOr<String>,
    pub timestamp: String,
    pub summary: String,
    /// v2+/v3 field.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "firstKeptEntryId"
    )]
    pub first_kept_entry_id: Option<String>,
    /// v1 field (index into file entries). Migrated to firstKeptEntryId on load.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "firstKeptEntryIndex"
    )]
    pub first_kept_entry_index: Option<u64>,
    #[serde(rename = "tokensBefore")]
    pub tokens_before: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "fromHook")]
    pub from_hook: Option<bool>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CompactionType {
    Compaction,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BranchSummaryEntry {
    #[serde(rename = "type")]
    pub entry_type: BranchSummaryType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "is_absent",
        serialize_with = "serialize_null_or",
        rename = "parentId"
    )]
    pub parent_id: NullOr<String>,
    pub timestamp: String,
    #[serde(rename = "fromId")]
    pub from_id: String,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "fromHook")]
    pub from_hook: Option<bool>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum BranchSummaryType {
    #[serde(rename = "branch_summary")]
    BranchSummary,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CustomEntry {
    #[serde(rename = "type")]
    pub entry_type: CustomTypeTag,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "is_absent",
        serialize_with = "serialize_null_or",
        rename = "parentId"
    )]
    pub parent_id: NullOr<String>,
    pub timestamp: String,
    #[serde(rename = "customType")]
    pub custom_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CustomTypeTag {
    Custom,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CustomMessageEntry {
    #[serde(rename = "type")]
    pub entry_type: CustomMessageType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "is_absent",
        serialize_with = "serialize_null_or",
        rename = "parentId"
    )]
    pub parent_id: NullOr<String>,
    pub timestamp: String,
    #[serde(rename = "customType")]
    pub custom_type: String,
    pub content: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
    pub display: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CustomMessageType {
    #[serde(rename = "custom_message")]
    CustomMessage,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LabelEntry {
    #[serde(rename = "type")]
    pub entry_type: LabelType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "is_absent",
        serialize_with = "serialize_null_or",
        rename = "parentId"
    )]
    pub parent_id: NullOr<String>,
    pub timestamp: String,
    #[serde(rename = "targetId")]
    pub target_id: String,
    /// May be explicitly null / missing when clearing a label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LabelType {
    Label,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionInfoEntry {
    #[serde(rename = "type")]
    pub entry_type: SessionInfoType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "is_absent",
        serialize_with = "serialize_null_or",
        rename = "parentId"
    )]
    pub parent_id: NullOr<String>,
    pub timestamp: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionInfoType {
    #[serde(rename = "session_info")]
    SessionInfo,
}

/// Session entry (non-header). Internally tagged on `type`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum SessionEntry {
    #[serde(rename = "message")]
    Message {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        #[serde(
            default,
            skip_serializing_if = "is_absent",
            serialize_with = "serialize_null_or",
            rename = "parentId"
        )]
        parent_id: NullOr<String>,
        timestamp: String,
        message: Value,
    },
    #[serde(rename = "thinking_level_change")]
    ThinkingLevelChange {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        #[serde(
            default,
            skip_serializing_if = "is_absent",
            serialize_with = "serialize_null_or",
            rename = "parentId"
        )]
        parent_id: NullOr<String>,
        timestamp: String,
        #[serde(rename = "thinkingLevel")]
        thinking_level: String,
    },
    #[serde(rename = "model_change")]
    ModelChange {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        #[serde(
            default,
            skip_serializing_if = "is_absent",
            serialize_with = "serialize_null_or",
            rename = "parentId"
        )]
        parent_id: NullOr<String>,
        timestamp: String,
        provider: String,
        #[serde(rename = "modelId")]
        model_id: String,
    },
    #[serde(rename = "compaction")]
    Compaction {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        #[serde(
            default,
            skip_serializing_if = "is_absent",
            serialize_with = "serialize_null_or",
            rename = "parentId"
        )]
        parent_id: NullOr<String>,
        timestamp: String,
        summary: String,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            rename = "firstKeptEntryId"
        )]
        first_kept_entry_id: Option<String>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            rename = "firstKeptEntryIndex"
        )]
        first_kept_entry_index: Option<u64>,
        #[serde(rename = "tokensBefore")]
        tokens_before: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        details: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none", rename = "fromHook")]
        from_hook: Option<bool>,
    },
    #[serde(rename = "branch_summary")]
    BranchSummary {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        #[serde(
            default,
            skip_serializing_if = "is_absent",
            serialize_with = "serialize_null_or",
            rename = "parentId"
        )]
        parent_id: NullOr<String>,
        timestamp: String,
        #[serde(rename = "fromId")]
        from_id: String,
        summary: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        details: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none", rename = "fromHook")]
        from_hook: Option<bool>,
    },
    #[serde(rename = "custom")]
    Custom {
        #[serde(rename = "customType")]
        custom_type: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        data: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        #[serde(
            default,
            skip_serializing_if = "is_absent",
            serialize_with = "serialize_null_or",
            rename = "parentId"
        )]
        parent_id: NullOr<String>,
        timestamp: String,
    },
    #[serde(rename = "custom_message")]
    CustomMessage {
        #[serde(rename = "customType")]
        custom_type: String,
        content: Value,
        display: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        details: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        #[serde(
            default,
            skip_serializing_if = "is_absent",
            serialize_with = "serialize_null_or",
            rename = "parentId"
        )]
        parent_id: NullOr<String>,
        timestamp: String,
    },
    #[serde(rename = "label")]
    Label {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        #[serde(
            default,
            skip_serializing_if = "is_absent",
            serialize_with = "serialize_null_or",
            rename = "parentId"
        )]
        parent_id: NullOr<String>,
        timestamp: String,
        #[serde(rename = "targetId")]
        target_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
    },
    #[serde(rename = "session_info")]
    SessionInfo {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        #[serde(
            default,
            skip_serializing_if = "is_absent",
            serialize_with = "serialize_null_or",
            rename = "parentId"
        )]
        parent_id: NullOr<String>,
        timestamp: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
}

impl SessionEntry {
    pub fn id(&self) -> Option<&str> {
        match self {
            Self::Message { id, .. }
            | Self::ThinkingLevelChange { id, .. }
            | Self::ModelChange { id, .. }
            | Self::Compaction { id, .. }
            | Self::BranchSummary { id, .. }
            | Self::Custom { id, .. }
            | Self::CustomMessage { id, .. }
            | Self::Label { id, .. }
            | Self::SessionInfo { id, .. } => id.as_deref(),
        }
    }

    pub fn parent_id(&self) -> &NullOr<String> {
        match self {
            Self::Message { parent_id, .. }
            | Self::ThinkingLevelChange { parent_id, .. }
            | Self::ModelChange { parent_id, .. }
            | Self::Compaction { parent_id, .. }
            | Self::BranchSummary { parent_id, .. }
            | Self::Custom { parent_id, .. }
            | Self::CustomMessage { parent_id, .. }
            | Self::Label { parent_id, .. }
            | Self::SessionInfo { parent_id, .. } => parent_id,
        }
    }

    pub fn timestamp(&self) -> &str {
        match self {
            Self::Message { timestamp, .. }
            | Self::ThinkingLevelChange { timestamp, .. }
            | Self::ModelChange { timestamp, .. }
            | Self::Compaction { timestamp, .. }
            | Self::BranchSummary { timestamp, .. }
            | Self::Custom { timestamp, .. }
            | Self::CustomMessage { timestamp, .. }
            | Self::Label { timestamp, .. }
            | Self::SessionInfo { timestamp, .. } => timestamp,
        }
    }

    pub fn set_id(&mut self, new_id: String) {
        match self {
            Self::Message { id, .. }
            | Self::ThinkingLevelChange { id, .. }
            | Self::ModelChange { id, .. }
            | Self::Compaction { id, .. }
            | Self::BranchSummary { id, .. }
            | Self::Custom { id, .. }
            | Self::CustomMessage { id, .. }
            | Self::Label { id, .. }
            | Self::SessionInfo { id, .. } => *id = Some(new_id),
        }
    }

    pub fn set_parent_id(&mut self, parent: NullOr<String>) {
        match self {
            Self::Message { parent_id, .. }
            | Self::ThinkingLevelChange { parent_id, .. }
            | Self::ModelChange { parent_id, .. }
            | Self::Compaction { parent_id, .. }
            | Self::BranchSummary { parent_id, .. }
            | Self::Custom { parent_id, .. }
            | Self::CustomMessage { parent_id, .. }
            | Self::Label { parent_id, .. }
            | Self::SessionInfo { parent_id, .. } => *parent_id = parent,
        }
    }
}

/// Header or entry as stored in the JSONL file.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum FileEntry {
    Header(SessionHeader),
    Entry(SessionEntry),
}

impl FileEntry {
    pub fn is_header(&self) -> bool {
        matches!(self, Self::Header(_))
    }

    pub fn as_header(&self) -> Option<&SessionHeader> {
        match self {
            Self::Header(h) => Some(h),
            Self::Entry(_) => None,
        }
    }

    pub fn as_entry(&self) -> Option<&SessionEntry> {
        match self {
            Self::Header(_) => None,
            Self::Entry(e) => Some(e),
        }
    }

    pub fn as_entry_mut(&mut self) -> Option<&mut SessionEntry> {
        match self {
            Self::Header(_) => None,
            Self::Entry(e) => Some(e),
        }
    }
}

/// Parse a single JSONL line into a [`FileEntry`]. Returns `None` for blank / malformed lines.
pub fn parse_session_entry_line(line: &str) -> Option<FileEntry> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    serde_json::from_str(trimmed).ok()
}

/// Parse a full session file body (JSONL text) into entries, skipping bad lines.
pub fn parse_session_entries(content: &str) -> Vec<FileEntry> {
    content
        .lines()
        .filter_map(parse_session_entry_line)
        .collect()
}

/// Serialize one file entry to a compact JSON line (no trailing newline).
pub fn serialize_file_entry_line(entry: &FileEntry) -> Result<String, serde_json::Error> {
    serde_json::to_string(entry)
}

/// Serialize entries as JSONL (each line + trailing newline, matching pi's writer).
pub fn serialize_session_jsonl(entries: &[FileEntry]) -> Result<String, serde_json::Error> {
    let mut out = String::new();
    for entry in entries {
        out.push_str(&serialize_file_entry_line(entry)?);
        out.push('\n');
    }
    Ok(out)
}
