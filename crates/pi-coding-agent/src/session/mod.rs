//! AgentSession — the stateful façade over the pi-agent loop.
//!
//! Port of `core/agent-session.ts:335` collapsed with the `Agent` class from
//! `packages/agent/src/agent.ts` (global constraint 6: nothing else consumes a
//! bare Agent, so the two layers are one type here). It owns agent state,
//! steering/follow-up queues, the active-run invariant, session persistence,
//! model/thinking management, compaction, tree navigation, and bash execution.
//!
//! Extension hooks are Phase 6; the seams they need (event fan-out, tool
//! registry, lifecycle hooks in [`runtime`]) exist without them.

pub mod events;
pub mod runtime;
pub mod services;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use pi_agent::{
    AgentContext, AgentEvent, AgentLoopConfig, AgentMessage, AgentThinkingLevel, CancellationToken,
    QueueMode, StreamFn, ToolDefinition, run_agent_loop, run_agent_loop_continue,
};
use pi_ai::models::{clamp_thinking_level, get_supported_thinking_levels, models_are_equal};
use pi_ai::types::ModelThinkingLevel;
use pi_ai::utils::{is_context_overflow, is_retryable_assistant_error};
use pi_ai::{
    AssistantMessage, Content, ImageContent, Message, Model, StopReason, TextContent, Usage,
    UserContent, UserMessage,
};

use crate::model_registry::ModelRegistry;
use crate::session_manager::SessionManager;
use crate::settings_manager::SettingsManager;
use crate::system_prompt::{
    BuildSystemPromptOptions, ContextFile, Skill, build_system_prompt, get_docs_path,
};

pub use events::{AgentSessionEvent, CompactionReason, CompactionResult};

// ============================================================================
// Verbatim message framing strings (core/messages.ts)
// ============================================================================

pub const COMPACTION_SUMMARY_PREFIX: &str = "The conversation history before this point was compacted into the following summary:\n\n<summary>\n";
pub const COMPACTION_SUMMARY_SUFFIX: &str = "\n</summary>";
pub const BRANCH_SUMMARY_PREFIX: &str =
    "The following is a summary of a branch that this conversation came back from:\n\n<summary>\n";
pub const BRANCH_SUMMARY_SUFFIX: &str = "</summary>";

/// Oracle `DEFAULT_THINKING_LEVEL` (core/defaults.ts).
pub const DEFAULT_THINKING_LEVEL: AgentThinkingLevel = AgentThinkingLevel::Medium;

// ============================================================================
// Auth guidance strings (core/auth-guidance.ts)
// ============================================================================

fn get_provider_login_help() -> String {
    let docs = get_docs_path();
    format!(
        "Use /login to log into a provider via OAuth or API key. See:\n  {}\n  {}",
        docs.join("providers.md").display(),
        docs.join("models.md").display()
    )
}

/// Oracle `formatNoModelsAvailableMessage`.
pub fn format_no_models_available_message() -> String {
    format!("No models available. {}", get_provider_login_help())
}

/// Oracle `formatNoModelSelectedMessage`.
pub fn format_no_model_selected_message() -> String {
    format!(
        "No model selected.\n\n{}\n\nThen use /model to select a model.",
        get_provider_login_help()
    )
}

/// Oracle `formatNoApiKeyFoundMessage`.
pub fn format_no_api_key_found_message(provider: &str) -> String {
    let provider_display = if provider == "unknown" {
        "the selected model"
    } else {
        provider
    };
    format!(
        "No API key found for {}.\n\n{}",
        provider_display,
        get_provider_login_help()
    )
}

fn oauth_reauth_message(provider: &str) -> String {
    format!(
        "Authentication failed for \"{provider}\". Credentials may have expired or network is unavailable. Run '/login {provider}' to re-authenticate.",
    )
}

// ============================================================================
// Public option/result types
// ============================================================================

/// `"steer" | "followUp"` (PromptOptions.streamingBehavior).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StreamingBehavior {
    Steer,
    FollowUp,
}

/// `"steer" | "followUp" | "nextTurn"` (sendCustomMessage `deliverAs`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CustomMessageDelivery {
    Steer,
    FollowUp,
    NextTurn,
}

/// Options for [`AgentSession::send_custom_message`].
#[derive(Clone, Copy, Debug, Default)]
pub struct SendCustomMessageOptions {
    /// If true and not streaming, triggers a new LLM turn.
    pub trigger_turn: bool,
    /// Delivery mode: steer (default while streaming), followUp, or nextTurn.
    pub deliver_as: Option<CustomMessageDelivery>,
}

/// Options for [`AgentSession::prompt`].
#[derive(Default)]
pub struct PromptOptions {
    /// Whether to expand file-based prompt templates (default: true).
    pub expand_prompt_templates: Option<bool>,
    /// Image attachments.
    pub images: Vec<ImageContent>,
    /// When streaming, how to queue the message. Required if streaming.
    pub streaming_behavior: Option<StreamingBehavior>,
    /// Hook used by RPC mode to observe prompt preflight acceptance.
    pub preflight_result: Option<Box<dyn FnOnce(bool) + Send>>,
}

/// A model available for Ctrl+P cycling (from `--models`).
#[derive(Clone, Debug)]
pub struct ScopedModel {
    pub model: Model,
    pub thinking_level: Option<AgentThinkingLevel>,
}

/// Result from [`AgentSession::cycle_model`].
#[derive(Clone, Debug)]
pub struct ModelCycleResult {
    pub model: Model,
    pub thinking_level: AgentThinkingLevel,
    /// Whether cycling through scoped models (`--models`) or all available.
    pub is_scoped: bool,
}

/// File-based prompt template (name/description/content), pre-loaded by the
/// resource layer. Expansion semantics are `prompt-templates.ts`.
#[derive(Clone, Debug)]
pub struct PromptTemplate {
    pub name: String,
    pub description: String,
    pub argument_hint: Option<String>,
    pub content: String,
    pub file_path: PathBuf,
    /// Provenance (oracle `PromptTemplate.sourceInfo`), set by the loader.
    pub source_info: crate::source_info::SourceInfo,
}

/// Tool definition plus the prompt metadata pi layers on top of `AgentTool`.
#[derive(Clone)]
pub struct SessionToolDefinition {
    pub definition: Arc<ToolDefinition>,
    pub prompt_snippet: Option<String>,
    pub prompt_guidelines: Vec<String>,
    /// `"builtin"` / `"sdk"` / `"extension"` (oracle sourceInfo synthetic
    /// label; extension tools carry their real provenance below).
    pub source: &'static str,
    /// Real provenance for extension tools (oracle `RegisteredTool.
    /// sourceInfo`); `None` for built-ins/SDK tools (synthesized on the
    /// wire like pi's `createSyntheticSourceInfo`).
    pub source_info: Option<crate::source_info::SourceInfo>,
}

/// Read-only tool info (oracle `getAllTools`).
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolInfo {
    pub name: String,
    pub description: String,
    pub parameters: Value,
    pub prompt_guidelines: Vec<String>,
    pub source: &'static str,
    /// Real provenance for extension tools; `None` for built-ins/SDK.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_info: Option<crate::source_info::SourceInfo>,
}

/// Context window usage (oracle `ContextUsage`; `null` fields stay `null`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextUsage {
    pub tokens: Option<u64>,
    pub context_window: u64,
    pub percent: Option<f64>,
}

/// Session statistics for `/session` (oracle `SessionStats`).
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionStats {
    pub session_file: Option<String>,
    pub session_id: String,
    pub user_messages: u64,
    pub assistant_messages: u64,
    pub tool_calls: u64,
    pub tool_results: u64,
    pub total_messages: u64,
    pub tokens: SessionTokenStats,
    pub cost: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_usage: Option<ContextUsage>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionTokenStats {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub total: u64,
}

/// Result of a bash execution (oracle `BashResult`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BashResult {
    pub output: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i64>,
    pub cancelled: bool,
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub full_output_path: Option<String>,
}

/// Listener for session events.
pub type AgentSessionEventListener = Arc<dyn Fn(&AgentSessionEvent) + Send + Sync>;

/// Streaming output callback for bash execution.
pub type BashChunkCallback = Arc<dyn Fn(&str) + Send + Sync>;

// ============================================================================
// Configuration
// ============================================================================

/// Inputs for [`AgentSession::new`] (oracle `AgentSessionConfig` + the Agent
/// wiring from sdk.ts `createAgentSession`).
pub struct AgentSessionConfig {
    pub session_manager: SessionManager,
    pub settings_manager: Arc<Mutex<SettingsManager>>,
    pub model_registry: Arc<tokio::sync::RwLock<ModelRegistry>>,
    pub cwd: PathBuf,
    /// Stream function driving provider calls. The default (`None`) resolves
    /// auth through the model registry per request (sdk.ts:302-345).
    pub stream_fn: Option<StreamFn>,
    pub model: Option<Model>,
    pub thinking_level: AgentThinkingLevel,
    pub scoped_models: Vec<ScopedModel>,
    /// SDK custom tools registered outside extensions.
    pub custom_tools: Vec<SessionToolDefinition>,
    /// Initial active built-in tool names. Default: `[read, bash, edit, write]`.
    pub initial_active_tool_names: Option<Vec<String>>,
    /// Optional allowlist of tool names.
    pub allowed_tool_names: Option<Vec<String>>,
    /// Optional denylist of tool names.
    pub excluded_tool_names: Option<Vec<String>>,
    /// Pre-loaded resources for system prompt assembly and expansion.
    pub skills: Vec<Skill>,
    pub prompt_templates: Vec<PromptTemplate>,
    pub context_files: Vec<ContextFile>,
    pub custom_system_prompt: Option<String>,
    pub append_system_prompt: Option<String>,
}

// ============================================================================
// Internal state
// ============================================================================

struct SessionState {
    session_manager: SessionManager,
    // Agent state (agent.ts MutableAgentState, collapsed).
    base_system_prompt: String,
    system_prompt_override: Option<String>,
    effective_system_prompt: String,
    model: Option<Model>,
    thinking_level: AgentThinkingLevel,
    messages: Vec<AgentMessage>,
    active_tool_names: Vec<String>,
    /// Insertion-ordered (built-ins first, then custom) like the oracle Map.
    tool_registry: Vec<SessionToolDefinition>,
    /// Built-in partition of the registry (oracle `_baseToolDefinitions`),
    /// kept for `refresh_extension_tools` rebuilds.
    base_tools: Vec<SessionToolDefinition>,
    /// SDK custom tools (oracle `_customTools`); merged after extension
    /// tools on every rebuild, so an SDK tool wins a name collision.
    sdk_tools: Vec<SessionToolDefinition>,
    /// Extension tools from the current sidecar registration snapshot.
    extension_tools: Vec<SessionToolDefinition>,
    /// Whether a registration snapshot was ever applied to this session
    /// (first application = construction-equivalent, include-all).
    extension_tools_applied: bool,
    // Queues (typed messages drained by the loop + display texts).
    steering_queue: Vec<AgentMessage>,
    follow_up_queue: Vec<AgentMessage>,
    steering_mode: QueueMode,
    follow_up_mode: QueueMode,
    steering_texts: Vec<String>,
    follow_up_texts: Vec<String>,
    // Custom messages queued for the next user prompt (oracle
    // `_pendingNextTurnMessages`, "asides").
    pending_next_turn: Vec<AgentMessage>,
    // Deferred bashExecution messages recorded mid-run.
    pending_bash: Vec<Value>,
    // Run lifecycle.
    run_active: bool,
    run_cancel: Option<CancellationToken>,
    // Retry state.
    retry_attempt: u32,
    retry_cancel: Option<CancellationToken>,
    // Compaction / branch summary / bash cancellation.
    compaction_cancel: Option<CancellationToken>,
    auto_compaction_cancel: Option<CancellationToken>,
    branch_summary_cancel: Option<CancellationToken>,
    bash_cancel: Option<CancellationToken>,
    overflow_recovery_attempted: bool,
    // Track last assistant message for post-run retry/compaction checks.
    last_assistant: Option<AssistantMessage>,
    // Resources.
    scoped_models: Vec<ScopedModel>,
    skills: Vec<Skill>,
    prompt_templates: Vec<PromptTemplate>,
    context_files: Vec<ContextFile>,
    custom_system_prompt: Option<String>,
    append_system_prompt: Option<String>,
    allowed_tool_names: Option<Vec<String>>,
    excluded_tool_names: Option<Vec<String>>,
    disposed: bool,
}

struct SessionInner {
    cwd: PathBuf,
    state: Mutex<SessionState>,
    settings: Arc<Mutex<SettingsManager>>,
    registry: Arc<tokio::sync::RwLock<ModelRegistry>>,
    stream_fn: StreamFn,
    listeners: Mutex<Vec<(u64, AgentSessionEventListener)>>,
    next_listener_id: AtomicU64,
    /// `true` while an agent run (incl. post-run continuation) is active.
    active_tx: tokio::sync::watch::Sender<bool>,
    active_rx: tokio::sync::watch::Receiver<bool>,
    /// Phase 6 blocking compaction hooks (sidecar `session_before_compact` /
    /// `session_compact`); `None` = zero extensions, zero overhead.
    compact_hooks: Mutex<Option<Arc<dyn crate::extension_bridge::CompactHooks>>>,
}

impl SessionInner {
    fn emit(&self, event: &AgentSessionEvent) {
        let listeners: Vec<AgentSessionEventListener> = {
            let guard = self.listeners.lock();
            guard.iter().map(|(_, l)| l.clone()).collect()
        };
        for listener in listeners {
            listener(event);
        }
    }

    fn emit_all(&self, events: Vec<AgentSessionEvent>) {
        for event in &events {
            self.emit(event);
        }
    }
}

/// RAII guard clearing the active-run flag and waking idle waiters.
struct RunGuard {
    inner: Arc<SessionInner>,
}

impl Drop for RunGuard {
    fn drop(&mut self) {
        let mut state = self.inner.state.lock();
        state.run_active = false;
        state.run_cancel = None;
        // Send under the SAME lock: a release outside the lock could land
        // after a new claim's send(true) and mark an active run as idle.
        let _ = self.inner.active_tx.send(false);
    }
}

fn now_ms() -> i64 {
    jiff::Timestamp::now().as_millisecond()
}

fn drain_queue(queue: &mut Vec<AgentMessage>, mode: QueueMode) -> Vec<AgentMessage> {
    match mode {
        QueueMode::All => std::mem::take(queue),
        QueueMode::OneAtATime => {
            if queue.is_empty() {
                Vec::new()
            } else {
                vec![queue.remove(0)]
            }
        }
    }
}

fn parse_queue_mode(raw: &str) -> QueueMode {
    match raw {
        "all" => QueueMode::All,
        _ => QueueMode::OneAtATime,
    }
}

fn queue_mode_str(mode: QueueMode) -> &'static str {
    match mode {
        QueueMode::All => "all",
        QueueMode::OneAtATime => "one-at-a-time",
    }
}

// ============================================================================
// AgentSession
// ============================================================================

/// Shared handle to one live session (cheap to clone).
#[derive(Clone)]
pub struct AgentSession {
    inner: Arc<SessionInner>,
}

impl AgentSession {
    pub fn new(config: AgentSessionConfig) -> Self {
        let (active_tx, active_rx) = tokio::sync::watch::channel(false);

        let settings = config.settings_manager.clone();
        let registry = config.model_registry.clone();

        let (steering_mode, follow_up_mode) = {
            let guard = settings.lock();
            (
                parse_queue_mode(guard.get_steering_mode()),
                parse_queue_mode(guard.get_follow_up_mode()),
            )
        };

        // Base tool definitions (oracle _buildRuntime): built-ins with
        // settings-derived options, then SDK custom tools.
        let (shell_command_prefix, shell_path, auto_resize_images) = {
            let guard = settings.lock();
            (
                guard
                    .settings()
                    .get_str("shellCommandPrefix")
                    .map(str::to_string),
                guard.settings().get_str("shellPath").map(str::to_string),
                guard.get_image_auto_resize(),
            )
        };
        let builtins = crate::tools::builtin_tools_with_options(
            &config.cwd,
            &crate::tools::BuiltinToolOptions {
                read: crate::tools::ReadToolOptions { auto_resize_images },
                bash: crate::tools::BashToolOptions {
                    shell_path,
                    command_prefix: shell_command_prefix,
                },
            },
        );

        let allowed: Option<Vec<String>> = config.allowed_tool_names.clone();
        let excluded: Option<Vec<String>> = config.excluded_tool_names.clone();
        let is_allowed = |name: &str| -> bool {
            allowed.as_ref().is_none_or(|a| a.iter().any(|n| n == name))
                && !excluded
                    .as_ref()
                    .is_some_and(|e| e.iter().any(|n| n == name))
        };
        let mut base_tools: Vec<SessionToolDefinition> = Vec::new();
        for tool in builtins {
            if !is_allowed(&tool.name) {
                continue;
            }
            let name = tool.name.clone();
            base_tools.push(SessionToolDefinition {
                definition: Arc::new(tool),
                prompt_snippet: builtin_prompt_snippet(&name).map(str::to_string),
                prompt_guidelines: builtin_prompt_guidelines(&name)
                    .iter()
                    .map(|s| s.to_string())
                    .collect(),
                source: "builtin",
                source_info: None,
            });
        }
        let sdk_tools: Vec<SessionToolDefinition> = config
            .custom_tools
            .into_iter()
            .filter(|tool| is_allowed(&tool.definition.name))
            .collect();
        let tool_registry = merge_tool_registry(&base_tools, &[], &sdk_tools);

        // Initial active tools: explicit list or default four, filtered to
        // the registry; plus all custom tools (includeAllExtensionTools).
        let default_active = ["read", "bash", "edit", "write"];
        let mut active: Vec<String> = config
            .initial_active_tool_names
            .clone()
            .unwrap_or_else(|| default_active.iter().map(|s| s.to_string()).collect());
        for tool in &tool_registry {
            if tool.source != "builtin" && !active.contains(&tool.definition.name) {
                active.push(tool.definition.name.clone());
            }
        }
        let active: Vec<String> = {
            let mut seen = std::collections::HashSet::new();
            active
                .into_iter()
                .filter(|n| {
                    tool_registry.iter().any(|t| t.definition.name == *n) && seen.insert(n.clone())
                })
                .collect()
        };

        let mut state = SessionState {
            session_manager: config.session_manager,
            base_system_prompt: String::new(),
            system_prompt_override: None,
            effective_system_prompt: String::new(),
            model: config.model,
            thinking_level: config.thinking_level,
            messages: Vec::new(),
            active_tool_names: active,
            tool_registry,
            base_tools,
            sdk_tools,
            extension_tools: Vec::new(),
            extension_tools_applied: false,
            steering_queue: Vec::new(),
            follow_up_queue: Vec::new(),
            steering_mode,
            follow_up_mode,
            steering_texts: Vec::new(),
            follow_up_texts: Vec::new(),
            pending_next_turn: Vec::new(),
            pending_bash: Vec::new(),
            run_active: false,
            run_cancel: None,
            retry_attempt: 0,
            retry_cancel: None,
            compaction_cancel: None,
            auto_compaction_cancel: None,
            branch_summary_cancel: None,
            bash_cancel: None,
            overflow_recovery_attempted: false,
            last_assistant: None,
            scoped_models: config.scoped_models,
            skills: config.skills,
            prompt_templates: config.prompt_templates,
            context_files: config.context_files,
            custom_system_prompt: config.custom_system_prompt,
            append_system_prompt: config.append_system_prompt,
            allowed_tool_names: config.allowed_tool_names,
            excluded_tool_names: config.excluded_tool_names,
            disposed: false,
        };

        // Restore messages from session context (sdk.ts:372-373).
        let context = state.session_manager.build_session_context();
        state.messages = context
            .messages
            .into_iter()
            .filter_map(|value| serde_json::from_value::<AgentMessage>(value).ok())
            .collect();

        rebuild_system_prompt(&mut state, &config.cwd);
        state.effective_system_prompt = state.base_system_prompt.clone();

        let stream_fn = config
            .stream_fn
            .unwrap_or_else(|| default_stream_fn(registry.clone(), settings.clone()));

        AgentSession {
            inner: Arc::new(SessionInner {
                cwd: config.cwd,
                state: Mutex::new(state),
                settings,
                registry,
                stream_fn,
                listeners: Mutex::new(Vec::new()),
                next_listener_id: AtomicU64::new(1),
                active_tx,
                active_rx,
                compact_hooks: Mutex::new(None),
            }),
        }
    }

