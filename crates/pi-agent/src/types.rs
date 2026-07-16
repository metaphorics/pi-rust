//! Agent types — port of packages/agent/src/types.ts.

use std::{future::Future, pin::Pin, sync::Arc};

use parking_lot::Mutex;
use pi_ai::{
    AssistantMessage, AssistantMessageEvent, AssistantMessageEventStream, Content, Context,
    ImageContent, Message, Model, TextContent, ThinkingLevel, ToolCall, ToolResultMessage,
    UserMessage,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::cancel::CancellationToken;

/// How tool calls from one assistant message are executed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ToolExecutionMode {
    Sequential,
    #[default]
    Parallel,
}

/// Queued user-message drain policy (used by higher-level Agent wrapper).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum QueueMode {
    #[default]
    All,
    OneAtATime,
}

/// A single tool-call content block from an assistant message.
pub type AgentToolCall = ToolCall;

/// Opaque UI renderer slot. Phase 5 binds real TUI/sidecar renderers here.
///
/// The agent loop never invokes this; it is payload carried on
/// [`ToolDefinition`] so UI layers can attach without forking the tool type.
#[derive(Default)]
pub struct ToolRenderer {
    _private: (),
}

impl std::fmt::Debug for ToolRenderer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ToolRenderer")
    }
}

/// Result returned from `before_tool_call`.
#[derive(Clone, Debug, Default)]
pub struct BeforeToolCallResult {
    pub block: bool,
    pub reason: Option<String>,
}

/// Partial override returned from `after_tool_call`.
#[derive(Clone, Debug, Default)]
pub struct AfterToolCallResult {
    pub content: Option<Vec<Content>>,
    pub details: Option<Value>,
    pub is_error: Option<bool>,
    pub terminate: Option<bool>,
}

/// Context passed to `before_tool_call`.
#[derive(Clone, Debug)]
pub struct BeforeToolCallContext {
    pub assistant_message: AssistantMessage,
    pub tool_call: AgentToolCall,
    /// Validated arguments. Shared so hooks can mutate them in place (JS object identity).
    pub args: Arc<Mutex<Value>>,
    pub context: AgentContext,
}

/// Context passed to `after_tool_call`.
#[derive(Clone, Debug)]
pub struct AfterToolCallContext {
    pub assistant_message: AssistantMessage,
    pub tool_call: AgentToolCall,
    pub args: Value,
    pub result: AgentToolResult,
    pub is_error: bool,
    pub context: AgentContext,
}

/// Context passed to `should_stop_after_turn` / `prepare_next_turn`.
#[derive(Clone, Debug)]
pub struct ShouldStopAfterTurnContext {
    pub message: AssistantMessage,
    pub tool_results: Vec<ToolResultMessage>,
    pub context: AgentContext,
    pub new_messages: Vec<AgentMessage>,
}

pub type PrepareNextTurnContext = ShouldStopAfterTurnContext;

/// Replacement runtime state used before the next provider request.
#[derive(Clone, Debug, Default)]
pub struct AgentLoopTurnUpdate {
    pub context: Option<AgentContext>,
    pub model: Option<Model>,
    pub thinking_level: Option<AgentThinkingLevel>,
}

/// Thinking level including `"off"` (agent-layer; maps to stream option absence).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AgentThinkingLevel {
    #[default]
    Off,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl From<AgentThinkingLevel> for Option<ThinkingLevel> {
    fn from(value: AgentThinkingLevel) -> Self {
        match value {
            AgentThinkingLevel::Off => None,
            AgentThinkingLevel::Minimal => Some(ThinkingLevel::Minimal),
            AgentThinkingLevel::Low => Some(ThinkingLevel::Low),
            AgentThinkingLevel::Medium => Some(ThinkingLevel::Medium),
            AgentThinkingLevel::High => Some(ThinkingLevel::High),
            AgentThinkingLevel::Xhigh => Some(ThinkingLevel::Xhigh),
            AgentThinkingLevel::Max => Some(ThinkingLevel::Max),
        }
    }
}

/// Agent-level message: LLM messages plus custom app messages.
///
/// Custom variants carry an arbitrary JSON object that must include a `role`
/// string field. `convert_to_llm` is responsible for projecting them to
/// wire [`Message`]s (or filtering them out).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
#[allow(clippy::large_enum_variant)]
pub enum AgentMessage {
    Standard(Message),
    Custom(Value),
}

impl AgentMessage {
    pub fn role(&self) -> &str {
        match self {
            Self::Standard(Message::User(_)) => "user",
            Self::Standard(Message::Assistant(_)) => "assistant",
            Self::Standard(Message::ToolResult(_)) => "toolResult",
            Self::Custom(value) => value
                .get("role")
                .and_then(Value::as_str)
                .unwrap_or("custom"),
        }
    }

    pub fn as_message(&self) -> Option<&Message> {
        match self {
            Self::Standard(message) => Some(message),
            Self::Custom(_) => None,
        }
    }

