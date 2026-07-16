//! RPC wire types — port of `modes/rpc/rpc-types.ts`.
//!
//! Commands arrive as JSON lines on stdin; responses and events leave as JSON
//! lines on stdout. Serde field order is the wire order; `undefined` fields
//! are omitted (`skip_serializing_if`), `null` fields serialize as `null`.

use pi_agent::AgentThinkingLevel;
use pi_ai::{ImageContent, Model};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::extension_bridge::{NotifyType, WidgetPlacement};
use crate::session::StreamingBehavior;
use crate::source_info::SourceInfo;

// ============================================================================
// Responses (stdout)
// ============================================================================

/// Response envelope: `{ id?, type: "response", command, success, data? }` on
/// success, `{ id?, type, command?, success: false, error }` on failure.
///
/// `id` and `command` are echoed verbatim (any JSON value, matching the
/// oracle's untyped passthrough); `data: Some(Value::Null)` serializes as
/// `"data":null` (oracle `success(id, cmd, null)`), `None` omits the key.
#[derive(Clone, Debug, Serialize)]
pub struct RpcResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<Value>,
    #[serde(rename = "type")]
    pub kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<Value>,
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl RpcResponse {
    /// Oracle `success(id, command, data?)`.
    pub fn success(id: Option<Value>, command: &str, data: Option<Value>) -> Self {
        Self {
            id,
            kind: "response",
            command: Some(Value::String(command.to_string())),
            success: true,
            data,
            error: None,
        }
    }

    /// Oracle `error(id, command, message)`. `command` is echoed as-is; a
    /// missing `type` on the incoming command omits the key (JS `undefined`).
    pub fn error(id: Option<Value>, command: Option<Value>, message: impl Into<String>) -> Self {
        Self {
            id,
            kind: "response",
            command,
            success: false,
            data: None,
            error: Some(message.into()),
        }
    }
}

/// Oracle `RpcSessionState` (rpc-types.ts:96-110); field order is wire order.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcSessionState {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<Model>,
    pub thinking_level: AgentThinkingLevel,
    pub is_streaming: bool,
    pub is_compacting: bool,
    pub steering_mode: &'static str,
    pub follow_up_mode: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_file: Option<String>,
    pub session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_name: Option<String>,
    pub auto_compaction_enabled: bool,
    pub message_count: usize,
    pub pending_message_count: usize,
}

/// A command available for invocation via prompt (oracle `RpcSlashCommand`).
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcSlashCommand {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub source: &'static str,
    pub source_info: SourceInfo,
}

// ============================================================================
// Commands (stdin)
// ============================================================================

/// Typed payloads for commands that carry fields. The `type`/`id` envelope is
/// parsed untyped first (parse errors and unknown commands need the raw
/// values); these structs decode the remainder.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptPayload {
    pub message: String,
    #[serde(default)]
    pub images: Option<Vec<ImageContent>>,
    #[serde(default)]
    pub streaming_behavior: Option<StreamingBehavior>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessagePayload {
    pub message: String,
    #[serde(default)]
    pub images: Option<Vec<ImageContent>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewSessionPayload {
    #[serde(default)]
    pub parent_session: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetModelPayload {
    pub provider: String,
    pub model_id: String,
}

#[derive(Debug, Deserialize)]
pub struct SetThinkingLevelPayload {
    /// Untyped in the oracle runtime: any string arrives and is clamped by
    /// `setThinkingLevel` (unknown parses to `off`, then model clamping).
    pub level: String,
}

#[derive(Debug, Deserialize)]
pub struct SetQueueModePayload {
    pub mode: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactPayload {
    #[serde(default)]
    pub custom_instructions: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct SetEnabledPayload {
    pub enabled: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BashPayload {
    pub command: String,
    #[serde(default)]
    pub exclude_from_context: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SwitchSessionPayload {
    pub session_path: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ForkPayload {
    pub entry_id: String,
}

#[derive(Debug, Deserialize)]
pub struct GetEntriesPayload {
    #[serde(default)]
    pub since: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct SetSessionNamePayload {
    pub name: String,
}

// ============================================================================
// Extension UI (stdout requests / stdin responses)
// ============================================================================

/// Emitted when an extension needs user input (oracle
/// `RpcExtensionUIRequest`). Wire order: `type`, `id`, `method`, fields.
#[derive(Clone, Debug, Serialize)]
pub struct RpcExtensionUiRequest {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub id: String,
    #[serde(flatten)]
    pub payload: UiRequestPayload,
}

impl RpcExtensionUiRequest {
    pub fn new(id: String, payload: UiRequestPayload) -> Self {
        Self {
            kind: "extension_ui_request",
            id,
            payload,
        }
    }
}

/// Method-specific request payloads (`method` + fields, oracle order).
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "method", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum UiRequestPayload {
    Select {
        title: String,
        options: Vec<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        timeout: Option<u64>,
    },
    Confirm {
        title: String,
        message: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        timeout: Option<u64>,
    },
    Input {
        title: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        placeholder: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        timeout: Option<u64>,
    },
    Editor {
        title: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        prefill: Option<String>,
    },
    Notify {
        message: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        notify_type: Option<NotifyType>,
    },
    SetStatus {
        status_key: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        status_text: Option<String>,
    },
    SetWidget {
        widget_key: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        widget_lines: Option<Vec<String>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        widget_placement: Option<WidgetPlacement>,
    },
    SetTitle {
        title: String,
    },
    #[serde(rename = "set_editor_text")]
    SetEditorText {
        text: String,
    },
}

/// Response to an extension UI request (oracle `RpcExtensionUIResponse`).
#[derive(Clone, Debug, Deserialize)]
pub struct RpcExtensionUiResponse {
    pub id: String,
    #[serde(default)]
    pub value: Option<String>,
    #[serde(default)]
    pub confirmed: Option<bool>,
    #[serde(default)]
    pub cancelled: Option<bool>,
}

impl RpcExtensionUiResponse {
    pub fn is_cancelled(&self) -> bool {
        self.cancelled == Some(true)
    }
}