    // =====================================================================
    // Event subscription
    // =====================================================================

    /// Subscribe to session events. Returns an unsubscribe closure.
    pub fn subscribe(&self, listener: AgentSessionEventListener) -> impl FnOnce() + Send + 'static {
        let id = self.inner.next_listener_id.fetch_add(1, Ordering::Relaxed);
        self.inner.listeners.lock().push((id, listener));
        let inner = self.inner.clone();
        move || {
            inner.listeners.lock().retain(|(lid, _)| *lid != id);
        }
    }

    /// Bind (or clear) the Phase 6 compaction hooks. Bound once by the
    /// extension binding right after session creation.
    pub fn bind_compact_hooks(
        &self,
        hooks: Option<Arc<dyn crate::extension_bridge::CompactHooks>>,
    ) {
        *self.inner.compact_hooks.lock() = hooks;
    }

    // =====================================================================
    // Read-only state access
    // =====================================================================

    pub fn cwd(&self) -> &PathBuf {
        &self.inner.cwd
    }

    pub fn model(&self) -> Option<Model> {
        self.inner.state.lock().model.clone()
    }

    pub fn thinking_level(&self) -> AgentThinkingLevel {
        self.inner.state.lock().thinking_level
    }

    pub fn is_streaming(&self) -> bool {
        self.inner.state.lock().run_active
    }

    pub fn is_idle(&self) -> bool {
        !self.is_streaming()
    }

    pub fn system_prompt(&self) -> String {
        self.inner.state.lock().effective_system_prompt.clone()
    }

    pub fn retry_attempt(&self) -> u32 {
        self.inner.state.lock().retry_attempt
    }

    pub fn is_retrying(&self) -> bool {
        self.inner.state.lock().retry_cancel.is_some()
    }

    pub fn is_compacting(&self) -> bool {
        let state = self.inner.state.lock();
        state.compaction_cancel.is_some()
            || state.auto_compaction_cancel.is_some()
            || state.branch_summary_cancel.is_some()
    }

    pub fn is_bash_running(&self) -> bool {
        self.inner.state.lock().bash_cancel.is_some()
    }

    pub fn messages(&self) -> Vec<AgentMessage> {
        self.inner.state.lock().messages.clone()
    }

    pub fn steering_mode(&self) -> &'static str {
        queue_mode_str(self.inner.state.lock().steering_mode)
    }

    pub fn follow_up_mode(&self) -> &'static str {
        queue_mode_str(self.inner.state.lock().follow_up_mode)
    }

    pub fn session_file(&self) -> Option<PathBuf> {
        self.inner
            .state
            .lock()
            .session_manager
            .get_session_file()
            .map(PathBuf::from)
    }

    pub fn session_id(&self) -> String {
        self.inner
            .state
            .lock()
            .session_manager
            .get_session_id()
            .to_string()
    }

    pub fn session_name(&self) -> Option<String> {
        self.inner.state.lock().session_manager.get_session_name()
    }

    pub fn scoped_models(&self) -> Vec<ScopedModel> {
        self.inner.state.lock().scoped_models.clone()
    }

    pub fn set_scoped_models(&self, scoped_models: Vec<ScopedModel>) {
        self.inner.state.lock().scoped_models = scoped_models;
    }

    pub fn prompt_templates(&self) -> Vec<PromptTemplate> {
        self.inner.state.lock().prompt_templates.clone()
    }

    pub fn skills(&self) -> Vec<Skill> {
        self.inner.state.lock().skills.clone()
    }

    /// Run a closure with the session manager (read-only access patterns).
    pub fn with_session_manager<R>(&self, f: impl FnOnce(&SessionManager) -> R) -> R {
        f(&self.inner.state.lock().session_manager)
    }

    /// Whether two handles refer to the same live session.
    pub fn ptr_eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }

    /// Run a closure with mutable session-manager access (runtime fork path,
    /// extension `appendEntry`/`setLabel` bindings, test seeding).
    pub fn with_session_manager_mut<R>(&self, f: impl FnOnce(&mut SessionManager) -> R) -> R {
        f(&mut self.inner.state.lock().session_manager)
    }

    /// Move the session manager out of a disposed session (runtime fork path
    /// reuses the live manager, oracle agent-session-runtime.ts:334-352).
    pub(crate) fn take_session_manager(&self) -> SessionManager {
        let placeholder =
            SessionManager::in_memory(None, None).expect("in-memory placeholder session manager");
        std::mem::replace(&mut self.inner.state.lock().session_manager, placeholder)
    }

    // =====================================================================
    // Queue accessors
    // =====================================================================

    pub fn pending_message_count(&self) -> usize {
        let state = self.inner.state.lock();
        state.steering_texts.len() + state.follow_up_texts.len()
    }

    pub fn get_steering_messages(&self) -> Vec<String> {
        self.inner.state.lock().steering_texts.clone()
    }

    pub fn get_follow_up_messages(&self) -> Vec<String> {
        self.inner.state.lock().follow_up_texts.clone()
    }

    /// Clear all queued messages and return them (oracle `clearQueue`).
    pub fn clear_queue(&self) -> (Vec<String>, Vec<String>) {
        let (steering, follow_up) = {
            let mut state = self.inner.state.lock();
            let steering = std::mem::take(&mut state.steering_texts);
            let follow_up = std::mem::take(&mut state.follow_up_texts);
            state.steering_queue.clear();
            state.follow_up_queue.clear();
            (steering, follow_up)
        };
        self.emit_queue_update();
        (steering, follow_up)
    }

    pub fn set_steering_mode(&self, mode: &str) {
        self.inner.state.lock().steering_mode = parse_queue_mode(mode);
        self.inner.settings.lock().set_steering_mode(mode);
    }

    pub fn set_follow_up_mode(&self, mode: &str) {
        self.inner.state.lock().follow_up_mode = parse_queue_mode(mode);
        self.inner.settings.lock().set_follow_up_mode(mode);
    }

    fn emit_queue_update(&self) {
        let event = {
            let state = self.inner.state.lock();
            AgentSessionEvent::QueueUpdate {
                steering: state.steering_texts.clone(),
                follow_up: state.follow_up_texts.clone(),
            }
        };
        self.inner.emit(&event);
    }

    // =====================================================================
    // Prompting
    // =====================================================================

    /// Send a prompt to the agent (oracle `prompt`, agent-session.ts:1071).
    pub async fn prompt(&self, text: &str, mut options: PromptOptions) -> Result<(), String> {
        let preflight = options.preflight_result.take();
        match self.prompt_inner(text, options).await {
            Ok(Some((messages, run_claim))) => {
                if let Some(preflight) = preflight {
                    preflight(true);
                }
                self.run_agent_prompt(messages, run_claim).await;
                Ok(())
            }
            Ok(None) => {
                if let Some(preflight) = preflight {
                    preflight(true);
                }
                Ok(())
            }
            Err(error) => {
                if let Some(preflight) = preflight {
                    preflight(false);
                }
                Err(error)
            }
        }
    }

    /// Preflight portion of `prompt()`. `Ok(Some(_))` means "run these
    /// messages under the returned run claim"; `Ok(None)` means the prompt
    /// was queued (steer/followUp).
    async fn prompt_inner(
        &self,
        text: &str,
        options: PromptOptions,
    ) -> Result<Option<(Vec<AgentMessage>, RunGuard)>, String> {
        let expand = options.expand_prompt_templates.unwrap_or(true);

        let mut expanded_text = text.to_string();
        if expand {
            expanded_text = self.expand_skill_command(&expanded_text);
            expanded_text = self.expand_prompt_template(&expanded_text);
        }

        // Claim-or-queue in ONE lock acquisition: if a run is active, queue
        // via steer/followUp (active-run invariant); otherwise claim the run
        // immediately so a concurrent prompt deterministically observes the
        // claim (no gap between the preflight check and the run start). Any
        // later preflight error drops the claim, releasing the run.
        let run_claim = {
            let mut state = self.inner.state.lock();
            if state.run_active {
                drop(state);
                let Some(behavior) = options.streaming_behavior else {
                    return Err(
                        "Agent is already processing. Specify streamingBehavior ('steer' or 'followUp') to queue the message."
                            .to_string(),
                    );
                };
                match behavior {
                    StreamingBehavior::FollowUp => {
                        self.queue_follow_up(expanded_text, options.images)
                    }
                    StreamingBehavior::Steer => self.queue_steer(expanded_text, options.images),
                }
                return Ok(None);
            }
            state.run_active = true;
            // Mirror the claim into the idle watch under the SAME lock so a
            // wait_for_idle caller can never observe run_active=true with a
            // still-false watch value (early spurious return).
            let _ = self.inner.active_tx.send(true);
            RunGuard {
                inner: self.inner.clone(),
            }
        };

        // Flush any pending bash messages before the new prompt.
        self.flush_pending_bash_messages();

        // Validate model + auth.
        let model = self.model().ok_or_else(format_no_model_selected_message)?;
        {
            let registry = self.inner.registry.read().await;
            if !registry.has_configured_auth(&model).await {
                if registry.is_using_oauth(&model).await {
                    return Err(oauth_reauth_message(&model.provider));
                }
                return Err(format_no_api_key_found_message(&model.provider));
            }
        }

        // Pre-prompt compaction check (catches aborted responses).
        if let Some(last_assistant) = self.find_last_assistant_message() {
            self.check_compaction(&last_assistant, false).await;
        }

        let mut content: Vec<Content> = vec![Content::Text(TextContent {
            text: expanded_text.into(),
            text_signature: None,
        })];
        for image in options.images {
            content.push(Content::Image(image));
        }
        let mut messages = vec![AgentMessage::user(UserMessage {
            content: UserContent::Blocks(content),
            timestamp: now_ms(),
        })];

        // Inject pending "nextTurn" custom messages as context alongside the
        // user message (agent-session.ts:1177-1181).
        {
            let mut state = self.inner.state.lock();
            messages.append(&mut state.pending_next_turn);
        }

        // No extensions: always reset to base prompt (agent-session.ts:1200).
        {
            let mut state = self.inner.state.lock();
            state.system_prompt_override = None;
            state.effective_system_prompt = state.base_system_prompt.clone();
        }

        Ok(Some((messages, run_claim)))
    }

    /// Queue a steering message while the agent is running.
    pub fn steer(&self, text: &str, images: Vec<ImageContent>) {
        let mut expanded = self.expand_skill_command(text);
        expanded = self.expand_prompt_template(&expanded);
        self.queue_steer(expanded, images);
    }

    /// Queue a follow-up message processed after the agent finishes.
    pub fn follow_up(&self, text: &str, images: Vec<ImageContent>) {
        let mut expanded = self.expand_skill_command(text);
        expanded = self.expand_prompt_template(&expanded);
        self.queue_follow_up(expanded, images);
    }

    /// Send a custom (extension-defined) message to the agent (oracle
    /// `sendCustomMessage`, agent-session.ts:1388).
    ///
    /// Delivery: `nextTurn` queues the message as context for the next user
    /// prompt; while streaming it queues via steer (default) or followUp;
    /// when idle it either triggers a turn (`trigger_turn`) or appends to
    /// state/session without a turn.
    pub async fn send_custom_message(
        &self,
        custom_type: &str,
        content: Option<Value>,
        display: bool,
        details: Option<Value>,
        options: SendCustomMessageOptions,
    ) {
        // Untyped extensions can pass null/missing content; normalize at
        // ingestion (agent-session.ts:1396).
        let content = match content {
            Some(Value::Null) | None => Value::Array(Vec::new()),
            Some(value) => value,
        };

        let mut value = serde_json::Map::new();
        value.insert("role".into(), Value::String("custom".into()));
        value.insert("customType".into(), Value::String(custom_type.into()));
        value.insert("content".into(), content.clone());
        value.insert("display".into(), Value::Bool(display));
        if let Some(details) = &details {
            value.insert("details".into(), details.clone());
        }
        value.insert("timestamp".into(), Value::Number(now_ms().into()));
        let app_message = AgentMessage::Custom(Value::Object(value));

        if options.deliver_as == Some(CustomMessageDelivery::NextTurn) {
            self.inner.state.lock().pending_next_turn.push(app_message);
            return;
        }

        // Streaming check and run claim in ONE lock acquisition (active-run
        // invariant, same discipline as prompt_inner).
        enum Action {
            Queued,
            Run(RunGuard),
            Append,
        }
        let action = {
            let mut state = self.inner.state.lock();
            if state.run_active {
                // Oracle queues on the agent-level typed queues only; the
                // display texts and queue_update event are for text prompts.
                if options.deliver_as == Some(CustomMessageDelivery::FollowUp) {
                    state.follow_up_queue.push(app_message.clone());
                } else {
                    state.steering_queue.push(app_message.clone());
                }
                Action::Queued
            } else if options.trigger_turn {
                state.run_active = true;
                let _ = self.inner.active_tx.send(true);
                Action::Run(RunGuard {
                    inner: self.inner.clone(),
                })
            } else {
                Action::Append
            }
        };

        match action {
            Action::Queued => {}
            Action::Run(run_claim) => {
                self.run_agent_prompt(vec![app_message], run_claim).await;
            }
            Action::Append => {
                {
                    let mut state = self.inner.state.lock();
                    state.messages.push(app_message.clone());
                    let _ = state.session_manager.append_custom_message_entry(
                        custom_type,
                        content,
                        display,
                        details,
                    );
                }
                self.inner.emit(&AgentSessionEvent::MessageStart {
                    message: app_message.clone(),
                });
                self.inner.emit(&AgentSessionEvent::MessageEnd {
                    message: app_message,
                });
            }
        }
    }

    fn queue_steer(&self, text: String, images: Vec<ImageContent>) {
        {
            let mut state = self.inner.state.lock();
            state.steering_texts.push(text.clone());
            let message = user_text_message(text, images);
            state.steering_queue.push(message);
        }
        self.emit_queue_update();
    }

    fn queue_follow_up(&self, text: String, images: Vec<ImageContent>) {
        {
            let mut state = self.inner.state.lock();
            state.follow_up_texts.push(text.clone());
            let message = user_text_message(text, images);
            state.follow_up_queue.push(message);
        }
        self.emit_queue_update();
    }

    // =====================================================================
    // Abort / idle
    // =====================================================================

    /// Abort current operation and wait for the agent to become idle.
    pub async fn abort(&self) {
        self.abort_retry();
        {
            let state = self.inner.state.lock();
            if let Some(cancel) = &state.run_cancel {
                cancel.cancel();
            }
        }
        self.wait_for_idle().await;
    }

    pub async fn wait_for_idle(&self) {
        if self.is_idle() {
            return;
        }
        let mut rx = self.inner.active_rx.clone();
        loop {
            if !*rx.borrow() {
                return;
            }
            if rx.changed().await.is_err() {
                return;
            }
        }
    }

    pub fn abort_retry(&self) {
        let cancel = self.inner.state.lock().retry_cancel.clone();
        if let Some(cancel) = cancel {
            cancel.cancel();
        }
    }

    /// Remove all listeners and cancel in-flight work (oracle `dispose`).
    pub fn dispose(&self) {
        self.abort_retry();
        self.abort_compaction();
        self.abort_branch_summary();
        self.abort_bash();
        {
            let mut state = self.inner.state.lock();
            if let Some(cancel) = &state.run_cancel {
                cancel.cancel();
            }
            state.disposed = true;
        }
        self.inner.listeners.lock().clear();
    }

    // =====================================================================
    // Run lifecycle (Agent.prompt/continue collapsed)
    // =====================================================================

    async fn run_agent_prompt(&self, messages: Vec<AgentMessage>, run_claim: RunGuard) {
        self.run_prompt_messages(messages, false).await;
        while self.handle_post_agent_run().await {
            self.continue_run().await;
        }

        // finally: reset override, flush bash, settle.
        {
            let mut state = self.inner.state.lock();
            state.system_prompt_override = None;
            state.effective_system_prompt = state.base_system_prompt.clone();
        }
        self.flush_pending_bash_messages();
        // Oracle `_emitAgentSettled` (agent-session.ts:534-541) clears the
        // active flag BEFORE emitting agent_settled, so listeners observe an
        // idle session and can prompt from the handler. Dropping the claim
        // clears run_active and wakes idle waiters.
        drop(run_claim);
        self.inner.emit(&AgentSessionEvent::AgentSettled);
    }

    /// One `runAgentLoop` invocation with fresh context snapshot.
    async fn run_prompt_messages(
        &self,
        messages: Vec<AgentMessage>,
        skip_initial_steering_poll: bool,
    ) {
        let (context, config, cancel) = match self.create_loop_inputs(skip_initial_steering_poll) {
            Some(inputs) => inputs,
            None => return,
        };
        let sink = self.event_sink();
        run_agent_loop(
            messages,
            context,
            config,
            sink,
            Some(cancel),
            self.inner.stream_fn.clone(),
        )
        .await;
    }

    /// Agent.continue(): drain steering/follow-up from an assistant tail, or
    /// continue from a user/tool-result tail.
    async fn continue_run(&self) {
        enum Next {
            Steering(Vec<AgentMessage>),
            FollowUps(Vec<AgentMessage>),
            Continue,
            Nothing,
        }
        let next = {
            let mut state = self.inner.state.lock();
            match state.messages.last().map(AgentMessage::role) {
                Some("assistant") => {
                    let steering_mode = state.steering_mode;
                    let steering = drain_queue(&mut state.steering_queue, steering_mode);
                    if !steering.is_empty() {
                        Next::Steering(steering)
                    } else {
                        let follow_up_mode = state.follow_up_mode;
                        let follow_ups = drain_queue(&mut state.follow_up_queue, follow_up_mode);
                        if !follow_ups.is_empty() {
                            Next::FollowUps(follow_ups)
                        } else {
                            Next::Nothing
                        }
                    }
                }
                Some(_) => Next::Continue,
                None => Next::Nothing,
            }
        };
        match next {
            Next::Steering(messages) => self.run_prompt_messages(messages, true).await,
            Next::FollowUps(messages) => self.run_prompt_messages(messages, false).await,
            Next::Continue => {
                let (context, config, cancel) = match self.create_loop_inputs(false) {
                    Some(inputs) => inputs,
                    None => return,
                };
                let sink = self.event_sink();
                if let Err(error) = run_agent_loop_continue(
                    context,
                    config,
                    sink,
                    Some(cancel),
                    self.inner.stream_fn.clone(),
                )
                .await
                {
                    self.handle_run_failure(&error.to_string()).await;
                }
            }
            Next::Nothing => {}
        }
    }

    /// Oracle `_handlePostAgentRun`: returns true if the run should continue.
    async fn handle_post_agent_run(&self) -> bool {
        let msg = {
            let mut state = self.inner.state.lock();
            state.last_assistant.take()
        };
        let Some(msg) = msg else {
            return false;
        };

        if self.is_retryable_error(&msg) && self.prepare_retry(&msg).await {
            return true;
        }

        if msg.stop_reason == StopReason::Error {
            let attempt = self.inner.state.lock().retry_attempt;
            if attempt > 0 {
                self.inner.state.lock().retry_attempt = 0;
                self.inner.emit(&AgentSessionEvent::AutoRetryEnd {
                    success: false,
                    attempt,
                    final_error: msg.error_message.clone(),
                });
            }
        }

        if self.check_compaction(&msg, true).await {
            return true;
        }

        // Messages queued by agent_end handlers need a continuation.
        let state = self.inner.state.lock();
        !state.steering_queue.is_empty() || !state.follow_up_queue.is_empty()
    }

    /// Oracle Agent.handleRunFailure: synthesize an error assistant message.
    async fn handle_run_failure(&self, error: &str) {
        let model = self.inner.state.lock().model.clone();
        let (api, provider, model_id) = match &model {
            Some(m) => (m.api.clone(), m.provider.clone(), m.id.clone()),
            None => (
                pi_ai::Api::new("unknown"),
                "unknown".to_string(),
                "unknown".to_string(),
            ),
        };
        let failure = AssistantMessage {
            content: vec![Content::Text(TextContent {
                text: "".into(),
                text_signature: None,
            })],
            api,
            provider,
            model: model_id,
            response_model: None,
            response_id: None,
            diagnostics: None,
            usage: Usage::default(),
            stop_reason: StopReason::Error,
            error_message: Some(error.to_string()),
            timestamp: now_ms(),
        };
        let message = AgentMessage::assistant(failure);
        let sink = self.event_sink();
        sink(AgentEvent::MessageStart {
            message: message.clone(),
        })
        .await;
        sink(AgentEvent::MessageEnd {
            message: message.clone(),
        })
        .await;
        sink(AgentEvent::TurnEnd {
            message: message.clone(),
            tool_results: Vec::new(),
        })
        .await;
        sink(AgentEvent::AgentEnd {
            messages: vec![message],
        })
        .await;
    }

    fn create_loop_inputs(
        &self,
        skip_initial_steering_poll: bool,
    ) -> Option<(AgentContext, AgentLoopConfig, CancellationToken)> {
        let inner = self.inner.clone();
        let mut state = self.inner.state.lock();
        let model = state.model.clone()?;

        let cancel = CancellationToken::new();
        state.run_cancel = Some(cancel.clone());

        let context = AgentContext {
            system_prompt: state.effective_system_prompt.clone(),
            messages: state.messages.clone(),
            tools: active_tool_definitions(&state),
        };

        let settings = self.inner.settings.clone();
        let mut config = AgentLoopConfig::new(
            model,
            Arc::new(move |messages| {
                let settings = settings.clone();
                Box::pin(async move { convert_to_llm_with_block_images(&settings, messages) })
            }),
        );
        config.session_id = Some(state.session_manager.get_session_id().to_string());
        config.reasoning = state.thinking_level.into();

        // prepareNextTurn refresh (oracle _installAgentNextTurnRefresh):
        // reapply the session's system prompt / tools / model / thinking level
        // before each provider call.
        let prepare_inner = inner.clone();
        config.prepare_next_turn = Some(Arc::new(move |ctx| {
            let prepare_inner = prepare_inner.clone();
            Box::pin(async move {
                let state = prepare_inner.state.lock();
                Some(pi_agent::AgentLoopTurnUpdate {
                    context: Some(AgentContext {
                        system_prompt: state
                            .system_prompt_override
                            .clone()
                            .unwrap_or_else(|| state.base_system_prompt.clone()),
                        // Oracle spreads `turn.context` (agent-session.ts:483-489):
                        // the turn's accumulated messages are preserved verbatim;
                        // only system prompt / tools / model / thinking refresh.
                        messages: ctx.context.messages,
                        tools: active_tool_definitions(&state),
                    }),
                    model: state.model.clone(),
                    thinking_level: Some(state.thinking_level),
                })
            })
        }));

        // Steering / follow-up drains.
        let steer_inner = inner.clone();
        let skip_flag = Arc::new(AtomicBool::new(skip_initial_steering_poll));
        config.get_steering_messages = Some(Arc::new(move || {
            let steer_inner = steer_inner.clone();
            let skip_flag = skip_flag.clone();
            Box::pin(async move {
                if skip_flag.swap(false, Ordering::SeqCst) {
                    return Vec::new();
                }
                let mut state = steer_inner.state.lock();
                let mode = state.steering_mode;
                drain_queue(&mut state.steering_queue, mode)
            })
        }));
        let follow_inner = inner.clone();
        config.get_follow_up_messages = Some(Arc::new(move || {
            let follow_inner = follow_inner.clone();
            Box::pin(async move {
                let mut state = follow_inner.state.lock();
                let mode = state.follow_up_mode;
                drain_queue(&mut state.follow_up_queue, mode)
            })
        }));

        Some((context, config, cancel))
    }

    /// Build the async event sink shared by all loop invocations.
    fn event_sink(&self) -> pi_agent::AgentEventSink {
        let inner = self.inner.clone();
        let session = self.clone();
        Arc::new(move |event: AgentEvent| {
            let inner = inner.clone();
            let session = session.clone();
            Box::pin(async move {
                session.handle_agent_event(&inner, event);
            })
        })
    }

    /// Port of Agent.processEvents + AgentSession._handleAgentEvent.
    fn handle_agent_event(&self, inner: &Arc<SessionInner>, event: AgentEvent) {
        let mut pending_events: Vec<AgentSessionEvent> = Vec::new();

        {
            let mut state = inner.state.lock();

            // Queue removal on user message_start (before emitting anything).
            if let AgentEvent::MessageStart { message } = &event
                && message.role() == "user"
            {
                state.overflow_recovery_attempted = false;
                if let Some(text) = user_message_text(message)
                    && !text.is_empty()
                {
                    if let Some(idx) = state.steering_texts.iter().position(|t| *t == text) {
                        state.steering_texts.remove(idx);
                        pending_events.push(AgentSessionEvent::QueueUpdate {
                            steering: state.steering_texts.clone(),
                            follow_up: state.follow_up_texts.clone(),
                        });
                    } else if let Some(idx) = state.follow_up_texts.iter().position(|t| *t == text)
                    {
                        state.follow_up_texts.remove(idx);
                        pending_events.push(AgentSessionEvent::QueueUpdate {
                            steering: state.steering_texts.clone(),
                            follow_up: state.follow_up_texts.clone(),
                        });
                    }
                }
            }

            // Agent state reduction: transcript grows on message_end.
            if let AgentEvent::MessageEnd { message } = &event {
                state.messages.push(message.clone());
            }

            // Map to wire event (agent_end gains willRetry).
            let will_retry = if let AgentEvent::AgentEnd { messages } = &event {
                will_retry_after_agent_end(&self.inner, &state, messages)
            } else {
                false
            };
            pending_events.push(AgentSessionEvent::from_agent_event(
                event.clone(),
                will_retry,
            ));

            // Session persistence + assistant bookkeeping.
            if let AgentEvent::MessageEnd { message } = &event {
                persist_message(&mut state, message);

                if let AgentMessage::Standard(Message::Assistant(assistant)) = message {
                    state.last_assistant = Some(assistant.clone());
                    if assistant.stop_reason != StopReason::Error {
                        state.overflow_recovery_attempted = false;
                        if state.retry_attempt > 0 {
                            pending_events.push(AgentSessionEvent::AutoRetryEnd {
                                success: true,
                                attempt: state.retry_attempt,
                                final_error: None,
                            });
                            state.retry_attempt = 0;
                        }
                    }
                }
            }
        }

        inner.emit_all(pending_events);
    }

    // =====================================================================
    // Retry (auto-retry with exponential backoff)
    // =====================================================================

    fn is_retryable_error(&self, message: &AssistantMessage) -> bool {
        let context_window = self
            .inner
            .state
            .lock()
            .model
            .as_ref()
            .map(|m| m.context_window);
        if is_context_overflow(message, context_window) {
            return false;
        }
        is_retryable_assistant_error(message)
    }

    /// Oracle `_prepareRetry`.
    async fn prepare_retry(&self, message: &AssistantMessage) -> bool {
        let (enabled, max_retries, base_delay_ms) = retry_settings(&self.inner.settings);
        if !enabled {
            return false;
        }

        let (attempt, delay_ms) = {
            let mut state = self.inner.state.lock();
            state.retry_attempt += 1;
            if state.retry_attempt > max_retries {
                // Preserve the completed attempt count for the final failure.
                state.retry_attempt -= 1;
                return false;
            }
            let attempt = state.retry_attempt;
            (attempt, base_delay_ms * 2u64.pow(attempt - 1))
        };

        self.inner.emit(&AgentSessionEvent::AutoRetryStart {
            attempt,
            max_attempts: max_retries,
            delay_ms,
            error_message: message
                .error_message
                .clone()
                .unwrap_or_else(|| "Unknown error".to_string()),
        });

        // Remove error message from agent state (kept in session history).
        {
            let mut state = self.inner.state.lock();
            if state
                .messages
                .last()
                .is_some_and(|m| m.role() == "assistant")
            {
                state.messages.pop();
            }
        }

        // Abortable exponential-backoff sleep.
        let cancel = CancellationToken::new();
        self.inner.state.lock().retry_cancel = Some(cancel.clone());
        let aborted = cancellable_sleep(delay_ms, &cancel).await;
        self.inner.state.lock().retry_cancel = None;

        if aborted {
            let attempt = {
                let mut state = self.inner.state.lock();
                let attempt = state.retry_attempt;
                state.retry_attempt = 0;
                attempt
            };
            self.inner.emit(&AgentSessionEvent::AutoRetryEnd {
                success: false,
                attempt,
                final_error: Some("Retry cancelled".to_string()),
            });
            return false;
        }
        true
    }

    pub fn auto_retry_enabled(&self) -> bool {
        self.inner.settings.lock().get_retry_enabled()
    }

    pub fn set_auto_retry_enabled(&self, enabled: bool) {
        self.inner.settings.lock().set_retry_enabled(enabled);
    }

    // =====================================================================
    // Helpers
    // =====================================================================

    fn find_last_assistant_message(&self) -> Option<AssistantMessage> {
        let state = self.inner.state.lock();
        state.messages.iter().rev().find_map(|m| match m {
            AgentMessage::Standard(Message::Assistant(a)) => Some(a.clone()),
            _ => None,
        })
    }

    fn flush_pending_bash_messages(&self) {
        let mut state = self.inner.state.lock();
        let pending = std::mem::take(&mut state.pending_bash);
        for bash_value in pending {
            state
                .messages
                .push(AgentMessage::Custom(bash_value.clone()));
            let _ = state.session_manager.append_message(bash_value);
        }
    }

    /// Expand `/skill:name args` to the full skill block.
    fn expand_skill_command(&self, text: &str) -> String {
        if !text.starts_with("/skill:") {
            return text.to_string();
        }
        let rest = &text[7..];
        let (skill_name, args) = match rest.find(' ') {
            Some(idx) => (&rest[..idx], rest[idx + 1..].trim()),
            None => (rest, ""),
        };
        let skill = {
            let state = self.inner.state.lock();
            state.skills.iter().find(|s| s.name == skill_name).cloned()
        };
        let Some(skill) = skill else {
            return text.to_string();
        };
        let Ok(content) = std::fs::read_to_string(&skill.file_path) else {
            return text.to_string();
        };
        let body = strip_frontmatter(&content);
        let body = body.trim();
        let skill_block = format!(
            "<skill name=\"{}\" location=\"{}\">\nReferences are relative to {}.\n\n{}\n</skill>",
            skill.name,
            skill.file_path.display(),
            skill.base_dir.display(),
            body
        );
        if args.is_empty() {
            skill_block
        } else {
            format!("{skill_block}\n\n{args}")
        }
    }

    /// Expand a `/template args` prompt template.
    fn expand_prompt_template(&self, text: &str) -> String {
        let state = self.inner.state.lock();
        expand_prompt_template(text, &state.prompt_templates)
    }
}