    pub fn into_message(self) -> Option<Message> {
        match self {
            Self::Standard(message) => Some(message),
            Self::Custom(_) => None,
        }
    }

    pub fn user(message: UserMessage) -> Self {
        Self::Standard(Message::User(message))
    }

    pub fn assistant(message: AssistantMessage) -> Self {
        Self::Standard(Message::Assistant(message))
    }

    pub fn tool_result(message: ToolResultMessage) -> Self {
        Self::Standard(Message::ToolResult(message))
    }
}

impl From<Message> for AgentMessage {
    fn from(value: Message) -> Self {
        Self::Standard(value)
    }
}

impl From<UserMessage> for AgentMessage {
    fn from(value: UserMessage) -> Self {
        Self::user(value)
    }
}

impl From<AssistantMessage> for AgentMessage {
    fn from(value: AssistantMessage) -> Self {
        Self::assistant(value)
    }
}

impl From<ToolResultMessage> for AgentMessage {
    fn from(value: ToolResultMessage) -> Self {
        Self::tool_result(value)
    }
}

/// Final or partial result produced by a tool.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentToolResult {
    pub content: Vec<Content>,
    #[serde(default)]
    pub details: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub added_tool_names: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminate: Option<bool>,
}

impl AgentToolResult {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            content: vec![Content::Text(TextContent {
                text: text.into().into(),
                text_signature: None,
            })],
            details: Value::Object(Default::default()),
            added_tool_names: None,
            terminate: None,
        }
    }

    pub fn error_text(message: impl Into<String>) -> Self {
        Self::text(message)
    }
}

/// Partial-result callback for streaming tool execution.
pub type AgentToolUpdateCallback = Arc<dyn Fn(AgentToolResult) + Send + Sync + 'static>;

type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Async tool execute function.
pub type ToolExecuteFn = Arc<
    dyn Fn(
            String,
            Value,
            Option<CancellationToken>,
            Option<AgentToolUpdateCallback>,
        ) -> BoxFuture<'static, Result<AgentToolResult, String>>
        + Send
        + Sync,
>;

/// Optional argument preparer (compat shim before schema validation).
pub type PrepareArgumentsFn = Arc<dyn Fn(Value) -> Value + Send + Sync>;

/// Tool definition used by the agent runtime.
///
/// Mirrors `AgentTool` / coding-agent `ToolDefinition` fields that the loop
/// needs, plus an opaque [`renderer`](Self::renderer) slot for Phase 5 UI.
pub struct ToolDefinition {
    pub name: String,
    pub label: String,
    pub description: String,
    /// JSON Schema object describing parameters (LLM-facing).
    pub parameters: Value,
    pub execution_mode: Option<ToolExecutionMode>,
    pub prepare_arguments: Option<PrepareArgumentsFn>,
    pub execute: ToolExecuteFn,
    /// Opaque renderer slot — Phase 5 binds UI; the loop never calls it.
    pub renderer: Option<ToolRenderer>,
}

impl std::fmt::Debug for ToolDefinition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolDefinition")
            .field("name", &self.name)
            .field("label", &self.label)
            .field("description", &self.description)
            .field("parameters", &self.parameters)
            .field("execution_mode", &self.execution_mode)
            .field("has_prepare_arguments", &self.prepare_arguments.is_some())
            .field("has_renderer", &self.renderer.is_some())
            .finish()
    }
}

impl ToolDefinition {
    /// Project to the pi-ai wire [`pi_ai::Tool`] shape for LLM context.
    pub fn to_llm_tool(&self) -> pi_ai::Tool {
        pi_ai::Tool {
            name: self.name.clone(),
            description: self.description.clone(),
            parameters: self.parameters.clone(),
        }
    }
}

/// Context snapshot passed into the low-level agent loop.
#[derive(Clone, Debug, Default)]
pub struct AgentContext {
    pub system_prompt: String,
    pub messages: Vec<AgentMessage>,
    pub tools: Vec<Arc<ToolDefinition>>,
}

/// Events emitted by the agent loop for UI / session updates.
#[derive(Clone, Debug)]
#[allow(clippy::large_enum_variant)]
pub enum AgentEvent {
    AgentStart,
    AgentEnd {
        messages: Vec<AgentMessage>,
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
}

impl AgentEvent {
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
        }
    }
}

/// Stream function used by the agent loop (injectable for tests / mocks).
pub type StreamFn = Arc<
    dyn Fn(Model, Context, StreamCallOptions) -> BoxFuture<'static, AssistantMessageEventStream>
        + Send
        + Sync,
>;

/// Options passed to [`StreamFn`] for one provider call.
#[derive(Clone, Debug, Default)]
pub struct StreamCallOptions {
    pub temperature: Option<f64>,
    pub max_tokens: Option<u64>,
    pub api_key: Option<String>,
    pub reasoning: Option<ThinkingLevel>,
    pub cancel: Option<CancellationToken>,
    pub session_id: Option<String>,
    pub metadata: Option<std::collections::HashMap<String, Value>>,
}

