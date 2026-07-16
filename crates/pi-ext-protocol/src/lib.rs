//! Versioned, transport-independent protocol for the Rust ↔ Bun extension bridge.
//!
//! The wire format is one JSON object per line. This crate deliberately performs no
//! I/O: [`encode_frame`] and [`decode_frame`] only encode or validate one NDJSON frame.

use std::collections::BTreeMap;
use std::num::NonZeroU64;

use pi_ai::{AssistantMessageEvent, Content, Context, Message, Model, ThinkingLevel, ToolResultMessage};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

pub const PROTOCOL_VERSION: u16 = 1;
pub const PI_COMPAT_VERSION: &str = "0.80.7";
pub const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_ERROR_MESSAGE_BYTES: usize = 64 * 1024;
pub const MAX_ERROR_STACK_BYTES: usize = 512 * 1024;

pub type JsonObject = serde_json::Map<String, Value>;
pub type RequestId = NonZeroU64;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Direction {
    RustToSidecar,
    SidecarToRust,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct CorrelationId {
    pub direction: Direction,
    pub id: RequestId,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Envelope {
    #[serde(rename = "req")]
    Request {
        id: RequestId,
        #[serde(flatten)]
        request: Request,
    },
    #[serde(rename = "res")]
    Response {
        id: RequestId,
        #[serde(flatten)]
        result: ResponseResult,
    },
    #[serde(rename = "ev")]
    Event {
        #[serde(flatten)]
        event: Notification,
    },
    #[serde(rename = "cancel")]
    Cancel { id: RequestId },
}

impl Envelope {
    pub fn correlation(&self, direction: Direction) -> Option<CorrelationId> {
        match self {
            Self::Request { id, .. } | Self::Response { id, .. } | Self::Cancel { id } => {
                Some(CorrelationId { direction, id: *id })
            }
            Self::Event { .. } => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ResponseResult {
    Ok { ok: Value },
    Err { err: ProtocolError },
}

impl ResponseResult {
    pub fn ok<T: Serialize>(value: &T) -> Result<Self, serde_json::Error> {
        Ok(Self::Ok { ok: serde_json::to_value(value)? })
    }

    pub fn decode_ok<T: serde::de::DeserializeOwned>(&self) -> Result<Option<T>, serde_json::Error> {
        match self {
            Self::Ok { ok } => serde_json::from_value(ok.clone()).map(Some),
            Self::Err { .. } => Ok(None),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProtocolError {
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stack: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extension_path: Option<String>,
}

impl ProtocolError {
    pub fn validate(&self) -> Result<(), ValidationError> {
        if self.message.len() > MAX_ERROR_MESSAGE_BYTES {
            return Err(ValidationError::ErrorMessageTooLarge);
        }
        if self.stack.as_ref().is_some_and(|s| s.len() > MAX_ERROR_STACK_BYTES) {
            return Err(ValidationError::ErrorStackTooLarge);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "method", content = "params")]
pub enum Request {
    #[serde(rename = "lifecycle/init")]
    LifecycleInit(Box<InitParams>),
    #[serde(rename = "lifecycle/load")]
    LifecycleLoad(LoadParams),
    #[serde(rename = "lifecycle/shutdown")]
    LifecycleShutdown(Empty),
    #[serde(rename = "event/emit")]
    EventEmit(Box<EventDispatch>),
    #[serde(rename = "action/setModel")]
    ActionSetModel(Box<SetModelParams>),
    #[serde(rename = "action/waitForIdle")]
    ActionWaitForIdle(Empty),
    #[serde(rename = "action/newSession")]
    ActionNewSession(NewSessionParams),
    #[serde(rename = "action/fork")]
    ActionFork(ForkParams),
    #[serde(rename = "action/navigateTree")]
    ActionNavigateTree(NavigateTreeParams),
    #[serde(rename = "action/switchSession")]
    ActionSwitchSession(SwitchSessionParams),
    #[serde(rename = "action/reload")]
    ActionReload(Empty),
    #[serde(rename = "action/replaced/sendMessage")]
    ActionReplacedSendMessage(SendMessageParams),
    #[serde(rename = "action/replaced/sendUserMessage")]
    ActionReplacedSendUserMessage(SendUserMessageParams),
    #[serde(rename = "ui/select")]
    UiSelect(SelectParams),
    #[serde(rename = "ui/confirm")]
    UiConfirm(ConfirmParams),
    #[serde(rename = "ui/input")]
    UiInput(InputDialogParams),
    #[serde(rename = "ui/editor")]
    UiEditor(EditorParams),
    #[serde(rename = "ui/custom")]
    UiCustom(CustomDialogParams),
    #[serde(rename = "ui/render")]
    UiRender(RenderParams),
    #[serde(rename = "ui/autocomplete")]
    UiAutocomplete(AutocompleteParams),
    #[serde(rename = "ui/terminal_input")]
    UiTerminalInput(TerminalInputParams),
    #[serde(rename = "ui/getAllThemes")]
    UiGetAllThemes(Empty),
    #[serde(rename = "ui/getTheme")]
    UiGetTheme(NameParams),
    #[serde(rename = "tool/execute")]
    ToolExecute(ToolExecuteParams),
    #[serde(rename = "provider/stream")]
    ProviderStream(Box<ProviderStreamParams>),
    #[serde(rename = "command/execute")]
    CommandExecute(CommandExecuteParams),
    #[serde(rename = "shortcut/invoke")]
    ShortcutInvoke(ShortcutInvokeParams),
    #[serde(rename = "session/setup")]
    SessionSetup(SessionSetupParams),
}

impl Request {
    pub const fn class(&self) -> MessageClass {
        MessageClass::BlockingRequest
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "method", content = "params")]
pub enum Notification {
    #[serde(rename = "lifecycle/hello")]
    LifecycleHello(HelloParams),
    #[serde(rename = "lifecycle/initialized")]
    LifecycleInitialized(InitializedParams),
    #[serde(rename = "lifecycle/ping")]
    LifecyclePing(HeartbeatParams),
    #[serde(rename = "lifecycle/pong")]
    LifecyclePong(HeartbeatParams),
    #[serde(rename = "event/notify")]
    EventNotify(Box<EventDispatch>),
    #[serde(rename = "action/sendMessage")]
    ActionSendMessage(SendMessageParams),
    #[serde(rename = "action/sendUserMessage")]
    ActionSendUserMessage(SendUserMessageParams),
    #[serde(rename = "action/appendEntry")]
    ActionAppendEntry(AppendEntryParams),
    #[serde(rename = "action/setSessionName")]
    ActionSetSessionName(SetSessionNameParams),
    #[serde(rename = "action/setLabel")]
    ActionSetLabel(SetLabelParams),
    #[serde(rename = "action/setActiveTools")]
    ActionSetActiveTools(SetActiveToolsParams),
    #[serde(rename = "action/refreshTools")]
    ActionRefreshTools(Empty),
    #[serde(rename = "action/setThinkingLevel")]
    ActionSetThinkingLevel(SetThinkingLevelParams),
    #[serde(rename = "action/shutdown")]
    ActionShutdown(Empty),
    #[serde(rename = "action/abort")]
    ActionAbort(Empty),
    #[serde(rename = "action/compact")]
    ActionCompact(CompactParams),
    #[serde(rename = "ui/notify")]
    UiNotify(NotifyParams),
    #[serde(rename = "ui/setStatus")]
    UiSetStatus(KeyValueParams),
    #[serde(rename = "ui/setWorkingMessage")]
    UiSetWorkingMessage(OptionalTextParams),
    #[serde(rename = "ui/setWorkingVisible")]
    UiSetWorkingVisible(VisibleParams),
    #[serde(rename = "ui/setWorkingIndicator")]
    UiSetWorkingIndicator(WorkingIndicatorParams),
    #[serde(rename = "ui/setHiddenThinkingLabel")]
    UiSetHiddenThinkingLabel(OptionalTextParams),
    #[serde(rename = "ui/setTitle")]
    UiSetTitle(TextParams),
    #[serde(rename = "ui/setEditorText")]
    UiSetEditorText(TextParams),
    #[serde(rename = "ui/pasteToEditor")]
    UiPasteToEditor(TextParams),
    #[serde(rename = "ui/setTheme")]
    UiSetTheme(ThemeSelectionParams),
    #[serde(rename = "ui/setToolsExpanded")]
    UiSetToolsExpanded(VisibleParams),
    #[serde(rename = "ui/frame")]
    UiFrame(FrameParams),
    #[serde(rename = "ui/input")]
    UiComponentInput(ComponentInputParams),
    #[serde(rename = "ui/dispose")]
    UiDispose(SlotParams),
    #[serde(rename = "ui/done")]
    UiDone(DoneParams),
    #[serde(rename = "ui/overlay")]
    UiOverlay(OverlayParams),
    #[serde(rename = "tool/update")]
    ToolUpdate(ToolUpdateParams),
    #[serde(rename = "provider/register")]
    ProviderRegister(ProviderRegistration),
    #[serde(rename = "provider/unregister")]
    ProviderUnregister(NameParams),
    #[serde(rename = "provider/event")]
    ProviderEvent(Box<ProviderEventParams>),
    #[serde(rename = "session/sync")]
    SessionSync(SessionSyncParams),
    #[serde(rename = "state/update")]
    StateUpdate(Box<StateUpdate>),
    #[serde(rename = "error/extension")]
    ExtensionError(ExtensionError),
}

impl Notification {
    pub const fn class(&self) -> MessageClass {
        MessageClass::Notification
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MessageClass {
    BlockingRequest,
    Notification,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectTrustResult {
    pub trusted: ProjectTrustDecision,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remember: Option<bool>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProjectTrustDecision { Yes, No, Undecided }

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourcesDiscoverResult {
    #[serde(skip_serializing_if = "Option::is_none")] pub skill_paths: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")] pub prompt_paths: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")] pub theme_paths: Option<Vec<String>>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ContextEventResult {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub messages: Option<Vec<AgentMessage>>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCallEventResult {
    #[serde(skip_serializing_if = "Option::is_none")] pub block: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")] pub reason: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolResultEventResult {
    #[serde(skip_serializing_if = "Option::is_none")] pub content: Option<Vec<Content>>,
    #[serde(skip_serializing_if = "Option::is_none")] pub details: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")] pub is_error: Option<bool>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct MessageEndEventResult {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<AgentMessage>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BeforeAgentStartEventResult {
    #[serde(skip_serializing_if = "Option::is_none")] pub message: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")] pub system_prompt: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionBeforeSwitchResult { #[serde(skip_serializing_if = "Option::is_none")] pub cancel: Option<bool> }
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionBeforeForkResult { #[serde(skip_serializing_if = "Option::is_none")] pub cancel: Option<bool>, #[serde(skip_serializing_if = "Option::is_none")] pub skip_conversation_restore: Option<bool> }
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionBeforeCompactResult { #[serde(skip_serializing_if = "Option::is_none")] pub cancel: Option<bool>, #[serde(skip_serializing_if = "Option::is_none")] pub compaction: Option<Value> }
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionBeforeTreeResult { #[serde(skip_serializing_if = "Option::is_none")] pub cancel: Option<bool>, #[serde(skip_serializing_if = "Option::is_none")] pub summary: Option<TreeSummary>, #[serde(skip_serializing_if = "Option::is_none")] pub custom_instructions: Option<String>, #[serde(skip_serializing_if = "Option::is_none")] pub replace_instructions: Option<bool>, #[serde(skip_serializing_if = "Option::is_none")] pub label: Option<String> }
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TreeSummary { pub summary: String, #[serde(skip_serializing_if = "Option::is_none")] pub details: Option<Value> }

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "camelCase")]
pub enum InputEventResult {
    Continue,
    Transform { text: String, #[serde(skip_serializing_if = "Option::is_none")] images: Option<Vec<pi_ai::ImageContent>> },
    Handled,
}

pub type BeforeProviderRequestEventResult = Value;
pub type BeforeProviderHeadersEventResult = BTreeMap<String, Option<String>>;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct UserBashEventResult {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operations: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CancelledResult { pub cancelled: bool }

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalInputResult {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub consume: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolExecuteResult {
    pub content: Vec<Content>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
    pub is_error: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub added_tool_names: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terminate: Option<bool>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Empty {}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelloParams { pub protocol: u16, pub pi: String, pub bun: String }

impl HelloParams {
    pub fn negotiate(&self) -> Result<NegotiatedVersion, VersionError> {
        if self.protocol != PROTOCOL_VERSION {
            return Err(VersionError::Unsupported { received: self.protocol, supported: PROTOCOL_VERSION });
        }
        Ok(NegotiatedVersion(PROTOCOL_VERSION))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NegotiatedVersion(u16);
impl NegotiatedVersion { pub const fn get(self) -> u16 { self.0 } }

#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum VersionError {
    #[error("unsupported extension protocol version {received}; supported version is {supported}")]
    Unsupported { received: u16, supported: u16 },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitParams {
    pub cwd: String,
    pub agent_dir: String,
    pub session_dir: String,
    pub configured_paths: Vec<String>,
    pub mode: ExtensionMode,
    pub has_ui: bool,
    pub flag_values: BTreeMap<String, FlagValue>,
    pub theme: ThemeDto,
    pub session: SessionSnapshot,
    pub state: StateBlock,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoadParams { pub paths: Vec<String> }

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExtensionMode { Tui, Rpc, Json, Print }

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ThemeDto { pub name: String, pub json: Value }

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThemeCatalogEntry {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct GetAllThemesResult(pub Vec<ThemeCatalogEntry>);

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct GetThemeResult(pub Option<ThemeDto>);

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum FlagValue { Boolean(bool), String(String) }

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializedParams {
    pub registrations: Registrations,
    pub subscribed_events: Vec<ExtensionEventKind>,
    pub errors: Vec<ExtensionError>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Registrations {
    pub tools: Vec<ToolRegistration>,
    pub commands: Vec<CommandRegistration>,
    pub shortcuts: Vec<ShortcutRegistration>,
    pub flags: Vec<FlagRegistration>,
    pub providers: Vec<ProviderRegistration>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeartbeatParams { pub nonce: u64 }

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EventDispatch { pub event: ExtensionEvent, pub state: StateBlock }

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionEventKind {
    ProjectTrust, ResourcesDiscover, SessionStart, SessionInfoChanged, SessionBeforeSwitch,
    SessionBeforeFork, SessionBeforeCompact, SessionCompact, SessionShutdown, SessionBeforeTree,
    SessionTree, Context, BeforeProviderRequest, BeforeProviderHeaders, AfterProviderResponse,
    BeforeAgentStart, AgentStart, AgentEnd, AgentSettled, TurnStart, TurnEnd, MessageStart,
    MessageUpdate, MessageEnd, ToolExecutionStart, ToolExecutionUpdate, ToolExecutionEnd,
    ModelSelect, ThinkingLevelSelect, UserBash, Input, ToolCall, ToolResult,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AgentMessage { Standard(Box<Message>), Custom(Value) }

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BuildSystemPromptOptions {
    #[serde(skip_serializing_if = "Option::is_none")] pub custom_prompt: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")] pub selected_tools: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")] pub tool_snippets: Option<BTreeMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")] pub prompt_guidelines: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")] pub append_system_prompt: Option<String>,
    pub cwd: String,
    #[serde(skip_serializing_if = "Option::is_none")] pub context_files: Option<Vec<ContextFile>>,
    #[serde(skip_serializing_if = "Option::is_none")] pub skills: Option<Vec<SkillDto>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextFile { pub path: String, pub content: String }

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillDto {
    pub name: String,
    pub description: String,
    pub file_path: String,
    pub base_dir: String,
    pub source_info: SourceInfo,
    pub disable_model_invocation: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", rename_all_fields = "camelCase")]
pub enum ExtensionEvent {
    ProjectTrust { cwd: String },
    ResourcesDiscover { cwd: String, reason: DiscoverReason },
    SessionStart { reason: SessionStartReason, #[serde(skip_serializing_if = "Option::is_none")] previous_session_file: Option<String> },
    SessionInfoChanged { #[serde(skip_serializing_if = "Option::is_none")] name: Option<String> },
    SessionBeforeSwitch { reason: SwitchReason, #[serde(skip_serializing_if = "Option::is_none")] target_session_file: Option<String> },
    SessionBeforeFork { entry_id: String, position: ForkPosition },
    SessionBeforeCompact { preparation: Value, branch_entries: Vec<Value>, #[serde(skip_serializing_if = "Option::is_none")] custom_instructions: Option<String>, reason: CompactReason, will_retry: bool },
    SessionCompact { compaction_entry: Value, from_extension: bool, reason: CompactReason, will_retry: bool },
    SessionShutdown { reason: ShutdownReason, #[serde(skip_serializing_if = "Option::is_none")] target_session_file: Option<String> },
    SessionBeforeTree { preparation: TreePreparation },
    SessionTree { new_leaf_id: Option<String>, old_leaf_id: Option<String>, #[serde(skip_serializing_if = "Option::is_none")] summary_entry: Option<Value>, #[serde(skip_serializing_if = "Option::is_none")] from_extension: Option<bool> },
    Context { messages: Vec<AgentMessage> },
    BeforeProviderRequest { payload: Value },
    BeforeProviderHeaders { headers: BTreeMap<String, Option<String>> },
    AfterProviderResponse { status: u16, headers: BTreeMap<String, String> },
    BeforeAgentStart { prompt: String, #[serde(skip_serializing_if = "Option::is_none")] images: Option<Vec<pi_ai::ImageContent>>, system_prompt: String, system_prompt_options: BuildSystemPromptOptions },
    AgentStart {},
    AgentEnd { messages: Vec<AgentMessage> },
    AgentSettled {},
    TurnStart { turn_index: u64, timestamp: u64 },
    TurnEnd { turn_index: u64, message: AgentMessage, tool_results: Vec<ToolResultMessage> },
    MessageStart { message: AgentMessage },
    MessageUpdate { message: AgentMessage, assistant_message_event: Box<AssistantMessageEvent> },
    MessageEnd { message: AgentMessage },
    ToolExecutionStart { tool_call_id: String, tool_name: String, args: Value },
    ToolExecutionUpdate { tool_call_id: String, tool_name: String, args: Value, partial_result: Value },
    ToolExecutionEnd { tool_call_id: String, tool_name: String, result: Value, is_error: bool },
    ModelSelect { model: Box<Model>, #[serde(skip_serializing_if = "Option::is_none")] previous_model: Option<Box<Model>>, source: ModelSelectSource },
    ThinkingLevelSelect { level: ThinkingLevel, previous_level: ThinkingLevel },
    UserBash { command: String, exclude_from_context: bool, cwd: String },
    Input { text: String, #[serde(skip_serializing_if = "Option::is_none")] images: Option<Vec<pi_ai::ImageContent>>, source: InputSource, #[serde(skip_serializing_if = "Option::is_none")] streaming_behavior: Option<StreamingBehavior> },
    ToolCall { tool_call_id: String, tool_name: String, input: Value },
    ToolResult { tool_call_id: String, tool_name: String, input: JsonObject, content: Vec<Content>, is_error: bool, #[serde(skip_serializing_if = "Option::is_none")] details: Option<Value> },
}

impl ExtensionEvent {
    pub const fn kind(&self) -> ExtensionEventKind {
        match self {
            Self::ProjectTrust { .. } => ExtensionEventKind::ProjectTrust,
            Self::ResourcesDiscover { .. } => ExtensionEventKind::ResourcesDiscover,
            Self::SessionStart { .. } => ExtensionEventKind::SessionStart,
            Self::SessionInfoChanged { .. } => ExtensionEventKind::SessionInfoChanged,
            Self::SessionBeforeSwitch { .. } => ExtensionEventKind::SessionBeforeSwitch,
            Self::SessionBeforeFork { .. } => ExtensionEventKind::SessionBeforeFork,
            Self::SessionBeforeCompact { .. } => ExtensionEventKind::SessionBeforeCompact,
            Self::SessionCompact { .. } => ExtensionEventKind::SessionCompact,
            Self::SessionShutdown { .. } => ExtensionEventKind::SessionShutdown,
            Self::SessionBeforeTree { .. } => ExtensionEventKind::SessionBeforeTree,
            Self::SessionTree { .. } => ExtensionEventKind::SessionTree,
            Self::Context { .. } => ExtensionEventKind::Context,
            Self::BeforeProviderRequest { .. } => ExtensionEventKind::BeforeProviderRequest,
            Self::BeforeProviderHeaders { .. } => ExtensionEventKind::BeforeProviderHeaders,
            Self::AfterProviderResponse { .. } => ExtensionEventKind::AfterProviderResponse,
            Self::BeforeAgentStart { .. } => ExtensionEventKind::BeforeAgentStart,
            Self::AgentStart { .. } => ExtensionEventKind::AgentStart,
            Self::AgentEnd { .. } => ExtensionEventKind::AgentEnd,
            Self::AgentSettled { .. } => ExtensionEventKind::AgentSettled,
            Self::TurnStart { .. } => ExtensionEventKind::TurnStart,
            Self::TurnEnd { .. } => ExtensionEventKind::TurnEnd,
            Self::MessageStart { .. } => ExtensionEventKind::MessageStart,
            Self::MessageUpdate { .. } => ExtensionEventKind::MessageUpdate,
            Self::MessageEnd { .. } => ExtensionEventKind::MessageEnd,
            Self::ToolExecutionStart { .. } => ExtensionEventKind::ToolExecutionStart,
            Self::ToolExecutionUpdate { .. } => ExtensionEventKind::ToolExecutionUpdate,
            Self::ToolExecutionEnd { .. } => ExtensionEventKind::ToolExecutionEnd,
            Self::ModelSelect { .. } => ExtensionEventKind::ModelSelect,
            Self::ThinkingLevelSelect { .. } => ExtensionEventKind::ThinkingLevelSelect,
            Self::UserBash { .. } => ExtensionEventKind::UserBash,
            Self::Input { .. } => ExtensionEventKind::Input,
            Self::ToolCall { .. } => ExtensionEventKind::ToolCall,
            Self::ToolResult { .. } => ExtensionEventKind::ToolResult,
        }
    }

    pub const fn is_blocking(&self) -> bool {
        matches!(self.kind(), ExtensionEventKind::ProjectTrust | ExtensionEventKind::ResourcesDiscover |
            ExtensionEventKind::SessionBeforeSwitch | ExtensionEventKind::SessionBeforeFork |
            ExtensionEventKind::SessionBeforeCompact | ExtensionEventKind::SessionBeforeTree |
            ExtensionEventKind::Context | ExtensionEventKind::BeforeProviderRequest |
            ExtensionEventKind::BeforeProviderHeaders | ExtensionEventKind::BeforeAgentStart |
            ExtensionEventKind::MessageEnd | ExtensionEventKind::ToolCall |
            ExtensionEventKind::ToolResult | ExtensionEventKind::UserBash | ExtensionEventKind::Input)
    }
}

macro_rules! string_enum {
    ($name:ident { $($variant:ident),+ $(,)? }) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
        #[serde(rename_all = "camelCase")]
        pub enum $name { $($variant),+ }
    };
}
string_enum!(DiscoverReason { Startup, Reload });
string_enum!(SessionStartReason { Startup, Reload, New, Resume, Fork });
string_enum!(ShutdownReason { Quit, Reload, New, Resume, Fork });
string_enum!(SwitchReason { New, Resume });
string_enum!(ForkPosition { Before, At });
string_enum!(CompactReason { Manual, Threshold, Overflow });
string_enum!(ModelSelectSource { Set, Cycle, Restore });
string_enum!(InputSource { Interactive, Rpc, Extension });
string_enum!(StreamingBehavior { Steer, FollowUp });

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TreePreparation {
    pub target_id: String,
    pub old_leaf_id: Option<String>,
    pub common_ancestor_id: Option<String>,
    pub entries_to_summarize: Vec<Value>,
    pub user_wants_summary: bool,
    #[serde(skip_serializing_if = "Option::is_none")] pub custom_instructions: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")] pub replace_instructions: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")] pub label: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StateBlock {
    #[serde(skip_serializing_if = "Option::is_none")] pub session_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")] pub model: Option<Model>,
    pub idle: bool,
    pub project_trusted: bool,
    pub pending_messages: bool,
    pub active_tools: Vec<String>,
    pub all_tools: Vec<ToolInfo>,
    pub commands: Vec<CommandInfo>,
    pub thinking_level: ThinkingLevel,
    #[serde(skip_serializing_if = "Option::is_none")] pub context_usage: Option<ContextUsageDto>,
    pub system_prompt: String,
    #[serde(skip_serializing_if = "Option::is_none")] pub system_prompt_options: Option<BuildSystemPromptOptions>,
    pub flag_values: BTreeMap<String, FlagValue>,
    pub editor_text: String,
    pub tools_expanded: bool,
    #[serde(skip_serializing_if = "Option::is_none")] pub footer: Option<Value>,
    pub theme: ThemeDto,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StateUpdate {
    #[serde(skip_serializing_if = "Option::is_none")] pub model: Option<Model>,
    #[serde(skip_serializing_if = "Option::is_none")] pub idle: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")] pub thinking_level: Option<ThinkingLevel>,
    #[serde(skip_serializing_if = "Option::is_none")] pub active_tools: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")] pub context_usage: Option<ContextUsageDto>,
    #[serde(skip_serializing_if = "Option::is_none")] pub system_prompt: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")] pub footer: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")] pub theme: Option<ThemeDto>,
    #[serde(skip_serializing_if = "Option::is_none")] pub editor_text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")] pub tools_expanded: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextUsageDto {
    pub tokens: Option<u64>,
    pub context_window: u64,
    pub percent: Option<f64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionSnapshot { pub epoch: u64, pub session_file: String, #[serde(skip_serializing_if = "Option::is_none")] pub header: Option<Value>, pub entries: Vec<Value>, pub leaf_id: Option<String>, #[serde(skip_serializing_if = "Option::is_none")] pub name: Option<String> }
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionSyncParams { pub epoch: u64, pub session_file: String, #[serde(skip_serializing_if = "Option::is_none")] pub header: Option<Value>, #[serde(skip_serializing_if = "Option::is_none")] pub entries: Option<Vec<Value>>, #[serde(skip_serializing_if = "Option::is_none")] pub appended: Option<Vec<Value>>, pub leaf_id: Option<String>, #[serde(skip_serializing_if = "Option::is_none")] pub name: Option<String> }
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionSetupParams { pub token: String, pub session_file: String }

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolRegistration { pub name: String, pub label: String, pub description: String, pub parameters: Value, #[serde(skip_serializing_if = "Option::is_none")] pub prompt_guidelines: Option<String>, pub source_info: SourceInfo, pub has_render_call: bool, pub has_render_result: bool }
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceInfo {
    pub path: String,
    pub source: String,
    pub scope: SourceScope,
    pub origin: SourceOrigin,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_dir: Option<String>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SourceScope { User, Project, Temporary }
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SourceOrigin { Package, TopLevel }
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandRegistration { pub name: String, #[serde(skip_serializing_if = "Option::is_none")] pub description: Option<String>, pub source_info: SourceInfo, pub has_argument_completions: bool }
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShortcutRegistration { pub key_id: String, #[serde(skip_serializing_if = "Option::is_none")] pub description: Option<String>, pub extension_path: String }
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FlagRegistration { pub name: String, #[serde(skip_serializing_if = "Option::is_none")] pub description: Option<String>, #[serde(rename = "type")] pub kind: FlagKind, #[serde(skip_serializing_if = "Option::is_none")] pub default: Option<FlagValue>, pub extension_path: String }
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FlagKind { Boolean, String }
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderRegistration { pub name: String, pub config_dto: Value, #[serde(skip_serializing_if = "Option::is_none")] pub extension_path: Option<String> }
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolInfo { pub name: String, pub description: String, pub parameters: Value, #[serde(skip_serializing_if = "Option::is_none")] pub prompt_guidelines: Option<String>, pub source_info: SourceInfo }
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandInfo { pub name: String, #[serde(skip_serializing_if = "Option::is_none")] pub description: Option<String> }

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SetModelParams { pub model: Model }
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewSessionParams { #[serde(skip_serializing_if = "Option::is_none")] pub parent_session: Option<String>, #[serde(skip_serializing_if = "Option::is_none")] pub setup_token: Option<String>, #[serde(skip_serializing_if = "Option::is_none")] pub with_session_token: Option<String> }
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ForkParams { pub entry_id: String, #[serde(skip_serializing_if = "Option::is_none")] pub position: Option<ForkPosition>, #[serde(skip_serializing_if = "Option::is_none")] pub with_session_token: Option<String> }
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NavigateTreeParams { pub target_id: String, #[serde(skip_serializing_if = "Option::is_none")] pub summarize: Option<bool>, #[serde(skip_serializing_if = "Option::is_none")] pub custom_instructions: Option<String>, #[serde(skip_serializing_if = "Option::is_none")] pub replace_instructions: Option<bool>, #[serde(skip_serializing_if = "Option::is_none")] pub label: Option<String> }
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SwitchSessionParams { pub session_path: String, #[serde(skip_serializing_if = "Option::is_none")] pub with_session_token: Option<String> }
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SendMessageParams { pub message: Value, #[serde(skip_serializing_if = "Option::is_none")] pub trigger_turn: Option<bool>, #[serde(skip_serializing_if = "Option::is_none")] pub deliver_as: Option<Delivery> }
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SendUserMessageParams { pub content: Value, #[serde(skip_serializing_if = "Option::is_none")] pub deliver_as: Option<UserMessageDelivery> }
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Delivery { Steer, FollowUp, NextTurn }
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum UserMessageDelivery { Steer, FollowUp }
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppendEntryParams { pub custom_type: String, #[serde(skip_serializing_if = "Option::is_none")] pub data: Option<Value> }
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetSessionNameParams { pub name: String }
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetLabelParams { pub entry_id: String, #[serde(skip_serializing_if = "Option::is_none")] pub label: Option<String> }
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetActiveToolsParams { pub tool_names: Vec<String> }
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetThinkingLevelParams { pub level: ThinkingLevel }
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CompactParams { #[serde(skip_serializing_if = "Option::is_none")] pub options: Option<Value> }

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DialogOptions { #[serde(skip_serializing_if = "Option::is_none")] pub timeout: Option<u64> }
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectParams { pub title: String, pub options: Vec<String>, #[serde(flatten)] pub dialog: DialogOptions }
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfirmParams { pub title: String, pub message: String, #[serde(flatten)] pub dialog: DialogOptions }
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputDialogParams { pub title: String, #[serde(skip_serializing_if = "Option::is_none")] pub placeholder: Option<String>, #[serde(flatten)] pub dialog: DialogOptions }
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditorParams { pub title: String, pub text: String, #[serde(flatten)] pub dialog: DialogOptions }
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CustomDialogParams { pub slot: String, #[serde(default)] pub overlay: bool, #[serde(skip_serializing_if = "Option::is_none")] pub overlay_options: Option<Value> }
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RenderParams { pub slot: String, pub width: u16 }
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AutocompleteParams { pub text: String, pub cursor: usize, #[serde(skip_serializing_if = "Option::is_none")] pub command_name: Option<String> }
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalInputParams { pub data: String }

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FrameParams { pub slot: String, pub lines: Vec<String>, pub version: u64, pub wants_key_release: bool, pub focusable: bool, #[serde(skip_serializing_if = "Option::is_none")] pub placement: Option<WidgetPlacement> }
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum WidgetPlacement { AboveEditor, BelowEditor }
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComponentInputParams { pub slot: String, pub data: String }
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlotParams { pub slot: String }
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DoneParams { pub slot: String, pub result: Value }
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OverlayParams { pub slot: String, pub options: Value }
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotifyParams { pub message: String, pub level: NotificationLevel }
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NotificationLevel { Info, Warning, Error }
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyValueParams { pub key: String, #[serde(skip_serializing_if = "Option::is_none")] pub value: Option<String> }
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OptionalTextParams { #[serde(skip_serializing_if = "Option::is_none")] pub text: Option<String> }
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkingIndicatorParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub options: Option<WorkingIndicatorOptions>,
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkingIndicatorOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frames: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub interval_ms: Option<u64>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VisibleParams { pub visible: bool }
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextParams { pub text: String }
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThemeSelectionParams { pub theme: String }

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolExecuteParams { pub tool_call_id: String, pub name: String, pub args: Value }
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolUpdateParams { pub tool_call_id: String, pub partial: Value }
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderStreamParams { pub stream_id: String, pub provider: String, pub model: Model, pub context: Context, #[serde(skip_serializing_if = "Option::is_none")] pub options: Option<Value> }
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderEventParams { pub stream_id: String, pub event: AssistantMessageEvent }
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NameParams { pub name: String }
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandExecuteParams { pub name: String, pub args: String }
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShortcutInvokeParams { pub key_id: String }

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtensionError { pub extension_path: String, pub event: String, pub error: String, #[serde(skip_serializing_if = "Option::is_none")] pub stack: Option<String> }

#[derive(Debug, Error)]
pub enum FrameError {
    #[error("empty NDJSON frame")]
    Empty,
    #[error("NDJSON frame is {size} bytes, maximum is {max}")]
    Oversize { size: usize, max: usize },
    #[error("frame contains more than one JSON line")]
    MultipleLines,
    #[error("malformed JSON frame: {0}")]
    Malformed(#[from] serde_json::Error),
    #[error("invalid protocol value: {0}")]
    Invalid(#[from] ValidationError),
}

#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum ValidationError {
    #[error("protocol error message exceeds its size bound")]
    ErrorMessageTooLarge,
    #[error("protocol error stack exceeds its size bound")]
    ErrorStackTooLarge,
    #[error("unsupported extension protocol version {received}; supported version is {supported}")]
    UnsupportedVersion { received: u16, supported: u16 },
}

pub fn encode_frame(message: &Envelope) -> Result<Vec<u8>, FrameError> {
    validate_envelope(message)?;
    let mut bytes = serde_json::to_vec(message)?;
    let framed_size = bytes.len() + 1;
    if framed_size > MAX_FRAME_BYTES {
        return Err(FrameError::Oversize { size: framed_size, max: MAX_FRAME_BYTES });
    }
    bytes.push(b'\n');
    Ok(bytes)
}

pub fn decode_frame(frame: &[u8]) -> Result<Envelope, FrameError> {
    if frame.is_empty() || frame == b"\n" || frame == b"\r\n" {
        return Err(FrameError::Empty);
    }
    if frame.len() > MAX_FRAME_BYTES {
        return Err(FrameError::Oversize { size: frame.len(), max: MAX_FRAME_BYTES });
    }
    let body = frame.strip_suffix(b"\n").unwrap_or(frame);
    let body = body.strip_suffix(b"\r").unwrap_or(body);
    if body.contains(&b'\n') || body.contains(&b'\r') {
        return Err(FrameError::MultipleLines);
    }
    let message = serde_json::from_slice(body)?;
    validate_envelope(&message)?;
    Ok(message)
}

fn validate_envelope(message: &Envelope) -> Result<(), ValidationError> {
    match message {
        Envelope::Response { result: ResponseResult::Err { err }, .. } => err.validate()?,
        Envelope::Event { event: Notification::LifecycleHello(hello) } if hello.protocol != PROTOCOL_VERSION => {
            return Err(ValidationError::UnsupportedVersion {
                received: hello.protocol,
                supported: PROTOCOL_VERSION,
            });
        }
        Envelope::Event { event: Notification::ExtensionError(error) } => {
            validate_error_text(&error.error, error.stack.as_deref())?;
        }
        Envelope::Event { event: Notification::LifecycleInitialized(initialized) } => {
            for error in &initialized.errors {
                validate_error_text(&error.error, error.stack.as_deref())?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn validate_error_text(message: &str, stack: Option<&str>) -> Result<(), ValidationError> {
    if message.len() > MAX_ERROR_MESSAGE_BYTES {
        return Err(ValidationError::ErrorMessageTooLarge);
    }
    if stack.is_some_and(|value| value.len() > MAX_ERROR_STACK_BYTES) {
        return Err(ValidationError::ErrorStackTooLarge);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn id() -> RequestId { RequestId::new(7).expect("non-zero") }

    fn model() -> Model {
        serde_json::from_value(json!({
            "id":"m","name":"Model","api":"test-api","provider":"test","baseUrl":"https://example.test",
            "reasoning":false,"input":["text"],"cost":{"input":0.0,"output":0.0,"cacheRead":0.0,"cacheWrite":0.0},
            "contextWindow":1000,"maxTokens":100
        })).unwrap()
    }

    fn assistant_event() -> AssistantMessageEvent {
        AssistantMessageEvent::Done {
            reason: pi_ai::StopReason::Stop,
            message: pi_ai::AssistantMessage {
                content: vec![],
                api: pi_ai::Api("test-api".into()),
                provider: "test".into(),
                model: "m".into(),
                response_model: None,
                response_id: None,
                diagnostics: None,
                usage: pi_ai::Usage::default(),
                stop_reason: pi_ai::StopReason::Stop,
                error_message: None,
                timestamp: 1,
            },
        }
    }

    fn state() -> StateBlock {
        StateBlock {
            session_name: None, model: None, idle: true, project_trusted: true,
            pending_messages: false, active_tools: vec![], all_tools: vec![], commands: vec![],
            thinking_level: ThinkingLevel::Minimal, context_usage: None, system_prompt: String::new(),
            system_prompt_options: None, flag_values: BTreeMap::new(), editor_text: String::new(),
            tools_expanded: false, footer: None, theme: ThemeDto { name: "default".into(), json: json!({}) },
        }
    }

    #[test]
    fn envelope_variants_round_trip_and_correlate() {
        let messages = [
            Envelope::Request { id: id(), request: Request::LifecycleShutdown(Empty {}) },
            Envelope::Response { id: id(), result: ResponseResult::Ok { ok: json!({"done":true}) } },
            Envelope::Response { id: id(), result: ResponseResult::Err { err: ProtocolError { code: "E".into(), message: "bad".into(), stack: None, extension_path: None } } },
            Envelope::Event { event: Notification::LifecyclePing(HeartbeatParams { nonce: 3 }) },
            Envelope::Cancel { id: id() },
        ];
        for message in &messages {
            let encoded = encode_frame(message).unwrap();
            assert_eq!(decode_frame(&encoded).unwrap(), *message);
        }
        assert_eq!(messages[0].correlation(Direction::RustToSidecar), Some(CorrelationId { direction: Direction::RustToSidecar, id: id() }));
        assert_eq!(messages[3].correlation(Direction::RustToSidecar), None);
    }

    #[test]
    fn request_variants_round_trip() {
        let requests = vec![
            Request::LifecycleInit(Box::new(InitParams {
                cwd: "/x".into(), agent_dir: "/a".into(), session_dir: "/s".into(),
                configured_paths: vec![], mode: ExtensionMode::Tui, has_ui: true,
                flag_values: BTreeMap::new(), theme: ThemeDto { name: "default".into(), json: json!({}) },
                session: SessionSnapshot { epoch: 1, session_file: "s".into(), header: None, entries: vec![], leaf_id: None, name: None },
                state: state(),
            })),
            Request::LifecycleLoad(LoadParams { paths: vec!["x.ts".into()] }),
            Request::LifecycleShutdown(Empty {}),
            Request::EventEmit(Box::new(EventDispatch { event: ExtensionEvent::ProjectTrust { cwd: "/x".into() }, state: state() })),
            Request::ActionSetModel(Box::new(SetModelParams { model: model() })),
            Request::ActionWaitForIdle(Empty {}), Request::ActionReload(Empty {}),
            Request::ActionNewSession(NewSessionParams::default()),
            Request::ActionFork(ForkParams { entry_id: "e".into(), position: None, with_session_token: None }),
            Request::ActionNavigateTree(NavigateTreeParams { target_id: "e".into(), summarize: None, custom_instructions: None, replace_instructions: None, label: None }),
            Request::ActionSwitchSession(SwitchSessionParams { session_path: "s".into(), with_session_token: None }),
            Request::ActionReplacedSendMessage(SendMessageParams { message: json!({"customType":"x","content":"y"}), trigger_turn: None, deliver_as: None }),
            Request::ActionReplacedSendUserMessage(SendUserMessageParams { content: json!("hi"), deliver_as: None }),
            Request::UiSelect(SelectParams { title: "pick".into(), options: vec!["a".into()], dialog: DialogOptions::default() }),
            Request::UiConfirm(ConfirmParams { title: "sure".into(), message: "?".into(), dialog: DialogOptions { timeout: Some(5) } }),
            Request::UiInput(InputDialogParams { title: "name".into(), placeholder: None, dialog: DialogOptions::default() }),
            Request::UiEditor(EditorParams { title: "edit".into(), text: "x".into(), dialog: DialogOptions::default() }),
            Request::UiCustom(CustomDialogParams { slot: "custom:1".into(), overlay: true, overlay_options: Some(json!({"anchor":"center"})) }),
            Request::UiRender(RenderParams { slot: "footer".into(), width: 80 }),
            Request::UiAutocomplete(AutocompleteParams { text: "/x".into(), cursor: 2, command_name: Some("x".into()) }),
            Request::UiTerminalInput(TerminalInputParams { data: "a".into() }),
            Request::UiGetAllThemes(Empty {}),
            Request::UiGetTheme(NameParams { name: "dark".into() }),
            Request::ToolExecute(ToolExecuteParams { tool_call_id: "t".into(), name: "x".into(), args: json!({"nested":[1,true]}) }),
            Request::ProviderStream(Box::new(ProviderStreamParams { stream_id: "s".into(), provider: "p".into(), model: model(), context: Context::default(), options: None })),
            Request::CommandExecute(CommandExecuteParams { name: "x".into(), args: "a".into() }),
            Request::ShortcutInvoke(ShortcutInvokeParams { key_id: "ctrl+x".into() }),
            Request::SessionSetup(SessionSetupParams { token: "t".into(), session_file: "s".into() }),
        ];
        for request in requests {
            let envelope = Envelope::Request { id: id(), request };
            assert_eq!(decode_frame(&encode_frame(&envelope).unwrap()).unwrap(), envelope);
        }
    }

    #[test]
    fn notification_variants_round_trip() {
        let notifications = vec![
            Notification::LifecycleInitialized(InitializedParams { registrations: Registrations::default(), subscribed_events: vec![ExtensionEventKind::Input], errors: vec![] }),
            Notification::LifecycleHello(HelloParams { protocol: 1, pi: PI_COMPAT_VERSION.into(), bun: "1.2".into() }),
            Notification::LifecyclePing(HeartbeatParams { nonce: 1 }), Notification::LifecyclePong(HeartbeatParams { nonce: 1 }),
            Notification::EventNotify(Box::new(EventDispatch { event: ExtensionEvent::AgentStart {}, state: state() })),
            Notification::ActionSendMessage(SendMessageParams { message: json!({"customType":"x"}), trigger_turn: None, deliver_as: None }),
            Notification::ActionSendUserMessage(SendUserMessageParams { content: json!("hi"), deliver_as: None }),
            Notification::ActionAppendEntry(AppendEntryParams { custom_type: "x".into(), data: Some(json!({"a":1})) }),
            Notification::ActionSetSessionName(SetSessionNameParams { name: "n".into() }),
            Notification::ActionSetLabel(SetLabelParams { entry_id: "e".into(), label: None }),
            Notification::ActionSetActiveTools(SetActiveToolsParams { tool_names: vec!["read".into()] }),
            Notification::ActionRefreshTools(Empty {}), Notification::ActionShutdown(Empty {}), Notification::ActionAbort(Empty {}),
            Notification::ActionSetThinkingLevel(SetThinkingLevelParams { level: ThinkingLevel::High }),
            Notification::ActionCompact(CompactParams::default()),
            Notification::UiNotify(NotifyParams { message: "hi".into(), level: NotificationLevel::Info }),
            Notification::UiSetStatus(KeyValueParams { key: "x".into(), value: None }),
            Notification::UiSetWorkingMessage(OptionalTextParams { text: None }),
            Notification::UiSetWorkingVisible(VisibleParams { visible: true }),
            Notification::UiSetWorkingIndicator(WorkingIndicatorParams { options: Some(WorkingIndicatorOptions { frames: Some(vec!["x".into()]), interval_ms: None }) }),
            Notification::UiSetHiddenThinkingLabel(OptionalTextParams { text: None }),
            Notification::UiSetTitle(TextParams { text: "pi".into() }),
            Notification::UiSetEditorText(TextParams { text: "x".into() }),
            Notification::UiPasteToEditor(TextParams { text: "x".into() }),
            Notification::UiSetTheme(ThemeSelectionParams { theme: "dark".into() }),
            Notification::UiSetToolsExpanded(VisibleParams { visible: true }),
            Notification::UiFrame(FrameParams { slot: "footer".into(), lines: vec!["x".into()], version: 1, wants_key_release: false, focusable: false, placement: None }),
            Notification::UiComponentInput(ComponentInputParams { slot: "editor".into(), data: "x".into() }),
            Notification::UiDispose(SlotParams { slot: "x".into() }), Notification::UiDone(DoneParams { slot: "x".into(), result: json!({"ok":1}) }),
            Notification::UiOverlay(OverlayParams { slot: "x".into(), options: json!({"width":10}) }),
            Notification::ToolUpdate(ToolUpdateParams { tool_call_id: "t".into(), partial: json!({"content":"x"}) }),
            Notification::ProviderRegister(ProviderRegistration { name: "p".into(), config_dto: json!({"baseUrl":"https://example.test"}), extension_path: None }),
            Notification::ProviderUnregister(NameParams { name: "p".into() }),
            Notification::ProviderEvent(Box::new(ProviderEventParams { stream_id: "s".into(), event: assistant_event() })),
            Notification::SessionSync(SessionSyncParams { epoch: 1, session_file: "s".into(), header: None, entries: Some(vec![json!({"type":"message","open":{"x":1}})]), appended: None, leaf_id: None, name: None }),
            Notification::StateUpdate(Box::new(StateUpdate { idle: Some(true), ..StateUpdate::default() })),
            Notification::ExtensionError(ExtensionError { extension_path: "x.ts".into(), event: "input".into(), error: "bad".into(), stack: None }),
        ];
        for event in notifications {
            let envelope = Envelope::Event { event };
            assert_eq!(decode_frame(&encode_frame(&envelope).unwrap()).unwrap(), envelope);
        }
    }

    #[test]
    fn every_extension_event_kind_round_trips() {
        let events = vec![
            ExtensionEvent::ProjectTrust { cwd: "/x".into() },
            ExtensionEvent::ResourcesDiscover { cwd: "/x".into(), reason: DiscoverReason::Startup },
            ExtensionEvent::SessionStart { reason: SessionStartReason::Startup, previous_session_file: None },
            ExtensionEvent::SessionInfoChanged { name: None },
            ExtensionEvent::SessionBeforeSwitch { reason: SwitchReason::New, target_session_file: None },
            ExtensionEvent::SessionBeforeFork { entry_id: "e".into(), position: ForkPosition::At },
            ExtensionEvent::SessionBeforeCompact { preparation: json!({"firstKeptEntryId":"e"}), branch_entries: vec![json!({"type":"message"})], custom_instructions: None, reason: CompactReason::Manual, will_retry: false },
            ExtensionEvent::SessionCompact { compaction_entry: json!({"type":"compaction"}), from_extension: false, reason: CompactReason::Manual, will_retry: false },
            ExtensionEvent::SessionShutdown { reason: ShutdownReason::Reload, target_session_file: None },
            ExtensionEvent::SessionBeforeTree { preparation: TreePreparation { target_id: "e".into(), old_leaf_id: None, common_ancestor_id: None, entries_to_summarize: vec![], user_wants_summary: false, custom_instructions: None, replace_instructions: None, label: None } },
            ExtensionEvent::SessionTree { new_leaf_id: Some("e".into()), old_leaf_id: None, summary_entry: None, from_extension: None },
            ExtensionEvent::Context { messages: vec![AgentMessage::Custom(json!({"role":"custom","opaque":{"x":1}}))] },
            ExtensionEvent::BeforeProviderRequest { payload: json!({"unknown":[1,{"x":true}]}) },
            ExtensionEvent::BeforeProviderHeaders { headers: BTreeMap::from([("x".into(), None)]) },
            ExtensionEvent::AfterProviderResponse { status: 200, headers: BTreeMap::new() },
            ExtensionEvent::BeforeAgentStart { prompt: "hi".into(), images: None, system_prompt: "sys".into(), system_prompt_options: BuildSystemPromptOptions { custom_prompt: None, selected_tools: None, tool_snippets: None, prompt_guidelines: None, append_system_prompt: None, cwd: "/x".into(), context_files: None, skills: None } },
            ExtensionEvent::AgentStart {}, ExtensionEvent::AgentEnd { messages: vec![] }, ExtensionEvent::AgentSettled {},
            ExtensionEvent::TurnStart { turn_index: 1, timestamp: 2 },
            ExtensionEvent::TurnEnd { turn_index: 1, message: AgentMessage::Custom(json!({})), tool_results: vec![] },
            ExtensionEvent::MessageStart { message: AgentMessage::Custom(json!({})) },
            ExtensionEvent::MessageUpdate { message: AgentMessage::Custom(json!({})), assistant_message_event: Box::new(assistant_event()) },
            ExtensionEvent::MessageEnd { message: AgentMessage::Custom(json!({})) },
            ExtensionEvent::ToolExecutionStart { tool_call_id: "t".into(), tool_name: "x".into(), args: json!({}) },
            ExtensionEvent::ToolExecutionUpdate { tool_call_id: "t".into(), tool_name: "x".into(), args: json!({}), partial_result: json!({}) },
            ExtensionEvent::ToolExecutionEnd { tool_call_id: "t".into(), tool_name: "x".into(), result: json!({}), is_error: false },
            ExtensionEvent::ModelSelect { model: Box::new(model()), previous_model: None, source: ModelSelectSource::Set },
            ExtensionEvent::ThinkingLevelSelect { level: ThinkingLevel::High, previous_level: ThinkingLevel::Low },
            ExtensionEvent::UserBash { command: "true".into(), exclude_from_context: false, cwd: "/x".into() },
            ExtensionEvent::Input { text: "x".into(), images: None, source: InputSource::Rpc, streaming_behavior: None },
            ExtensionEvent::ToolCall { tool_call_id: "t".into(), tool_name: "x".into(), input: json!({"future":1}) },
            ExtensionEvent::ToolResult { tool_call_id: "t".into(), tool_name: "x".into(), input: JsonObject::new(), content: vec![], is_error: false, details: Some(json!({"future":1})) },
        ];
        for event in events {
            let value = serde_json::to_value(&event).unwrap();
            assert_eq!(serde_json::from_value::<ExtensionEvent>(value).unwrap(), event);
        }
    }

    #[test]
    fn field_order_and_omission_are_golden() {
        let message = Envelope::Request { id: id(), request: Request::UiInput(InputDialogParams { title: "Name".into(), placeholder: None, dialog: DialogOptions::default() }) };
        assert_eq!(String::from_utf8(encode_frame(&message).unwrap()).unwrap(), "{\"type\":\"req\",\"id\":7,\"method\":\"ui/input\",\"params\":{\"title\":\"Name\"}}\n");
        let error = Envelope::Response { id: id(), result: ResponseResult::Err { err: ProtocolError { code: "BAD".into(), message: "no".into(), stack: None, extension_path: None } } };
        assert_eq!(String::from_utf8(encode_frame(&error).unwrap()).unwrap(), "{\"type\":\"res\",\"id\":7,\"err\":{\"code\":\"BAD\",\"message\":\"no\"}}\n");
    }

    #[test]
    fn send_user_message_rejects_next_turn_delivery() {
        let invalid = br#"{"type":"ev","method":"action/sendUserMessage","params":{"content":"later","deliverAs":"nextTurn"}}
"#;
        assert!(matches!(decode_frame(invalid), Err(FrameError::Malformed(_))));

        let valid = Envelope::Event {
            event: Notification::ActionSendMessage(SendMessageParams {
                message: json!("later"),
                trigger_turn: None,
                deliver_as: Some(Delivery::NextTurn),
            }),
        };
        assert_eq!(decode_frame(&encode_frame(&valid).unwrap()).unwrap(), valid);
    }

    #[test]
    fn rejects_unknown_version_malformed_and_oversize_frames() {
        let hello = HelloParams { protocol: 2, pi: PI_COMPAT_VERSION.into(), bun: "1".into() };
        assert!(matches!(hello.negotiate(), Err(VersionError::Unsupported { received: 2, supported: 1 })));
        let unknown = serde_json::to_vec(&Envelope::Event {
            event: Notification::LifecycleHello(hello),
        }).unwrap();
        assert!(matches!(
            decode_frame(&unknown),
            Err(FrameError::Invalid(ValidationError::UnsupportedVersion { received: 2, supported: 1 }))
        ));
        assert!(matches!(decode_frame(b"not json\n"), Err(FrameError::Malformed(_))));
        assert!(matches!(decode_frame(b"{}\n{}\n"), Err(FrameError::MultipleLines)));
        let oversized = vec![b'x'; MAX_FRAME_BYTES + 1];
        assert!(matches!(decode_frame(&oversized), Err(FrameError::Oversize { .. })));
        assert!(matches!(
            decode_frame(b"{\"type\":\"cancel\",\"id\":0}\n"),
            Err(FrameError::Malformed(_))
        ));
    }

    #[test]
    fn preserves_open_json_payloads_exactly() {
        let payload = json!({"z":null,"nested":[1,"two",{"future":true}],"number":1.25});
        let message = Envelope::Request { id: id(), request: Request::ToolExecute(ToolExecuteParams { tool_call_id: "t".into(), name: "custom".into(), args: payload.clone() }) };
        let decoded = decode_frame(&encode_frame(&message).unwrap()).unwrap();
        let Envelope::Request { request: Request::ToolExecute(params), .. } = decoded else { panic!("wrong message") };
        assert_eq!(params.args, payload);
    }

    #[test]
    fn classifies_blocking_hooks_and_notifications() {
        assert_eq!(Request::ActionWaitForIdle(Empty {}).class(), MessageClass::BlockingRequest);
        assert_eq!(Notification::LifecyclePing(HeartbeatParams { nonce: 1 }).class(), MessageClass::Notification);
        assert!(ExtensionEvent::ProjectTrust { cwd: "/x".into() }.is_blocking());
        assert!(!ExtensionEvent::AgentStart {}.is_blocking());
    }

    #[test]
    fn typed_success_payload_decodes_without_shape_loss() {
        let expected = SessionBeforeForkResult { cancel: Some(true), skip_conversation_restore: Some(false) };
        let result = ResponseResult::ok(&expected).unwrap();
        assert_eq!(result.decode_ok::<SessionBeforeForkResult>().unwrap(), Some(expected));

        let terminal_input = TerminalInputResult { consume: Some(false), data: Some("rewritten".into()) };
        let result = ResponseResult::ok(&terminal_input).unwrap();
        assert_eq!(result.decode_ok::<TerminalInputResult>().unwrap(), Some(terminal_input));

        let catalog = GetAllThemesResult(vec![
            ThemeCatalogEntry { name: "dark".into(), path: Some("/themes/dark.json".into()) },
            ThemeCatalogEntry { name: "builtin".into(), path: None },
        ]);
        let result = ResponseResult::ok(&catalog).unwrap();
        assert_eq!(result.decode_ok::<GetAllThemesResult>().unwrap(), Some(catalog));

        let theme = GetThemeResult(Some(ThemeDto { name: "dark".into(), json: json!({"name":"dark"}) }));
        let result = ResponseResult::ok(&theme).unwrap();
        assert_eq!(result.decode_ok::<GetThemeResult>().unwrap(), Some(theme));

        let old_json = json!({
            "content": [],
            "isError": false
        });
        let old_decoded: ToolExecuteResult = serde_json::from_value(old_json.clone()).unwrap();
        assert_eq!(
            old_decoded,
            ToolExecuteResult {
                content: vec![],
                details: None,
                is_error: false,
                added_tool_names: None,
                terminate: None,
            }
        );
        let old_serialized = serde_json::to_value(&old_decoded).unwrap();
        assert_eq!(old_serialized, old_json);

        let new_json = json!({
            "content": [{"type": "text", "text": "hello"}],
            "details": {"progress": 1.0},
            "isError": false,
            "addedToolNames": ["new_tool"],
            "terminate": true
        });
        let new_decoded: ToolExecuteResult = serde_json::from_value(new_json.clone()).unwrap();
        let expected_new = ToolExecuteResult {
            content: vec![Content::Text(pi_ai::TextContent {
                text: "hello".into(),
                text_signature: None,
            })],
            details: Some(json!({"progress": 1.0})),
            is_error: false,
            added_tool_names: Some(vec!["new_tool".into()]),
            terminate: Some(true),
        };
        assert_eq!(new_decoded, expected_new);
        let serialized_new_str = serde_json::to_string(&expected_new).unwrap();
        let expected_json_str = "{\"content\":[{\"type\":\"text\",\"text\":\"hello\"}],\"details\":{\"progress\":1.0},\"isError\":false,\"addedToolNames\":[\"new_tool\"],\"terminate\":true}";
        assert_eq!(serialized_new_str, expected_json_str);

        let result = ResponseResult::ok(&expected_new).unwrap();
        assert_eq!(result.decode_ok::<ToolExecuteResult>().unwrap(), Some(expected_new));
    }

    #[test]
    fn enforces_error_bounds() {
        let err = ProtocolError { code: "E".into(), message: "x".repeat(MAX_ERROR_MESSAGE_BYTES + 1), stack: None, extension_path: None };
        let message = Envelope::Response { id: id(), result: ResponseResult::Err { err } };
        assert!(matches!(encode_frame(&message), Err(FrameError::Invalid(ValidationError::ErrorMessageTooLarge))));
    }
}