// ============================================================================
// Free helpers
// ============================================================================

fn user_text_message(text: String, images: Vec<ImageContent>) -> AgentMessage {
    let mut content: Vec<Content> = vec![Content::Text(TextContent {
        text: text.into(),
        text_signature: None,
    })];
    for image in images {
        content.push(Content::Image(image));
    }
    AgentMessage::user(UserMessage {
        content: UserContent::Blocks(content),
        timestamp: now_ms(),
    })
}

fn user_message_text(message: &AgentMessage) -> Option<String> {
    match message {
        AgentMessage::Standard(Message::User(user)) => Some(match &user.content {
            UserContent::Text(text) => text.clone(),
            UserContent::Blocks(blocks) => blocks
                .iter()
                .filter_map(|c| match c {
                    Content::Text(t) => Some(t.text.to_string()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join(""),
        }),
        _ => None,
    }
}

fn active_tool_definitions(state: &SessionState) -> Vec<Arc<ToolDefinition>> {
    state
        .active_tool_names
        .iter()
        .filter_map(|name| find_tool(state, name))
        .map(|tool| tool.definition.clone())
        .collect()
}

fn find_tool<'a>(state: &'a SessionState, name: &str) -> Option<&'a SessionToolDefinition> {
    state
        .tool_registry
        .iter()
        .find(|t| t.definition.name == name)
}

/// Rebuild the tool registry from its partitions with JS `Map.set`
/// semantics: built-ins first, then extension tools, then SDK customs; an
/// existing name keeps its insertion position but takes the new definition
/// (oracle `_refreshToolRegistry`, agent-session.ts:2397-2466).
fn merge_tool_registry(
    base: &[SessionToolDefinition],
    extension: &[SessionToolDefinition],
    sdk: &[SessionToolDefinition],
) -> Vec<SessionToolDefinition> {
    let mut registry: Vec<SessionToolDefinition> = base.to_vec();
    for tool in extension.iter().chain(sdk.iter()) {
        match registry
            .iter_mut()
            .find(|t| t.definition.name == tool.definition.name)
        {
            Some(existing) => *existing = tool.clone(),
            None => registry.push(tool.clone()),
        }
    }
    registry
}

fn retry_settings(settings: &Arc<Mutex<SettingsManager>>) -> (bool, u32, u64) {
    let guard = settings.lock();
    let enabled = guard.get_retry_enabled();
    let retry = guard.settings().get("retry").cloned();
    let max_retries = retry
        .as_ref()
        .and_then(|r| r.get("maxRetries"))
        .and_then(Value::as_u64)
        .unwrap_or(3) as u32;
    let base_delay_ms = retry
        .as_ref()
        .and_then(|r| r.get("baseDelayMs"))
        .and_then(Value::as_u64)
        .unwrap_or(2000);
    (enabled, max_retries, base_delay_ms)
}

fn will_retry_after_agent_end(
    inner: &Arc<SessionInner>,
    state: &SessionState,
    messages: &[AgentMessage],
) -> bool {
    let (enabled, max_retries, _) = retry_settings(&inner.settings);
    if !enabled || state.retry_attempt >= max_retries {
        return false;
    }
    for message in messages.iter().rev() {
        if let AgentMessage::Standard(Message::Assistant(assistant)) = message {
            let context_window = state.model.as_ref().map(|m| m.context_window);
            if is_context_overflow(assistant, context_window) {
                return false;
            }
            return is_retryable_assistant_error(assistant);
        }
    }
    false
}

/// Persist a finished message to the session file (oracle message_end
/// persistence, agent-session.ts:465-486).
fn persist_message(state: &mut SessionState, message: &AgentMessage) {
    match message {
        AgentMessage::Standard(_) => {
            if let Ok(value) = serde_json::to_value(message) {
                let _ = state.session_manager.append_message(value);
            }
        }
        AgentMessage::Custom(value) => {
            if value.get("role").and_then(Value::as_str) == Some("custom") {
                let custom_type = value
                    .get("customType")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let content = value
                    .get("content")
                    .cloned()
                    .unwrap_or(Value::Array(vec![]));
                let display = value
                    .get("display")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let details = value.get("details").cloned();
                let _ = state.session_manager.append_custom_message_entry(
                    custom_type,
                    content,
                    display,
                    details,
                );
            }
            // Other custom roles (bashExecution, compactionSummary,
            // branchSummary) are persisted where they are created.
        }
    }
}

/// Sleep `delay_ms`, returning true if cancelled first.
async fn cancellable_sleep(delay_ms: u64, cancel: &CancellationToken) -> bool {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(delay_ms);
    loop {
        if cancel.is_cancelled() {
            return true;
        }
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return false;
        }
        let step = std::cmp::min(std::time::Duration::from_millis(25), deadline - now);
        tokio::time::sleep(step).await;
    }
}

// ============================================================================
// convertToLlm (core/messages.ts) + blockImages filter (sdk.ts:256-289)
// ============================================================================

/// Oracle `bashExecutionToText` (messages.ts:84-101).
fn bash_execution_to_text(msg: &Value) -> String {
    let command = msg.get("command").and_then(Value::as_str).unwrap_or("");
    let output = msg.get("output").and_then(Value::as_str).unwrap_or("");
    let mut text = format!("Ran `{command}`\n");
    if !output.is_empty() {
        text.push_str(&format!("```\n{output}\n```"));
    } else {
        text.push_str("(no output)");
    }
    let cancelled = msg
        .get("cancelled")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let exit_code = msg.get("exitCode").and_then(Value::as_i64);
    if cancelled {
        text.push_str("\n\n(command cancelled)");
    } else if let Some(code) = exit_code
        && code != 0
    {
        text.push_str(&format!("\n\nCommand exited with code {code}"));
    }
    let truncated = msg
        .get("truncated")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if truncated && let Some(path) = msg.get("fullOutputPath").and_then(Value::as_str) {
        text.push_str(&format!("\n\n[Output truncated. Full output: {path}]"));
    }
    text
}

fn text_message_value_to_user(text: String, timestamp: i64) -> Message {
    Message::User(UserMessage {
        content: UserContent::Blocks(vec![Content::Text(TextContent {
            text: text.into(),
            text_signature: None,
        })]),
        timestamp,
    })
}

/// Oracle `convertToLlm` (messages.ts:141-195).
pub fn convert_to_llm(messages: Vec<AgentMessage>) -> Vec<Message> {
    let mut result = Vec::with_capacity(messages.len());
    for message in messages {
        match message {
            AgentMessage::Standard(standard) => result.push(standard),
            AgentMessage::Custom(value) => {
                let role = value.get("role").and_then(Value::as_str).unwrap_or("");
                let timestamp = value.get("timestamp").and_then(Value::as_i64).unwrap_or(0);
                match role {
                    "bashExecution" => {
                        if value
                            .get("excludeFromContext")
                            .and_then(Value::as_bool)
                            .unwrap_or(false)
                        {
                            continue;
                        }
                        result.push(text_message_value_to_user(
                            bash_execution_to_text(&value),
                            timestamp,
                        ));
                    }
                    "custom" => {
                        let content = match value.get("content") {
                            Some(Value::String(text)) => {
                                UserContent::Blocks(vec![Content::Text(TextContent {
                                    text: text.clone().into(),
                                    text_signature: None,
                                })])
                            }
                            Some(other) => {
                                match serde_json::from_value::<Vec<Content>>(other.clone()) {
                                    Ok(blocks) => UserContent::Blocks(blocks),
                                    Err(_) => UserContent::Blocks(Vec::new()),
                                }
                            }
                            None => UserContent::Blocks(Vec::new()),
                        };
                        result.push(Message::User(UserMessage { content, timestamp }));
                    }
                    "branchSummary" => {
                        let summary = value.get("summary").and_then(Value::as_str).unwrap_or("");
                        result.push(text_message_value_to_user(
                            format!("{BRANCH_SUMMARY_PREFIX}{summary}{BRANCH_SUMMARY_SUFFIX}"),
                            timestamp,
                        ));
                    }
                    "compactionSummary" => {
                        let summary = value.get("summary").and_then(Value::as_str).unwrap_or("");
                        result.push(text_message_value_to_user(
                            format!(
                                "{COMPACTION_SUMMARY_PREFIX}{summary}{COMPACTION_SUMMARY_SUFFIX}"
                            ),
                            timestamp,
                        ));
                    }
                    _ => {}
                }
            }
        }
    }
    result
}

/// convertToLlm + `blockImages` defense-in-depth filter (sdk.ts:256-289).
fn convert_to_llm_with_block_images(
    settings: &Arc<Mutex<SettingsManager>>,
    messages: Vec<AgentMessage>,
) -> Vec<Message> {
    let converted = convert_to_llm(messages);
    let block_images = settings
        .lock()
        .settings()
        .get_bool("blockImages")
        .unwrap_or(false);
    if !block_images {
        return converted;
    }
    const PLACEHOLDER: &str = "Image reading is disabled.";
    let filter_blocks = |blocks: &[Content]| -> Option<Vec<Content>> {
        if !blocks.iter().any(|c| matches!(c, Content::Image(_))) {
            return None;
        }
        let mapped: Vec<Content> = blocks
            .iter()
            .map(|c| match c {
                Content::Image(_) => Content::Text(TextContent {
                    text: PLACEHOLDER.into(),
                    text_signature: None,
                }),
                other => other.clone(),
            })
            .collect();
        // Dedupe consecutive placeholder texts.
        let mut result: Vec<Content> = Vec::with_capacity(mapped.len());
        for content in mapped {
            let is_dup = matches!(
                (&content, result.last()),
                (Content::Text(current), Some(Content::Text(previous)))
                    if current.text.to_string() == PLACEHOLDER
                        && previous.text.to_string() == PLACEHOLDER
            );
            if !is_dup {
                result.push(content);
            }
        }
        Some(result)
    };
    converted
        .into_iter()
        .map(|message| match message {
            Message::User(mut user) => {
                if let UserContent::Blocks(blocks) = &user.content
                    && let Some(filtered) = filter_blocks(blocks)
                {
                    user.content = UserContent::Blocks(filtered);
                }
                Message::User(user)
            }
            Message::ToolResult(mut tool_result) => {
                if let Some(filtered) = filter_blocks(&tool_result.content) {
                    tool_result.content = filtered;
                }
                Message::ToolResult(tool_result)
            }
            other => other,
        })
        .collect()
}

