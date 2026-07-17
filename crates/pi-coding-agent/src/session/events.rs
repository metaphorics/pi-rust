//! Serializable session wire events.
//!
//! Port of `AgentSessionEvent` from `core/agent-session.ts:127-153`. This enum
//! IS the `--mode json` and RPC event surface: `pi_agent::AgentEvent`
//! (deliberately not `Serialize`) maps into it at the session boundary, adding
//! `willRetry` on `agent_end`.

use pi_agent::{AgentEvent, AgentMessage, AgentThinkingLevel, AgentToolResult};
use pi_ai::{AssistantMessageEvent, ToolResultMessage};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::session_types::SessionEntry;

/// Why a compaction ran (`"manual" | "threshold" | "overflow"`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CompactionReason {
    Manual,
    Threshold,
    Overflow,
}

/// Result from `compact()` (oracle `CompactionResult`, compaction.ts:86-93).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionResult {
    pub summary: String,
    pub first_kept_entry_id: String,
    pub tokens_before: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_tokens_after: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}

/// Session-level event stream (19 variants, wire-compatible with pi 0.80.7).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
#[allow(clippy::large_enum_variant)]
pub enum AgentSessionEvent {
    AgentStart,
    AgentEnd {
        messages: Vec<AgentMessage>,
        will_retry: bool,
    },
    TurnStart,
    TurnEnd {
        message: AgentMessage,
        tool_results: Vec<ToolResultMessage>,
    },
    MessageStart {
        message: AgentMessage,
    },
    MessageUpdate {
        message: AgentMessage,
        assistant_message_event: AssistantMessageEvent,
    },
    MessageEnd {
        message: AgentMessage,
    },
    ToolExecutionStart {
        tool_call_id: String,
        tool_name: String,
        args: Value,
    },
    ToolExecutionUpdate {
        tool_call_id: String,
        tool_name: String,
        args: Value,
        partial_result: AgentToolResult,
    },
    ToolExecutionEnd {
        tool_call_id: String,
        tool_name: String,
        result: AgentToolResult,
        is_error: bool,
    },
    AgentSettled,
    QueueUpdate {
        steering: Vec<String>,
        follow_up: Vec<String>,
    },
    CompactionStart {
        reason: CompactionReason,
    },
    CompactionEnd {
        reason: CompactionReason,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result: Option<CompactionResult>,
        aborted: bool,
        will_retry: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error_message: Option<String>,
    },
    EntryAppended {
        entry: SessionEntry,
    },
    SessionInfoChanged {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    ThinkingLevelChanged {
        level: AgentThinkingLevel,
    },
    AutoRetryStart {
        attempt: u32,
        max_attempts: u32,
        delay_ms: u64,
        error_message: String,
    },
    AutoRetryEnd {
        success: bool,
        attempt: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        final_error: Option<String>,
    },
}

impl AgentSessionEvent {
    /// Wire tag for this event (matches the serialized `type` field).
    pub fn event_type(&self) -> &'static str {
        match self {
            Self::AgentStart => "agent_start",
            Self::AgentEnd { .. } => "agent_end",
            Self::TurnStart => "turn_start",
            Self::TurnEnd { .. } => "turn_end",
            Self::MessageStart { .. } => "message_start",
            Self::MessageUpdate { .. } => "message_update",
            Self::MessageEnd { .. } => "message_end",
            Self::ToolExecutionStart { .. } => "tool_execution_start",
            Self::ToolExecutionUpdate { .. } => "tool_execution_update",
            Self::ToolExecutionEnd { .. } => "tool_execution_end",
            Self::AgentSettled => "agent_settled",
            Self::QueueUpdate { .. } => "queue_update",
            Self::CompactionStart { .. } => "compaction_start",
            Self::CompactionEnd { .. } => "compaction_end",
            Self::EntryAppended { .. } => "entry_appended",
            Self::SessionInfoChanged { .. } => "session_info_changed",
            Self::ThinkingLevelChanged { .. } => "thinking_level_changed",
            Self::AutoRetryStart { .. } => "auto_retry_start",
            Self::AutoRetryEnd { .. } => "auto_retry_end",
        }
    }

    /// Map a loop-level [`AgentEvent`] into the session wire event.
    ///
    /// `will_retry` is only consulted for `agent_end` (oracle adds it in
    /// `_handleAgentEvent`, agent-session.ts:452).
    pub fn from_agent_event(event: AgentEvent, will_retry: bool) -> Self {
        match event {
            AgentEvent::AgentStart => Self::AgentStart,
            AgentEvent::AgentEnd { messages } => Self::AgentEnd {
                messages,
                will_retry,
            },
            AgentEvent::TurnStart => Self::TurnStart,
            AgentEvent::TurnEnd {
                message,
                tool_results,
            } => Self::TurnEnd {
                message,
                tool_results,
            },
            AgentEvent::MessageStart { message } => Self::MessageStart { message },
            AgentEvent::MessageUpdate {
                message,
                assistant_message_event,
            } => Self::MessageUpdate {
                message,
                assistant_message_event,
            },
            AgentEvent::MessageEnd { message } => Self::MessageEnd { message },
            AgentEvent::ToolExecutionStart {
                tool_call_id,
                tool_name,
                args,
            } => Self::ToolExecutionStart {
                tool_call_id,
                tool_name,
                args,
            },
            AgentEvent::ToolExecutionUpdate {
                tool_call_id,
                tool_name,
                args,
                partial_result,
            } => Self::ToolExecutionUpdate {
                tool_call_id,
                tool_name,
                args,
                partial_result,
            },
            AgentEvent::ToolExecutionEnd {
                tool_call_id,
                tool_name,
                result,
                is_error,
            } => Self::ToolExecutionEnd {
                tool_call_id,
                tool_name,
                result,
                is_error,
            },
        }
    }
}