type AsyncHookFuture<'a, T> = BoxFuture<'a, T>;

pub type ConvertToLlmFn =
    Arc<dyn Fn(Vec<AgentMessage>) -> BoxFuture<'static, Vec<Message>> + Send + Sync>;

pub type TransformContextFn = Arc<
    dyn Fn(Vec<AgentMessage>, Option<CancellationToken>) -> BoxFuture<'static, Vec<AgentMessage>>
        + Send
        + Sync,
>;

pub type GetApiKeyFn = Arc<dyn Fn(String) -> BoxFuture<'static, Option<String>> + Send + Sync>;

pub type ShouldStopAfterTurnFn =
    Arc<dyn Fn(ShouldStopAfterTurnContext) -> BoxFuture<'static, bool> + Send + Sync>;

pub type PrepareNextTurnFn = Arc<
    dyn Fn(PrepareNextTurnContext) -> BoxFuture<'static, Option<AgentLoopTurnUpdate>> + Send + Sync,
>;

pub type GetMessagesFn = Arc<dyn Fn() -> BoxFuture<'static, Vec<AgentMessage>> + Send + Sync>;

pub type BeforeToolCallFn = Arc<
    dyn Fn(
            BeforeToolCallContext,
            Option<CancellationToken>,
        ) -> BoxFuture<'static, Option<BeforeToolCallResult>>
        + Send
        + Sync,
>;

pub type AfterToolCallFn = Arc<
    dyn Fn(
            AfterToolCallContext,
            Option<CancellationToken>,
        ) -> BoxFuture<'static, Option<AfterToolCallResult>>
        + Send
        + Sync,
>;

/// Configuration for the low-level agent loop.
pub struct AgentLoopConfig {
    pub model: Model,
    pub convert_to_llm: ConvertToLlmFn,
    pub transform_context: Option<TransformContextFn>,
    pub get_api_key: Option<GetApiKeyFn>,
    pub should_stop_after_turn: Option<ShouldStopAfterTurnFn>,
    pub prepare_next_turn: Option<PrepareNextTurnFn>,
    pub get_steering_messages: Option<GetMessagesFn>,
    pub get_follow_up_messages: Option<GetMessagesFn>,
    pub tool_execution: ToolExecutionMode,
    pub before_tool_call: Option<BeforeToolCallFn>,
    pub after_tool_call: Option<AfterToolCallFn>,
    pub api_key: Option<String>,
    pub reasoning: Option<ThinkingLevel>,
    pub temperature: Option<f64>,
    pub max_tokens: Option<u64>,
    pub session_id: Option<String>,
    pub metadata: Option<std::collections::HashMap<String, Value>>,
}

impl AgentLoopConfig {
    pub fn new(model: Model, convert_to_llm: ConvertToLlmFn) -> Self {
        Self {
            model,
            convert_to_llm,
            transform_context: None,
            get_api_key: None,
            should_stop_after_turn: None,
            prepare_next_turn: None,
            get_steering_messages: None,
            get_follow_up_messages: None,
            tool_execution: ToolExecutionMode::Parallel,
            before_tool_call: None,
            after_tool_call: None,
            api_key: None,
            reasoning: None,
            temperature: None,
            max_tokens: None,
            session_id: None,
            metadata: None,
        }
    }
}

/// Event sink used by the imperative run APIs.
pub type AgentEventSink = Arc<dyn Fn(AgentEvent) -> BoxFuture<'static, ()> + Send + Sync>;

/// Helper: identity convert that keeps standard LLM messages only.
pub fn identity_convert_to_llm(messages: Vec<AgentMessage>) -> Vec<Message> {
    messages
        .into_iter()
        .filter_map(AgentMessage::into_message)
        .collect()
}

pub fn identity_convert_to_llm_fn() -> ConvertToLlmFn {
    Arc::new(|messages| Box::pin(async move { identity_convert_to_llm(messages) }))
}

/// Helper constructors for text content used by tools/tests.
pub fn text_content(text: impl Into<String>) -> Content {
    Content::Text(TextContent {
        text: text.into().into(),
        text_signature: None,
    })
}

pub fn image_content(data: impl Into<String>, mime_type: impl Into<String>) -> Content {
    Content::Image(ImageContent {
        data: data.into(),
        mime_type: mime_type.into(),
    })
}

/// Extract tool calls from an assistant message (content order preserved).
pub fn tool_calls_from_message(message: &AssistantMessage) -> Vec<AgentToolCall> {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            Content::ToolCall(call) => Some(call.clone()),
            _ => None,
        })
        .collect()
}

// Silence unused import of AsyncHookFuture alias if not referenced externally.
const _: fn() = || {
    let _: Option<AsyncHookFuture<'static, ()>> = None;
};