// ============================================================================
// Default stream fn (sdk.ts:302-345, registry-aware)
// ============================================================================

fn default_stream_fn(
    registry: Arc<tokio::sync::RwLock<ModelRegistry>>,
    settings: Arc<Mutex<SettingsManager>>,
) -> StreamFn {
    Arc::new(move |model: Model, context, options| {
        let registry = registry.clone();
        let settings = settings.clone();
        Box::pin(async move {
            let auth = {
                let registry = registry.read().await;
                registry.get_api_key_and_headers(&model).await
            };
            if !auth.ok {
                return error_event_stream(
                    &model,
                    auth.error
                        .unwrap_or_else(|| "Authentication failed".to_string()),
                );
            }
            let timeout_ms = {
                let guard = settings.lock();
                let t = guard.get_http_idle_timeout_ms();
                if t == 0 { i32::MAX as u64 } else { t }
            };
            let stream_options = pi_ai::StreamOptions {
                temperature: options.temperature,
                max_tokens: options.max_tokens,
                api_key: options.api_key.or(auth.api_key),
                transport: None,
                cache_retention: None,
                session_id: options.session_id,
                headers: auth
                    .headers
                    .map(|h| h.into_iter().map(|(k, v)| (k, Some(v))).collect()),
                timeout_ms: Some(timeout_ms),
                websocket_connect_timeout_ms: None,
                max_retries: None,
                max_retry_delay_ms: None,
                metadata: options.metadata,
                env: auth.env.map(|e| e.into_iter().collect()),
            };
            pi_ai::api::stream_dispatch(model.api.as_ref(), model.clone(), context, stream_options)
        })
    })
}

fn error_event_stream(model: &Model, error: String) -> pi_ai::AssistantMessageEventStream {
    let mut message = pi_ai::models::create_empty_assistant_message(model);
    message.stop_reason = StopReason::Error;
    message.error_message = Some(error);
    let stream = pi_ai::create_assistant_message_event_stream();
    stream.push(pi_ai::AssistantMessageEvent::Error {
        reason: StopReason::Error,
        error: message,
    });
    stream
}

// ============================================================================
// Built-in tool prompt metadata (verbatim from core/tools/*.ts)
// ============================================================================

fn builtin_prompt_snippet(name: &str) -> Option<&'static str> {
    match name {
        "read" => Some("Read file contents"),
        "bash" => Some("Execute bash commands (ls, grep, find, etc.)"),
        "edit" => Some(
            "Make precise file edits with exact text replacement, including multiple disjoint edits in one call",
        ),
        "write" => Some("Create or overwrite files"),
        "grep" => Some("Search file contents for patterns (respects .gitignore)"),
        "find" => Some("Find files by glob pattern (respects .gitignore)"),
        "ls" => Some("List directory contents"),
        _ => None,
    }
}

fn builtin_prompt_guidelines(name: &str) -> &'static [&'static str] {
    match name {
        "read" => &["Use read to examine files instead of cat or sed."],
        "edit" => &[
            "Use edit for precise changes (edits[].oldText must match exactly)",
            "When changing multiple separate locations in one file, use one edit call with multiple entries in edits[] instead of multiple edit calls",
            "Each edits[].oldText is matched against the original file, not after earlier edits are applied. Do not emit overlapping or nested edits. Merge nearby changes into one edit.",
            "Keep edits[].oldText as small as possible while still being unique in the file. Do not pad with large unchanged regions.",
        ],
        "write" => &["Use write only for new files or complete rewrites."],
        _ => &[],
    }
}

/// Rebuild the base system prompt from the active tool set (oracle
/// `_rebuildSystemPrompt`).
fn rebuild_system_prompt(state: &mut SessionState, cwd: &std::path::Path) {
    let valid_tool_names: Vec<String> = state
        .active_tool_names
        .iter()
        .filter(|name| find_tool(state, name).is_some())
        .cloned()
        .collect();

    let mut tool_snippets = std::collections::HashMap::new();
    let mut prompt_guidelines = Vec::new();
    for name in &valid_tool_names {
        let Some(tool) = find_tool(state, name) else {
            continue;
        };
        if let Some(snippet) = normalize_prompt_snippet(tool.prompt_snippet.as_deref()) {
            tool_snippets.insert(name.clone(), snippet);
        }
        for guideline in normalize_prompt_guidelines(&tool.prompt_guidelines) {
            prompt_guidelines.push(guideline);
        }
    }

    let options = BuildSystemPromptOptions {
        cwd: cwd.to_string_lossy().into_owned(),
        skills: state.skills.clone(),
        context_files: state.context_files.clone(),
        custom_prompt: state.custom_system_prompt.clone(),
        append_system_prompt: state.append_system_prompt.clone(),
        selected_tools: Some(valid_tool_names),
        tool_snippets,
        prompt_guidelines,
    };
    state.base_system_prompt = build_system_prompt(&options);
}

fn normalize_prompt_snippet(text: Option<&str>) -> Option<String> {
    let text = text?;
    let one_line = text
        .split(['\r', '\n'])
        .collect::<Vec<_>>()
        .join(" ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if one_line.is_empty() {
        None
    } else {
        Some(one_line)
    }
}

fn normalize_prompt_guidelines(guidelines: &[String]) -> Vec<String> {
    let mut unique = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for guideline in guidelines {
        let normalized = guideline.trim();
        if !normalized.is_empty() && seen.insert(normalized.to_string()) {
            unique.push(normalized.to_string());
        }
    }
    unique
}

// ============================================================================
// Frontmatter stripping (utils/frontmatter.ts)
// ============================================================================

fn strip_frontmatter(content: &str) -> String {
    let normalized = content.replace("\r\n", "\n").replace('\r', "\n");
    if !normalized.starts_with("---") {
        return normalized;
    }
    match normalized[3..].find("\n---") {
        Some(rel_idx) => {
            let end_index = rel_idx + 3;
            normalized[end_index + 4..].trim().to_string()
        }
        None => normalized,
    }
}

// ============================================================================
// Prompt template expansion (prompt-templates.ts)
// ============================================================================

/// Oracle `parseCommandArgs`: bash-style quoted argument splitting.
pub fn parse_command_args(args_string: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut in_quote: Option<char> = None;

    for ch in args_string.chars() {
        if let Some(quote) = in_quote {
            if ch == quote {
                in_quote = None;
            } else {
                current.push(ch);
            }
        } else if ch == '"' || ch == '\'' {
            in_quote = Some(ch);
        } else if ch.is_whitespace() {
            if !current.is_empty() {
                args.push(std::mem::take(&mut current));
            }
        } else {
            current.push(ch);
        }
    }
    if !current.is_empty() {
        args.push(current);
    }
    args
}

/// Oracle `substituteArgs`: `$1`, `$@`, `$ARGUMENTS`, `${N:-default}`,
/// `${@:N}`, `${@:N:L}` substitution.
pub fn substitute_args(content: &str, args: &[String]) -> String {
    static PATTERN: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"\$\{(\d+):-([^}]*)\}|\$\{@:(\d+)(?::(\d+))?\}|\$(ARGUMENTS|@|\d+)")
            .expect("template regex")
    });
    let all_args = args.join(" ");
    PATTERN
        .replace_all(content, |caps: &regex::Captures<'_>| {
            if let Some(default_num) = caps.get(1) {
                let index = default_num.as_str().parse::<usize>().unwrap_or(0);
                let value = index.checked_sub(1).and_then(|i| args.get(i));
                return match value {
                    Some(v) if !v.is_empty() => v.clone(),
                    _ => caps
                        .get(2)
                        .map(|m| m.as_str().to_string())
                        .unwrap_or_default(),
                };
            }
            if let Some(slice_start) = caps.get(3) {
                let mut start = slice_start.as_str().parse::<i64>().unwrap_or(1) - 1;
                if start < 0 {
                    start = 0;
                }
                let start = start as usize;
                if let Some(slice_len) = caps.get(4) {
                    let length = slice_len.as_str().parse::<usize>().unwrap_or(0);
                    let end = std::cmp::min(start.saturating_add(length), args.len());
                    if start >= args.len() {
                        return String::new();
                    }
                    return args[start..end].join(" ");
                }
                if start >= args.len() {
                    return String::new();
                }
                return args[start..].join(" ");
            }
            let simple = caps.get(5).map(|m| m.as_str()).unwrap_or("");
            if simple == "ARGUMENTS" || simple == "@" {
                return all_args.clone();
            }
            let index = simple.parse::<usize>().unwrap_or(0);
            index
                .checked_sub(1)
                .and_then(|i| args.get(i))
                .cloned()
                .unwrap_or_default()
        })
        .into_owned()
}

/// Oracle `expandPromptTemplate`.
pub fn expand_prompt_template(text: &str, templates: &[PromptTemplate]) -> String {
    if !text.starts_with('/') {
        return text.to_string();
    }
    static COMMAND: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"^/(\S+)(?:\s+([\s\S]*))?$").expect("command regex")
    });
    let Some(caps) = COMMAND.captures(text) else {
        return text.to_string();
    };
    let template_name = caps.get(1).map(|m| m.as_str()).unwrap_or("");
    let args_string = caps.get(2).map(|m| m.as_str()).unwrap_or("");
    match templates.iter().find(|t| t.name == template_name) {
        Some(template) => {
            let args = parse_command_args(args_string);
            substitute_args(&template.content, &args)
        }
        None => text.to_string(),
    }
}

// ============================================================================
// Model & thinking management, tool registry, session info, stats
// ============================================================================

/// Standard thinking levels offered without a model (agent-session.ts:262).
const THINKING_LEVELS: [AgentThinkingLevel; 5] = [
    AgentThinkingLevel::Off,
    AgentThinkingLevel::Minimal,
    AgentThinkingLevel::Low,
    AgentThinkingLevel::Medium,
    AgentThinkingLevel::High,
];

fn agent_to_model_thinking(level: AgentThinkingLevel) -> ModelThinkingLevel {
    match level {
        AgentThinkingLevel::Off => ModelThinkingLevel::Off,
        AgentThinkingLevel::Minimal => ModelThinkingLevel::Minimal,
        AgentThinkingLevel::Low => ModelThinkingLevel::Low,
        AgentThinkingLevel::Medium => ModelThinkingLevel::Medium,
        AgentThinkingLevel::High => ModelThinkingLevel::High,
        AgentThinkingLevel::Xhigh => ModelThinkingLevel::Xhigh,
        AgentThinkingLevel::Max => ModelThinkingLevel::Max,
    }
}

fn model_to_agent_thinking(level: ModelThinkingLevel) -> AgentThinkingLevel {
    match level {
        ModelThinkingLevel::Off => AgentThinkingLevel::Off,
        ModelThinkingLevel::Minimal => AgentThinkingLevel::Minimal,
        ModelThinkingLevel::Low => AgentThinkingLevel::Low,
        ModelThinkingLevel::Medium => AgentThinkingLevel::Medium,
        ModelThinkingLevel::High => AgentThinkingLevel::High,
        ModelThinkingLevel::Xhigh => AgentThinkingLevel::Xhigh,
        ModelThinkingLevel::Max => AgentThinkingLevel::Max,
    }
}

/// Wire string for a thinking level ("off", "minimal", ...).
pub fn thinking_level_str(level: AgentThinkingLevel) -> &'static str {
    match level {
        AgentThinkingLevel::Off => "off",
        AgentThinkingLevel::Minimal => "minimal",
        AgentThinkingLevel::Low => "low",
        AgentThinkingLevel::Medium => "medium",
        AgentThinkingLevel::High => "high",
        AgentThinkingLevel::Xhigh => "xhigh",
        AgentThinkingLevel::Max => "max",
    }
}

/// Parse a session thinking-level string (unknown -> Off).
pub fn parse_thinking_level(raw: &str) -> AgentThinkingLevel {
    match raw {
        "minimal" => AgentThinkingLevel::Minimal,
        "low" => AgentThinkingLevel::Low,
        "medium" => AgentThinkingLevel::Medium,
        "high" => AgentThinkingLevel::High,
        "xhigh" => AgentThinkingLevel::Xhigh,
        "max" => AgentThinkingLevel::Max,
        _ => AgentThinkingLevel::Off,
    }
}

impl AgentSession {
    // =====================================================================
    // Model management
    // =====================================================================

    /// Set model directly (oracle `setModel`).
    pub async fn set_model(&self, model: Model) -> Result<(), String> {
        {
            let registry = self.inner.registry.read().await;
            if !registry.has_configured_auth(&model).await {
                return Err(format!("No API key for {}/{}", model.provider, model.id));
            }
        }

        let thinking_level = self.thinking_level_for_model_switch(None);
        {
            let mut state = self.inner.state.lock();
            state.model = Some(model.clone());
            let _ = state
                .session_manager
                .append_model_change(model.provider.clone(), model.id.clone());
        }
        {
            let mut settings = self.inner.settings.lock();
            settings.set_default_provider(model.provider.clone());
            settings.set_default_model(model.id.clone());
        }

        // Re-clamp thinking level for new model's capabilities.
        self.set_thinking_level(thinking_level);
        Ok(())
    }

    /// Cycle to next/previous model (oracle `cycleModel`).
    pub async fn cycle_model(&self, forward: bool) -> Option<ModelCycleResult> {
        let scoped = self.inner.state.lock().scoped_models.clone();
        if !scoped.is_empty() {
            self.cycle_scoped_model(scoped, forward).await
        } else {
            self.cycle_available_model(forward).await
        }
    }

    async fn cycle_scoped_model(
        &self,
        scoped: Vec<ScopedModel>,
        forward: bool,
    ) -> Option<ModelCycleResult> {
        let mut available = Vec::new();
        {
            let registry = self.inner.registry.read().await;
            for entry in scoped {
                if registry.has_configured_auth(&entry.model).await {
                    available.push(entry);
                }
            }
        }
        if available.len() <= 1 {
            return None;
        }

        let current_model = self.model();
        let mut current_index = available
            .iter()
            .position(|sm| models_are_equal(Some(&sm.model), current_model.as_ref()))
            .unwrap_or(0) as i64;
        if !available
            .iter()
            .any(|sm| models_are_equal(Some(&sm.model), current_model.as_ref()))
        {
            current_index = 0;
        }
        let len = available.len() as i64;
        let next_index = if forward {
            (current_index + 1).rem_euclid(len)
        } else {
            (current_index - 1).rem_euclid(len)
        };
        let next = available[next_index as usize].clone();
        let thinking_level = self.thinking_level_for_model_switch(next.thinking_level);

        {
            let mut state = self.inner.state.lock();
            state.model = Some(next.model.clone());
            let _ = state
                .session_manager
                .append_model_change(next.model.provider.clone(), next.model.id.clone());
        }
        {
            let mut settings = self.inner.settings.lock();
            settings.set_default_provider(next.model.provider.clone());
            settings.set_default_model(next.model.id.clone());
        }
        self.set_thinking_level(thinking_level);

        Some(ModelCycleResult {
            model: next.model,
            thinking_level: self.thinking_level(),
            is_scoped: true,
        })
    }

    async fn cycle_available_model(&self, forward: bool) -> Option<ModelCycleResult> {
        let available = {
            let registry = self.inner.registry.read().await;
            registry.get_available().await
        };
        if available.len() <= 1 {
            return None;
        }

        let current_model = self.model();
        let current_index = available
            .iter()
            .position(|m| models_are_equal(Some(m), current_model.as_ref()))
            .unwrap_or(0) as i64;
        let len = available.len() as i64;
        let next_index = if forward {
            (current_index + 1).rem_euclid(len)
        } else {
            (current_index - 1).rem_euclid(len)
        };
        let next_model = available[next_index as usize].clone();

        let thinking_level = self.thinking_level_for_model_switch(None);
        {
            let mut state = self.inner.state.lock();
            state.model = Some(next_model.clone());
            let _ = state
                .session_manager
                .append_model_change(next_model.provider.clone(), next_model.id.clone());
        }
        {
            let mut settings = self.inner.settings.lock();
            settings.set_default_provider(next_model.provider.clone());
            settings.set_default_model(next_model.id.clone());
        }
        self.set_thinking_level(thinking_level);

        Some(ModelCycleResult {
            model: next_model,
            thinking_level: self.thinking_level(),
            is_scoped: false,
        })
    }

    // =====================================================================
    // Thinking level management
    // =====================================================================

    /// Set thinking level, clamped to model capabilities (oracle
    /// `setThinkingLevel`). Persists only on actual change.
    pub fn set_thinking_level(&self, level: AgentThinkingLevel) {
        let available = self.get_available_thinking_levels();
        let effective = if available.contains(&level) {
            level
        } else {
            self.clamp_thinking_level_to_model(level)
        };

        let is_changing = {
            let mut state = self.inner.state.lock();
            let previous = state.thinking_level;
            state.thinking_level = effective;
            effective != previous
        };

        if is_changing {
            {
                let mut state = self.inner.state.lock();
                let _ = state
                    .session_manager
                    .append_thinking_level_change(thinking_level_str(effective));
            }
            if self.supports_thinking() || effective != AgentThinkingLevel::Off {
                self.inner
                    .settings
                    .lock()
                    .set_default_thinking_level(thinking_level_str(effective));
            }
            self.inner
                .emit(&AgentSessionEvent::ThinkingLevelChanged { level: effective });
        }
    }

    /// Cycle to the next thinking level (oracle `cycleThinkingLevel`).
    pub fn cycle_thinking_level(&self) -> Option<AgentThinkingLevel> {
        if !self.supports_thinking() {
            return None;
        }
        let levels = self.get_available_thinking_levels();
        let current = self.thinking_level();
        let current_index = levels.iter().position(|l| *l == current).unwrap_or(0);
        let next = levels[(current_index + 1) % levels.len()];
        self.set_thinking_level(next);
        Some(next)
    }

    /// Available thinking levels for the current model.
    pub fn get_available_thinking_levels(&self) -> Vec<AgentThinkingLevel> {
        let model = self.inner.state.lock().model.clone();
        match model {
            None => THINKING_LEVELS.to_vec(),
            Some(model) => get_supported_thinking_levels(&model)
                .into_iter()
                .map(model_to_agent_thinking)
                .collect(),
        }
    }

    /// Whether the current model supports thinking/reasoning.
    pub fn supports_thinking(&self) -> bool {
        self.inner
            .state
            .lock()
            .model
            .as_ref()
            .is_some_and(|m| m.reasoning)
    }

    fn thinking_level_for_model_switch(
        &self,
        explicit_level: Option<AgentThinkingLevel>,
    ) -> AgentThinkingLevel {
        if let Some(level) = explicit_level {
            return level;
        }
        if !self.supports_thinking() {
            let default = self
                .inner
                .settings
                .lock()
                .get_default_thinking_level()
                .map(parse_thinking_level);
            return default.unwrap_or(DEFAULT_THINKING_LEVEL);
        }
        self.thinking_level()
    }

    fn clamp_thinking_level_to_model(&self, level: AgentThinkingLevel) -> AgentThinkingLevel {
        let model = self.inner.state.lock().model.clone();
        match model {
            Some(model) => model_to_agent_thinking(clamp_thinking_level(
                &model,
                agent_to_model_thinking(level),
            )),
            None => AgentThinkingLevel::Off,
        }
    }

    // =====================================================================
    // Compaction settings
    // =====================================================================

    pub fn auto_compaction_enabled(&self) -> bool {
        self.inner.settings.lock().get_compaction_enabled()
    }

    pub fn set_auto_compaction_enabled(&self, enabled: bool) {
        self.inner.settings.lock().set_compaction_enabled(enabled);
    }

    // =====================================================================
    // Tool registry
    // =====================================================================

    /// Names of currently active tools.
    pub fn get_active_tool_names(&self) -> Vec<String> {
        self.inner.state.lock().active_tool_names.clone()
    }

    /// All configured tools with prompt metadata (oracle `getAllTools`).
    pub fn get_all_tools(&self) -> Vec<ToolInfo> {
        let state = self.inner.state.lock();
        state
            .tool_registry
            .iter()
            .map(|tool| ToolInfo {
                name: tool.definition.name.clone(),
                description: tool.definition.description.clone(),
                parameters: tool.definition.parameters.clone(),
                prompt_guidelines: tool.prompt_guidelines.clone(),
                source: tool.source,
                source_info: tool.source_info.clone(),
            })
            .collect()
    }

    pub fn get_tool_definition(&self, name: &str) -> Option<Arc<ToolDefinition>> {
        let state = self.inner.state.lock();
        find_tool(&state, name).map(|t| t.definition.clone())
    }

    /// Set active tools by name; unknown names are ignored. Rebuilds the
    /// system prompt (oracle `setActiveToolsByName`).
    pub fn set_active_tools_by_name(&self, tool_names: Vec<String>) {
        let mut state = self.inner.state.lock();
        let valid: Vec<String> = tool_names
            .into_iter()
            .filter(|name| find_tool(&state, name).is_some())
            .collect();
        state.active_tool_names = valid;
        rebuild_system_prompt(&mut state, &self.inner.cwd);
        state.effective_system_prompt = state
            .system_prompt_override
            .clone()
            .unwrap_or_else(|| state.base_system_prompt.clone());
    }

    /// Replace the extension-tool partition and rebuild the registry
    /// (oracle `_refreshToolRegistry`, agent-session.ts:2397-2488):
    /// previously-active names survive the allow/deny filter, and the final
    /// list is deduplicated before `set_active_tools_by_name` semantics
    /// (filter-to-registry + system prompt rebuild) apply.
    ///
    /// The FIRST application on a session is construction-equivalent (pi
    /// loads extensions during session construction with
    /// `includeAllExtensionTools`; pi-rust binds after): every custom tool
    /// becomes active even when it shadows an existing inactive name. Later
    /// applications (`refreshTools`) use plain "newly-appeared names become
    /// active" semantics.
    pub fn refresh_extension_tools(&self, extension_tools: Vec<SessionToolDefinition>) {
        let mut state = self.inner.state.lock();
        let include_all_extension_tools = !state.extension_tools_applied;
        state.extension_tools_applied = true;
        let is_allowed = |state: &SessionState, name: &str| -> bool {
            state
                .allowed_tool_names
                .as_ref()
                .is_none_or(|allow| allow.iter().any(|n| n == name))
                && !state
                    .excluded_tool_names
                    .as_ref()
                    .is_some_and(|deny| deny.iter().any(|n| n == name))
        };

        let previous_names: std::collections::HashSet<String> = state
            .tool_registry
            .iter()
            .map(|t| t.definition.name.clone())
            .collect();
        let previous_active = state.active_tool_names.clone();

        state.extension_tools = extension_tools
            .into_iter()
            .filter(|tool| is_allowed(&state, &tool.definition.name))
            .collect();
        state.tool_registry =
            merge_tool_registry(&state.base_tools, &state.extension_tools, &state.sdk_tools);

        let mut next_active: Vec<String> = previous_active
            .into_iter()
            .filter(|name| is_allowed(&state, name))
            .collect();
        if state.allowed_tool_names.is_some() {
            // Allowlist present: every allowed registry tool becomes active
            // (oracle allowedToolNames branch).
            for tool in &state.tool_registry {
                if is_allowed(&state, &tool.definition.name) {
                    next_active.push(tool.definition.name.clone());
                }
            }
        } else if include_all_extension_tools {
            // Oracle includeAllExtensionTools branch: all custom tools
            // (extension + sdk) activate, shadows included.
            for tool in &state.extension_tools {
                next_active.push(tool.definition.name.clone());
            }
            for tool in &state.sdk_tools {
                next_active.push(tool.definition.name.clone());
            }
        } else {
            // Default branch: tools that newly appeared become active.
            for tool in &state.tool_registry {
                if !previous_names.contains(&tool.definition.name) {
                    next_active.push(tool.definition.name.clone());
                }
            }
        }

        // `[...new Set(...)]` + setActiveToolsByName (filter to registry).
        let mut seen = std::collections::HashSet::new();
        let valid: Vec<String> = next_active
            .into_iter()
            .filter(|name| seen.insert(name.clone()) && find_tool(&state, name).is_some())
            .collect();
        state.active_tool_names = valid;
        rebuild_system_prompt(&mut state, &self.inner.cwd);
        state.effective_system_prompt = state
            .system_prompt_override
            .clone()
            .unwrap_or_else(|| state.base_system_prompt.clone());
    }

    /// Whether a tool name passes the allow/deny registry filters.
    pub fn is_tool_allowed(&self, name: &str) -> bool {
        let state = self.inner.state.lock();
        state
            .allowed_tool_names
            .as_ref()
            .is_none_or(|allow| allow.iter().any(|n| n == name))
            && !state
                .excluded_tool_names
                .as_ref()
                .is_some_and(|deny| deny.iter().any(|n| n == name))
    }

    // =====================================================================
    // Session info / stats / utilities
    // =====================================================================

    /// Set a display name for the current session (oracle `setSessionName`).
    pub fn set_session_name(&self, name: &str) {
        let current_name = {
            let mut state = self.inner.state.lock();
            let _ = state.session_manager.append_session_info(name);
            state.session_manager.get_session_name()
        };
        self.inner
            .emit(&AgentSessionEvent::SessionInfoChanged { name: current_name });
    }

    /// Session statistics aggregated over ALL entries (oracle
    /// `getSessionStats`).
    pub fn get_session_stats(&self) -> SessionStats {
        let (entries, session_file, session_id) = {
            let state = self.inner.state.lock();
            (
                state.session_manager.get_entries(),
                state
                    .session_manager
                    .get_session_file()
                    .map(|p| p.to_string_lossy().into_owned()),
                state.session_manager.get_session_id().to_string(),
            )
        };

        let mut user_messages = 0u64;
        let mut assistant_messages = 0u64;
        let mut tool_results = 0u64;
        let mut total_messages = 0u64;
        let mut tool_calls = 0u64;
        let mut total_input = 0u64;
        let mut total_output = 0u64;
        let mut total_cache_read = 0u64;
        let mut total_cache_write = 0u64;
        let mut total_cost = 0f64;

        for entry in &entries {
            let crate::session_types::SessionEntry::Message { message, .. } = entry else {
                continue;
            };
            total_messages += 1;
            match message.get("role").and_then(Value::as_str) {
                Some("user") => user_messages += 1,
                Some("toolResult") => tool_results += 1,
                Some("assistant") => {
                    assistant_messages += 1;
                    if let Some(content) = message.get("content").and_then(Value::as_array) {
                        tool_calls += content
                            .iter()
                            .filter(|c| c.get("type").and_then(Value::as_str) == Some("toolCall"))
                            .count() as u64;
                    }
                    if let Some(usage) = message.get("usage") {
                        total_input += usage.get("input").and_then(Value::as_u64).unwrap_or(0);
                        total_output += usage.get("output").and_then(Value::as_u64).unwrap_or(0);
                        total_cache_read +=
                            usage.get("cacheRead").and_then(Value::as_u64).unwrap_or(0);
                        total_cache_write +=
                            usage.get("cacheWrite").and_then(Value::as_u64).unwrap_or(0);
                        total_cost += usage
                            .get("cost")
                            .and_then(|c| c.get("total"))
                            .and_then(Value::as_f64)
                            .unwrap_or(0.0);
                    }
                }
                _ => {}
            }
        }

        SessionStats {
            session_file,
            session_id,
            user_messages,
            assistant_messages,
            tool_calls,
            tool_results,
            total_messages,
            tokens: SessionTokenStats {
                input: total_input,
                output: total_output,
                cache_read: total_cache_read,
                cache_write: total_cache_write,
                total: total_input + total_output + total_cache_read + total_cache_write,
            },
            cost: total_cost,
            context_usage: self.get_context_usage(),
        }
    }

    /// Context window usage (oracle `getContextUsage`).
    pub fn get_context_usage(&self) -> Option<ContextUsage> {
        let state = self.inner.state.lock();
        let model = state.model.as_ref()?;
        let context_window = model.context_window;
        if context_window == 0 {
            return None;
        }

        let branch_entries = state.session_manager.get_branch(None);
        if let Some(compaction_index) = latest_compaction_index(&branch_entries) {
            // Only trust usage from an assistant that responded after the
            // latest compaction boundary.
            let mut has_post_compaction_usage = false;
            for entry in branch_entries.iter().skip(compaction_index + 1).rev() {
                let crate::session_types::SessionEntry::Message { message, .. } = entry else {
                    continue;
                };
                if message.get("role").and_then(Value::as_str) != Some("assistant") {
                    continue;
                }
                let stop_reason = message.get("stopReason").and_then(Value::as_str);
                if stop_reason == Some("aborted") || stop_reason == Some("error") {
                    continue;
                }
                if let Some(usage) = message.get("usage")
                    && let Ok(usage) = serde_json::from_value::<Usage>(usage.clone())
                    && calculate_context_tokens(&usage) > 0
                {
                    has_post_compaction_usage = true;
                    break;
                }
            }
            if !has_post_compaction_usage {
                return Some(ContextUsage {
                    tokens: None,
                    context_window,
                    percent: None,
                });
            }
        }

        let estimate = estimate_context_tokens(&state.messages);
        let percent = (estimate.tokens as f64 / context_window as f64) * 100.0;
        Some(ContextUsage {
            tokens: Some(estimate.tokens),
            context_window,
            percent: Some(percent),
        })
    }

    /// Text content of the last assistant message (oracle
    /// `getLastAssistantText`).
    pub fn get_last_assistant_text(&self) -> Option<String> {
        let state = self.inner.state.lock();
        let last_assistant = state.messages.iter().rev().find_map(|m| match m {
            AgentMessage::Standard(Message::Assistant(a)) => {
                // Skip aborted messages with no content.
                if a.stop_reason == StopReason::Aborted && a.content.is_empty() {
                    None
                } else {
                    Some(a)
                }
            }
            _ => None,
        })?;

        let mut text = String::new();
        for content in &last_assistant.content {
            if let Content::Text(t) = content {
                text.push_str(&t.text.to_string());
            }
        }
        let trimmed = text.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    }

    /// All user messages for the fork selector (oracle
    /// `getUserMessagesForForking`).
    pub fn get_user_messages_for_forking(&self) -> Vec<(String, String)> {
        let state = self.inner.state.lock();
        let mut result = Vec::new();
        for entry in state.session_manager.get_entries() {
            let crate::session_types::SessionEntry::Message { id, message, .. } = &entry else {
                continue;
            };
            if message.get("role").and_then(Value::as_str) != Some("user") {
                continue;
            }
            let text = extract_user_content_text(message.get("content"));
            if !text.is_empty()
                && let Some(id) = id
            {
                result.push((id.clone(), text));
            }
        }
        result
    }
}

/// Extract text from a user-message content Value (string or blocks).
fn extract_user_content_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

/// Index of the latest compaction entry in a branch (oracle
/// `getLatestCompactionEntry`).
fn latest_compaction_index(entries: &[crate::session_types::SessionEntry]) -> Option<usize> {
    entries
        .iter()
        .rposition(|e| matches!(e, crate::session_types::SessionEntry::Compaction { .. }))
}

// ============================================================================
// Token estimation (compaction/compaction.ts)
// ============================================================================

const ESTIMATED_IMAGE_CHARS: u64 = 4800;

/// Oracle `calculateContextTokens`.
pub fn calculate_context_tokens(usage: &Usage) -> u64 {
    if usage.total_tokens != 0 {
        usage.total_tokens
    } else {
        usage.input + usage.output + usage.cache_read + usage.cache_write
    }
}

fn text_and_image_chars_value(content: Option<&Value>) -> u64 {
    match content {
        Some(Value::String(text)) => text.len() as u64,
        Some(Value::Array(blocks)) => {
            let mut chars = 0u64;
            for block in blocks {
                match block.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        chars += block
                            .get("text")
                            .and_then(Value::as_str)
                            .map(|t| t.len() as u64)
                            .unwrap_or(0);
                    }
                    Some("image") => chars += ESTIMATED_IMAGE_CHARS,
                    _ => {}
                }
            }
            chars
        }
        _ => 0,
    }
}

fn text_and_image_chars_blocks(blocks: &[Content]) -> u64 {
    let mut chars = 0u64;
    for block in blocks {
        match block {
            Content::Text(t) => chars += t.text.to_string().len() as u64,
            Content::Image(_) => chars += ESTIMATED_IMAGE_CHARS,
            _ => {}
        }
    }
    chars
}

/// Oracle `estimateTokens` (chars/4 heuristic).
pub fn estimate_tokens(message: &AgentMessage) -> u64 {
    let chars: u64 = match message {
        AgentMessage::Standard(Message::User(user)) => match &user.content {
            UserContent::Text(text) => text.len() as u64,
            UserContent::Blocks(blocks) => text_and_image_chars_blocks(blocks),
        },
        AgentMessage::Standard(Message::Assistant(assistant)) => {
            let mut chars = 0u64;
            for block in &assistant.content {
                match block {
                    Content::Text(t) => chars += t.text.to_string().len() as u64,
                    Content::Thinking(t) => chars += t.thinking.to_string().len() as u64,
                    Content::ToolCall(call) => {
                        chars += call.name.len() as u64;
                        chars += serde_json::to_string(&call.arguments)
                            .map(|s| s.len() as u64)
                            .unwrap_or(0);
                    }
                    Content::Image(_) => {}
                }
            }
            chars
        }
        AgentMessage::Standard(Message::ToolResult(result)) => {
            text_and_image_chars_blocks(&result.content)
        }
        AgentMessage::Custom(value) => match value.get("role").and_then(Value::as_str) {
            Some("custom") => text_and_image_chars_value(value.get("content")),
            Some("bashExecution") => {
                let command = value.get("command").and_then(Value::as_str).unwrap_or("");
                let output = value.get("output").and_then(Value::as_str).unwrap_or("");
                (command.len() + output.len()) as u64
            }
            Some("branchSummary") | Some("compactionSummary") => value
                .get("summary")
                .and_then(Value::as_str)
                .map(|s| s.len() as u64)
                .unwrap_or(0),
            _ => return 0,
        },
    };
    chars.div_ceil(4)
}

/// Oracle `ContextUsageEstimate`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ContextUsageEstimate {
    pub tokens: u64,
    pub usage_tokens: u64,
    pub trailing_tokens: u64,
    pub last_usage_index: Option<usize>,
}

fn assistant_usage(message: &AgentMessage) -> Option<&Usage> {
    if let AgentMessage::Standard(Message::Assistant(assistant)) = message
        && assistant.stop_reason != StopReason::Aborted
        && assistant.stop_reason != StopReason::Error
        && calculate_context_tokens(&assistant.usage) > 0
    {
        Some(&assistant.usage)
    } else {
        None
    }
}

/// Oracle `estimateContextTokens`.
pub fn estimate_context_tokens(messages: &[AgentMessage]) -> ContextUsageEstimate {
    let usage_info = messages
        .iter()
        .enumerate()
        .rev()
        .find_map(|(i, m)| assistant_usage(m).map(|u| (u, i)));

    match usage_info {
        None => {
            let estimated: u64 = messages.iter().map(estimate_tokens).sum();
            ContextUsageEstimate {
                tokens: estimated,
                usage_tokens: 0,
                trailing_tokens: estimated,
                last_usage_index: None,
            }
        }
        Some((usage, index)) => {
            let usage_tokens = calculate_context_tokens(usage);
            let trailing_tokens: u64 = messages[index + 1..].iter().map(estimate_tokens).sum();
            ContextUsageEstimate {
                tokens: usage_tokens + trailing_tokens,
                usage_tokens,
                trailing_tokens,
                last_usage_index: Some(index),
            }
        }
    }
}

/// Oracle `shouldCompact`.
pub fn should_compact(
    context_tokens: u64,
    context_window: u64,
    enabled: bool,
    reserve_tokens: u64,
) -> bool {
    if !enabled {
        return false;
    }
    context_tokens > context_window.saturating_sub(reserve_tokens)
}
// ============================================================================
// Compaction machinery (core/compaction/{compaction,utils}.ts)
// ============================================================================

/// Oracle `CompactionSettings`.
#[derive(Clone, Copy, Debug)]
pub struct CompactionSettings {
    pub enabled: bool,
    pub reserve_tokens: u64,
    pub keep_recent_tokens: u64,
}

fn compaction_settings(settings: &Arc<Mutex<SettingsManager>>) -> CompactionSettings {
    let guard = settings.lock();
    CompactionSettings {
        enabled: guard.get_compaction_enabled(),
        reserve_tokens: guard.get_compaction_reserve_tokens(),
        keep_recent_tokens: guard.get_compaction_keep_recent_tokens(),
    }
}

/// Oracle `FileOperations` (utils.ts).
#[derive(Clone, Debug, Default)]
pub struct FileOperations {
    pub read: std::collections::BTreeSet<String>,
    pub written: std::collections::BTreeSet<String>,
    pub edited: std::collections::BTreeSet<String>,
}

/// Oracle `extractFileOpsFromMessage`.
fn extract_file_ops_from_message(message: &AgentMessage, file_ops: &mut FileOperations) {
    let AgentMessage::Standard(Message::Assistant(assistant)) = message else {
        return;
    };
    for block in &assistant.content {
        let Content::ToolCall(call) = block else {
            continue;
        };
        let Some(Value::String(path)) = call.arguments.get("path") else {
            continue;
        };
        match call.name.as_str() {
            "read" => {
                file_ops.read.insert(path.clone());
            }
            "write" => {
                file_ops.written.insert(path.clone());
            }
            "edit" => {
                file_ops.edited.insert(path.clone());
            }
            _ => {}
        }
    }
}

/// Oracle `computeFileLists`: (readFiles, modifiedFiles), both sorted.
fn compute_file_lists(file_ops: &FileOperations) -> (Vec<String>, Vec<String>) {
    let modified: std::collections::BTreeSet<String> = file_ops
        .edited
        .iter()
        .chain(file_ops.written.iter())
        .cloned()
        .collect();
    let read_only: Vec<String> = file_ops
        .read
        .iter()
        .filter(|f| !modified.contains(*f))
        .cloned()
        .collect();
    (read_only, modified.into_iter().collect())
}

/// Oracle `formatFileOperations`.
fn format_file_operations(read_files: &[String], modified_files: &[String]) -> String {
    let mut sections: Vec<String> = Vec::new();
    if !read_files.is_empty() {
        sections.push(format!(
            "<read-files>\n{}\n</read-files>",
            read_files.join("\n")
        ));
    }
    if !modified_files.is_empty() {
        sections.push(format!(
            "<modified-files>\n{}\n</modified-files>",
            modified_files.join("\n")
        ));
    }
    if sections.is_empty() {
        return String::new();
    }
    format!("\n\n{}", sections.join("\n\n"))
}

/// Maximum characters for a tool result in serialized summaries.
const TOOL_RESULT_MAX_CHARS: usize = 2000;

/// Oracle `truncateForSummary`.
fn truncate_for_summary(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let total = text.chars().count();
    let truncated_chars = total - max_chars;
    let head: String = text.chars().take(max_chars).collect();
    format!("{head}\n\n[... {truncated_chars} more characters truncated]")
}

/// Oracle `serializeConversation` (utils.ts:109-166).
///
/// Tool-call arguments serialize with sorted keys (JS preserves object
/// insertion order, which `HashMap` cannot recover; sorting keeps the
/// serialization deterministic).
pub fn serialize_conversation(messages: &[Message]) -> String {
    let mut parts: Vec<String> = Vec::new();

    for msg in messages {
        match msg {
            Message::User(user) => {
                let content = match &user.content {
                    UserContent::Text(text) => text.clone(),
                    UserContent::Blocks(blocks) => blocks
                        .iter()
                        .filter_map(|c| match c {
                            Content::Text(t) => Some(t.text.to_string()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join(""),
                };
                if !content.is_empty() {
                    parts.push(format!("[User]: {content}"));
                }
            }
            Message::Assistant(assistant) => {
                let mut text_parts: Vec<String> = Vec::new();
                let mut thinking_parts: Vec<String> = Vec::new();
                let mut tool_calls: Vec<String> = Vec::new();

                for block in &assistant.content {
                    match block {
                        Content::Text(t) => text_parts.push(t.text.to_string()),
                        Content::Thinking(t) => thinking_parts.push(t.thinking.to_string()),
                        Content::ToolCall(call) => {
                            let mut keys: Vec<&String> = call.arguments.keys().collect();
                            keys.sort();
                            let args_str = keys
                                .iter()
                                .map(|k| {
                                    let value = serde_json::to_string(&call.arguments[*k])
                                        .unwrap_or_else(|_| "null".to_string());
                                    format!("{k}={value}")
                                })
                                .collect::<Vec<_>>()
                                .join(", ");
                            tool_calls.push(format!("{}({args_str})", call.name));
                        }
                        Content::Image(_) => {}
                    }
                }

                if !thinking_parts.is_empty() {
                    parts.push(format!(
                        "[Assistant thinking]: {}",
                        thinking_parts.join("\n")
                    ));
                }
                if !text_parts.is_empty() {
                    parts.push(format!("[Assistant]: {}", text_parts.join("\n")));
                }
                if !tool_calls.is_empty() {
                    parts.push(format!("[Assistant tool calls]: {}", tool_calls.join("; ")));
                }
            }
            Message::ToolResult(result) => {
                let content = result
                    .content
                    .iter()
                    .filter_map(|c| match c {
                        Content::Text(t) => Some(t.text.to_string()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("");
                if !content.is_empty() {
                    parts.push(format!(
                        "[Tool result]: {}",
                        truncate_for_summary(&content, TOOL_RESULT_MAX_CHARS)
                    ));
                }
            }
        }
    }

    parts.join("\n\n")
}

// Verbatim summarization prompts (utils.ts / compaction.ts).

pub const SUMMARIZATION_SYSTEM_PROMPT: &str = "You are a context summarization assistant. Your task is to read a conversation between a user and an AI assistant, then produce a structured summary following the exact format specified.\n\nDo NOT continue the conversation. Do NOT respond to any questions in the conversation. ONLY output the structured summary.";

const SUMMARIZATION_PROMPT: &str = r#"The messages above are a conversation to summarize. Create a structured context checkpoint summary that another LLM will use to continue the work.

Use this EXACT format:

## Goal
[What is the user trying to accomplish? Can be multiple items if the session covers different tasks.]

## Constraints & Preferences
- [Any constraints, preferences, or requirements mentioned by user]
- [Or "(none)" if none were mentioned]

## Progress
### Done
- [x] [Completed tasks/changes]

### In Progress
- [ ] [Current work]

### Blocked
- [Issues preventing progress, if any]

## Key Decisions
- **[Decision]**: [Brief rationale]

## Next Steps
1. [Ordered list of what should happen next]

## Critical Context
- [Any data, examples, or references needed to continue]
- [Or "(none)" if not applicable]

Keep each section concise. Preserve exact file paths, function names, and error messages."#;

const UPDATE_SUMMARIZATION_PROMPT: &str = r#"The messages above are NEW conversation messages to incorporate into the existing summary provided in <previous-summary> tags.

Update the existing structured summary with new information. RULES:
- PRESERVE all existing information from the previous summary
- ADD new progress, decisions, and context from the new messages
- UPDATE the Progress section: move items from "In Progress" to "Done" when completed
- UPDATE "Next Steps" based on what was accomplished
- PRESERVE exact file paths, function names, and error messages
- If something is no longer relevant, you may remove it

Use this EXACT format:

## Goal
[Preserve existing goals, add new ones if the task expanded]

## Constraints & Preferences
- [Preserve existing, add new ones discovered]

## Progress
### Done
- [x] [Include previously done items AND newly completed items]

### In Progress
- [ ] [Current work - update based on progress]

### Blocked
- [Current blockers - remove if resolved]

## Key Decisions
- **[Decision]**: [Brief rationale] (preserve all previous, add new)

## Next Steps
1. [Update based on current state]

## Critical Context
- [Preserve important context, add new if needed]

Keep each section concise. Preserve exact file paths, function names, and error messages."#;

const TURN_PREFIX_SUMMARIZATION_PROMPT: &str = r#"This is the PREFIX of a turn that was too large to keep. The SUFFIX (recent work) is retained.

Summarize the prefix to provide context for the retained suffix:

## Original Request
[What did the user ask for in this turn?]

## Early Progress
- [Key decisions and work done in the prefix]

## Context for Suffix
- [Information needed to understand the retained recent work]

Be concise. Focus on what's needed to understand the kept suffix."#;

// ----------------------------------------------------------------------------
// Cut point detection
// ----------------------------------------------------------------------------

fn value_to_agent_message(value: Value) -> Option<AgentMessage> {
    serde_json::from_value::<AgentMessage>(value).ok()
}

/// Context-visible messages for an entry, as typed [`AgentMessage`]s.
fn entry_context_messages(entry: &crate::session_types::SessionEntry) -> Vec<AgentMessage> {
    crate::session_manager::session_entry_to_context_messages(entry)
        .into_iter()
        .filter_map(value_to_agent_message)
        .collect()
}

/// Oracle `getMessageFromEntryForCompaction`.
fn message_from_entry_for_compaction(
    entry: &crate::session_types::SessionEntry,
) -> Option<AgentMessage> {
    if matches!(entry, crate::session_types::SessionEntry::Compaction { .. }) {
        return None;
    }
    entry_context_messages(entry).into_iter().next()
}

/// Oracle `isCutPointMessage`.
fn is_cut_point_message(message: &AgentMessage) -> bool {
    matches!(
        message.role(),
        "user" | "assistant" | "bashExecution" | "custom" | "branchSummary" | "compactionSummary"
    )
}

/// Oracle `isTurnStartMessage`.
fn is_turn_start_message(message: &AgentMessage) -> bool {
    matches!(
        message.role(),
        "user" | "bashExecution" | "custom" | "branchSummary" | "compactionSummary"
    )
}

fn is_turn_start_entry(entry: &crate::session_types::SessionEntry) -> bool {
    if matches!(entry, crate::session_types::SessionEntry::Compaction { .. }) {
        return false;
    }
    entry_context_messages(entry)
        .iter()
        .any(is_turn_start_message)
}

/// Oracle `findValidCutPoints`.
fn find_valid_cut_points(
    entries: &[crate::session_types::SessionEntry],
    start_index: usize,
    end_index: usize,
) -> Vec<usize> {
    let mut cut_points = Vec::new();
    for (i, entry) in entries.iter().enumerate().take(end_index).skip(start_index) {
        if matches!(entry, crate::session_types::SessionEntry::Compaction { .. }) {
            continue;
        }
        if entry_context_messages(entry)
            .iter()
            .any(is_cut_point_message)
        {
            cut_points.push(i);
        }
    }
    cut_points
}

/// Oracle `findTurnStartIndex` (None = -1).
pub fn find_turn_start_index(
    entries: &[crate::session_types::SessionEntry],
    entry_index: usize,
    start_index: usize,
) -> Option<usize> {
    let mut i = entry_index;
    loop {
        if is_turn_start_entry(&entries[i]) {
            return Some(i);
        }
        if i == start_index {
            return None;
        }
        i -= 1;
    }
}

/// Oracle `CutPointResult`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CutPointResult {
    pub first_kept_entry_index: usize,
    pub turn_start_index: Option<usize>,
    pub is_split_turn: bool,
}

/// Oracle `findCutPoint`: keep ~`keep_recent_tokens` newest tokens; never cut
/// at tool results.
pub fn find_cut_point(
    entries: &[crate::session_types::SessionEntry],
    start_index: usize,
    end_index: usize,
    keep_recent_tokens: u64,
) -> CutPointResult {
    let cut_points = find_valid_cut_points(entries, start_index, end_index);
    if cut_points.is_empty() {
        return CutPointResult {
            first_kept_entry_index: start_index,
            turn_start_index: None,
            is_split_turn: false,
        };
    }

    let mut accumulated_tokens: u64 = 0;
    let mut cut_index = cut_points[0];

    let mut i = end_index;
    while i > start_index {
        i -= 1;
        let entry = &entries[i];
        let message_tokens: u64 = entry_context_messages(entry)
            .iter()
            .map(estimate_tokens)
            .sum();
        if message_tokens == 0 {
            continue;
        }
        accumulated_tokens += message_tokens;
        if accumulated_tokens >= keep_recent_tokens {
            for c in &cut_points {
                if *c >= i {
                    cut_index = *c;
                    break;
                }
            }
            break;
        }
    }

    // Include adjacent metadata entries that do not affect context.
    while cut_index > start_index {
        let prev_entry = &entries[cut_index - 1];
        if matches!(
            prev_entry,
            crate::session_types::SessionEntry::Compaction { .. }
        ) || !crate::session_manager::session_entry_to_context_messages(prev_entry).is_empty()
        {
            break;
        }
        cut_index -= 1;
    }

    let starts_turn = is_turn_start_entry(&entries[cut_index]);
    let turn_start_index = if starts_turn {
        None
    } else {
        find_turn_start_index(entries, cut_index, start_index)
    };

    CutPointResult {
        first_kept_entry_index: cut_index,
        turn_start_index,
        is_split_turn: !starts_turn && turn_start_index.is_some(),
    }
}

// ----------------------------------------------------------------------------
// Compaction preparation + LLM summarization
// ----------------------------------------------------------------------------

/// Oracle `CompactionPreparation`.
#[derive(Clone, Debug)]
pub struct CompactionPreparation {
    pub first_kept_entry_id: String,
    pub messages_to_summarize: Vec<AgentMessage>,
    pub turn_prefix_messages: Vec<AgentMessage>,
    pub is_split_turn: bool,
    pub tokens_before: u64,
    pub previous_summary: Option<String>,
    pub file_ops: FileOperations,
    pub settings: CompactionSettings,
}

/// Serialize a [`CompactionPreparation`] with the oracle's camelCase field
/// names (compaction.ts:615-631) for the `session_before_compact` wire event.
/// pi hands handlers real `Set` objects for `fileOps`; over the wire those
/// become sorted arrays (the only JSON encoding of a set).
pub fn preparation_to_value(preparation: &CompactionPreparation) -> Value {
    let mut value = serde_json::Map::new();
    value.insert(
        "firstKeptEntryId".into(),
        Value::String(preparation.first_kept_entry_id.clone()),
    );
    value.insert(
        "messagesToSummarize".into(),
        serde_json::to_value(&preparation.messages_to_summarize).unwrap_or(Value::Array(vec![])),
    );
    value.insert(
        "turnPrefixMessages".into(),
        serde_json::to_value(&preparation.turn_prefix_messages).unwrap_or(Value::Array(vec![])),
    );
    value.insert("isSplitTurn".into(), Value::Bool(preparation.is_split_turn));
    value.insert("tokensBefore".into(), preparation.tokens_before.into());
    if let Some(previous_summary) = &preparation.previous_summary {
        value.insert(
            "previousSummary".into(),
            Value::String(previous_summary.clone()),
        );
    }
    let set_to_value = |set: &std::collections::BTreeSet<String>| {
        Value::Array(set.iter().cloned().map(Value::String).collect())
    };
    value.insert(
        "fileOps".into(),
        serde_json::json!({
            "read": set_to_value(&preparation.file_ops.read),
            "written": set_to_value(&preparation.file_ops.written),
            "edited": set_to_value(&preparation.file_ops.edited),
        }),
    );
    value.insert(
        "settings".into(),
        serde_json::json!({
            "enabled": preparation.settings.enabled,
            "reserveTokens": preparation.settings.reserve_tokens,
            "keepRecentTokens": preparation.settings.keep_recent_tokens,
        }),
    );
    Value::Object(value)
}

/// Serialize session entries for wire transport (byte-compatible field order
/// comes from the `SessionEntry` serde impls).
pub fn entries_to_values(entries: &[crate::session_types::SessionEntry]) -> Vec<Value> {
    entries
        .iter()
        .filter_map(|entry| serde_json::to_value(entry).ok())
        .collect()
}

/// Oracle `prepareCompaction` (compaction.ts:559-641).
pub fn prepare_compaction(
    path_entries: &[crate::session_types::SessionEntry],
    settings: CompactionSettings,
) -> Option<CompactionPreparation> {
    use crate::session_types::SessionEntry;

    if matches!(path_entries.last(), Some(SessionEntry::Compaction { .. })) {
        return None;
    }

    let prev_compaction_index = path_entries
        .iter()
        .rposition(|e| matches!(e, SessionEntry::Compaction { .. }));

    let mut previous_summary: Option<String> = None;
    let mut boundary_start = 0usize;
    if let Some(prev_index) = prev_compaction_index
        && let SessionEntry::Compaction {
            summary,
            first_kept_entry_id,
            ..
        } = &path_entries[prev_index]
    {
        previous_summary = Some(summary.clone());
        let first_kept_index = first_kept_entry_id.as_ref().and_then(|id| {
            path_entries
                .iter()
                .position(|e| e.id() == Some(id.as_str()))
        });
        boundary_start = first_kept_index.unwrap_or(prev_index + 1);
    }
    let boundary_end = path_entries.len();

    let context = crate::session_manager::build_session_context(path_entries, None, None);
    let context_messages: Vec<AgentMessage> = context
        .messages
        .into_iter()
        .filter_map(value_to_agent_message)
        .collect();
    let tokens_before = estimate_context_tokens(&context_messages).tokens;

    let cut_point = find_cut_point(
        path_entries,
        boundary_start,
        boundary_end,
        settings.keep_recent_tokens,
    );

    let first_kept_entry_id = path_entries
        .get(cut_point.first_kept_entry_index)
        .and_then(|e| e.id())?
        .to_string();

    let history_end = if cut_point.is_split_turn {
        cut_point
            .turn_start_index
            .unwrap_or(cut_point.first_kept_entry_index)
    } else {
        cut_point.first_kept_entry_index
    };

    let mut messages_to_summarize: Vec<AgentMessage> = Vec::new();
    for entry in path_entries.iter().take(history_end).skip(boundary_start) {
        if let Some(message) = message_from_entry_for_compaction(entry) {
            messages_to_summarize.push(message);
        }
    }

    let mut turn_prefix_messages: Vec<AgentMessage> = Vec::new();
    if cut_point.is_split_turn
        && let Some(turn_start) = cut_point.turn_start_index
    {
        for entry in path_entries
            .iter()
            .take(cut_point.first_kept_entry_index)
            .skip(turn_start)
        {
            if let Some(message) = message_from_entry_for_compaction(entry) {
                turn_prefix_messages.push(message);
            }
        }
    }

    if messages_to_summarize.is_empty() && turn_prefix_messages.is_empty() {
        return None;
    }

    // Extract file operations from messages and previous compaction.
    let mut file_ops = FileOperations::default();
    if let Some(prev_index) = prev_compaction_index
        && let SessionEntry::Compaction {
            details, from_hook, ..
        } = &path_entries[prev_index]
        && !from_hook.unwrap_or(false)
        && let Some(details) = details
    {
        if let Some(read_files) = details.get("readFiles").and_then(Value::as_array) {
            for f in read_files.iter().filter_map(Value::as_str) {
                file_ops.read.insert(f.to_string());
            }
        }
        if let Some(modified) = details.get("modifiedFiles").and_then(Value::as_array) {
            for f in modified.iter().filter_map(Value::as_str) {
                file_ops.edited.insert(f.to_string());
            }
        }
    }
    for message in &messages_to_summarize {
        extract_file_ops_from_message(message, &mut file_ops);
    }
    if cut_point.is_split_turn {
        for message in &turn_prefix_messages {
            extract_file_ops_from_message(message, &mut file_ops);
        }
    }

    Some(CompactionPreparation {
        first_kept_entry_id,
        messages_to_summarize,
        turn_prefix_messages,
        is_split_turn: cut_point.is_split_turn,
        tokens_before,
        previous_summary,
        file_ops,
        settings,
    })
}

/// Cancellation error marker used by compaction paths.
const COMPACTION_CANCELLED: &str = "Compaction cancelled";

async fn await_result_with_cancel(
    stream: pi_ai::AssistantMessageEventStream,
    cancel: &CancellationToken,
) -> Result<AssistantMessage, String> {
    let result = stream.result();
    tokio::pin!(result);
    loop {
        if cancel.is_cancelled() {
            return Err(COMPACTION_CANCELLED.to_string());
        }
        match tokio::time::timeout(std::time::Duration::from_millis(25), &mut result).await {
            Ok(message) => return Ok(message),
            Err(_) => continue,
        }
    }
}

struct SummarizationRequest<'a> {
    stream_fn: &'a StreamFn,
    model: &'a Model,
    thinking_level: AgentThinkingLevel,
    cancel: &'a CancellationToken,
}

impl SummarizationRequest<'_> {
    /// Oracle `createSummarizationOptions` + `completeSummarization`.
    async fn complete(
        &self,
        prompt_text: String,
        max_tokens: u64,
    ) -> Result<AssistantMessage, String> {
        let messages = vec![Message::User(UserMessage {
            content: UserContent::Blocks(vec![Content::Text(TextContent {
                text: prompt_text.into(),
                text_signature: None,
            })]),
            timestamp: now_ms(),
        })];
        let context = pi_ai::Context {
            messages,
            tools: Vec::new(),
            system_prompt: Some(SUMMARIZATION_SYSTEM_PROMPT.to_string()),
        };
        let mut options = pi_agent::StreamCallOptions {
            max_tokens: Some(max_tokens),
            cancel: Some(self.cancel.clone()),
            ..Default::default()
        };
        if self.model.reasoning && self.thinking_level != AgentThinkingLevel::Off {
            options.reasoning = Option::<pi_ai::ThinkingLevel>::from(self.thinking_level);
        }
        let stream = (self.stream_fn)(self.model.clone(), context, options).await;
        await_result_with_cancel(stream, self.cancel).await
    }
}

fn summarization_max_tokens(reserve_tokens: u64, model_max_tokens: u64, fraction: f64) -> u64 {
    let budget = (fraction * reserve_tokens as f64).floor() as u64;
    if model_max_tokens > 0 {
        budget.min(model_max_tokens)
    } else {
        budget
    }
}

/// Oracle `generateSummary`.
async fn generate_summary(
    request: &SummarizationRequest<'_>,
    current_messages: &[AgentMessage],
    reserve_tokens: u64,
    custom_instructions: Option<&str>,
    previous_summary: Option<&str>,
) -> Result<String, String> {
    let max_tokens = summarization_max_tokens(reserve_tokens, request.model.max_tokens, 0.8);

    let mut base_prompt = if previous_summary.is_some() {
        UPDATE_SUMMARIZATION_PROMPT.to_string()
    } else {
        SUMMARIZATION_PROMPT.to_string()
    };
    if let Some(ci) = custom_instructions {
        base_prompt = format!("{base_prompt}\n\nAdditional focus: {ci}");
    }

    let llm_messages = convert_to_llm(current_messages.to_vec());
    let conversation_text = serialize_conversation(&llm_messages);

    let mut prompt_text = format!("<conversation>\n{conversation_text}\n</conversation>\n\n");
    if let Some(previous) = previous_summary {
        prompt_text.push_str(&format!(
            "<previous-summary>\n{previous}\n</previous-summary>\n\n"
        ));
    }
    prompt_text.push_str(&base_prompt);

    let response = request.complete(prompt_text, max_tokens).await?;
    if response.stop_reason == StopReason::Error {
        return Err(format!(
            "Summarization failed: {}",
            response.error_message.as_deref().unwrap_or("Unknown error")
        ));
    }

    Ok(response
        .content
        .iter()
        .filter_map(|c| match c {
            Content::Text(t) => Some(t.text.to_string()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n"))
}

/// Oracle `generateTurnPrefixSummary`.
async fn generate_turn_prefix_summary(
    request: &SummarizationRequest<'_>,
    messages: &[AgentMessage],
    reserve_tokens: u64,
) -> Result<String, String> {
    let max_tokens = summarization_max_tokens(reserve_tokens, request.model.max_tokens, 0.5);
    let llm_messages = convert_to_llm(messages.to_vec());
    let conversation_text = serialize_conversation(&llm_messages);
    let prompt_text = format!(
        "<conversation>\n{conversation_text}\n</conversation>\n\n{TURN_PREFIX_SUMMARIZATION_PROMPT}"
    );
    let response = request.complete(prompt_text, max_tokens).await?;
    if response.stop_reason == StopReason::Error {
        return Err(format!(
            "Turn prefix summarization failed: {}",
            response.error_message.as_deref().unwrap_or("Unknown error")
        ));
    }
    Ok(response
        .content
        .iter()
        .filter_map(|c| match c {
            Content::Text(t) => Some(t.text.to_string()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n"))
}

/// Oracle `compact` (compaction.ts:657-742): generate summaries + file lists.
async fn compact_prepared(
    request: &SummarizationRequest<'_>,
    preparation: &CompactionPreparation,
    custom_instructions: Option<&str>,
) -> Result<CompactionResult, String> {
    let settings = preparation.settings;
    let summary = if preparation.is_split_turn && !preparation.turn_prefix_messages.is_empty() {
        let history_result = if !preparation.messages_to_summarize.is_empty() {
            generate_summary(
                request,
                &preparation.messages_to_summarize,
                settings.reserve_tokens,
                custom_instructions,
                preparation.previous_summary.as_deref(),
            )
            .await?
        } else {
            "No prior history.".to_string()
        };
        let turn_prefix_result = generate_turn_prefix_summary(
            request,
            &preparation.turn_prefix_messages,
            settings.reserve_tokens,
        )
        .await?;
        format!("{history_result}\n\n---\n\n**Turn Context (split turn):**\n\n{turn_prefix_result}")
    } else {
        generate_summary(
            request,
            &preparation.messages_to_summarize,
            settings.reserve_tokens,
            custom_instructions,
            preparation.previous_summary.as_deref(),
        )
        .await?
    };

    let (read_files, modified_files) = compute_file_lists(&preparation.file_ops);
    let summary = format!(
        "{summary}{}",
        format_file_operations(&read_files, &modified_files)
    );

    Ok(CompactionResult {
        summary,
        first_kept_entry_id: preparation.first_kept_entry_id.clone(),
        tokens_before: preparation.tokens_before,
        estimated_tokens_after: None,
        details: Some(json!({
            "readFiles": read_files,
            "modifiedFiles": modified_files,
        })),
    })
}

fn timestamp_millis(timestamp: &str) -> i64 {
    timestamp
        .parse::<jiff::Timestamp>()
        .map(|t| t.as_millisecond())
        .unwrap_or(0)
}

// ============================================================================
// AgentSession compaction methods
// ============================================================================

impl AgentSession {
    /// Manually compact the session context (oracle `compact`).
    pub async fn compact(
        &self,
        custom_instructions: Option<String>,
    ) -> Result<CompactionResult, String> {
        self.abort().await;
        let cancel = CancellationToken::new();
        self.inner.state.lock().compaction_cancel = Some(cancel.clone());
        self.inner.emit(&AgentSessionEvent::CompactionStart {
            reason: CompactionReason::Manual,
        });

        let result = self
            .compact_body(&cancel, custom_instructions.as_deref())
            .await;
        self.inner.state.lock().compaction_cancel = None;

        match result {
            Ok(compaction_result) => {
                self.inner.emit(&AgentSessionEvent::CompactionEnd {
                    reason: CompactionReason::Manual,
                    result: Some(compaction_result.clone()),
                    aborted: false,
                    will_retry: false,
                    error_message: None,
                });
                Ok(compaction_result)
            }
            Err(message) => {
                let aborted = message == COMPACTION_CANCELLED;
                self.inner.emit(&AgentSessionEvent::CompactionEnd {
                    reason: CompactionReason::Manual,
                    result: None,
                    aborted,
                    will_retry: false,
                    error_message: if aborted {
                        None
                    } else {
                        Some(format!("Compaction failed: {message}"))
                    },
                });
                Err(message)
            }
        }
    }

    async fn compact_body(
        &self,
        cancel: &CancellationToken,
        custom_instructions: Option<&str>,
    ) -> Result<CompactionResult, String> {
        let model = self.model().ok_or_else(format_no_model_selected_message)?;

        let (path_entries, settings) = {
            let state = self.inner.state.lock();
            (
                state.session_manager.get_branch(None),
                compaction_settings(&self.inner.settings),
            )
        };

        let preparation = match prepare_compaction(&path_entries, settings) {
            Some(preparation) => preparation,
            None => {
                if matches!(
                    path_entries.last(),
                    Some(crate::session_types::SessionEntry::Compaction { .. })
                ) {
                    return Err("Already compacted".to_string());
                }
                return Err("Nothing to compact (session too small)".to_string());
            }
        };

        // Phase 6 hook: oracle emits `session_before_compact` after
        // preparation, before summarization (agent-session.ts:1765).
        let hooks = self.inner.compact_hooks.lock().clone();
        let mut from_extension = false;
        let mut extension_compaction: Option<crate::extension_bridge::CompactionOverride> = None;
        if let Some(hooks) = &hooks {
            let decision = hooks
                .session_before_compact(
                    preparation_to_value(&preparation),
                    entries_to_values(&path_entries),
                    custom_instructions.map(str::to_string),
                    CompactionReason::Manual,
                    false,
                    cancel.clone(),
                )
                .await;
            match decision {
                crate::extension_bridge::BeforeCompactDecision::Proceed => {}
                crate::extension_bridge::BeforeCompactDecision::Cancel => {
                    return Err(COMPACTION_CANCELLED.to_string());
                }
                crate::extension_bridge::BeforeCompactDecision::Replace(over) => {
                    from_extension = true;
                    extension_compaction = Some(over);
                }
            }
        }

        let mut result = match extension_compaction {
            Some(over) => CompactionResult {
                summary: over.summary,
                first_kept_entry_id: over.first_kept_entry_id,
                tokens_before: over.tokens_before,
                estimated_tokens_after: None,
                details: over.details,
            },
            None => {
                let thinking_level = self.thinking_level();
                let request = SummarizationRequest {
                    stream_fn: &self.inner.stream_fn,
                    model: &model,
                    thinking_level,
                    cancel,
                };
                compact_prepared(&request, &preparation, custom_instructions).await?
            }
        };

        if cancel.is_cancelled() {
            return Err(COMPACTION_CANCELLED.to_string());
        }

        let (estimated_tokens_after, compaction_entry) = {
            let mut state = self.inner.state.lock();
            let entry_id = state
                .session_manager
                .append_compaction(
                    result.summary.clone(),
                    result.first_kept_entry_id.clone(),
                    result.tokens_before,
                    result.details.clone(),
                    Some(from_extension),
                )
                .ok();
            let compaction_entry = entry_id
                .and_then(|id| state.session_manager.get_entry(&id).cloned())
                .and_then(|entry| serde_json::to_value(entry).ok());
            let session_context = state.session_manager.build_session_context();
            state.messages = session_context
                .messages
                .into_iter()
                .filter_map(value_to_agent_message)
                .collect();
            (
                estimate_context_tokens(&state.messages).tokens,
                compaction_entry,
            )
        };
        result.estimated_tokens_after = Some(estimated_tokens_after);

        // Oracle emits `session_compact` before compact() resolves
        // (agent-session.ts:1832); pending ctx.compact() callbacks in the
        // sidecar settle off this event.
        if let (Some(hooks), Some(entry)) = (&hooks, compaction_entry) {
            hooks
                .session_compact(entry, from_extension, CompactionReason::Manual, false)
                .await;
        }
        Ok(result)
    }

    /// Oracle `_checkCompaction`: overflow + threshold auto-compaction.
    async fn check_compaction(
        &self,
        assistant_message: &AssistantMessage,
        skip_aborted_check: bool,
    ) -> bool {
        let settings = compaction_settings(&self.inner.settings);
        if !settings.enabled {
            return false;
        }
        if skip_aborted_check && assistant_message.stop_reason == StopReason::Aborted {
            return false;
        }

        let model = self.inner.state.lock().model.clone();
        let context_window = model.as_ref().map_or(0, |m| m.context_window);

        // Skip overflow check if the message came from a different model.
        let same_model = model.as_ref().is_some_and(|m| {
            assistant_message.provider == m.provider && assistant_message.model == m.id
        });

        // Skip checks if this assistant message is older than the latest
        // compaction boundary.
        let compaction_timestamp = {
            let state = self.inner.state.lock();
            let branch = state.session_manager.get_branch(None);
            latest_compaction_index(&branch).map(|i| timestamp_millis(branch[i].timestamp()))
        };
        if let Some(compaction_ms) = compaction_timestamp
            && assistant_message.timestamp <= compaction_ms
        {
            return false;
        }

        // Case 1: overflow.
        if same_model && is_context_overflow(assistant_message, Some(context_window)) {
            let will_retry = assistant_message.stop_reason != StopReason::Stop;

            if !will_retry {
                return self
                    .run_auto_compaction(CompactionReason::Overflow, false)
                    .await;
            }

            let already_attempted = {
                let state = self.inner.state.lock();
                state.overflow_recovery_attempted
            };
            if already_attempted {
                self.inner.emit(&AgentSessionEvent::CompactionEnd {
                    reason: CompactionReason::Overflow,
                    result: None,
                    aborted: false,
                    will_retry: false,
                    error_message: Some(
                        "Context overflow recovery failed after one compact-and-retry attempt. Try reducing context or switching to a larger-context model."
                            .to_string(),
                    ),
                });
                return false;
            }

            {
                let mut state = self.inner.state.lock();
                state.overflow_recovery_attempted = true;
                // Remove the error message from agent state (kept in session).
                if state
                    .messages
                    .last()
                    .is_some_and(|m| m.role() == "assistant")
                {
                    state.messages.pop();
                }
            }
            return self
                .run_auto_compaction(CompactionReason::Overflow, will_retry)
                .await;
        }

        // Case 2: threshold.
        let direct_context_tokens = calculate_context_tokens(&assistant_message.usage);
        let context_tokens =
            if assistant_message.stop_reason == StopReason::Error || direct_context_tokens == 0 {
                let state = self.inner.state.lock();
                let estimate = estimate_context_tokens(&state.messages);
                let Some(last_usage_index) = estimate.last_usage_index else {
                    return false; // No usage data at all.
                };
                // Verify the usage source is post-compaction.
                if let Some(compaction_ms) = compaction_timestamp
                    && let Some(AgentMessage::Standard(Message::Assistant(usage_msg))) =
                        state.messages.get(last_usage_index)
                    && usage_msg.timestamp <= compaction_ms
                {
                    return false;
                }
                estimate.tokens
            } else {
                direct_context_tokens
            };

        if should_compact(
            context_tokens,
            context_window,
            settings.enabled,
            settings.reserve_tokens,
        ) {
            return self
                .run_auto_compaction(CompactionReason::Threshold, false)
                .await;
        }
        false
    }

    /// Oracle `_runAutoCompaction`.
    async fn run_auto_compaction(&self, reason: CompactionReason, will_retry: bool) -> bool {
        let Some(model) = self.model() else {
            return false;
        };

        // Oracle declines auto-compaction silently when no auth resolves.
        {
            let registry = self.inner.registry.read().await;
            let auth = registry.get_api_key_and_headers(&model).await;
            if !auth.ok {
                return false;
            }
        }

        let (path_entries, settings) = {
            let state = self.inner.state.lock();
            (
                state.session_manager.get_branch(None),
                compaction_settings(&self.inner.settings),
            )
        };
        let Some(preparation) = prepare_compaction(&path_entries, settings) else {
            return false;
        };

        self.inner
            .emit(&AgentSessionEvent::CompactionStart { reason });
        let cancel = CancellationToken::new();
        self.inner.state.lock().auto_compaction_cancel = Some(cancel.clone());

        // Phase 6 hook (oracle agent-session.ts:2032): after compaction_start
        // and the abort controller, before summarization.
        let hooks = self.inner.compact_hooks.lock().clone();
        let mut from_extension = false;
        let mut extension_compaction: Option<crate::extension_bridge::CompactionOverride> = None;
        if let Some(hooks) = &hooks {
            let decision = hooks
                .session_before_compact(
                    preparation_to_value(&preparation),
                    entries_to_values(&path_entries),
                    None,
                    reason,
                    will_retry,
                    cancel.clone(),
                )
                .await;
            match decision {
                crate::extension_bridge::BeforeCompactDecision::Proceed => {}
                crate::extension_bridge::BeforeCompactDecision::Cancel => {
                    // Oracle: a cancelling handler aborts silently
                    // (agent-session.ts:2043-2052).
                    self.inner.state.lock().auto_compaction_cancel = None;
                    self.inner.emit(&AgentSessionEvent::CompactionEnd {
                        reason,
                        result: None,
                        aborted: true,
                        will_retry: false,
                        error_message: None,
                    });
                    return false;
                }
                crate::extension_bridge::BeforeCompactDecision::Replace(over) => {
                    from_extension = true;
                    extension_compaction = Some(over);
                }
            }
        }

        let compact_result = match extension_compaction {
            Some(over) => Ok(CompactionResult {
                summary: over.summary,
                first_kept_entry_id: over.first_kept_entry_id,
                tokens_before: over.tokens_before,
                estimated_tokens_after: None,
                details: over.details,
            }),
            None => {
                let thinking_level = self.thinking_level();
                let request = SummarizationRequest {
                    stream_fn: &self.inner.stream_fn,
                    model: &model,
                    thinking_level,
                    cancel: &cancel,
                };
                compact_prepared(&request, &preparation, None).await
            }
        };
        self.inner.state.lock().auto_compaction_cancel = None;

        let mut result = match compact_result {
            Ok(result) => result,
            Err(message) => {
                if message == COMPACTION_CANCELLED {
                    self.inner.emit(&AgentSessionEvent::CompactionEnd {
                        reason,
                        result: None,
                        aborted: true,
                        will_retry: false,
                        error_message: None,
                    });
                } else {
                    let error_message = match reason {
                        CompactionReason::Overflow => {
                            format!("Context overflow recovery failed: {message}")
                        }
                        _ => format!("Auto-compaction failed: {message}"),
                    };
                    self.inner.emit(&AgentSessionEvent::CompactionEnd {
                        reason,
                        result: None,
                        aborted: false,
                        will_retry: false,
                        error_message: Some(error_message),
                    });
                }
                return false;
            }
        };

        if cancel.is_cancelled() {
            self.inner.emit(&AgentSessionEvent::CompactionEnd {
                reason,
                result: None,
                aborted: true,
                will_retry: false,
                error_message: None,
            });
            return false;
        }

        let (estimated_tokens_after, compaction_entry) = {
            let mut state = self.inner.state.lock();
            let entry_id = state
                .session_manager
                .append_compaction(
                    result.summary.clone(),
                    result.first_kept_entry_id.clone(),
                    result.tokens_before,
                    result.details.clone(),
                    Some(from_extension),
                )
                .ok();
            let compaction_entry = entry_id
                .and_then(|id| state.session_manager.get_entry(&id).cloned())
                .and_then(|entry| serde_json::to_value(entry).ok());
            let session_context = state.session_manager.build_session_context();
            state.messages = session_context
                .messages
                .into_iter()
                .filter_map(value_to_agent_message)
                .collect();
            (
                estimate_context_tokens(&state.messages).tokens,
                compaction_entry,
            )
        };
        result.estimated_tokens_after = Some(estimated_tokens_after);

        // Oracle emits `session_compact` before compaction_end
        // (agent-session.ts:2112-2119).
        if let (Some(hooks), Some(entry)) = (&hooks, compaction_entry) {
            hooks
                .session_compact(entry, from_extension, reason, will_retry)
                .await;
        }
        self.inner.emit(&AgentSessionEvent::CompactionEnd {
            reason,
            result: Some(result),
            aborted: false,
            will_retry,
            error_message: None,
        });

        if will_retry {
            let mut state = self.inner.state.lock();
            if let Some(AgentMessage::Standard(Message::Assistant(assistant))) =
                state.messages.last()
                && assistant.stop_reason == StopReason::Error
            {
                state.messages.pop();
            }
            return true;
        }

        // Continue once so queued messages are delivered.
        let state = self.inner.state.lock();
        !state.steering_queue.is_empty() || !state.follow_up_queue.is_empty()
    }

    pub fn abort_compaction(&self) {
        let (a, b) = {
            let state = self.inner.state.lock();
            (
                state.compaction_cancel.clone(),
                state.auto_compaction_cancel.clone(),
            )
        };
        if let Some(cancel) = a {
            cancel.cancel();
        }
        if let Some(cancel) = b {
            cancel.cancel();
        }
    }

    pub fn abort_branch_summary(&self) {
        let cancel = self.inner.state.lock().branch_summary_cancel.clone();
        if let Some(cancel) = cancel {
            cancel.cancel();
        }
    }

    pub fn abort_bash(&self) {
        let cancel = self.inner.state.lock().bash_cancel.clone();
        if let Some(cancel) = cancel {
            cancel.cancel();
        }
    }
}

// ============================================================================
// Bash execution (core/bash-executor.ts + utils/{ansi,shell}.ts)
// ============================================================================

static ANSI_REGEX: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
    // Port of pi's ansi-regex derivative (utils/ansi.ts): OSC sequences up to
    // the first string terminator, plus CSI/C1 sequences.
    let st = "(?:\\x07|\\x1B\\x5C|\\u{9C})";
    let osc = format!("(?:\\x1B\\][\\s\\S]*?{st})");
    let csi =
        "[\\x1B\\u{9B}][\\[\\]()#;?]*(?:[0-9]{1,4}(?:[;:][0-9]{0,4})*)?[0-9A-PR-TZcf-nq-uy=><~]";
    regex::RegexBuilder::new(&format!("{osc}|{csi}"))
        .build()
        .expect("ansi regex")
});

/// Oracle `stripAnsi`.
pub fn strip_ansi(value: &str) -> String {
    if !value.contains('\u{1B}') && !value.contains('\u{9B}') {
        return value.to_string();
    }
    ANSI_REGEX.replace_all(value, "").into_owned()
}

/// Oracle `sanitizeBinaryOutput` (utils/shell.ts).
pub fn sanitize_binary_output(value: &str) -> String {
    value
        .chars()
        .filter(|ch| {
            let code = *ch as u32;
            if code == 0x09 || code == 0x0a || code == 0x0d {
                return true;
            }
            if code <= 0x1f {
                return false;
            }
            if (0xfff9..=0xfffb).contains(&code) {
                return false;
            }
            true
        })
        .collect()
}

/// Resolve the shell binary (oracle `getShellConfig`, unix paths).
fn resolve_shell(custom_shell_path: Option<&str>) -> Result<String, String> {
    if let Some(custom) = custom_shell_path {
        if std::path::Path::new(custom).exists() {
            return Ok(custom.to_string());
        }
        return Err(format!("Custom shell path not found: {custom}"));
    }
    if std::path::Path::new("/bin/bash").exists() {
        return Ok("/bin/bash".to_string());
    }
    Ok("sh".to_string())
}

const BASH_MAX_LINES: usize = 2000;
const BASH_MAX_BYTES: usize = 50 * 1024;

/// Oracle `executeBashWithOperations` with local operations.
async fn execute_bash_impl(
    command: &str,
    shell: &str,
    cwd: &std::path::Path,
    on_chunk: Option<BashChunkCallback>,
    cancel: CancellationToken,
) -> Result<BashResult, String> {
    use std::io::Write as _;
    use tokio::io::AsyncReadExt as _;

    let mut cmd = tokio::process::Command::new(shell);
    cmd.arg("-c")
        .arg(command)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    #[cfg(unix)]
    {
        cmd.process_group(0);
    }
    let mut child = cmd.spawn().map_err(|error| error.to_string())?;
    let child_pid = child.id().unwrap_or(0);

    enum Chunk {
        Data(Vec<u8>),
        End,
    }
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Chunk>(100);
    for mut stream in [
        Box::new(child.stdout.take().expect("stdout piped"))
            as Box<dyn tokio::io::AsyncRead + Unpin + Send>,
        Box::new(child.stderr.take().expect("stderr piped")),
    ] {
        let tx = tx.clone();
        tokio::spawn(async move {
            let mut buf = [0u8; 4096];
            loop {
                match stream.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if tx.send(Chunk::Data(buf[..n].to_vec())).await.is_err() {
                            return;
                        }
                    }
                }
            }
            let _ = tx.send(Chunk::End).await;
        });
    }
    drop(tx);

    let mut decoder = crate::tools::StreamingUtf8Decoder::new();
    let mut output_chunks: Vec<String> = Vec::new();
    let mut output_bytes: usize = 0;
    let max_output_bytes = BASH_MAX_BYTES * 2;
    let mut total_bytes: usize = 0;
    let mut temp_file: Option<(std::path::PathBuf, std::fs::File)> = None;
    let mut killed = false;
    let mut streams_done = 0;

    let ensure_temp_file = |chunks: &[String]| -> Option<(std::path::PathBuf, std::fs::File)> {
        let id = uuid::Uuid::new_v4().simple().to_string();
        let path = std::env::temp_dir().join(format!("pi-bash-{}.log", &id[..16]));
        let mut file = std::fs::File::create(&path).ok()?;
        for chunk in chunks {
            let _ = file.write_all(chunk.as_bytes());
        }
        Some((path, file))
    };

    loop {
        if cancel.is_cancelled() && !killed {
            crate::tools::kill_process_tree(child_pid);
            killed = true;
        }
        let next = tokio::time::timeout(std::time::Duration::from_millis(25), rx.recv()).await;
        let chunk = match next {
            Err(_) => continue,
            Ok(None) => break,
            Ok(Some(Chunk::End)) => {
                streams_done += 1;
                if streams_done >= 2 {
                    break;
                }
                continue;
            }
            Ok(Some(Chunk::Data(data))) => data,
        };

        total_bytes += chunk.len();
        let text =
            sanitize_binary_output(&strip_ansi(&decoder.decode(&chunk, false))).replace('\r', "");

        if total_bytes > BASH_MAX_BYTES && temp_file.is_none() {
            temp_file = ensure_temp_file(&output_chunks);
        }
        if let Some((_, file)) = temp_file.as_mut() {
            let _ = file.write_all(text.as_bytes());
        }

        output_bytes += text.len();
        output_chunks.push(text.clone());
        while output_bytes > max_output_bytes && output_chunks.len() > 1 {
            let removed = output_chunks.remove(0);
            output_bytes -= removed.len();
        }

        if let Some(on_chunk) = &on_chunk {
            on_chunk(&text);
        }
    }

    let status = loop {
        if cancel.is_cancelled() && !killed {
            crate::tools::kill_process_tree(child_pid);
            killed = true;
        }
        match tokio::time::timeout(std::time::Duration::from_millis(25), child.wait()).await {
            Ok(Ok(status)) => break Some(status),
            Ok(Err(_)) => break None,
            Err(_) => continue,
        }
    };

    let full_output = output_chunks.concat();
    let truncation = crate::tools::truncate_tail(&full_output, BASH_MAX_LINES, BASH_MAX_BYTES);
    if truncation.truncated && temp_file.is_none() {
        temp_file = ensure_temp_file(&output_chunks);
    }
    let cancelled = cancel.is_cancelled();

    Ok(BashResult {
        output: if truncation.truncated {
            truncation.content
        } else {
            full_output
        },
        exit_code: if cancelled {
            None
        } else {
            status.and_then(|s| s.code()).map(i64::from)
        },
        cancelled,
        truncated: truncation.truncated,
        full_output_path: temp_file.map(|(path, _)| path.to_string_lossy().into_owned()),
    })
}

impl AgentSession {
    /// Execute a bash command (`!` command; oracle `executeBash`).
    pub async fn execute_bash(
        &self,
        command: &str,
        on_chunk: Option<BashChunkCallback>,
        exclude_from_context: Option<bool>,
    ) -> Result<BashResult, String> {
        let cancel = CancellationToken::new();
        self.inner.state.lock().bash_cancel = Some(cancel.clone());

        let (prefix, shell_path) = {
            let guard = self.inner.settings.lock();
            (
                guard
                    .settings()
                    .get_str("shellCommandPrefix")
                    .map(str::to_string),
                guard.settings().get_str("shellPath").map(str::to_string),
            )
        };
        let resolved_command = match &prefix {
            Some(prefix) => format!("{prefix}\n{command}"),
            None => command.to_string(),
        };
        let cwd = {
            let state = self.inner.state.lock();
            state.session_manager.get_cwd().to_path_buf()
        };

        let result = match resolve_shell(shell_path.as_deref()) {
            Ok(shell) => execute_bash_impl(&resolved_command, &shell, &cwd, on_chunk, cancel).await,
            Err(error) => Err(error),
        };
        self.inner.state.lock().bash_cancel = None;

        let result = result?;
        self.record_bash_result(command, &result, exclude_from_context);
        Ok(result)
    }

    /// Record a bash execution result in session history (oracle
    /// `recordBashResult`). Mid-run results are deferred to preserve
    /// tool_use/tool_result ordering.
    pub fn record_bash_result(
        &self,
        command: &str,
        result: &BashResult,
        exclude_from_context: Option<bool>,
    ) {
        // Field order = oracle BashExecutionMessage literal (agent-session.ts
        // recordBashResult); undefined fields are omitted.
        let mut message = serde_json::Map::new();
        message.insert("role".into(), Value::String("bashExecution".into()));
        message.insert("command".into(), Value::String(command.to_string()));
        message.insert("output".into(), Value::String(result.output.clone()));
        if let Some(exit_code) = result.exit_code {
            message.insert("exitCode".into(), Value::from(exit_code));
        }
        message.insert("cancelled".into(), Value::Bool(result.cancelled));
        message.insert("truncated".into(), Value::Bool(result.truncated));
        if let Some(path) = &result.full_output_path {
            message.insert("fullOutputPath".into(), Value::String(path.clone()));
        }
        message.insert("timestamp".into(), Value::from(now_ms()));
        if let Some(exclude) = exclude_from_context {
            message.insert("excludeFromContext".into(), Value::Bool(exclude));
        }
        let value = Value::Object(message);

        let mut state = self.inner.state.lock();
        if state.run_active {
            state.pending_bash.push(value);
        } else {
            state.messages.push(AgentMessage::Custom(value.clone()));
            let _ = state.session_manager.append_message(value);
        }
    }

    pub fn has_pending_bash_messages(&self) -> bool {
        !self.inner.state.lock().pending_bash.is_empty()
    }
}

// ============================================================================
// Branch summarization + tree navigation
// (core/compaction/branch-summarization.ts + agent-session.ts navigateTree)
// ============================================================================

const BRANCH_SUMMARY_PREAMBLE: &str = "The user explored a different conversation branch before returning here.\nSummary of that exploration:\n\n";

const BRANCH_SUMMARY_PROMPT: &str = r#"Create a structured summary of this conversation branch for context when returning later.

Use this EXACT format:

## Goal
[What was the user trying to accomplish in this branch?]

## Constraints & Preferences
- [Any constraints, preferences, or requirements mentioned]
- [Or "(none)" if none were mentioned]

## Progress
### Done
- [x] [Completed tasks/changes]

### In Progress
- [ ] [Work that was started but not finished]

### Blocked
- [Issues preventing progress, if any]

## Key Decisions
- **[Decision]**: [Brief rationale]

## Next Steps
1. [What should happen next to continue this work]

Keep each section concise. Preserve exact file paths, function names, and error messages."#;

/// Oracle `collectEntriesForBranchSummary`.
fn collect_entries_for_branch_summary(
    session_manager: &SessionManager,
    old_leaf_id: Option<&str>,
    target_id: &str,
) -> (Vec<crate::session_types::SessionEntry>, Option<String>) {
    let Some(old_leaf_id) = old_leaf_id else {
        return (Vec::new(), None);
    };

    let old_path: std::collections::HashSet<String> = session_manager
        .get_branch(Some(old_leaf_id))
        .iter()
        .filter_map(|e| e.id().map(str::to_string))
        .collect();
    let target_path = session_manager.get_branch(Some(target_id));

    let mut common_ancestor_id: Option<String> = None;
    for entry in target_path.iter().rev() {
        if let Some(id) = entry.id()
            && old_path.contains(id)
        {
            common_ancestor_id = Some(id.to_string());
            break;
        }
    }

    let mut entries = Vec::new();
    let mut current = Some(old_leaf_id.to_string());
    while let Some(id) = current {
        if Some(id.as_str()) == common_ancestor_id.as_deref() {
            break;
        }
        let Some(entry) = session_manager.get_entry(&id) else {
            break;
        };
        entries.push(entry.clone());
        current = entry.parent_id().as_option().cloned();
    }
    entries.reverse();

    (entries, common_ancestor_id)
}

/// Oracle `getMessageFromEntry` (branch variant: includes compaction, skips
/// tool results).
fn message_from_entry_for_branch(
    entry: &crate::session_types::SessionEntry,
) -> Option<AgentMessage> {
    use crate::session_types::SessionEntry;
    match entry {
        SessionEntry::Message { message, .. } => {
            if message.get("role").and_then(Value::as_str) == Some("toolResult") {
                return None;
            }
            value_to_agent_message(message.clone())
        }
        SessionEntry::CustomMessage { .. }
        | SessionEntry::BranchSummary { .. }
        | SessionEntry::Compaction { .. } => entry_context_messages(entry).into_iter().next(),
        _ => None,
    }
}

/// Oracle `prepareBranchEntries` (newest-to-oldest within token budget).
fn prepare_branch_entries(
    entries: &[crate::session_types::SessionEntry],
    token_budget: u64,
) -> (Vec<AgentMessage>, FileOperations) {
    use crate::session_types::SessionEntry;

    let mut messages: std::collections::VecDeque<AgentMessage> = std::collections::VecDeque::new();
    let mut file_ops = FileOperations::default();
    let mut total_tokens: u64 = 0;

    // First pass: cumulative file tracking from pi-generated branch summaries.
    for entry in entries {
        if let SessionEntry::BranchSummary {
            details, from_hook, ..
        } = entry
            && !from_hook.unwrap_or(false)
            && let Some(details) = details
        {
            if let Some(read_files) = details.get("readFiles").and_then(Value::as_array) {
                for f in read_files.iter().filter_map(Value::as_str) {
                    file_ops.read.insert(f.to_string());
                }
            }
            if let Some(modified) = details.get("modifiedFiles").and_then(Value::as_array) {
                for f in modified.iter().filter_map(Value::as_str) {
                    file_ops.edited.insert(f.to_string());
                }
            }
        }
    }

    // Second pass: newest to oldest, adding messages until the budget.
    for entry in entries.iter().rev() {
        let Some(message) = message_from_entry_for_branch(entry) else {
            continue;
        };
        extract_file_ops_from_message(&message, &mut file_ops);
        let tokens = estimate_tokens(&message);

        if token_budget > 0 && total_tokens + tokens > token_budget {
            // Summary entries are important context: fit if under 90% budget.
            if matches!(
                entry,
                SessionEntry::Compaction { .. } | SessionEntry::BranchSummary { .. }
            ) && (total_tokens as f64) < token_budget as f64 * 0.9
            {
                messages.push_front(message);
            }
            break;
        }

        messages.push_front(message);
        total_tokens += tokens;
    }

    (messages.into(), file_ops)
}

/// Result of branch summarization.
struct BranchSummaryOutcome {
    summary: Option<String>,
    read_files: Vec<String>,
    modified_files: Vec<String>,
    aborted: bool,
    error: Option<String>,
}

/// Oracle `generateBranchSummary`.
async fn generate_branch_summary(
    stream_fn: &StreamFn,
    model: &Model,
    entries: &[crate::session_types::SessionEntry],
    cancel: &CancellationToken,
    custom_instructions: Option<&str>,
    replace_instructions: bool,
    reserve_tokens: u64,
) -> BranchSummaryOutcome {
    let context_window = if model.context_window > 0 {
        model.context_window
    } else {
        128000
    };
    let token_budget = context_window.saturating_sub(reserve_tokens);

    let (messages, file_ops) = prepare_branch_entries(entries, token_budget);
    if messages.is_empty() {
        return BranchSummaryOutcome {
            summary: Some("No content to summarize".to_string()),
            read_files: Vec::new(),
            modified_files: Vec::new(),
            aborted: false,
            error: None,
        };
    }

    let llm_messages = convert_to_llm(messages);
    let conversation_text = serialize_conversation(&llm_messages);

    let instructions = match (replace_instructions, custom_instructions) {
        (true, Some(ci)) => ci.to_string(),
        (false, Some(ci)) => format!("{BRANCH_SUMMARY_PROMPT}\n\nAdditional focus: {ci}"),
        (_, None) => BRANCH_SUMMARY_PROMPT.to_string(),
    };
    let prompt_text =
        format!("<conversation>\n{conversation_text}\n</conversation>\n\n{instructions}");

    let request_messages = vec![Message::User(UserMessage {
        content: UserContent::Blocks(vec![Content::Text(TextContent {
            text: prompt_text.into(),
            text_signature: None,
        })]),
        timestamp: now_ms(),
    })];
    let context = pi_ai::Context {
        messages: request_messages,
        tools: Vec::new(),
        system_prompt: Some(SUMMARIZATION_SYSTEM_PROMPT.to_string()),
    };
    let options = pi_agent::StreamCallOptions {
        max_tokens: Some(2048),
        cancel: Some(cancel.clone()),
        ..Default::default()
    };
    let stream = (stream_fn)(model.clone(), context, options).await;
    let response = match await_result_with_cancel(stream, cancel).await {
        Ok(response) => response,
        Err(_) => {
            return BranchSummaryOutcome {
                summary: None,
                read_files: Vec::new(),
                modified_files: Vec::new(),
                aborted: true,
                error: None,
            };
        }
    };

    if response.stop_reason == StopReason::Aborted {
        return BranchSummaryOutcome {
            summary: None,
            read_files: Vec::new(),
            modified_files: Vec::new(),
            aborted: true,
            error: None,
        };
    }
    if response.stop_reason == StopReason::Error {
        return BranchSummaryOutcome {
            summary: None,
            read_files: Vec::new(),
            modified_files: Vec::new(),
            aborted: false,
            error: Some(
                response
                    .error_message
                    .clone()
                    .unwrap_or_else(|| "Summarization failed".to_string()),
            ),
        };
    }

    let text = response
        .content
        .iter()
        .filter_map(|c| match c {
            Content::Text(t) => Some(t.text.to_string()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");

    let (read_files, modified_files) = compute_file_lists(&file_ops);
    let mut summary = format!("{BRANCH_SUMMARY_PREAMBLE}{text}");
    summary.push_str(&format_file_operations(&read_files, &modified_files));
    if summary.is_empty() {
        summary = "No summary generated".to_string();
    }

    BranchSummaryOutcome {
        summary: Some(summary),
        read_files,
        modified_files,
        aborted: false,
        error: None,
    }
}

/// Options for [`AgentSession::navigate_tree`].
#[derive(Clone, Debug, Default)]
pub struct NavigateTreeOptions {
    pub summarize: bool,
    pub custom_instructions: Option<String>,
    pub replace_instructions: bool,
    pub label: Option<String>,
}

/// Result of [`AgentSession::navigate_tree`].
#[derive(Clone, Debug, Default)]
pub struct NavigateTreeResult {
    pub editor_text: Option<String>,
    pub cancelled: bool,
    pub aborted: bool,
    pub summary_entry_id: Option<String>,
}

impl AgentSession {
    /// Navigate to a different node in the session tree (oracle
    /// `navigateTree`). Stays in the same session file.
    pub async fn navigate_tree(
        &self,
        target_id: &str,
        options: NavigateTreeOptions,
    ) -> Result<NavigateTreeResult, String> {
        use crate::session_types::SessionEntry;

        let (old_leaf_id, target_entry) = {
            let state = self.inner.state.lock();
            (
                state.session_manager.get_leaf_id().map(str::to_string),
                state.session_manager.get_entry(target_id).cloned(),
            )
        };

        // No-op if already at target.
        if old_leaf_id.as_deref() == Some(target_id) {
            return Ok(NavigateTreeResult {
                cancelled: false,
                ..Default::default()
            });
        }

        let model = self.model();
        if options.summarize && model.is_none() {
            return Err("No model available for summarization".to_string());
        }

        let Some(target_entry) = target_entry else {
            return Err(format!("Entry {target_id} not found"));
        };

        let (entries_to_summarize, _common_ancestor_id) = {
            let state = self.inner.state.lock();
            collect_entries_for_branch_summary(
                &state.session_manager,
                old_leaf_id.as_deref(),
                target_id,
            )
        };

        let cancel = CancellationToken::new();
        self.inner.state.lock().branch_summary_cancel = Some(cancel.clone());

        let outcome = if options.summarize && !entries_to_summarize.is_empty() {
            let model = model.expect("validated above");
            let reserve_tokens = {
                let guard = self.inner.settings.lock();
                guard
                    .settings()
                    .get("branchSummary")
                    .and_then(|b| b.get("reserveTokens"))
                    .and_then(Value::as_u64)
                    .unwrap_or(16384)
            };
            let outcome = generate_branch_summary(
                &self.inner.stream_fn,
                &model,
                &entries_to_summarize,
                &cancel,
                options.custom_instructions.as_deref(),
                options.replace_instructions,
                reserve_tokens,
            )
            .await;
            if outcome.aborted {
                self.inner.state.lock().branch_summary_cancel = None;
                return Ok(NavigateTreeResult {
                    cancelled: true,
                    aborted: true,
                    ..Default::default()
                });
            }
            if let Some(error) = outcome.error {
                self.inner.state.lock().branch_summary_cancel = None;
                return Err(error);
            }
            Some(outcome)
        } else {
            None
        };
        self.inner.state.lock().branch_summary_cancel = None;

        let summary_text = outcome.as_ref().and_then(|o| o.summary.clone());
        let summary_details = outcome.as_ref().map(|o| {
            json!({
                "readFiles": o.read_files,
                "modifiedFiles": o.modified_files,
            })
        });

        // Determine the new leaf position based on target type.
        let mut editor_text: Option<String> = None;
        let new_leaf_id: Option<String> = match &target_entry {
            SessionEntry::Message {
                message, parent_id, ..
            } if message.get("role").and_then(Value::as_str) == Some("user") => {
                editor_text = Some(extract_user_content_text(message.get("content")));
                parent_id.as_option().cloned()
            }
            SessionEntry::CustomMessage {
                content, parent_id, ..
            } => {
                editor_text = Some(extract_user_content_text(Some(content)));
                parent_id.as_option().cloned()
            }
            _ => Some(target_id.to_string()),
        };

        let summary_entry_id = {
            let mut state = self.inner.state.lock();
            let mut summary_entry_id: Option<String> = None;

            if let Some(summary) = &summary_text {
                let id = state
                    .session_manager
                    .branch_with_summary(
                        new_leaf_id.clone(),
                        summary.clone(),
                        summary_details.clone(),
                        Some(false),
                    )
                    .map_err(|e| e.to_string())?;
                if let Some(label) = &options.label {
                    let _ = state
                        .session_manager
                        .append_label_change(id.clone(), Some(label.clone()));
                }
                summary_entry_id = Some(id);
            } else if let Some(new_leaf) = &new_leaf_id {
                state
                    .session_manager
                    .branch(new_leaf)
                    .map_err(|e| e.to_string())?;
            } else {
                state.session_manager.reset_leaf();
            }

            // Attach label to target entry when not summarizing.
            if let Some(label) = &options.label
                && summary_text.is_none()
            {
                let _ = state
                    .session_manager
                    .append_label_change(target_id.to_string(), Some(label.clone()));
            }

            // Update agent state from the new branch.
            let session_context = state.session_manager.build_session_context();
            state.messages = session_context
                .messages
                .into_iter()
                .filter_map(value_to_agent_message)
                .collect();

            summary_entry_id
        };

        Ok(NavigateTreeResult {
            editor_text,
            cancelled: false,
            aborted: false,
            summary_entry_id,
        })
    }
}
