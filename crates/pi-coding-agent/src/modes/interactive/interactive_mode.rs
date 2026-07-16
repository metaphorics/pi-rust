//! InteractiveMode — the TUI loop.
//!
//! Port of `modes/interactive/interactive-mode.ts`. Handles rendering and
//! user interaction, delegating business logic to [`AgentSession`]. The
//! component tree mirrors the oracle (header, loaded resources, chat, pending
//! messages, status, editor slot, footer); mutable components are shared via
//! [`super::shared::Shared`] so event handlers can mutate mounted components.
//!
//! Architecture differences from the TS oracle (single-threaded callbacks →
//! Rust ownership) are mechanical, not behavioral:
//! - editor/selector callbacks push [`UiCommand`]s drained by the loop;
//! - session events arrive over an mpsc channel from the `subscribe` seam;
//! - async work (prompts, bash, compaction) runs on a local `FuturesUnordered`.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;
use std::task::{Context as TaskContext, Poll, Waker};
use std::time::{Duration, Instant};

use pi_agent::{AgentMessage, AgentThinkingLevel, AgentToolResult};
use pi_ai::{Message, Model, ModelThinkingLevel, StopReason, Transport};
use pi_tui::autocomplete::{CombinedAutocompleteProvider, CommandEntry, SlashCommand};
use pi_tui::component::{Component, ComponentBox};
use pi_tui::components::editor::{Editor, EditorOptions, EditorTheme, EditorTui};
use pi_tui::components::markdown::Markdown;
use pi_tui::components::spacer::Spacer;
use pi_tui::components::text::Text;
use pi_tui::components::truncated_text::TruncatedText;
use pi_tui::keybindings::set_keybindings;
use pi_tui::terminal::Terminal;
use pi_tui::{Container, Tui};

use super::app_keybindings::create_app_keybindings;
use super::components::assistant_message::AssistantMessageComponent;
use super::components::bash_execution::BashExecutionComponent;
use super::components::branch_summary_message::BranchSummaryMessageComponent;
use super::components::compaction_summary_message::CompactionSummaryMessageComponent;
use super::components::custom_editor::CustomEditor;
use super::components::custom_entry::CustomEntryComponent;
use super::components::dynamic_border::DynamicBorder;
use super::components::extension_selector::ExtensionSelector;
use super::components::footer::{FooterComponent, FooterData, FooterStats};
use super::components::keybinding_hints::{key_display_text, key_text};
use super::components::login_dialog::LoginDialogComponent;
use super::components::model_selector::ModelSelectorComponent;
use super::components::oauth_selector::{
    AuthType, OAuthProvider, OAuthSelector, OAuthSelectorMode,
};
use super::components::session_selector::{SessionSelectorComponent, SessionSelectorOptions};
use super::components::settings_selector::{
    SettingsCallbacks, SettingsConfig, SettingsSelectorComponent,
};
use super::components::show_images_selector::ShowImagesSelectorComponent;
use super::components::status_indicator::{
    CompactionStatusReason, IdleStatus, StatusIndicator, StatusIndicatorKind,
};
use super::components::theme_selector::ThemeSelectorComponent;
use super::components::thinking_selector::ThinkingSelectorComponent;
use super::components::tool_execution::ToolExecutionComponent;
use super::components::tree_selector::TreeSelectorComponent;
use super::components::trust_selector::{
    TrustSelection, TrustSelectorComponent, TrustSelectorOptions,
};
use super::components::user_message::UserMessageComponent;
use super::components::user_message_selector::{UserMessageItem, UserMessageSelectorComponent};
use super::dispatch::{BuiltinCommand, DispatchAction, DispatchContext, dispatch_input};
use super::shared::{Shared, SlotHandle, SwapSlot};
use super::theme::{
    ThemeColor, current_theme_name, detect_terminal_background_from_env, get_available_themes,
    on_theme_change, set_theme, theme,
};
use crate::session::events::{AgentSessionEvent, CompactionReason};
use crate::session::runtime::AgentSessionRuntime;
use crate::session::{AgentSession, PromptOptions, StreamingBehavior};
use crate::session_manager::SessionManager;
use crate::session_types::SessionEntry;

/// Oracle `quoteIfNeeded` (interactive-mode.ts:225-230).
#[must_use]
pub fn quote_if_needed(value: &str) -> String {
    if !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
    {
        value.to_owned()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

/// Oracle `formatResumeCommand` (interactive-mode.ts:232-245): the shell
/// command that reopens this session, or `None` for in-memory sessions.
#[must_use]
pub fn format_resume_command(session_manager: &SessionManager) -> Option<String> {
    session_manager.get_session_file()?;
    let session_id = session_manager.get_session_id().to_string();
    let mut command = String::from("pi");
    if !session_manager.uses_default_session_dir() {
        command.push_str(" --session-dir ");
        command.push_str(&quote_if_needed(
            &session_manager.get_session_dir().to_string_lossy(),
        ));
    }
    command.push_str(" --session ");
    command.push_str(&quote_if_needed(&session_id));
    Some(command)
}

/// Options for InteractiveMode (oracle `InteractiveModeOptions`, subset that
/// exists pre-Phase-6).
#[derive(Default)]
pub struct InteractiveModeOptions {
    pub initial_message: Option<String>,
    pub initial_messages: Vec<String>,
    pub model_fallback_message: Option<String>,
    /// Install SIGTERM/SIGHUP handlers (binary entry points only).
    pub handle_signals: bool,
    /// Time source for double-press windows (tests inject a fake clock).
    pub clock: Option<Rc<dyn Fn() -> Instant>>,
}

// ============================================================================
// Editor host signal
// ============================================================================

/// Per-mode editor host: collects render requests and mirrors terminal rows
/// (the pi-tui `Editor` sees its TUI through this seam).
struct EditorSignal {
    render_requested: Cell<bool>,
    rows: Cell<u16>,
}

impl EditorTui for EditorSignal {
    fn request_render(&self) {
        self.render_requested.set(true);
    }
    fn terminal_rows(&self) -> u16 {
        self.rows.get()
    }
}

// ============================================================================
// UI command queue (editor/selector callbacks → loop)
// ============================================================================

/// Commands pushed by component callbacks, drained by the loop.
enum UiCommand {
    /// Editor submit.
    Submit(String),
    /// Editor text changed (bash-mode border tracking).
    EditorChanged(String),
    /// `app.*` keybinding intercepted before the editor.
    Action(AppAction),
    /// Restore the editor into the slot and refocus it.
    RestoreEditor,
    ModelSelected(Box<Model>),
    ThemeSelected(String),
    ThemePreview(String),
    SessionSelected(PathBuf),
    SessionSelectorExit,
    ForkSelected(String),
    TreeSelected(String),
    TrustSelected(Box<TrustSelection>),
    SettingChanged(Box<SettingChange>),
    LoginProviderSelected(String, AuthType),
    LoginApiKey(String, String),
    LogoutProviderSelected(String, AuthType),
    OAuthPromptSubmitted(String),
    OAuthSelectSubmitted(Option<String>),
    OAuthCancelled,
}

enum SettingChange {
    AutoCompact(bool),
    Steering(String),
    FollowUp(String),
    Thinking(ModelThinkingLevel),
    Theme(String),
    Top {
        key: &'static str,
        value: serde_json::Value,
        rebuild_chat: bool,
    },
    Nested {
        section: &'static str,
        key: &'static str,
        value: serde_json::Value,
        rebuild_chat: bool,
    },
    HideThinking(bool),
    OutputPad(u8),
    HardwareCursor(bool),
    EditorPadding(u32),
    AutocompleteMax(u32),
    ClearOnShrink(bool),
    HttpIdleTimeout(u64),
}

fn agent_thinking_to_model(level: AgentThinkingLevel) -> ModelThinkingLevel {
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

fn model_thinking_to_agent(level: ModelThinkingLevel) -> AgentThinkingLevel {
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

fn queue_top_bool(
    queue: Rc<RefCell<VecDeque<UiCommand>>>,
    key: &'static str,
    rebuild_chat: bool,
) -> Box<dyn Fn(bool)> {
    Box::new(move |value| {
        queue
            .borrow_mut()
            .push_back(UiCommand::SettingChanged(Box::new(SettingChange::Top {
                key,
                value: serde_json::Value::Bool(value),
                rebuild_chat,
            })));
    })
}

fn queue_nested_bool(
    queue: Rc<RefCell<VecDeque<UiCommand>>>,
    section: &'static str,
    key: &'static str,
    rebuild_chat: bool,
) -> Box<dyn Fn(bool)> {
    Box::new(move |value| {
        queue
            .borrow_mut()
            .push_back(UiCommand::SettingChanged(Box::new(SettingChange::Nested {
                section,
                key,
                value: serde_json::Value::Bool(value),
                rebuild_chat,
            })));
    })
}

fn queue_top_string(
    queue: Rc<RefCell<VecDeque<UiCommand>>>,
    key: &'static str,
) -> Box<dyn Fn(&str)> {
    Box::new(move |value| {
        queue
            .borrow_mut()
            .push_back(UiCommand::SettingChanged(Box::new(SettingChange::Top {
                key,
                value: serde_json::Value::String(value.to_owned()),
                rebuild_chat: false,
            })));
    })
}

fn create_autocomplete_provider(
    session: &AgentSession,
    runtime: &AgentSessionRuntime,
    enable_skill_commands: bool,
    cwd: &std::path::Path,
) -> CombinedAutocompleteProvider {
    let mut entries: Vec<CommandEntry> = super::dispatch::BUILTIN_SLASH_COMMANDS
        .iter()
        .map(|command| {
            CommandEntry::Slash(SlashCommand {
                name: command.name.to_owned(),
                description: Some(command.description.to_owned()),
                argument_hint: command.argument_hint.map(str::to_owned),
                get_argument_completions: None,
            })
        })
        .collect();
    let builtin_names: HashSet<&str> = super::dispatch::BUILTIN_SLASH_COMMANDS
        .iter()
        .map(|command| command.name)
        .collect();
    entries.extend(session.prompt_templates().into_iter().map(|template| {
        CommandEntry::Slash(SlashCommand {
            name: template.name,
            description: Some(template.description),
            argument_hint: template.argument_hint,
            get_argument_completions: None,
        })
    }));
    entries.extend(
        runtime
            .bridge()
            .registered_commands()
            .into_iter()
            .filter(|command| !builtin_names.contains(command.invocation_name.as_str()))
            .map(|command| {
                CommandEntry::Slash(SlashCommand {
                    name: command.invocation_name,
                    description: command.description,
                    argument_hint: None,
                    get_argument_completions: None,
                })
            }),
    );
    if enable_skill_commands {
        entries.extend(session.skills().into_iter().map(|skill| {
            CommandEntry::Slash(SlashCommand {
                name: format!("skill:{}", skill.name),
                description: Some(skill.description),
                argument_hint: None,
                get_argument_completions: None,
            })
        }));
    }
    CombinedAutocompleteProvider::new(entries, cwd)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AppAction {
    Interrupt,
    Clear,
    Exit,
    ThinkingCycle,
    ModelCycleForward,
    ModelCycleBackward,
    ModelSelect,
    ToolsExpand,
    ThinkingToggle,
    MessageCopy,
    MessageFollowUp,
    MessageDequeue,
    SessionNew,
    SessionTree,
    SessionFork,
    SessionResume,
}

const INTERCEPTED_ACTIONS: &[(&str, AppAction)] = &[
    ("app.clear", AppAction::Clear),
    ("app.thinking.cycle", AppAction::ThinkingCycle),
    ("app.model.cycleForward", AppAction::ModelCycleForward),
    ("app.model.cycleBackward", AppAction::ModelCycleBackward),
    ("app.model.select", AppAction::ModelSelect),
    ("app.tools.expand", AppAction::ToolsExpand),
    ("app.thinking.toggle", AppAction::ThinkingToggle),
    ("app.message.copy", AppAction::MessageCopy),
    ("app.message.followUp", AppAction::MessageFollowUp),
    ("app.message.dequeue", AppAction::MessageDequeue),
    ("app.session.new", AppAction::SessionNew),
    ("app.session.tree", AppAction::SessionTree),
    ("app.session.fork", AppAction::SessionFork),
    ("app.session.resume", AppAction::SessionResume),
];

/// Escape-handler override while a background task runs (oracle swaps
/// `defaultEditor.onEscape` and restores it afterwards).
#[derive(Clone, Copy, PartialEq, Eq)]
enum EscapeOverride {
    AbortCompaction,
    AbortRetry,
}

/// Results of async operations resumed on the loop.
enum OpOutcome {
    PromptFinished(Result<(), String>),
    BashFinished {
        component: Rc<RefCell<BashExecutionComponent>>,
        result: Result<crate::session::BashResult, String>,
        command: String,
        excluded: bool,
    },
    CompactFinished(Result<(), String>),
    MountModelSelector(Box<ModelSelectorComponent>),
    ModelSet {
        model: Box<Model>,
        result: Result<(), String>,
    },
    ModelCycled(Option<Box<crate::session::ModelCycleResult>>),
    SessionSwitched(Result<crate::session::runtime::ReplaceResult, String>),
    NewSessionCreated(Result<crate::session::runtime::ReplaceResult, String>),
    ForkFinished(Result<crate::session::runtime::ReplaceResult, String>),
    TreeNavigated(Result<crate::session::NavigateTreeResult, String>),
    FlushQueuePromptFailed(String),
    AuthChanged {
        provider: String,
        auth_type: AuthType,
        logging_in: bool,
        result: Result<(), String>,
    },
}

/// One queued-during-compaction message (oracle `CompactionQueuedMessage`).
struct CompactionQueuedMessage {
    text: String,
    mode: StreamingBehavior,
}

enum OAuthUiRequest {
    Auth(pi_ai::oauth::OAuthAuthInfo),
    DeviceCode(pi_ai::oauth::OAuthDeviceCodeInfo),
    Prompt(
        pi_ai::oauth::OAuthPrompt,
        tokio::sync::oneshot::Sender<String>,
    ),
    Select(
        pi_ai::oauth::OAuthSelectPrompt,
        tokio::sync::oneshot::Sender<Option<String>>,
    ),
    Progress(String),
}

/// Why the loop should exit.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ExitReason {
    Quit,
    Signal,
}

/// Outcome of a completed run, for the host binary to act on.
pub struct RunOutcome {
    pub exit_code: i32,
    /// "To resume this session: pi --session …" (host prints after TUI stop).
    pub farewell: Option<String>,
}

/// Single-threaded pending-operation set, polled once per pump tick with a
/// no-op waker (the 16ms loop tick re-polls; no cross-thread wakeups needed).
struct LocalOps {
    futures: Vec<Pin<Box<dyn Future<Output = OpOutcome>>>>,
}

impl LocalOps {
    fn new() -> Self {
        Self {
            futures: Vec::new(),
        }
    }

    fn push(&mut self, future: Pin<Box<dyn Future<Output = OpOutcome>>>) {
        self.futures.push(future);
    }

    fn poll_ready(&mut self) -> Vec<OpOutcome> {
        let mut cx = TaskContext::from_waker(Waker::noop());
        let mut ready = Vec::new();
        self.futures
            .retain_mut(|future| match future.as_mut().poll(&mut cx) {
                Poll::Ready(outcome) => {
                    ready.push(outcome);
                    false
                }
                Poll::Pending => true,
            });
        ready
    }
}

// ============================================================================
// InteractiveMode
// ============================================================================

pub struct InteractiveMode {
    runtime: Arc<AgentSessionRuntime>,
    session: AgentSession,
    tui: Tui,

    // Shared mounted components (root order mirrors the oracle).
    chat: Rc<RefCell<Container>>,
    pending_messages: Rc<RefCell<Container>>,
    status: Rc<RefCell<Container>>,
    footer: Rc<RefCell<FooterComponent>>,
    editor: Rc<RefCell<Editor<'static>>>,
    custom_editor: Rc<RefCell<CustomEditor<Shared<Editor<'static>>>>>,
    editor_slot: SlotHandle,
    editor_signal: Rc<EditorSignal>,

    commands: Rc<RefCell<VecDeque<UiCommand>>>,
    events_rx: tokio::sync::mpsc::UnboundedReceiver<AgentSessionEvent>,
    ops: LocalOps,
    unsubscribe: Option<Box<dyn FnOnce() + Send>>,

    // Streaming/tool state (oracle fields).
    streaming_component: Option<(usize, Rc<RefCell<AssistantMessageComponent>>)>,
    pending_tools: HashMap<String, Rc<RefCell<ToolExecutionComponent>>>,
    tool_output_expanded: bool,
    hide_thinking_block: bool,
    output_pad: usize,

    // Status line/indicator state.
    active_status: Option<(StatusIndicatorKind, Rc<RefCell<StatusIndicator>>)>,
    last_status_text: Option<(usize, Rc<RefCell<Text>>)>,
    working_message: Option<String>,
    working_visible: bool,

    // Input gating state.
    escape_override: Option<EscapeOverride>,
    last_sigint_time: Option<Instant>,
    last_escape_time: Option<Instant>,
    /// Monotonic time source (defaults to `Instant::now`).
    now: Rc<dyn Fn() -> Instant>,
    is_bash_mode: bool,
    selector_open: bool,
    startup_messages: VecDeque<String>,
    compaction_queued: Vec<CompactionQueuedMessage>,
    bash_component: Option<Rc<RefCell<BashExecutionComponent>>>,
    bash_chunks: Option<Arc<parking_lot::Mutex<Vec<String>>>>,
    oauth_ui_tx: std::sync::mpsc::Sender<OAuthUiRequest>,
    oauth_ui_rx: std::sync::mpsc::Receiver<OAuthUiRequest>,
    oauth_dialog: Option<Rc<RefCell<LoginDialogComponent>>>,
    oauth_prompt_reply: Option<tokio::sync::oneshot::Sender<String>>,
    oauth_select_reply: Option<tokio::sync::oneshot::Sender<Option<String>>>,
    oauth_cancel: Option<pi_ai::oauth::device_code::CancellationFlag>,

    options: InteractiveModeOptions,
    theme_changed: Rc<Cell<bool>>,
    exit: Option<ExitReason>,
    initialized: bool,
}

/// Root child indices (construction order, oracle init() :726-736).
const IDX_EDITOR_SLOT: usize = 5;

impl InteractiveMode {
    pub fn new(
        runtime: Arc<AgentSessionRuntime>,
        terminal: impl Terminal + 'static,
        options: InteractiveModeOptions,
    ) -> Self {
        let clock: Rc<dyn Fn() -> Instant> = options
            .clock
            .clone()
            .unwrap_or_else(|| Rc::new(Instant::now));
        let session = runtime.session();
        let services = runtime.services();

        // Keybindings: app catalog + user overrides, installed globally
        // (oracle KeybindingsManager.create() + setKeybindings :485-486).
        set_keybindings(create_app_keybindings(&services.agent_dir));

        let mut tui = Tui::new(terminal);
        let rows = tui.terminal().rows();

        // Editor over the host signal seam.
        let editor_signal = Rc::new(EditorSignal {
            render_requested: Cell::new(false),
            rows: Cell::new(rows),
        });
        let (editor_padding_x, autocomplete_max_visible, enable_skill_commands) = {
            let settings = services.settings_manager.lock();
            (
                settings
                    .settings()
                    .get("editorPaddingX")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0) as usize,
                settings
                    .settings()
                    .get("autocompleteMaxVisible")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(5) as usize,
                settings
                    .settings()
                    .get("enableSkillCommands")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(true),
            )
        };
        let editor = Rc::new(RefCell::new(Editor::with_shared_tui(
            editor_signal.clone() as Rc<dyn EditorTui>,
            EditorTheme,
            EditorOptions {
                padding_x: editor_padding_x,
                autocomplete_max_visible,
            },
        )));
        editor
            .borrow_mut()
            .set_autocomplete_provider(Box::new(create_autocomplete_provider(
                &session,
                runtime.as_ref(),
                enable_skill_commands,
                &services.cwd,
            )));

        let commands: Rc<RefCell<VecDeque<UiCommand>>> = Rc::new(RefCell::new(VecDeque::new()));

        // Editor callbacks → command queue.
        {
            let mut ed = editor.borrow_mut();
            let queue = commands.clone();
            ed.on_submit = Some(Box::new(move |text: String| {
                queue.borrow_mut().push_back(UiCommand::Submit(text));
            }));
            let queue = commands.clone();
            ed.on_change = Some(Box::new(move |text: String| {
                queue.borrow_mut().push_back(UiCommand::EditorChanged(text));
            }));
        }

        // CustomEditor interceptor: app keybindings before editor input
        // (oracle custom-editor.ts handleInput ordering).
        let custom_editor = {
            let queue = commands.clone();
            let editor_for_interceptor = editor.clone();
            let interceptor = move |data: &str| -> bool {
                let kb = pi_tui::keybindings::get_keybindings();
                // Escape/interrupt — only when autocomplete is NOT active.
                if kb.matches(data, "app.interrupt") {
                    if editor_for_interceptor.borrow().is_showing_autocomplete() {
                        return false;
                    }
                    queue
                        .borrow_mut()
                        .push_back(UiCommand::Action(AppAction::Interrupt));
                    return true;
                }
                // Exit (Ctrl+D) — only when the editor is empty; otherwise
                // fall through to delete-char-forward.
                if kb.matches(data, "app.exit") {
                    if editor_for_interceptor.borrow().get_text().is_empty() {
                        queue
                            .borrow_mut()
                            .push_back(UiCommand::Action(AppAction::Exit));
                        return true;
                    }
                    return false;
                }
                for (id, action) in INTERCEPTED_ACTIONS {
                    if kb.matches(data, id) {
                        queue.borrow_mut().push_back(UiCommand::Action(*action));
                        return true;
                    }
                }
                false
            };
            Rc::new(RefCell::new(CustomEditor::new(
                Shared::new(editor.clone()),
                interceptor,
            )))
        };

        // Containers (construction order mirrors the oracle).
        let header = Rc::new(RefCell::new(Container::new()));
        let loaded_resources = Rc::new(RefCell::new(Container::new()));
        let chat = Rc::new(RefCell::new(Container::new()));
        let pending_messages = Rc::new(RefCell::new(Container::new()));
        let status = Rc::new(RefCell::new(Container::new()));
        {
            let mut s = status.borrow_mut();
            s.add_child(Shared::new(Rc::new(RefCell::new(IdleStatus::new()))));
        }

        // Footer over live session getters (oracle FooterComponent(session,
        // footerDataProvider)).
        let footer = {
            let cwd_session = session.clone();
            let branch_session = session.clone();
            let name_session = session.clone();
            let stats_session = session.clone();
            let data = FooterData {
                cwd: Box::new(move || cwd_session.cwd().to_string_lossy().into_owned()),
                git_branch: Box::new(move || {
                    FooterComponent::read_git_branch(branch_session.cwd())
                }),
                session_name: Box::new(move || name_session.session_name()),
                stats: Box::new(move || footer_stats(&stats_session)),
                extension_statuses: Box::new(Vec::new),
                available_provider_count: Box::new(|| 0),
            };
            let mut component = FooterComponent::new(data);
            component.set_auto_compact_enabled(session.auto_compaction_enabled());
            Rc::new(RefCell::new(component))
        };

        // Mount (oracle init() :726-736 order).
        let editor_slot =
            SlotHandle::new(Box::new(Shared::new(custom_editor.clone())) as ComponentBox);
        tui.add_child(Shared::new(header));
        tui.add_child(Shared::new(loaded_resources));
        tui.add_child(Shared::new(chat.clone()));
        tui.add_child(Shared::new(pending_messages.clone()));
        tui.add_child(Shared::new(status.clone()));
        tui.add_child(SwapSlot::new(editor_slot.clone()));
        tui.add_child(Shared::new(footer.clone()));
        tui.set_focus_child(Some(IDX_EDITOR_SLOT));

        // Session events → channel (subscribe seam).
        let (events_tx, events_rx) = tokio::sync::mpsc::unbounded_channel();
        let unsubscribe = session.subscribe(Arc::new(move |event: &AgentSessionEvent| {
            let _ = events_tx.send(event.clone());
        }));

        // Theme change → invalidate + re-render (oracle onThemeChange :816-821).
        let theme_changed = Rc::new(Cell::new(false));
        {
            let flag_source = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let flag_write = flag_source.clone();
            on_theme_change(move || {
                flag_write.store(true, std::sync::atomic::Ordering::Relaxed);
            });
            // The loop polls the atomic through this Rc<Cell> mirror.
            let mirror = theme_changed.clone();
            let _ = (&mirror, &flag_source);
            // Direct wiring: keep the atomic and read it in pump.
            THEME_CHANGE_FLAG.with(|f| *f.borrow_mut() = Some(flag_source));
        }

        let hide_thinking_block = false;
        let output_pad = 1;

        let (oauth_ui_tx, oauth_ui_rx) = std::sync::mpsc::channel();

        Self {
            runtime,
            session,
            tui,
            chat,
            pending_messages,
            status,
            footer,
            editor,
            custom_editor,
            editor_slot,
            editor_signal,
            commands,
            events_rx,
            ops: LocalOps::new(),
            unsubscribe: Some(Box::new(unsubscribe)),
            streaming_component: None,
            pending_tools: HashMap::new(),
            tool_output_expanded: false,
            hide_thinking_block,
            output_pad,
            active_status: None,
            last_status_text: None,
            working_message: None,
            working_visible: true,
            escape_override: None,
            last_sigint_time: None,
            last_escape_time: None,
            now: clock,
            is_bash_mode: false,
            selector_open: false,
            compaction_queued: Vec::new(),
            startup_messages: VecDeque::new(),
            bash_component: None,
            bash_chunks: None,
            oauth_ui_tx,
            oauth_ui_rx,
            oauth_dialog: None,
            oauth_prompt_reply: None,
            oauth_select_reply: None,
            oauth_cancel: None,
            options,
            theme_changed,
            exit: None,
            initialized: false,
        }
    }

    /// Direct access to the Tui (tests drive `poll_terminal` themselves).
    pub fn tui_mut(&mut self) -> &mut Tui {
        &mut self.tui
    }

    /// Oracle `init()` tail: render initial state, start painting.
    pub fn init(&mut self) {
        if self.initialized {
            return;
        }
        self.initialized = true;
        self.update_editor_border_color();
        self.update_terminal_title();
        self.render_current_session_state();
        if let Some(fallback) = self.options.model_fallback_message.take() {
            self.show_warning(&fallback);
        }
        self.tui.start_render_loop_hooks();
    }

    /// Run until quit; returns the exit code and farewell line.
    pub async fn run(mut self) -> RunOutcome {
        self.init();

        if let Some(initial) = self.options.initial_message.take() {
            self.startup_messages.push_back(initial);
        }
        self.startup_messages
            .extend(std::mem::take(&mut self.options.initial_messages));
        self.spawn_next_startup_message();

        #[cfg(unix)]
        let mut sigterm = if self.options.handle_signals {
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok()
        } else {
            None
        };
        #[cfg(unix)]
        let mut sighup = if self.options.handle_signals {
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup()).ok()
        } else {
            None
        };

        loop {
            self.pump();
            if let Some(reason) = self.exit {
                return self.shutdown(reason).await;
            }

            #[cfg(unix)]
            {
                let term_fut = async {
                    match sigterm.as_mut() {
                        Some(s) => {
                            s.recv().await;
                        }
                        None => std::future::pending::<()>().await,
                    }
                };
                let hup_fut = async {
                    match sighup.as_mut() {
                        Some(s) => {
                            s.recv().await;
                        }
                        None => std::future::pending::<()>().await,
                    }
                };
                tokio::select! {
                    event = self.events_rx.recv() => {
                        if let Some(event) = event {
                            self.handle_event(event);
                        }
                    }
                    () = term_fut => { self.exit = Some(ExitReason::Signal); }
                    () = hup_fut => { self.exit = Some(ExitReason::Signal); }
                    () = tokio::time::sleep(Duration::from_millis(16)) => {}
                }
            }
            #[cfg(not(unix))]
            {
                tokio::select! {
                    event = self.events_rx.recv() => {
                        if let Some(event) = event {
                            self.handle_event(event);
                        }
                    }
                    () = tokio::time::sleep(Duration::from_millis(16)) => {}
                }
            }
        }
    }

    /// One synchronous UI step: terminal input, queued events/commands,
    /// spinner tick, render. Tests drive this directly.
    pub fn pump(&mut self) {
        self.tui.poll_terminal();
        self.flush_bash_chunks();

        // Session events that arrived while not awaiting.
        while let Ok(event) = self.events_rx.try_recv() {
            self.handle_event(event);
        }

        let oauth_requests: Vec<_> = self.oauth_ui_rx.try_iter().collect();
        for request in oauth_requests {
            self.handle_oauth_ui_request(request);
        }

        // Async op completions.
        for outcome in self.ops.poll_ready() {
            self.handle_op_outcome(outcome);
        }

        // Commands from editor/selector callbacks.
        loop {
            let command = self.commands.borrow_mut().pop_front();
            let Some(command) = command else { break };
            self.handle_command(command);
        }

        // Theme change → full invalidation (the one sanctioned full repaint).
        let theme_flag = THEME_CHANGE_FLAG.with(|f| {
            f.borrow()
                .as_ref()
                .is_some_and(|a| a.swap(false, std::sync::atomic::Ordering::Relaxed))
        });
        if theme_flag || self.theme_changed.replace(false) {
            self.tui.invalidate();
            self.update_editor_border_color();
            self.tui.request_render(false);
        }

        // Spinner tick.
        if let Some((_, indicator)) = &self.active_status {
            indicator.borrow_mut().tick();
            self.status.borrow_mut().mark_changed();
            self.tui.request_render(false);
        }

        // Editor render requests.
        if self.editor_signal.render_requested.replace(false) {
            self.tui.request_render(false);
        }
        self.editor_signal.rows.set(self.tui.terminal().rows());

        self.tui.do_render();
    }

    /// True once `/quit` (or a signal) has been requested.
    pub fn exit_requested(&self) -> bool {
        self.exit.is_some()
    }

    /// Test seam: finish teardown outside `run()`.
    pub async fn finish(mut self) -> RunOutcome {
        let reason = if self.exit == Some(ExitReason::Signal) {
            ExitReason::Signal
        } else {
            ExitReason::Quit
        };
        self.shutdown(reason).await
    }

    // ========================================================================
    // Teardown (load-bearing orderings, oracle :3495-3539 and :6002-6019)
    // ========================================================================

    async fn shutdown(&mut self, reason: ExitReason) -> RunOutcome {
        let farewell = self
            .session
            .with_session_manager(format_resume_command)
            .map(|command| format!("To resume this session: {command}"));

        match reason {
            ExitReason::Signal => {
                // Signal path: dispose the runtime BEFORE touching the
                // terminal so session_shutdown work runs even if stdio is
                // dead (oracle :3508-3520).
                self.runtime.dispose();
                self.tui.terminal_mut().drain_input(1000, 20);
                self.stop();
            }
            ExitReason::Quit => {
                // Interactive path: drain pending input (Kitty release
                // sequences), stop the TUI, THEN dispose (oracle :3522-3537).
                self.tui.terminal_mut().drain_input(1000, 20);
                self.stop();
                self.runtime.dispose();
            }
        }

        RunOutcome {
            exit_code: 0,
            farewell,
        }
    }

    /// Oracle `stop()` (:6002-6019) ordering.
    fn stop(&mut self) {
        if self.show_terminal_progress_enabled() {
            self.tui.terminal_mut().set_progress(false);
        }
        self.clear_status_indicator(None);
        // Theme auto-sync off: drop the change listener.
        THEME_CHANGE_FLAG.with(|f| *f.borrow_mut() = None);
        if let Some(unsubscribe) = self.unsubscribe.take() {
            unsubscribe();
        }
        if self.initialized {
            self.tui.stop();
            self.initialized = false;
        }
    }

    // ========================================================================
    // Command handling (editor callbacks → loop)
    // ========================================================================

    fn handle_command(&mut self, command: UiCommand) {
        match command {
            UiCommand::Submit(text) => self.on_submit(&text),
            UiCommand::EditorChanged(text) => {
                let bash = text.trim_start().starts_with('!');
                if bash != self.is_bash_mode {
                    self.is_bash_mode = bash;
                    self.update_editor_border_color();
                }
            }
            UiCommand::Action(action) => self.handle_app_action(action),
            UiCommand::RestoreEditor => self.restore_editor(),
            UiCommand::ModelSelected(model) => {
                self.restore_editor();
                let session = self.session.clone();
                self.ops.push(Box::pin(async move {
                    let result = session.set_model((*model).clone()).await;
                    OpOutcome::ModelSet { model, result }
                }));
            }
            UiCommand::ThemeSelected(name) => {
                self.restore_editor();
                if let Err(error) = set_theme(&name, false) {
                    self.show_error(&error);
                } else {
                    let services = self.runtime.services();
                    let mut settings = services.settings_manager.lock();
                    settings.set_theme(&name);
                }
            }
            UiCommand::ThemePreview(name) => {
                let _ = set_theme(&name, false);
            }
            UiCommand::TrustSelected(selection) => {
                self.restore_editor();
                let services = self.runtime.services();
                let store = super::trust_store::ProjectTrustStore::new(&services.agent_dir);
                match store.set_many(&selection.updates) {
                    Ok(()) => self.show_status(&format!(
                        "Saved trust decision: {}. Restart pi for this to take effect.",
                        if selection.trusted {
                            "trusted"
                        } else {
                            "untrusted"
                        }
                    )),
                    Err(error) => self.show_error(&error),
                }
            }
            UiCommand::SessionSelected(path) => {
                self.restore_editor();
                let runtime = self.runtime.clone();
                self.ops.push(Box::pin(async move {
                    OpOutcome::SessionSwitched(runtime.switch_session(&path, None).await)
                }));
            }
            UiCommand::SessionSelectorExit => {
                self.restore_editor();
                self.exit = Some(ExitReason::Quit);
            }
            UiCommand::ForkSelected(entry_id) => {
                self.restore_editor();
                let runtime = self.runtime.clone();
                self.ops.push(Box::pin(async move {
                    OpOutcome::ForkFinished(
                        runtime
                            .fork(&entry_id, crate::extension_bridge::ForkPosition::Before)
                            .await,
                    )
                }));
            }
            UiCommand::TreeSelected(id) => {
                self.restore_editor();
                let current = self
                    .session
                    .with_session_manager(|sm| sm.get_leaf_id().map(str::to_owned));
                if current.as_deref() == Some(id.as_str()) {
                    self.show_status("Already at this point");
                    return;
                }
                let session = self.session.clone();
                self.ops.push(Box::pin(async move {
                    OpOutcome::TreeNavigated(
                        session
                            .navigate_tree(&id, crate::session::NavigateTreeOptions::default())
                            .await,
                    )
                }));
            }
            UiCommand::LoginProviderSelected(provider, auth_type) => {
                self.restore_editor();
                match auth_type {
                    AuthType::ApiKey => self.show_api_key_login_dialog(&provider),
                    AuthType::OAuth => self.start_oauth_login(provider),
                }
            }
            UiCommand::LoginApiKey(provider, api_key) => {
                self.restore_editor();
                let api_key = api_key.trim().to_owned();
                if api_key.is_empty() {
                    self.show_error(&format!(
                        "Failed to save API key for {provider}: API key cannot be empty."
                    ));
                } else {
                    self.spawn_auth_change(provider, AuthType::ApiKey, true, Some(api_key));
                }
            }
            UiCommand::LogoutProviderSelected(provider, auth_type) => {
                self.restore_editor();
                self.spawn_auth_change(provider, auth_type, false, None);
            }
            UiCommand::OAuthPromptSubmitted(value) => {
                if let Some(reply) = self.oauth_prompt_reply.take() {
                    let _ = reply.send(value);
                }
            }
            UiCommand::OAuthSelectSubmitted(value) => {
                if let Some(reply) = self.oauth_select_reply.take() {
                    let _ = reply.send(value);
                }
                self.remount_oauth_dialog();
            }
            UiCommand::OAuthCancelled => {
                if let Some(cancel) = self.oauth_cancel.take() {
                    cancel.cancel();
                }
                if let Some(reply) = self.oauth_prompt_reply.take() {
                    let _ = reply.send(String::new());
                }
                if let Some(reply) = self.oauth_select_reply.take() {
                    let _ = reply.send(None);
                }
                self.oauth_dialog = None;
                self.restore_editor();
            }
            UiCommand::SettingChanged(change) => self.apply_setting_change(*change),
        }
    }

    fn apply_setting_change(&mut self, change: SettingChange) {
        let services = self.runtime.services();
        match change {
            SettingChange::AutoCompact(enabled) => {
                self.session.set_auto_compaction_enabled(enabled);
                self.footer.borrow_mut().set_auto_compact_enabled(enabled);
            }
            SettingChange::Steering(mode) => self.session.set_steering_mode(&mode),
            SettingChange::FollowUp(mode) => self.session.set_follow_up_mode(&mode),
            SettingChange::Thinking(level) => {
                self.session
                    .set_thinking_level(model_thinking_to_agent(level));
                self.footer.borrow_mut().invalidate();
                self.update_editor_border_color();
            }
            SettingChange::Theme(name) => {
                if let Err(error) = set_theme(&name, false) {
                    self.show_error(&error);
                } else {
                    services.settings_manager.lock().set_theme(name);
                }
            }
            SettingChange::Top {
                key,
                value,
                rebuild_chat,
            } => {
                services
                    .settings_manager
                    .lock()
                    .set_global_value(key, value);
                if key == "enableSkillCommands" {
                    self.refresh_autocomplete_provider();
                }
                if rebuild_chat {
                    self.rebuild_chat_from_messages();
                }
            }
            SettingChange::Nested {
                section,
                key,
                value,
                rebuild_chat,
            } => {
                services
                    .settings_manager
                    .lock()
                    .set_global_nested_value(section, key, value);
                if rebuild_chat {
                    self.rebuild_chat_from_messages();
                }
            }
            SettingChange::HideThinking(hidden) => {
                self.hide_thinking_block = hidden;
                services
                    .settings_manager
                    .lock()
                    .set_global_value("hideThinkingBlock", serde_json::Value::Bool(hidden));
                self.rebuild_chat_from_messages();
            }
            SettingChange::OutputPad(padding) => {
                self.output_pad = usize::from(padding);
                services
                    .settings_manager
                    .lock()
                    .set_global_value("outputPad", serde_json::Value::from(padding));
                self.rebuild_chat_from_messages();
            }
            SettingChange::HardwareCursor(enabled) => {
                services
                    .settings_manager
                    .lock()
                    .set_global_value("showHardwareCursor", serde_json::Value::Bool(enabled));
                self.tui.set_show_hardware_cursor(enabled);
            }
            SettingChange::EditorPadding(padding) => {
                services
                    .settings_manager
                    .lock()
                    .set_global_value("editorPaddingX", serde_json::Value::from(padding));
                self.editor.borrow_mut().set_padding_x(padding as usize);
            }
            SettingChange::AutocompleteMax(max_visible) => {
                services.settings_manager.lock().set_global_value(
                    "autocompleteMaxVisible",
                    serde_json::Value::from(max_visible),
                );
                self.editor
                    .borrow_mut()
                    .set_autocomplete_max_visible(max_visible as usize);
            }
            SettingChange::ClearOnShrink(enabled) => {
                services.settings_manager.lock().set_global_nested_value(
                    "terminal",
                    "clearOnShrink",
                    serde_json::Value::Bool(enabled),
                );
                self.tui.set_clear_on_shrink(enabled);
            }
            SettingChange::HttpIdleTimeout(timeout_ms) => {
                services
                    .settings_manager
                    .lock()
                    .set_global_value("httpIdleTimeoutMs", serde_json::Value::from(timeout_ms));
                self.show_status(&format!(
                    "HTTP idle timeout: {}",
                    super::components::settings_selector::format_http_idle_timeout_ms(timeout_ms)
                ));
            }
        }
        self.tui.request_render(false);
    }

    fn refresh_autocomplete_provider(&mut self) {
        let services = self.runtime.services();
        let enabled = services
            .settings_manager
            .lock()
            .settings()
            .get("enableSkillCommands")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(true);
        self.editor
            .borrow_mut()
            .set_autocomplete_provider(Box::new(create_autocomplete_provider(
                &self.session,
                self.runtime.as_ref(),
                enabled,
                &services.cwd,
            )));
    }

    fn handle_app_action(&mut self, action: AppAction) {
        match action {
            AppAction::Interrupt => self.handle_escape(),
            AppAction::Clear => self.handle_ctrl_c(),
            AppAction::Exit => {
                self.exit = Some(ExitReason::Quit);
            }
            AppAction::ThinkingCycle => {
                if self.session.cycle_thinking_level().is_some() {
                    self.footer.borrow_mut().invalidate();
                    self.update_editor_border_color();
                }
            }
            AppAction::ModelCycleForward => self.spawn_cycle_model(true),
            AppAction::ModelCycleBackward => self.spawn_cycle_model(false),
            AppAction::ModelSelect => self.show_model_selector(None),
            AppAction::ToolsExpand => self.set_tools_expanded(!self.tool_output_expanded),
            AppAction::ThinkingToggle => {
                self.hide_thinking_block = !self.hide_thinking_block;
                if let Some((_, component)) = &self.streaming_component {
                    component
                        .borrow_mut()
                        .set_hide_thinking_block(self.hide_thinking_block);
                }
                self.show_status(if self.hide_thinking_block {
                    "Thinking blocks: hidden"
                } else {
                    "Thinking blocks: visible"
                });
            }
            AppAction::MessageCopy => self.handle_copy_command(),
            AppAction::MessageFollowUp => self.handle_follow_up(),
            AppAction::MessageDequeue => {
                self.restore_queued_messages_to_editor(false);
            }
            AppAction::SessionNew => self.handle_clear_command(),
            AppAction::SessionTree => self.show_tree_selector(),
            AppAction::SessionFork => self.show_user_message_selector(),
            AppAction::SessionResume => self.show_session_selector(),
        }
    }

    /// Escape context chain (oracle setupKeyHandlers :2544-2570, with the
    /// compaction/retry overrides from the event handler).
    fn handle_escape(&mut self) {
        match self.escape_override {
            Some(EscapeOverride::AbortCompaction) => {
                self.session.abort_compaction();
                return;
            }
            Some(EscapeOverride::AbortRetry) => {
                self.session.abort_retry();
                return;
            }
            None => {}
        }
        if self.session.is_streaming() {
            self.restore_queued_messages_to_editor(true);
            return;
        }
        if self.session.is_bash_running() {
            self.session.abort_bash();
            return;
        }
        if self.is_bash_mode {
            self.editor.borrow_mut().set_text("");
            self.is_bash_mode = false;
            self.update_editor_border_color();
            return;
        }
        if self.editor.borrow().get_text().trim().is_empty() {
            // Double-escape with empty editor triggers /tree, /fork, or
            // nothing based on the setting (oracle :2553-2569).
            let action = self
                .runtime
                .services()
                .settings_manager
                .lock()
                .get_double_escape_action();
            if action != "none" {
                let now = (self.now)();
                if self
                    .last_escape_time
                    .is_some_and(|last| now.duration_since(last) < Duration::from_millis(500))
                {
                    if action == "tree" {
                        self.show_tree_selector();
                    } else {
                        self.show_user_message_selector();
                    }
                    self.last_escape_time = None;
                } else {
                    self.last_escape_time = Some(now);
                }
            }
        }
    }

    /// Double-press Ctrl+C quit semantics (oracle handleCtrlC :3478-3486).
    fn handle_ctrl_c(&mut self) {
        let now = (self.now)();
        if self
            .last_sigint_time
            .is_some_and(|last| now.duration_since(last) < Duration::from_millis(500))
        {
            self.exit = Some(ExitReason::Quit);
        } else {
            self.editor.borrow_mut().set_text("");
            self.last_sigint_time = Some(now);
        }
    }

    /// Oracle `updateEditorBorderColor` (:3713-3721): bash mode wins, else
    /// the current thinking level picks the editor border color.
    fn update_editor_border_color(&mut self) {
        {
            let mut editor = self.custom_editor.borrow_mut();
            editor.set_bash_mode(self.is_bash_mode);
            let level = crate::session::thinking_level_str(self.session.thinking_level());
            editor.set_border_color(theme().thinking_border_color(level));
        }
        self.tui.request_render(false);
    }

    // ========================================================================
    // Submit dispatch
    // ========================================================================

    fn dispatch_context(&self) -> DispatchContext {
        DispatchContext {
            is_compacting: self.session.is_compacting(),
            is_streaming: self.session.is_streaming(),
            is_bash_running: self.session.is_bash_running(),
            extension_commands: self
                .runtime
                .bridge()
                .registered_commands()
                .into_iter()
                .map(|c| c.invocation_name)
                .collect(),
        }
    }

    fn on_submit(&mut self, text: &str) {
        let action = dispatch_input(text, &self.dispatch_context());
        match action {
            DispatchAction::Nothing => {}
            DispatchAction::Builtin(command) => self.execute_builtin(command),
            DispatchAction::Bash { command, excluded } => {
                self.editor.borrow_mut().add_to_history(text.trim());
                self.editor.borrow_mut().set_text("");
                self.handle_bash_command(command, excluded);
                self.is_bash_mode = false;
                self.update_editor_border_color();
            }
            DispatchAction::BashBusy { original_text } => {
                self.show_warning(
                    "A bash command is already running. Press Esc to cancel it first.",
                );
                self.editor.borrow_mut().set_text(&original_text);
            }
            DispatchAction::ExtensionDuringCompaction { text } => {
                self.editor.borrow_mut().add_to_history(&text);
                self.editor.borrow_mut().set_text("");
                self.spawn_prompt(text);
            }
            DispatchAction::QueueCompaction { text } => {
                self.queue_compaction_message(text, StreamingBehavior::Steer);
            }
            DispatchAction::SteerStreaming { text } => {
                self.editor.borrow_mut().add_to_history(&text);
                self.editor.borrow_mut().set_text("");
                let session = self.session.clone();
                self.ops.push(Box::pin(async move {
                    OpOutcome::PromptFinished(
                        session
                            .prompt(
                                &text,
                                PromptOptions {
                                    streaming_behavior: Some(StreamingBehavior::Steer),
                                    ..Default::default()
                                },
                            )
                            .await,
                    )
                }));
                self.update_pending_messages_display();
                self.tui.request_render(false);
            }
            DispatchAction::Prompt { text } => {
                self.editor.borrow_mut().add_to_history(&text);
                self.editor.borrow_mut().set_text("");
                self.spawn_prompt(text);
            }
        }
    }

    fn spawn_prompt(&mut self, text: String) {
        let session = self.session.clone();
        self.ops.push(Box::pin(async move {
            OpOutcome::PromptFinished(session.prompt(&text, PromptOptions::default()).await)
        }));
    }

    fn spawn_next_startup_message(&mut self) {
        if !self.session.is_streaming()
            && let Some(message) = self.startup_messages.pop_front()
        {
            self.spawn_prompt(message);
        }
    }

    fn spawn_cycle_model(&mut self, forward: bool) {
        let session = self.session.clone();
        self.ops.push(Box::pin(async move {
            OpOutcome::ModelCycled(session.cycle_model(forward).await.map(Box::new))
        }));
    }

    // ========================================================================
    // Built-in slash commands
    // ========================================================================

    fn execute_builtin(&mut self, command: BuiltinCommand) {
        match command {
            BuiltinCommand::Quit => {
                self.editor.borrow_mut().set_text("");
                self.exit = Some(ExitReason::Quit);
            }
            BuiltinCommand::Theme => {
                self.editor.borrow_mut().set_text("");
                self.show_theme_selector();
            }
            BuiltinCommand::Thinking => {
                self.editor.borrow_mut().set_text("");
                self.show_thinking_selector();
            }
            BuiltinCommand::Images => {
                self.editor.borrow_mut().set_text("");
                self.show_images_selector();
            }
            BuiltinCommand::Help => {
                self.handle_hotkeys_command();
                self.editor.borrow_mut().set_text("");
            }
            BuiltinCommand::Model { search } => {
                self.editor.borrow_mut().set_text("");
                self.show_model_selector(search);
            }
            BuiltinCommand::New => {
                self.editor.borrow_mut().set_text("");
                self.handle_clear_command();
            }
            BuiltinCommand::Compact { instructions } => {
                self.editor.borrow_mut().set_text("");
                self.clear_status_indicator(None);
                let session = self.session.clone();
                self.ops.push(Box::pin(async move {
                    OpOutcome::CompactFinished(session.compact(instructions).await.map(|_| ()))
                }));
            }
            BuiltinCommand::Fork => {
                self.editor.borrow_mut().set_text("");
                self.show_user_message_selector();
            }
            BuiltinCommand::Tree => {
                self.editor.borrow_mut().set_text("");
                self.show_tree_selector();
            }
            BuiltinCommand::Resume => {
                self.editor.borrow_mut().set_text("");
                self.show_session_selector();
            }
            BuiltinCommand::Clone => {
                self.editor.borrow_mut().set_text("");
                self.handle_clone_command();
            }
            BuiltinCommand::Name { raw } => {
                self.handle_name_command(&raw);
                self.editor.borrow_mut().set_text("");
            }
            BuiltinCommand::Session => {
                self.handle_session_command();
                self.editor.borrow_mut().set_text("");
            }
            BuiltinCommand::Hotkeys => {
                self.handle_hotkeys_command();
                self.editor.borrow_mut().set_text("");
            }
            BuiltinCommand::Changelog => {
                self.handle_changelog_command();
                self.editor.borrow_mut().set_text("");
            }
            BuiltinCommand::Copy => {
                self.handle_copy_command();
                self.editor.borrow_mut().set_text("");
            }
            BuiltinCommand::Export { raw } => {
                self.handle_export_command(&raw);
                self.editor.borrow_mut().set_text("");
            }
            BuiltinCommand::Import { raw } => {
                self.handle_import_command(&raw);
                self.editor.borrow_mut().set_text("");
            }
            BuiltinCommand::Settings => {
                self.show_settings_selector();
                self.editor.borrow_mut().set_text("");
            }
            BuiltinCommand::ScopedModels => {
                self.editor.borrow_mut().set_text("");
                self.show_theme_or_status("Model scoping requires configured models");
            }
            BuiltinCommand::Trust => {
                self.show_trust_selector();
                self.editor.borrow_mut().set_text("");
            }
            BuiltinCommand::Login { provider } => {
                self.editor.borrow_mut().set_text("");
                self.show_auth_selector(OAuthSelectorMode::Login, provider.as_deref());
            }
            BuiltinCommand::Logout => {
                self.editor.borrow_mut().set_text("");
                self.show_auth_selector(OAuthSelectorMode::Logout, None);
            }
            BuiltinCommand::Share => {
                self.editor.borrow_mut().set_text("");
                self.handle_share_command();
            }
            BuiltinCommand::Reload => {
                self.editor.borrow_mut().set_text("");
                self.handle_reload_command();
            }
            BuiltinCommand::Debug => {
                self.handle_debug_command();
                self.editor.borrow_mut().set_text("");
            }
            BuiltinCommand::ArminSaysHi => {
                self.chat
                    .borrow_mut()
                    .add_child(super::components::armin::Armin::new());
                self.editor.borrow_mut().set_text("");
                self.tui.request_render(false);
            }
            BuiltinCommand::DementedElves => {
                self.chat.borrow_mut().add_child(
                    super::components::earendil_announcement::EarendilAnnouncement::new(),
                );
                self.editor.borrow_mut().set_text("");
                self.tui.request_render(false);
            }
        }
    }

    fn show_theme_or_status(&mut self, message: &str) {
        self.show_status(message);
    }

    // ========================================================================
    // Session event handler (oracle handleEvent :2829-3126)
    // ========================================================================

    /// Oracle `settingsManager.getShowTerminalProgress()` gate for the OSC
    /// 9;4 progress calls (interactive-mode.ts:2839,3027,3046,3060,6003).
    fn show_terminal_progress_enabled(&self) -> bool {
        self.runtime
            .services()
            .settings_manager
            .lock()
            .get_show_terminal_progress()
    }

    fn handle_event(&mut self, event: AgentSessionEvent) {
        self.footer.borrow_mut().invalidate();

        match event {
            AgentSessionEvent::AgentStart => {
                self.pending_tools.clear();
                if self.show_terminal_progress_enabled() {
                    self.tui.terminal_mut().set_progress(true);
                }
                if self.escape_override == Some(EscapeOverride::AbortRetry) {
                    self.escape_override = None;
                }
                if self.working_visible {
                    let message = self
                        .working_message
                        .clone()
                        .unwrap_or_else(|| "Working...".to_owned());
                    let message = format!("{message} ({} to interrupt)", key_text("app.interrupt"));
                    self.show_status_indicator(StatusIndicator::working(message));
                } else {
                    self.clear_status_indicator(None);
                }
                self.tui.request_render(false);
            }
            AgentSessionEvent::QueueUpdate { .. } => {
                self.update_pending_messages_display();
                self.tui.request_render(false);
            }
            AgentSessionEvent::EntryAppended { entry } => {
                if let SessionEntry::Custom {
                    custom_type, data, ..
                } = entry
                {
                    // No extension entry renderers pre-Phase-6; render body text.
                    let body = data
                        .as_ref()
                        .and_then(|d| d.as_str())
                        .unwrap_or_default()
                        .to_owned();
                    let mut component = CustomEntryComponent::new(custom_type, body);
                    component.set_expanded(self.tool_output_expanded);
                    self.chat.borrow_mut().add_child(component);
                    self.tui.request_render(false);
                }
            }
            AgentSessionEvent::SessionInfoChanged { .. } => {
                self.update_terminal_title();
                self.footer.borrow_mut().invalidate();
                self.tui.request_render(false);
            }
            AgentSessionEvent::ThinkingLevelChanged { .. } => {
                self.footer.borrow_mut().invalidate();
                self.update_editor_border_color();
            }
            AgentSessionEvent::MessageStart { message } => match message.role() {
                "custom" => {
                    self.add_message_to_chat(&message);
                    self.tui.request_render(false);
                }
                "user" => {
                    self.add_message_to_chat(&message);
                    self.update_pending_messages_display();
                    self.tui.request_render(false);
                }
                "assistant" => {
                    let mut component = AssistantMessageComponent::new(None);
                    component.set_hide_thinking_block(self.hide_thinking_block);
                    component.set_output_pad(self.output_pad);
                    if let AgentMessage::Standard(Message::Assistant(am)) = &message {
                        component.update_content(am.clone());
                    }
                    let component = Rc::new(RefCell::new(component));
                    let index = self.chat.borrow().len();
                    self.chat
                        .borrow_mut()
                        .add_child(Shared::new(component.clone()));
                    self.streaming_component = Some((index, component));
                    self.tui.request_render(false);
                }
                _ => {}
            },
            AgentSessionEvent::MessageUpdate { message, .. } => {
                if let (Some((_, component)), AgentMessage::Standard(Message::Assistant(am))) =
                    (&self.streaming_component, &message)
                {
                    component.borrow_mut().update_content(am.clone());
                    for content in &am.content {
                        if let pi_ai::Content::ToolCall(call) = content {
                            if let Some(existing) = self.pending_tools.get(&call.id) {
                                existing
                                    .borrow_mut()
                                    .update_args(serde_json::Value::Object(call.arguments.clone()));
                            } else {
                                let mut tool = ToolExecutionComponent::with_call_id(
                                    call.id.clone(),
                                    call.name.clone(),
                                    serde_json::Value::Object(call.arguments.clone()),
                                );
                                tool.set_expanded(self.tool_output_expanded);
                                self.apply_tool_image_settings(&mut tool);
                                let tool = Rc::new(RefCell::new(tool));
                                self.chat.borrow_mut().add_child(Shared::new(tool.clone()));
                                self.pending_tools.insert(call.id.clone(), tool);
                            }
                        }
                    }
                    self.chat.borrow_mut().mark_changed();
                    self.tui.request_render(false);
                }
            }
            AgentSessionEvent::MessageEnd { message } => {
                if message.role() == "user" {
                    return;
                }
                if let (Some((_, component)), AgentMessage::Standard(Message::Assistant(am))) =
                    (&self.streaming_component, &message)
                {
                    let mut am = am.clone();
                    let mut error_message: Option<String> = None;
                    if am.stop_reason == StopReason::Aborted {
                        let retry_attempt = self.session.retry_attempt();
                        let text = if retry_attempt > 0 {
                            format!(
                                "Aborted after {retry_attempt} retry attempt{}",
                                if retry_attempt > 1 { "s" } else { "" }
                            )
                        } else {
                            "Operation aborted".to_owned()
                        };
                        am.error_message = Some(text.clone());
                        error_message = Some(text);
                    }
                    component.borrow_mut().update_content(am.clone());

                    if am.stop_reason == StopReason::Aborted || am.stop_reason == StopReason::Error
                    {
                        let error_text = error_message
                            .or_else(|| am.error_message.clone())
                            .unwrap_or_else(|| "Error".to_owned());
                        for component in self.pending_tools.values() {
                            component
                                .borrow_mut()
                                .end(AgentToolResult::text(error_text.clone()), true);
                        }
                        self.pending_tools.clear();
                    }
                    self.streaming_component = None;
                    self.footer.borrow_mut().invalidate();
                }
                self.chat.borrow_mut().mark_changed();
                self.tui.request_render(false);
            }
            AgentSessionEvent::ToolExecutionStart {
                tool_call_id,
                tool_name,
                args,
            } => {
                if !self.pending_tools.contains_key(&tool_call_id) {
                    let mut tool =
                        ToolExecutionComponent::with_call_id(tool_call_id.clone(), tool_name, args);
                    tool.set_expanded(self.tool_output_expanded);
                    self.apply_tool_image_settings(&mut tool);
                    let tool = Rc::new(RefCell::new(tool));
                    self.chat.borrow_mut().add_child(Shared::new(tool.clone()));
                    self.pending_tools.insert(tool_call_id, tool);
                }
                self.tui.request_render(false);
            }
            AgentSessionEvent::ToolExecutionUpdate {
                tool_call_id,
                partial_result,
                ..
            } => {
                if let Some(component) = self.pending_tools.get(&tool_call_id) {
                    component.borrow_mut().update_result(partial_result);
                    self.chat.borrow_mut().mark_changed();
                    self.tui.request_render(false);
                }
            }
            AgentSessionEvent::ToolExecutionEnd {
                tool_call_id,
                result,
                is_error,
                ..
            } => {
                if let Some(component) = self.pending_tools.remove(&tool_call_id) {
                    component.borrow_mut().end(result, is_error);
                    self.chat.borrow_mut().mark_changed();
                    self.tui.request_render(false);
                }
            }
            AgentSessionEvent::AgentEnd { .. } => {
                if self.show_terminal_progress_enabled() {
                    self.tui.terminal_mut().set_progress(false);
                }
                self.clear_status_indicator(Some(StatusIndicatorKind::Working));
                if let Some((index, _)) = self.streaming_component.take() {
                    self.chat.borrow_mut().remove_child_at(index);
                }
                self.pending_tools.clear();
                self.tui.request_render(false);
            }
            AgentSessionEvent::AgentSettled => {}
            AgentSessionEvent::TurnStart | AgentSessionEvent::TurnEnd { .. } => {}
            AgentSessionEvent::CompactionStart { reason } => {
                if self.show_terminal_progress_enabled() {
                    self.tui.terminal_mut().set_progress(true);
                }
                self.escape_override = Some(EscapeOverride::AbortCompaction);
                self.show_status_indicator(StatusIndicator::compaction(match reason {
                    CompactionReason::Manual => CompactionStatusReason::Manual,
                    CompactionReason::Threshold => CompactionStatusReason::Threshold,
                    CompactionReason::Overflow => CompactionStatusReason::Overflow,
                }));
                self.tui.request_render(false);
            }
            AgentSessionEvent::CompactionEnd {
                reason,
                result,
                aborted,
                will_retry,
                error_message,
            } => {
                if self.show_terminal_progress_enabled() {
                    self.tui.terminal_mut().set_progress(false);
                }
                if self.escape_override == Some(EscapeOverride::AbortCompaction) {
                    self.escape_override = None;
                }
                self.clear_status_indicator(Some(StatusIndicatorKind::Compaction));
                if aborted {
                    if reason == CompactionReason::Manual {
                        self.show_error("Compaction cancelled");
                    } else {
                        self.show_status("Auto-compaction cancelled");
                    }
                } else if let Some(result) = result {
                    self.chat.borrow_mut().clear();
                    self.streaming_component = None;
                    self.rebuild_chat_from_messages();
                    {
                        let mut chat = self.chat.borrow_mut();
                        chat.add_child(Spacer::new(1));
                        let mut summary = CompactionSummaryMessageComponent::new(
                            result.summary,
                            result.tokens_before,
                        );
                        summary.set_expanded(self.tool_output_expanded);
                        chat.add_child(summary);
                    }
                    self.footer.borrow_mut().invalidate();
                } else if let Some(error_message) = error_message {
                    if reason == CompactionReason::Manual {
                        self.show_error(&error_message);
                    } else {
                        let mut chat = self.chat.borrow_mut();
                        chat.add_child(Spacer::new(1));
                        chat.add_child(Text::new(
                            theme().fg(ThemeColor::Error, &error_message),
                            1,
                            0,
                            None,
                        ));
                    }
                }
                self.flush_compaction_queue(will_retry);
                self.tui.request_render(false);
            }
            AgentSessionEvent::AutoRetryStart {
                attempt,
                max_attempts,
                delay_ms,
                ..
            } => {
                self.escape_override = Some(EscapeOverride::AbortRetry);
                self.show_status_indicator(StatusIndicator::retry(
                    attempt,
                    max_attempts,
                    delay_ms.div_ceil(1000),
                ));
                self.tui.request_render(false);
            }
            AgentSessionEvent::AutoRetryEnd {
                success,
                attempt,
                final_error,
            } => {
                if self.escape_override == Some(EscapeOverride::AbortRetry) {
                    self.escape_override = None;
                }
                self.clear_status_indicator(Some(StatusIndicatorKind::Retry));
                if !success {
                    self.show_error(&format!(
                        "Retry failed after {attempt} attempts: {}",
                        final_error.as_deref().unwrap_or("Unknown error")
                    ));
                }
                self.tui.request_render(false);
            }
        }
    }

    fn apply_tool_image_settings(&self, tool: &mut ToolExecutionComponent) {
        let settings = self.runtime.services().settings_manager;
        let settings = settings.lock();
        let terminal = settings
            .settings()
            .get("terminal")
            .and_then(serde_json::Value::as_object);
        if let Some(show) = terminal
            .and_then(|settings| settings.get("showImages"))
            .and_then(serde_json::Value::as_bool)
        {
            tool.set_show_images(show);
        }
        if let Some(width) = terminal
            .and_then(|settings| settings.get("imageWidthCells"))
            .and_then(serde_json::Value::as_u64)
        {
            tool.set_image_width_cells(width as u32);
        }
    }

    // ========================================================================
    // Async op outcomes
    // ========================================================================

    fn handle_op_outcome(&mut self, outcome: OpOutcome) {
        match outcome {
            OpOutcome::PromptFinished(result) => {
                if let Err(error) = result {
                    self.show_error(&error);
                }
                self.spawn_next_startup_message();
            }
            OpOutcome::BashFinished {
                component,
                result,
                command,
                excluded,
            } => {
                match result {
                    Ok(bash) => {
                        component
                            .borrow_mut()
                            .set_complete(bash.exit_code.map(|c| c as i32), bash.cancelled);
                        self.session
                            .record_bash_result(&command, &bash, Some(excluded));
                    }
                    Err(error) => {
                        component.borrow_mut().set_complete(None, false);
                        self.show_error(&format!("Bash command failed: {error}"));
                    }
                }
                self.chat.borrow_mut().mark_changed();
                self.bash_component = None;
                self.bash_chunks = None;
                self.tui.request_render(false);
            }
            OpOutcome::CompactFinished(result) => {
                if let Err(error) = result {
                    self.show_error(&error);
                }
            }
            OpOutcome::MountModelSelector(component) => {
                self.mount_selector(Box::new(*component));
            }
            OpOutcome::ModelSet { model, result } => match result {
                Ok(()) => {
                    self.footer.borrow_mut().invalidate();
                    self.update_editor_border_color();
                    self.show_status(&format!("Model set to {}/{}", model.provider, model.id));
                }
                Err(error) => self.show_error(&error),
            },
            OpOutcome::ModelCycled(result) => {
                if result.is_some() {
                    self.footer.borrow_mut().invalidate();
                    self.update_editor_border_color();
                    self.tui.request_render(false);
                }
            }
            OpOutcome::SessionSwitched(result) => match result {
                Ok(replace) => {
                    if replace.cancelled {
                        self.show_status("Resume cancelled");
                    } else {
                        self.rebind_session();
                        self.show_status("Resumed session");
                    }
                }
                Err(error) => self.show_error(&error),
            },
            OpOutcome::NewSessionCreated(result) => match result {
                Ok(replace) => {
                    if !replace.cancelled {
                        self.rebind_session();
                    }
                }
                Err(error) => self.show_error(&error),
            },
            OpOutcome::ForkFinished(result) => match result {
                Ok(replace) => {
                    if replace.cancelled {
                        self.show_status("Navigation cancelled");
                    } else {
                        self.rebind_session();
                        if let Some(text) = replace.selected_text {
                            self.editor.borrow_mut().set_text(&text);
                        }
                        self.show_status("Forked to new session");
                    }
                }
                Err(error) => self.show_error(&error),
            },
            OpOutcome::TreeNavigated(result) => match result {
                Ok(nav) => {
                    let _ = nav;
                    self.chat.borrow_mut().clear();
                    self.streaming_component = None;
                    self.rebuild_chat_from_messages();
                    self.show_status("Navigated to selected point");
                    self.tui.request_render(false);
                }
                Err(error) => self.show_error(&error),
            },
            OpOutcome::AuthChanged {
                provider,
                auth_type,
                logging_in,
                result,
            } => {
                if logging_in && auth_type == AuthType::OAuth {
                    self.oauth_dialog = None;
                    self.oauth_cancel = None;
                    self.oauth_prompt_reply = None;
                    self.oauth_select_reply = None;
                    self.restore_editor();
                }
                match result {
                    Ok(()) if logging_in => {
                        let auth_path = self.runtime.services().agent_dir.join("auth.json");
                        let action = if auth_type == AuthType::OAuth {
                            format!("Logged in to {provider}")
                        } else {
                            format!("Saved API key for {provider}")
                        };
                        self.show_status(&format!(
                            "{action}. Credentials saved to {}",
                            auth_path.display()
                        ));
                        self.footer.borrow_mut().invalidate();
                        self.refresh_autocomplete_provider();
                    }
                    Ok(()) if auth_type == AuthType::OAuth => {
                        self.show_status(&format!("Logged out of {provider}"));
                        self.footer.borrow_mut().invalidate();
                    }
                    Ok(()) => {
                        self.show_status(&format!(
                            "Removed stored API key for {provider}. Environment variables and models.json config are unchanged."
                        ));
                        self.footer.borrow_mut().invalidate();
                    }
                    Err(error) if logging_in && error != "Login cancelled" => {
                        let action = if auth_type == AuthType::OAuth {
                            "login"
                        } else {
                            "save API key"
                        };
                        self.show_error(&format!("Failed to {action} for {provider}: {error}"));
                    }
                    Err(_) if logging_in => {}
                    Err(error) => self.show_error(&format!("Logout failed: {error}")),
                }
            }
            OpOutcome::FlushQueuePromptFailed(error) => {
                self.show_error(&error);
            }
        }
    }

    /// Re-fetch the session after a runtime replacement (switch/new/fork) and
    /// repaint from the new session state.
    fn rebind_session(&mut self) {
        self.session = self.runtime.session();
        // Re-subscribe: events from the new session flow into the channel.
        if let Some(unsubscribe) = self.unsubscribe.take() {
            unsubscribe();
        }
        let (events_tx, events_rx) = tokio::sync::mpsc::unbounded_channel();
        let unsubscribe = self
            .session
            .subscribe(Arc::new(move |event: &AgentSessionEvent| {
                let _ = events_tx.send(event.clone());
            }));
        self.events_rx = events_rx;
        self.unsubscribe = Some(Box::new(unsubscribe));
        self.render_current_session_state();
        self.update_editor_border_color();
        self.update_terminal_title();
    }

    // ========================================================================
    // Bash execution
    // ========================================================================

    fn handle_bash_command(&mut self, command: String, excluded: bool) {
        let component = Rc::new(RefCell::new(BashExecutionComponent::new(
            command.clone(),
            excluded,
        )));
        if self.session.is_streaming() {
            self.pending_messages
                .borrow_mut()
                .add_child(Shared::new(component.clone()));
        } else {
            self.chat
                .borrow_mut()
                .add_child(Shared::new(component.clone()));
        }
        self.bash_component = Some(component.clone());
        self.tui.request_render(false);

        let session = self.session.clone();
        // Output chunks cross threads; buffer them and drain on the loop.
        let chunks: Arc<parking_lot::Mutex<Vec<String>>> =
            Arc::new(parking_lot::Mutex::new(Vec::new()));
        self.bash_chunks = Some(chunks.clone());
        let chunks_for_cb = chunks.clone();
        let on_chunk: crate::session::BashChunkCallback = Arc::new(move |chunk: &str| {
            chunks_for_cb.lock().push(chunk.to_owned());
        });
        let component_for_op = component;
        self.ops.push(Box::pin(async move {
            let result = session
                .execute_bash(&command, Some(on_chunk), Some(excluded))
                .await;
            // Drain buffered output into the component before completing.
            for chunk in chunks.lock().drain(..) {
                component_for_op.borrow_mut().append_output(&chunk);
            }
            OpOutcome::BashFinished {
                component: component_for_op,
                result,
                command,
                excluded,
            }
        }));
    }

    fn flush_bash_chunks(&mut self) {
        let Some(chunks) = &self.bash_chunks else {
            return;
        };
        let drained: Vec<String> = chunks.lock().drain(..).collect();
        if drained.is_empty() {
            return;
        }
        if let Some(component) = &self.bash_component {
            let mut component = component.borrow_mut();
            for chunk in drained {
                component.append_output(&chunk);
            }
            self.chat.borrow_mut().mark_changed();
            self.tui.request_render(false);
        }
    }

    // ========================================================================
    // Selectors (oracle showSelector :4102-4113)
    // ========================================================================

    fn mount_selector(&mut self, component: ComponentBox) {
        self.editor_slot.replace(component);
        self.refocus_slot();
        self.selector_open = true;
        self.tui.request_render(false);
    }

    fn restore_editor(&mut self) {
        if !self.selector_open {
            return;
        }
        self.editor_slot
            .replace(Box::new(Shared::new(self.custom_editor.clone())) as ComponentBox);
        self.refocus_slot();
        self.selector_open = false;
        self.tui.request_render(false);
    }

    fn refocus_slot(&mut self) {
        if let Some(focusable) = self.editor_slot.borrow_mut().as_focusable() {
            focusable.set_focused(true);
        }
        self.tui.set_focus_child(Some(IDX_EDITOR_SLOT));
    }

    fn show_model_selector(&mut self, initial_search: Option<String>) {
        let session = self.session.clone();
        let services = self.runtime.services();
        let queue = self.commands.clone();
        let queue_cancel = self.commands.clone();
        self.ops.push(Box::pin(async move {
            let selector = ModelSelectorComponent::new(
                session.model(),
                services.settings_manager.clone(),
                services.model_registry.clone(),
                session.scoped_models(),
                Box::new(move |model: Model| {
                    queue
                        .borrow_mut()
                        .push_back(UiCommand::ModelSelected(Box::new(model)));
                }),
                Box::new(move || {
                    queue_cancel
                        .borrow_mut()
                        .push_back(UiCommand::RestoreEditor);
                }),
                initial_search,
            )
            .await;
            OpOutcome::MountModelSelector(Box::new(selector))
        }));
    }

    /// Theme selector (reached via /settings in the oracle; exposed for the
    /// selector-swap surface).
    pub fn show_theme_selector(&mut self) {
        let current = current_theme_name().unwrap_or_else(|| "dark".to_owned());
        let queue = self.commands.clone();
        let queue_cancel = self.commands.clone();
        let queue_preview = self.commands.clone();
        let original = current.clone();
        let selector = ThemeSelectorComponent::new(
            &current,
            Box::new(move |name: String| {
                queue.borrow_mut().push_back(UiCommand::ThemeSelected(name));
            }),
            Box::new(move || {
                queue_cancel
                    .borrow_mut()
                    .push_back(UiCommand::ThemePreview(original.clone()));
                queue_cancel
                    .borrow_mut()
                    .push_back(UiCommand::RestoreEditor);
            }),
            Box::new(move |name: String| {
                queue_preview
                    .borrow_mut()
                    .push_back(UiCommand::ThemePreview(name));
            }),
        );
        self.mount_selector(Box::new(selector));
    }

    fn show_thinking_selector(&mut self) {
        let available: Vec<ModelThinkingLevel> = self
            .session
            .get_available_thinking_levels()
            .into_iter()
            .map(agent_thinking_to_model)
            .collect();
        if available.is_empty() {
            self.show_status("No thinking levels available for the current model");
            return;
        }
        let queue = self.commands.clone();
        let queue_cancel = self.commands.clone();
        let selector = ThinkingSelectorComponent::new(
            agent_thinking_to_model(self.session.thinking_level()),
            &available,
            Box::new(move |level| {
                let mut queue = queue.borrow_mut();
                queue.push_back(UiCommand::SettingChanged(Box::new(
                    SettingChange::Thinking(level),
                )));
                queue.push_back(UiCommand::RestoreEditor);
            }),
            Box::new(move || {
                queue_cancel
                    .borrow_mut()
                    .push_back(UiCommand::RestoreEditor);
            }),
        );
        self.mount_selector(Box::new(selector));
    }

    fn auth_providers(&self, mode: OAuthSelectorMode) -> Vec<OAuthProvider> {
        let services = self.runtime.services();
        let Ok(registry) = services.model_registry.try_read() else {
            return Vec::new();
        };
        let mut provider_ids: Vec<String> = registry
            .get_all()
            .iter()
            .map(|model| model.provider.clone())
            .collect();
        provider_ids.sort();
        provider_ids.dedup();
        let mut providers = Vec::new();
        for id in provider_ids {
            let configured =
                services.auth_storage.get_sync(&id).ok().flatten().map(
                    |credential| match credential {
                        pi_ai::auth::Credential::OAuth(_) => AuthType::OAuth,
                        pi_ai::auth::Credential::ApiKey(_) => AuthType::ApiKey,
                    },
                );
            if pi_ai::oauth::get_oauth_login_provider(&id).is_some()
                && (mode == OAuthSelectorMode::Login || configured == Some(AuthType::OAuth))
            {
                providers.push(OAuthProvider {
                    id: id.clone(),
                    name: id.clone(),
                    auth_type: AuthType::OAuth,
                    configured_credential: configured,
                    auth_status: None,
                });
            }
            if mode == OAuthSelectorMode::Login || configured == Some(AuthType::ApiKey) {
                providers.push(OAuthProvider {
                    id: id.clone(),
                    name: id,
                    auth_type: AuthType::ApiKey,
                    configured_credential: configured,
                    auth_status: None,
                });
            }
        }
        providers
    }

    fn start_oauth_login(&mut self, provider_id: String) {
        let Some(provider) = pi_ai::oauth::get_oauth_login_provider(&provider_id) else {
            self.show_error(&format!("Failed to login to {provider_id}"));
            return;
        };
        let cancellation = pi_ai::oauth::device_code::CancellationFlag::default();
        self.oauth_cancel = Some(cancellation.clone());

        let cancel_queue = self.commands.clone();
        let submit_queue = self.commands.clone();
        let mut dialog = LoginDialogComponent::new(
            &provider_id,
            move |success, _message| {
                if !success {
                    cancel_queue
                        .borrow_mut()
                        .push_back(UiCommand::OAuthCancelled);
                }
            },
            Some(&provider_id),
            None,
        );
        dialog.on_submit = Some(Box::new(move |value| {
            submit_queue
                .borrow_mut()
                .push_back(UiCommand::OAuthPromptSubmitted(value));
        }));
        dialog.show_waiting("Preparing login...");
        let dialog = Rc::new(RefCell::new(dialog));
        self.oauth_dialog = Some(dialog.clone());
        self.mount_selector(Box::new(Shared::new(dialog)));

        let ui_tx = self.oauth_ui_tx.clone();
        let on_auth_tx = ui_tx.clone();
        let on_device_tx = ui_tx.clone();
        let on_prompt_tx = ui_tx.clone();
        let on_progress_tx = ui_tx.clone();
        let on_manual_tx = ui_tx.clone();
        let on_select_tx = ui_tx;
        let callbacks = pi_ai::oauth::OAuthLoginCallbacks {
            on_auth: Box::new(move |info| {
                let _ = on_auth_tx.send(OAuthUiRequest::Auth(info));
            }),
            on_device_code: Box::new(move |info| {
                let _ = on_device_tx.send(OAuthUiRequest::DeviceCode(info));
            }),
            on_prompt: Box::new(move |prompt| {
                let (tx, rx) = tokio::sync::oneshot::channel();
                let _ = on_prompt_tx.send(OAuthUiRequest::Prompt(prompt, tx));
                Box::pin(async move { rx.await.unwrap_or_default() })
            }),
            on_progress: Some(Box::new(move |message| {
                let _ = on_progress_tx.send(OAuthUiRequest::Progress(message.to_owned()));
            })),
            on_manual_code_input: Some(Box::new(move || {
                let (tx, rx) = tokio::sync::oneshot::channel();
                let prompt = pi_ai::oauth::OAuthPrompt {
                    message: "Paste authorization code or redirect URL:".to_owned(),
                    placeholder: None,
                    allow_empty: Some(false),
                };
                let _ = on_manual_tx.send(OAuthUiRequest::Prompt(prompt, tx));
                Box::pin(async move { rx.await.unwrap_or_default() })
            })),
            on_select: Box::new(move |prompt| {
                let (tx, rx) = tokio::sync::oneshot::channel();
                let _ = on_select_tx.send(OAuthUiRequest::Select(prompt, tx));
                Box::pin(async move { rx.await.unwrap_or(None) })
            }),
            cancellation: Some(cancellation),
            open_browser: true,
        };

        let services = self.runtime.services();
        let auth = services.auth_storage.clone();
        let registry = services.model_registry.clone();
        self.ops.push(Box::pin(async move {
            let result = match provider.login(&callbacks).await {
                Ok(credential) => auth
                    .set(&provider_id, pi_ai::auth::Credential::OAuth(credential))
                    .await
                    .map_err(|error| error.to_string()),
                Err(error) => Err(error.to_string()),
            };
            if result.is_ok() {
                registry.write().await.refresh();
            }
            OpOutcome::AuthChanged {
                provider: provider_id,
                auth_type: AuthType::OAuth,
                logging_in: true,
                result,
            }
        }));
    }

    fn handle_oauth_ui_request(&mut self, request: OAuthUiRequest) {
        match request {
            OAuthUiRequest::Auth(info) => {
                if let Some(dialog) = &self.oauth_dialog {
                    dialog
                        .borrow_mut()
                        .show_auth(&info.url, info.instructions.as_deref());
                }
                self.tui.request_render(false);
            }
            OAuthUiRequest::DeviceCode(info) => {
                if let Some(dialog) = &self.oauth_dialog {
                    dialog
                        .borrow_mut()
                        .show_device_code(&info.verification_uri, &info.user_code);
                }
                self.tui.request_render(false);
            }
            OAuthUiRequest::Prompt(prompt, reply) => {
                self.oauth_prompt_reply = Some(reply);
                if let Some(dialog) = &self.oauth_dialog {
                    dialog
                        .borrow_mut()
                        .show_prompt(&prompt.message, prompt.placeholder.as_deref());
                }
                self.tui.request_render(false);
            }
            OAuthUiRequest::Progress(message) => {
                if let Some(dialog) = &self.oauth_dialog {
                    dialog.borrow_mut().show_progress(&message);
                }
                self.tui.request_render(false);
            }
            OAuthUiRequest::Select(prompt, reply) => {
                self.oauth_select_reply = Some(reply);
                let options = prompt.options;
                let labels: Vec<String> =
                    options.iter().map(|option| option.label.clone()).collect();
                let queue = self.commands.clone();
                let cancel_queue = self.commands.clone();
                let mut selector = ExtensionSelector::new(prompt.message, labels);
                selector.on_submit = Some(Box::new(move |label| {
                    let selected = options
                        .iter()
                        .find(|option| option.label == label)
                        .map(|option| option.id.clone());
                    queue
                        .borrow_mut()
                        .push_back(UiCommand::OAuthSelectSubmitted(selected));
                }));
                selector.on_cancel = Some(Box::new(move || {
                    cancel_queue
                        .borrow_mut()
                        .push_back(UiCommand::OAuthSelectSubmitted(None));
                }));
                self.mount_selector(Box::new(selector));
            }
        }
    }

    fn remount_oauth_dialog(&mut self) {
        let Some(dialog) = self.oauth_dialog.clone() else {
            return;
        };
        self.mount_selector(Box::new(Shared::new(dialog)));
    }

    fn show_auth_selector(&mut self, mode: OAuthSelectorMode, initial_search: Option<&str>) {
        let providers = self.auth_providers(mode);
        if providers.is_empty() {
            self.show_status(
                "No stored credentials to remove. /logout only removes credentials saved by /login; environment variables and models.json config are unchanged.",
            );
            return;
        }
        let queue = self.commands.clone();
        let cancel_queue = self.commands.clone();
        let mut selector = OAuthSelector::new(
            mode,
            providers,
            move |provider, auth_type| {
                let command = match mode {
                    OAuthSelectorMode::Login => {
                        UiCommand::LoginProviderSelected(provider, auth_type)
                    }
                    OAuthSelectorMode::Logout => {
                        UiCommand::LogoutProviderSelected(provider, auth_type)
                    }
                };
                queue.borrow_mut().push_back(command);
            },
            move || {
                cancel_queue
                    .borrow_mut()
                    .push_back(UiCommand::RestoreEditor)
            },
        );
        if let Some(initial_search) = initial_search {
            selector.set_initial_search(initial_search);
        }
        self.mount_selector(Box::new(selector));
    }

    fn show_api_key_login_dialog(&mut self, provider: &str) {
        let restore_queue = self.commands.clone();
        let submit_queue = self.commands.clone();
        let provider_id = provider.to_owned();
        let submit_provider = provider_id.clone();
        let mut dialog = LoginDialogComponent::new(
            &provider_id,
            move |success, _message| {
                if !success {
                    restore_queue
                        .borrow_mut()
                        .push_back(UiCommand::RestoreEditor);
                }
            },
            Some(provider),
            None,
        );
        dialog.set_masked(true);
        dialog.show_prompt("Enter API key:", None);
        dialog.on_submit = Some(Box::new(move |api_key| {
            submit_queue
                .borrow_mut()
                .push_back(UiCommand::LoginApiKey(submit_provider.clone(), api_key));
        }));
        self.mount_selector(Box::new(dialog));
    }

    fn spawn_auth_change(
        &mut self,
        provider: String,
        auth_type: AuthType,
        logging_in: bool,
        api_key: Option<String>,
    ) {
        let services = self.runtime.services();
        let auth = services.auth_storage.clone();
        let registry = services.model_registry.clone();
        self.ops.push(Box::pin(async move {
            let result = if logging_in {
                let credential = pi_ai::auth::Credential::ApiKey(pi_ai::auth::ApiKeyCredential {
                    key: api_key,
                    ..Default::default()
                });
                auth.set(&provider, credential)
                    .await
                    .map_err(|error| error.to_string())
            } else {
                auth.remove(&provider)
                    .await
                    .map_err(|error| error.to_string())
            };
            if result.is_ok() {
                registry.write().await.refresh();
            }
            OpOutcome::AuthChanged {
                provider,
                auth_type,
                logging_in,
                result,
            }
        }));
    }

    fn show_images_selector(&mut self) {
        let current = self
            .runtime
            .services()
            .settings_manager
            .lock()
            .settings()
            .get("terminal")
            .and_then(|value| value.get("showImages"))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(true);
        let queue = self.commands.clone();
        let queue_cancel = self.commands.clone();
        let selector = ShowImagesSelectorComponent::new(
            current,
            Box::new(move |shown| {
                let mut queue = queue.borrow_mut();
                queue.push_back(UiCommand::SettingChanged(Box::new(SettingChange::Nested {
                    section: "terminal",
                    key: "showImages",
                    value: serde_json::Value::Bool(shown),
                    rebuild_chat: true,
                })));
                queue.push_back(UiCommand::RestoreEditor);
            }),
            Box::new(move || {
                queue_cancel
                    .borrow_mut()
                    .push_back(UiCommand::RestoreEditor);
            }),
        );
        self.mount_selector(Box::new(selector));
    }

    fn show_session_selector(&mut self) {
        let (cwd, session_dir, session_file) = self.session.with_session_manager(|sm| {
            (
                sm.get_cwd().to_string_lossy().into_owned(),
                sm.get_session_dir().to_path_buf(),
                sm.get_session_file().map(std::path::Path::to_path_buf),
            )
        });
        let queue = self.commands.clone();
        let queue_cancel = self.commands.clone();
        let queue_exit = self.commands.clone();
        let dir_all = session_dir.clone();
        let selector = SessionSelectorComponent::new(
            Box::new(move |_on_progress| {
                SessionManager::list(&cwd, Some(session_dir.clone()), None)
                    .map_err(|e| e.to_string())
            }),
            Box::new(move |_on_progress| {
                SessionManager::list_all(Some(dir_all.clone()), None).map_err(|e| e.to_string())
            }),
            Box::new(move |path: &std::path::Path| {
                queue
                    .borrow_mut()
                    .push_back(UiCommand::SessionSelected(path.to_path_buf()));
            }),
            Box::new(move || {
                queue_cancel
                    .borrow_mut()
                    .push_back(UiCommand::RestoreEditor);
            }),
            Box::new(move || {
                queue_exit
                    .borrow_mut()
                    .push_back(UiCommand::SessionSelectorExit);
            }),
            Box::new(|| {}),
            SessionSelectorOptions::default(),
            session_file.as_deref(),
        );
        self.mount_selector(Box::new(selector));
    }

    fn show_user_message_selector(&mut self) {
        let user_messages = self.session.get_user_messages_for_forking();
        if user_messages.is_empty() {
            self.show_status("No messages to fork from");
            return;
        }
        let items: Vec<UserMessageItem> = user_messages
            .into_iter()
            .map(|(id, text)| UserMessageItem {
                id,
                text,
                timestamp: None,
            })
            .collect();
        let queue = self.commands.clone();
        let queue_cancel = self.commands.clone();
        let selector = UserMessageSelectorComponent::new(
            items,
            Box::new(move |id: &str| {
                queue
                    .borrow_mut()
                    .push_back(UiCommand::ForkSelected(id.to_owned()));
            }),
            Box::new(move || {
                queue_cancel
                    .borrow_mut()
                    .push_back(UiCommand::RestoreEditor);
            }),
            None,
        );
        self.mount_selector(Box::new(selector));
    }

    fn show_tree_selector(&mut self) {
        let (tree, leaf_id) = self
            .session
            .with_session_manager(|sm| (sm.get_tree(), sm.get_leaf_id().map(str::to_owned)));
        if tree.is_empty() {
            self.show_status("No entries in session");
            return;
        }
        let rows = self.tui.terminal().rows();
        let queue = self.commands.clone();
        let queue_cancel = self.commands.clone();
        let selector = TreeSelectorComponent::new(
            tree,
            leaf_id.as_deref(),
            rows,
            Box::new(move |id: &str| {
                queue
                    .borrow_mut()
                    .push_back(UiCommand::TreeSelected(id.to_owned()));
            }),
            Box::new(move || {
                queue_cancel
                    .borrow_mut()
                    .push_back(UiCommand::RestoreEditor);
            }),
            None,
            None,
            None,
        );
        self.mount_selector(Box::new(selector));
    }

    fn show_settings_selector(&mut self) {
        let services = self.runtime.services();
        let auto_compact = self.session.auto_compaction_enabled();
        let steering_mode = self.session.steering_mode().to_owned();
        let follow_up_mode = self.session.follow_up_mode().to_owned();
        let thinking_level = agent_thinking_to_model(self.session.thinking_level());
        let available_thinking_levels = self
            .session
            .get_available_thinking_levels()
            .into_iter()
            .map(agent_thinking_to_model)
            .collect();
        let config = {
            let settings = services.settings_manager.lock();
            let raw = settings.settings();
            let top_bool = |key: &str, default: bool| {
                raw.get(key)
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(default)
            };
            let nested_bool = |section: &str, key: &str, default: bool| {
                raw.get(section)
                    .and_then(|value| value.get(key))
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(default)
            };
            let top_u64 = |key: &str, default: u64| {
                raw.get(key)
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(default)
            };
            let nested_u64 = |section: &str, key: &str, default: u64| {
                raw.get(section)
                    .and_then(|value| value.get(key))
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(default)
            };
            let transport = match settings.get_transport() {
                "sse" => Transport::Sse,
                "websocket" => Transport::Websocket,
                "websocket-cached" => Transport::WebsocketCached,
                _ => Transport::Auto,
            };
            SettingsConfig {
                auto_compact,
                show_images: nested_bool("terminal", "showImages", true),
                image_width_cells: nested_u64("terminal", "imageWidthCells", 60) as u32,
                auto_resize_images: settings.get_image_auto_resize(),
                block_images: nested_bool("images", "blockImages", false),
                enable_skill_commands: top_bool("enableSkillCommands", true),
                steering_mode,
                follow_up_mode,
                transport,
                http_idle_timeout_ms: settings.get_http_idle_timeout_ms(),
                thinking_level,
                available_thinking_levels,
                current_theme: raw.get_str("theme").unwrap_or("dark").to_owned(),
                terminal_theme: detect_terminal_background_from_env(None).theme,
                available_themes: get_available_themes(),
                hide_thinking_block: self.hide_thinking_block,
                show_cache_miss_notices: top_bool("showCacheMissNotices", false),
                collapse_changelog: top_bool("collapseChangelog", false),
                enable_install_telemetry: top_bool("enableInstallTelemetry", true),
                double_escape_action: raw
                    .get_str("doubleEscapeAction")
                    .unwrap_or("tree")
                    .to_owned(),
                tree_filter_mode: raw
                    .get_str("treeFilterMode")
                    .unwrap_or("default")
                    .to_owned(),
                show_hardware_cursor: top_bool(
                    "showHardwareCursor",
                    std::env::var("PI_HARDWARE_CURSOR").ok().as_deref() == Some("1"),
                ),
                editor_padding_x: top_u64("editorPaddingX", 0) as u32,
                output_pad: if top_u64("outputPad", 1) == 0 { 0 } else { 1 },
                autocomplete_max_visible: top_u64("autocompleteMaxVisible", 5) as u32,
                quiet_startup: settings.get_quiet_startup(),
                default_project_trust: settings.get_default_project_trust().to_owned(),
                clear_on_shrink: nested_bool(
                    "terminal",
                    "clearOnShrink",
                    std::env::var("PI_CLEAR_ON_SHRINK").ok().as_deref() == Some("1"),
                ),
                show_terminal_progress: nested_bool("terminal", "showTerminalProgress", false),
            }
        };

        let q = self.commands.clone();
        let q_auto = q.clone();
        let q_steering = q.clone();
        let q_follow = q.clone();
        let q_transport = q.clone();
        let q_timeout = q.clone();
        let q_thinking = q.clone();
        let q_theme = q.clone();
        let q_hide = q.clone();
        let q_hardware = q.clone();
        let q_padding = q.clone();
        let q_output = q.clone();
        let q_autocomplete = q.clone();
        let q_clear = q.clone();
        let q_width = q.clone();
        let q_cancel = q.clone();
        let callbacks = SettingsCallbacks {
            on_auto_compact_change: Box::new(move |value| {
                q_auto
                    .borrow_mut()
                    .push_back(UiCommand::SettingChanged(Box::new(
                        SettingChange::AutoCompact(value),
                    )));
            }),
            on_show_images_change: queue_nested_bool(q.clone(), "terminal", "showImages", true),
            on_image_width_cells_change: Box::new(move |value| {
                q_width
                    .borrow_mut()
                    .push_back(UiCommand::SettingChanged(Box::new(SettingChange::Nested {
                        section: "terminal",
                        key: "imageWidthCells",
                        value: serde_json::Value::from(value),
                        rebuild_chat: true,
                    })));
            }),
            on_auto_resize_images_change: queue_nested_bool(
                q.clone(),
                "images",
                "autoResize",
                false,
            ),
            on_block_images_change: queue_nested_bool(q.clone(), "images", "blockImages", false),
            on_enable_skill_commands_change: queue_top_bool(
                q.clone(),
                "enableSkillCommands",
                false,
            ),
            on_steering_mode_change: Box::new(move |value| {
                q_steering
                    .borrow_mut()
                    .push_back(UiCommand::SettingChanged(Box::new(
                        SettingChange::Steering(value.to_owned()),
                    )));
            }),
            on_follow_up_mode_change: Box::new(move |value| {
                q_follow
                    .borrow_mut()
                    .push_back(UiCommand::SettingChanged(Box::new(
                        SettingChange::FollowUp(value.to_owned()),
                    )));
            }),
            on_transport_change: Box::new(move |value| {
                let value = match value {
                    Transport::Sse => "sse",
                    Transport::Websocket => "websocket",
                    Transport::WebsocketCached => "websocket-cached",
                    Transport::Auto => "auto",
                };
                q_transport
                    .borrow_mut()
                    .push_back(UiCommand::SettingChanged(Box::new(SettingChange::Top {
                        key: "transport",
                        value: serde_json::Value::String(value.to_owned()),
                        rebuild_chat: false,
                    })));
            }),
            on_http_idle_timeout_ms_change: Box::new(move |value| {
                q_timeout
                    .borrow_mut()
                    .push_back(UiCommand::SettingChanged(Box::new(
                        SettingChange::HttpIdleTimeout(value),
                    )));
            }),
            on_thinking_level_change: Box::new(move |value| {
                q_thinking
                    .borrow_mut()
                    .push_back(UiCommand::SettingChanged(Box::new(
                        SettingChange::Thinking(value),
                    )));
            }),
            on_theme_change: Box::new(move |value| {
                q_theme
                    .borrow_mut()
                    .push_back(UiCommand::SettingChanged(Box::new(SettingChange::Theme(
                        value.to_owned(),
                    ))));
            }),
            on_theme_preview: Some(Box::new(|name| {
                let _ = set_theme(name, false);
            })),
            on_hide_thinking_block_change: Box::new(move |value| {
                q_hide
                    .borrow_mut()
                    .push_back(UiCommand::SettingChanged(Box::new(
                        SettingChange::HideThinking(value),
                    )));
            }),
            on_show_cache_miss_notices_change: queue_top_bool(
                q.clone(),
                "showCacheMissNotices",
                true,
            ),
            on_collapse_changelog_change: queue_top_bool(q.clone(), "collapseChangelog", false),
            on_enable_install_telemetry_change: queue_top_bool(
                q.clone(),
                "enableInstallTelemetry",
                false,
            ),
            on_double_escape_action_change: queue_top_string(q.clone(), "doubleEscapeAction"),
            on_tree_filter_mode_change: queue_top_string(q.clone(), "treeFilterMode"),
            on_show_hardware_cursor_change: Box::new(move |value| {
                q_hardware
                    .borrow_mut()
                    .push_back(UiCommand::SettingChanged(Box::new(
                        SettingChange::HardwareCursor(value),
                    )));
            }),
            on_editor_padding_x_change: Box::new(move |value| {
                q_padding
                    .borrow_mut()
                    .push_back(UiCommand::SettingChanged(Box::new(
                        SettingChange::EditorPadding(value),
                    )));
            }),
            on_output_pad_change: Box::new(move |value| {
                q_output
                    .borrow_mut()
                    .push_back(UiCommand::SettingChanged(Box::new(
                        SettingChange::OutputPad(value),
                    )));
            }),
            on_autocomplete_max_visible_change: Box::new(move |value| {
                q_autocomplete
                    .borrow_mut()
                    .push_back(UiCommand::SettingChanged(Box::new(
                        SettingChange::AutocompleteMax(value),
                    )));
            }),
            on_quiet_startup_change: queue_top_bool(q.clone(), "quietStartup", false),
            on_default_project_trust_change: queue_top_string(q.clone(), "defaultProjectTrust"),
            on_clear_on_shrink_change: Box::new(move |value| {
                q_clear
                    .borrow_mut()
                    .push_back(UiCommand::SettingChanged(Box::new(
                        SettingChange::ClearOnShrink(value),
                    )));
            }),
            on_show_terminal_progress_change: queue_nested_bool(
                q,
                "terminal",
                "showTerminalProgress",
                false,
            ),
            on_cancel: Box::new(move || {
                q_cancel.borrow_mut().push_back(UiCommand::RestoreEditor);
            }),
        };
        self.mount_selector(Box::new(SettingsSelectorComponent::new(config, callbacks)));
    }

    fn show_trust_selector(&mut self) {
        let services = self.runtime.services();
        let cwd = services.cwd.clone();
        let store = super::trust_store::ProjectTrustStore::new(&services.agent_dir);
        let saved_decision = match store.get_entry(&cwd) {
            Ok(entry) => entry,
            Err(error) => {
                self.show_error(&error);
                return;
            }
        };
        let project_trusted = services.settings_manager.lock().is_project_trusted();
        let queue = self.commands.clone();
        let queue_cancel = self.commands.clone();
        self.mount_selector(Box::new(TrustSelectorComponent::new(
            TrustSelectorOptions {
                cwd: cwd.display().to_string(),
                saved_decision,
                project_trusted,
                on_select: Box::new(move |selection| {
                    queue
                        .borrow_mut()
                        .push_back(UiCommand::TrustSelected(Box::new(selection)));
                }),
                on_cancel: Box::new(move || {
                    queue_cancel
                        .borrow_mut()
                        .push_back(UiCommand::RestoreEditor);
                }),
            },
        )));
    }

    // ========================================================================
    // Command handlers
    // ========================================================================

    fn handle_clone_command(&mut self) {
        let leaf = self
            .session
            .with_session_manager(|sm| sm.get_leaf_id().map(str::to_owned));
        let Some(leaf) = leaf else {
            self.show_status("Nothing to clone yet");
            return;
        };
        let runtime = self.runtime.clone();
        self.ops.push(Box::pin(async move {
            OpOutcome::NewSessionCreated(
                runtime
                    .fork(&leaf, crate::extension_bridge::ForkPosition::At)
                    .await,
            )
        }));
        self.show_status("Cloned to new session");
    }

    fn handle_clear_command(&mut self) {
        self.clear_status_indicator(None);
        let runtime = self.runtime.clone();
        self.ops.push(Box::pin(async move {
            OpOutcome::NewSessionCreated(runtime.new_session(None).await)
        }));
    }

    fn handle_name_command(&mut self, raw: &str) {
        let name = raw.strip_prefix("/name").map(str::trim).unwrap_or_default();
        if name.is_empty() {
            self.show_status("Usage: /name <name>");
            return;
        }
        self.session.set_session_name(name);
        let stored = self.session.session_name();
        match stored {
            Some(stored) if stored != name => {
                self.show_status(&format!(
                    "Session name was normalized from {name:?} to {stored:?}"
                ));
                self.show_status(&format!("Session name set: {stored}"));
            }
            Some(stored) => {
                self.show_status(&format!("Session name set: {stored}"));
            }
            None => {}
        }
        self.update_terminal_title();
    }

    fn handle_copy_command(&mut self) {
        match self.session.get_last_assistant_text() {
            Some(text) => {
                // OSC 52 through the terminal (TUI-safe); arboard fallback.
                if let Some(sequence) = pi_tui::clipboard::encode_osc52(&text) {
                    self.tui.terminal_mut().write(&sequence);
                    self.show_status("Copied last agent message to clipboard");
                } else {
                    match pi_tui::clipboard::set_text(&text) {
                        Ok(()) => self.show_status("Copied last agent message to clipboard"),
                        Err(error) => self.show_error(&error.to_string()),
                    }
                }
            }
            None => self.show_status("No agent messages to copy yet."),
        }
    }

    fn handle_follow_up(&mut self) {
        let text = self.editor.borrow().get_text();
        let text = text.trim().to_owned();
        if text.is_empty() {
            return;
        }
        if !self.session.is_streaming() && !self.session.is_compacting() {
            return;
        }
        self.editor.borrow_mut().add_to_history(&text);
        self.editor.borrow_mut().set_text("");
        if self.session.is_compacting() {
            self.queue_compaction_message(text, StreamingBehavior::FollowUp);
            return;
        }
        self.session.follow_up(&text, Vec::new());
        self.update_pending_messages_display();
        self.tui.request_render(false);
    }

    fn handle_session_command(&mut self) {
        let stats = self.session.get_session_stats();
        let session_name = self.session.session_name();
        let t = theme();

        let mut info = format!("{}\n\n", t.bold("Session Info"));
        if let Some(name) = session_name {
            info.push_str(&format!("{} {name}\n", t.fg(ThemeColor::Dim, "Name:")));
        }
        info.push_str(&format!(
            "{} {}\n",
            t.fg(ThemeColor::Dim, "File:"),
            stats.session_file.as_deref().unwrap_or("In-memory")
        ));
        info.push_str(&format!(
            "{} {}\n\n",
            t.fg(ThemeColor::Dim, "ID:"),
            stats.session_id
        ));
        info.push_str(&format!("{}\n", t.bold("Messages")));
        info.push_str(&format!(
            "{} {}\n",
            t.fg(ThemeColor::Dim, "Total:"),
            stats.total_messages
        ));
        info.push_str(&format!(
            "{} {}\n",
            t.fg(ThemeColor::Dim, "User:"),
            stats.user_messages
        ));
        info.push_str(&format!(
            "{} {}\n",
            t.fg(ThemeColor::Dim, "Assistant:"),
            stats.assistant_messages
        ));
        info.push_str(&format!(
            "{} {} calls, {} results\n\n",
            t.fg(ThemeColor::Dim, "Tools:"),
            stats.tool_calls,
            stats.tool_results
        ));
        info.push_str(&format!("{}\n", t.bold("Tokens")));
        let prompt_tokens = stats.tokens.input + stats.tokens.cache_read + stats.tokens.cache_write;
        info.push_str(&format!(
            "{} {}\n",
            t.fg(ThemeColor::Dim, "Input:"),
            group_thousands(prompt_tokens)
        ));
        if prompt_tokens > 0 && (stats.tokens.cache_read > 0 || stats.tokens.cache_write > 0) {
            let hit_rate = t.fg(
                ThemeColor::Dim,
                &format!(
                    "({:.1}%)",
                    stats.tokens.cache_read as f64 / prompt_tokens as f64 * 100.0
                ),
            );
            info.push_str(&format!(
                "  {} {} {hit_rate}\n",
                t.fg(ThemeColor::Dim, "Cached:"),
                group_thousands(stats.tokens.cache_read)
            ));
            let written = if stats.tokens.cache_write > 0 {
                format!(
                    " {}",
                    t.fg(
                        ThemeColor::Dim,
                        &format!(
                            "({} written to cache)",
                            group_thousands(stats.tokens.cache_write)
                        )
                    )
                )
            } else {
                String::new()
            };
            info.push_str(&format!(
                "  {} {}{written}\n",
                t.fg(ThemeColor::Dim, "Uncached:"),
                group_thousands(stats.tokens.input + stats.tokens.cache_write)
            ));
        }
        info.push_str(&format!(
            "{} {}\n",
            t.fg(ThemeColor::Dim, "Output:"),
            group_thousands(stats.tokens.output)
        ));
        info.push_str(&format!(
            "{} {}\n",
            t.fg(ThemeColor::Dim, "Total:"),
            group_thousands(stats.tokens.total)
        ));
        if stats.cost > 0.0 {
            info.push_str(&format!("\n{}\n", t.bold("Cost")));
            info.push_str(&format!(
                "{} ${:.3}",
                t.fg(ThemeColor::Dim, "Total:"),
                stats.cost
            ));
        }

        let mut chat = self.chat.borrow_mut();
        chat.add_child(Spacer::new(1));
        chat.add_child(Text::new(info, 1, 0, None));
        drop(chat);
        self.tui.request_render(false);
    }

    fn handle_changelog_command(&mut self) {
        let changelog_markdown = "No changelog entries found.".to_owned();
        let t = theme();
        let mut chat = self.chat.borrow_mut();
        chat.add_child(Spacer::new(1));
        chat.add_child(DynamicBorder::new(None));
        chat.add_child(Text::new(
            t.bold(&t.fg(ThemeColor::Accent, "What's New")),
            1,
            0,
            None,
        ));
        chat.add_child(Spacer::new(1));
        chat.add_child(Markdown::new(
            changelog_markdown,
            1,
            1,
            super::theme::get_markdown_theme(),
            None,
            None,
        ));
        chat.add_child(DynamicBorder::new(None));
        drop(chat);
        self.tui.request_render(false);
    }

    fn handle_hotkeys_command(&mut self) {
        let e = key_display_text;
        let hotkeys = format!(
            "\
**Navigation**
| Key | Action |
|-----|--------|
| `{cursor_up}` / `{cursor_down}` / `{cursor_left}` / `{cursor_right}` | Move cursor / browse history |
| `{cursor_word_left}` / `{cursor_word_right}` | Move by word |
| `{cursor_line_start}` | Start of line |
| `{cursor_line_end}` | End of line |
| `{jump_forward}` | Jump forward to character |
| `{jump_backward}` | Jump backward to character |
| `{page_up}` / `{page_down}` | Scroll by page |

**Editing**
| Key | Action |
|-----|--------|
| `{submit}` | Send message |
| `{new_line}` | New line |
| `{delete_word_backward}` | Delete word backwards |
| `{delete_word_forward}` | Delete word forwards |
| `{delete_to_line_start}` | Delete to start of line |
| `{delete_to_line_end}` | Delete to end of line |
| `{yank}` | Paste the most-recently-deleted text |
| `{yank_pop}` | Cycle through the deleted text after pasting |
| `{undo}` | Undo |

**Other**
| Key | Action |
|-----|--------|
| `{tab}` | Path completion / accept autocomplete |
| `{interrupt}` | Cancel autocomplete / abort streaming |
| `{clear}` | Clear editor (first) / exit (second) |
| `{exit}` | Exit (when editor is empty) |
| `{suspend}` | Suspend to background |
| `{cycle_thinking}` | Cycle thinking level |
| `{cycle_model_forward}` / `{cycle_model_backward}` | Cycle models |
| `{select_model}` | Open model selector |
| `{expand_tools}` | Toggle tool output expansion |
| `{toggle_thinking}` | Toggle thinking block visibility |
| `{external_editor}` | Edit message in external editor |
| `{copy_message}` | Copy last assistant message |
| `{follow_up}` | Queue follow-up message |
| `{dequeue}` | Restore queued messages |
| `{paste_image}` | Paste image or text from clipboard |
| `/` | Slash commands |
| `!` | Run bash command |
| `!!` | Run bash command (excluded from context) |",
            cursor_up = e("tui.editor.cursorUp"),
            cursor_down = e("tui.editor.cursorDown"),
            cursor_left = e("tui.editor.cursorLeft"),
            cursor_right = e("tui.editor.cursorRight"),
            cursor_word_left = e("tui.editor.cursorWordLeft"),
            cursor_word_right = e("tui.editor.cursorWordRight"),
            cursor_line_start = e("tui.editor.cursorLineStart"),
            cursor_line_end = e("tui.editor.cursorLineEnd"),
            jump_forward = e("tui.editor.jumpForward"),
            jump_backward = e("tui.editor.jumpBackward"),
            page_up = e("tui.editor.pageUp"),
            page_down = e("tui.editor.pageDown"),
            submit = e("tui.input.submit"),
            new_line = e("tui.input.newLine"),
            delete_word_backward = e("tui.editor.deleteWordBackward"),
            delete_word_forward = e("tui.editor.deleteWordForward"),
            delete_to_line_start = e("tui.editor.deleteToLineStart"),
            delete_to_line_end = e("tui.editor.deleteToLineEnd"),
            yank = e("tui.editor.yank"),
            yank_pop = e("tui.editor.yankPop"),
            undo = e("tui.editor.undo"),
            tab = e("tui.input.tab"),
            interrupt = e("app.interrupt"),
            clear = e("app.clear"),
            exit = e("app.exit"),
            suspend = e("app.suspend"),
            cycle_thinking = e("app.thinking.cycle"),
            cycle_model_forward = e("app.model.cycleForward"),
            cycle_model_backward = e("app.model.cycleBackward"),
            select_model = e("app.model.select"),
            expand_tools = e("app.tools.expand"),
            toggle_thinking = e("app.thinking.toggle"),
            external_editor = e("app.editor.external"),
            copy_message = e("app.message.copy"),
            follow_up = e("app.message.followUp"),
            dequeue = e("app.message.dequeue"),
            paste_image = e("app.clipboard.pasteImage"),
        );

        let t = theme();
        let mut chat = self.chat.borrow_mut();
        chat.add_child(Spacer::new(1));
        chat.add_child(DynamicBorder::new(None));
        chat.add_child(Text::new(
            t.bold(&t.fg(ThemeColor::Accent, "Keyboard Shortcuts")),
            1,
            0,
            None,
        ));
        chat.add_child(Spacer::new(1));
        chat.add_child(Markdown::new(
            hotkeys,
            1,
            1,
            super::theme::get_markdown_theme(),
            None,
            None,
        ));
        chat.add_child(DynamicBorder::new(None));
        drop(chat);
        self.tui.request_render(false);
    }

    fn handle_export_command(&mut self, raw: &str) {
        let arg = raw
            .strip_prefix("/export")
            .map(str::trim)
            .unwrap_or_default();
        let target = if arg.is_empty() {
            format!("{}.jsonl", self.session.session_id())
        } else {
            arg.to_owned()
        };
        if !target.ends_with(".jsonl") {
            self.show_error("Export failed: only .jsonl export is available in this build");
            return;
        }
        let entries = self
            .session
            .with_session_manager(crate::session_manager::SessionManager::get_entries);
        let mut out = String::new();
        for entry in &entries {
            match serde_json::to_string(entry) {
                Ok(line) => {
                    out.push_str(&line);
                    out.push('\n');
                }
                Err(error) => {
                    self.show_error(&format!("Export failed: {error}"));
                    return;
                }
            }
        }
        match std::fs::write(&target, out) {
            Ok(()) => self.show_status(&format!("Exported session to: {target}")),
            Err(error) => self.show_error(&format!("Export failed: {error}")),
        }
    }

    fn handle_import_command(&mut self, raw: &str) {
        let arg = raw
            .strip_prefix("/import")
            .map(str::trim)
            .unwrap_or_default();
        if arg.is_empty() || !arg.ends_with(".jsonl") {
            self.show_status("Usage: /import <path.jsonl>");
            return;
        }
        let path = PathBuf::from(arg);
        if !path.exists() {
            self.show_error(&format!("Import failed: {arg} not found"));
            return;
        }
        let runtime = self.runtime.clone();
        self.ops.push(Box::pin(async move {
            OpOutcome::SessionSwitched(runtime.switch_session(&path, None).await)
        }));
    }

    fn handle_share_command(&mut self) {
        if std::process::Command::new("gh")
            .arg("--version")
            .output()
            .is_err()
        {
            self.show_error(
                "GitHub CLI (gh) is not installed. Install it from https://cli.github.com/",
            );
            return;
        }
        let logged_in = std::process::Command::new("gh")
            .args(["auth", "status"])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !logged_in {
            self.show_error("GitHub CLI is not logged in. Run 'gh auth login' first.");
            return;
        }
        self.show_error("Share failed: HTML export is not available in this build");
    }

    fn handle_reload_command(&mut self) {
        let services = self.runtime.services();
        set_keybindings(create_app_keybindings(&services.agent_dir));
        let theme_setting = {
            let settings = services.settings_manager.lock();
            settings.get_theme().map(str::to_owned)
        };
        if let Some(name) = theme_setting {
            let _ = set_theme(&name, false);
        }
        self.tui.invalidate();
        self.show_status("Reloaded keybindings and themes");
        self.tui.request_render(false);
    }

    fn handle_debug_command(&mut self) {
        let path = std::env::temp_dir().join(format!("pi-debug-{}.txt", std::process::id()));
        let mut dump = String::new();
        dump.push_str(&format!(
            "terminal: {}x{}\n",
            self.tui.terminal().columns(),
            self.tui.terminal().rows()
        ));
        dump.push_str(&format!("session: {}\n", self.session.session_id()));
        match std::fs::write(&path, dump) {
            Ok(()) => self.show_status(&format!("✓ Debug log written to {}", path.display())),
            Err(error) => self.show_error(&error.to_string()),
        }
    }

    // ========================================================================
    // Queue display + compaction queue
    // ========================================================================

    fn all_queued_messages(&self) -> (Vec<String>, Vec<String>) {
        let mut steering = self.session.get_steering_messages();
        let mut follow_up = self.session.get_follow_up_messages();
        for queued in &self.compaction_queued {
            match queued.mode {
                StreamingBehavior::Steer => steering.push(queued.text.clone()),
                StreamingBehavior::FollowUp => follow_up.push(queued.text.clone()),
            }
        }
        (steering, follow_up)
    }

    fn update_pending_messages_display(&mut self) {
        let (steering, follow_up) = self.all_queued_messages();
        let mut pending = self.pending_messages.borrow_mut();
        pending.clear();
        if !steering.is_empty() || !follow_up.is_empty() {
            let t = theme();
            pending.add_child(Spacer::new(1));
            for message in &steering {
                let text = t.fg(ThemeColor::Dim, &format!("Steering: {message}"));
                pending.add_child(TruncatedText::new(text, 1, 0));
            }
            for message in &follow_up {
                let text = t.fg(ThemeColor::Dim, &format!("Follow-up: {message}"));
                pending.add_child(TruncatedText::new(text, 1, 0));
            }
            let dequeue_hint = key_display_text("app.message.dequeue");
            let hint = t.fg(
                ThemeColor::Dim,
                &format!("↳ {dequeue_hint} to edit all queued messages"),
            );
            pending.add_child(TruncatedText::new(hint, 1, 0));
        }
    }

    /// Oracle `restoreQueuedMessagesToEditor` (:3969-3987).
    fn restore_queued_messages_to_editor(&mut self, abort: bool) -> usize {
        let (steering, follow_up) = {
            let (s, f) = self.session.clear_queue();
            let mut steering = s;
            let mut follow_up = f;
            for queued in self.compaction_queued.drain(..) {
                match queued.mode {
                    StreamingBehavior::Steer => steering.push(queued.text),
                    StreamingBehavior::FollowUp => follow_up.push(queued.text),
                }
            }
            (steering, follow_up)
        };
        let all_queued: Vec<String> = steering.into_iter().chain(follow_up).collect();
        if all_queued.is_empty() {
            self.update_pending_messages_display();
            if abort {
                self.spawn_abort();
            }
            return 0;
        }
        let queued_text = all_queued.join("\n\n");
        let current_text = self.editor.borrow().get_text();
        let combined: Vec<&str> = [queued_text.as_str(), current_text.as_str()]
            .into_iter()
            .filter(|t| !t.trim().is_empty())
            .collect();
        self.editor.borrow_mut().set_text(&combined.join("\n\n"));
        self.update_pending_messages_display();
        if abort {
            self.spawn_abort();
        }
        all_queued.len()
    }

    fn spawn_abort(&mut self) {
        let session = self.session.clone();
        self.ops.push(Box::pin(async move {
            session.abort().await;
            OpOutcome::PromptFinished(Ok(()))
        }));
    }

    fn queue_compaction_message(&mut self, text: String, mode: StreamingBehavior) {
        self.editor.borrow_mut().add_to_history(&text);
        self.editor.borrow_mut().set_text("");
        self.compaction_queued
            .push(CompactionQueuedMessage { text, mode });
        self.update_pending_messages_display();
        self.show_status("Queued message for after compaction");
    }

    /// Oracle `flushCompactionQueue` (:4008-4083), extension paths elided
    /// (no extension commands pre-Phase-6).
    fn flush_compaction_queue(&mut self, will_retry: bool) {
        if self.compaction_queued.is_empty() {
            return;
        }
        let queued: Vec<CompactionQueuedMessage> = self.compaction_queued.drain(..).collect();
        self.update_pending_messages_display();

        if will_retry {
            for message in queued {
                match message.mode {
                    StreamingBehavior::FollowUp => {
                        self.session.follow_up(&message.text, Vec::new())
                    }
                    StreamingBehavior::Steer => self.session.steer(&message.text, Vec::new()),
                }
            }
            self.update_pending_messages_display();
            return;
        }

        let mut iter = queued.into_iter();
        if let Some(first) = iter.next() {
            let session = self.session.clone();
            let count = 1 + iter.len();
            self.ops.push(Box::pin(async move {
                match session.prompt(&first.text, PromptOptions::default()).await {
                    Ok(()) => OpOutcome::PromptFinished(Ok(())),
                    Err(error) => OpOutcome::FlushQueuePromptFailed(format!(
                        "Failed to send queued message{}: {error}",
                        if count > 1 { "s" } else { "" }
                    )),
                }
            }));
        }
        for message in iter {
            match message.mode {
                StreamingBehavior::FollowUp => self.session.follow_up(&message.text, Vec::new()),
                StreamingBehavior::Steer => self.session.steer(&message.text, Vec::new()),
            }
        }
        self.update_pending_messages_display();
    }

    // ========================================================================
    // Status line / indicator
    // ========================================================================

    fn show_status_indicator(&mut self, indicator: StatusIndicator) {
        let kind = indicator.kind;
        let shared = Rc::new(RefCell::new(indicator));
        let mut status = self.status.borrow_mut();
        status.clear();
        status.add_child(Shared::new(shared.clone()));
        drop(status);
        self.active_status = Some((kind, shared));
    }

    fn clear_status_indicator(&mut self, kind: Option<StatusIndicatorKind>) {
        if let Some(expected) = kind
            && self
                .active_status
                .as_ref()
                .is_none_or(|(active, _)| *active != expected)
        {
            return;
        }
        let had_active = self.active_status.take().is_some();
        let mut status = self.status.borrow_mut();
        status.clear();
        status.add_child(Shared::new(Rc::new(RefCell::new(IdleStatus::new()))));
        drop(status);
        if had_active {
            self.tui.request_render(false);
        }
    }

    /// Oracle `showStatus` (:3144-3162): consecutive status lines update in
    /// place instead of appending.
    fn show_status(&mut self, message: &str) {
        let styled = theme().fg(ThemeColor::Dim, message);
        let chat_len = self.chat.borrow().len();
        if let Some((index, text)) = &self.last_status_text
            && *index + 1 == chat_len
        {
            text.borrow_mut().set_text(styled);
            self.chat.borrow_mut().invalidate();
            self.tui.request_render(false);
            return;
        }
        let text = Rc::new(RefCell::new(Text::new(styled, 1, 0, None)));
        let mut chat = self.chat.borrow_mut();
        chat.add_child(Spacer::new(1));
        let index = chat.len();
        chat.add_child(Shared::new(text.clone()));
        drop(chat);
        self.last_status_text = Some((index, text));
        self.tui.request_render(false);
    }

    fn show_error(&mut self, message: &str) {
        self.last_status_text = None;
        let mut chat = self.chat.borrow_mut();
        chat.add_child(Spacer::new(1));
        chat.add_child(Text::new(
            theme().fg(ThemeColor::Error, message),
            1,
            0,
            None,
        ));
        drop(chat);
        self.tui.request_render(false);
    }

    fn show_warning(&mut self, message: &str) {
        self.last_status_text = None;
        let mut chat = self.chat.borrow_mut();
        chat.add_child(Spacer::new(1));
        chat.add_child(Text::new(
            theme().fg(ThemeColor::Warning, message),
            1,
            0,
            None,
        ));
        drop(chat);
        self.tui.request_render(false);
    }

    // ========================================================================
    // Transcript rendering
    // ========================================================================

    fn set_tools_expanded(&mut self, expanded: bool) {
        self.tool_output_expanded = expanded;
        for tool in self.pending_tools.values() {
            tool.borrow_mut().set_expanded(expanded);
        }
        self.chat.borrow_mut().mark_changed();
        self.tui.request_render(false);
    }

    fn update_terminal_title(&mut self) {
        let (name, cwd) = (self.session.session_name(), self.session.cwd().clone());
        let basename = cwd
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let title = match name {
            Some(name) => format!("pi - {name} - {basename}"),
            None => format!("pi - {basename}"),
        };
        self.tui.terminal_mut().set_title(&title);
    }

    fn render_current_session_state(&mut self) {
        self.chat.borrow_mut().clear();
        self.pending_messages.borrow_mut().clear();
        self.compaction_queued.clear();
        self.streaming_component = None;
        self.pending_tools.clear();
        self.render_initial_messages();
        self.tui.request_render(false);
    }

    fn render_initial_messages(&mut self) {
        let entries = self
            .session
            .with_session_manager(crate::session_manager::SessionManager::build_context_entries);
        self.render_session_entries(&entries, true);

        let compaction_count = self.session.with_session_manager(|sm| {
            sm.get_entries()
                .iter()
                .filter(|e| matches!(e, SessionEntry::Compaction { .. }))
                .count()
        });
        if compaction_count > 0 {
            let times = if compaction_count == 1 {
                "1 time".to_owned()
            } else {
                format!("{compaction_count} times")
            };
            self.show_status(&format!("Session compacted {times}"));
        }
    }

    fn rebuild_chat_from_messages(&mut self) {
        self.chat.borrow_mut().clear();
        self.last_status_text = None;
        let entries = self
            .session
            .with_session_manager(crate::session_manager::SessionManager::build_context_entries);
        self.render_session_entries(&entries, false);
    }

    fn render_session_entries(&mut self, entries: &[SessionEntry], populate_history: bool) {
        self.pending_tools.clear();
        self.last_status_text = None;
        let mut rendered_tools: HashMap<String, Rc<RefCell<ToolExecutionComponent>>> =
            HashMap::new();

        for entry in entries {
            match entry {
                SessionEntry::Message { message, .. } => {
                    let Ok(agent_message) = serde_json::from_value::<AgentMessage>(message.clone())
                    else {
                        continue;
                    };
                    match &agent_message {
                        AgentMessage::Standard(Message::Assistant(am)) => {
                            self.add_message_to_chat(&agent_message);
                            // Mount tool components for the calls in this message.
                            for content in &am.content {
                                if let pi_ai::Content::ToolCall(call) = content {
                                    let mut tool = ToolExecutionComponent::with_call_id(
                                        call.id.clone(),
                                        call.name.clone(),
                                        serde_json::Value::Object(call.arguments.clone()),
                                    );
                                    tool.set_expanded(self.tool_output_expanded);
                                    self.apply_tool_image_settings(&mut tool);
                                    let tool = Rc::new(RefCell::new(tool));
                                    self.chat.borrow_mut().add_child(Shared::new(tool.clone()));
                                    rendered_tools.insert(call.id.clone(), tool);
                                }
                            }
                        }
                        AgentMessage::Standard(Message::ToolResult(tr)) => {
                            if let Some(tool) = rendered_tools.get(&tr.tool_call_id) {
                                tool.borrow_mut().end(
                                    AgentToolResult {
                                        content: tr.content.clone(),
                                        details: serde_json::Value::Object(Default::default()),
                                        added_tool_names: None,
                                        terminate: None,
                                    },
                                    tr.is_error,
                                );
                            }
                        }
                        _ => {
                            if populate_history
                                && agent_message.role() == "user"
                                && let Some(text) = user_message_text(&agent_message)
                            {
                                self.editor.borrow_mut().add_to_history(&text);
                            }
                            self.add_message_to_chat(&agent_message);
                        }
                    }
                }
                SessionEntry::Compaction {
                    summary,
                    tokens_before,
                    ..
                } => {
                    let mut chat = self.chat.borrow_mut();
                    chat.add_child(Spacer::new(1));
                    let mut component =
                        CompactionSummaryMessageComponent::new(summary.clone(), *tokens_before);
                    component.set_expanded(self.tool_output_expanded);
                    chat.add_child(component);
                }
                SessionEntry::BranchSummary { summary, .. } => {
                    let mut chat = self.chat.borrow_mut();
                    chat.add_child(Spacer::new(1));
                    let mut component = BranchSummaryMessageComponent::new(summary.clone());
                    component.set_expanded(self.tool_output_expanded);
                    chat.add_child(component);
                }
                SessionEntry::Custom {
                    custom_type, data, ..
                } => {
                    let body = data
                        .as_ref()
                        .and_then(|d| d.as_str())
                        .unwrap_or_default()
                        .to_owned();
                    let mut component = CustomEntryComponent::new(custom_type.clone(), body);
                    component.set_expanded(self.tool_output_expanded);
                    self.chat.borrow_mut().add_child(component);
                }
                SessionEntry::CustomMessage {
                    custom_type,
                    content,
                    display: true,
                    ..
                } => {
                    let text = content.as_str().unwrap_or_default().to_owned();
                    let mut component =
                        super::components::custom_message::CustomMessageComponent::from_text(
                            custom_type.clone(),
                            text,
                        );
                    component.set_expanded(self.tool_output_expanded);
                    self.chat.borrow_mut().add_child(component);
                }
                _ => {}
            }
        }
        self.tui.request_render(false);
    }

    /// Oracle `addMessageToChat` (:3186-3283), the roles that exist pre-Phase-6.
    fn add_message_to_chat(&mut self, message: &AgentMessage) {
        match message {
            AgentMessage::Standard(Message::User(_)) => {
                if let Some(text) = user_message_text(message)
                    && !text.is_empty()
                {
                    let mut chat = self.chat.borrow_mut();
                    if !chat.is_empty() {
                        chat.add_child(Spacer::new(1));
                    }
                    let mut component = UserMessageComponent::new(text);
                    component.set_output_pad(self.output_pad);
                    chat.add_child(component);
                }
            }
            AgentMessage::Standard(Message::Assistant(am)) => {
                let mut component = AssistantMessageComponent::new(Some(am.clone()));
                component.set_hide_thinking_block(self.hide_thinking_block);
                component.set_output_pad(self.output_pad);
                self.chat.borrow_mut().add_child(component);
            }
            AgentMessage::Standard(Message::ToolResult(_)) => {}
            AgentMessage::Custom(value) => {
                let role = message.role();
                if role == "bashExecution" {
                    let command = value
                        .get("command")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default();
                    let excluded = value
                        .get("excludeFromContext")
                        .and_then(serde_json::Value::as_bool)
                        .unwrap_or(false);
                    let mut component = BashExecutionComponent::new(command, excluded);
                    if let Some(output) = value.get("output").and_then(serde_json::Value::as_str) {
                        component.append_output(output);
                    }
                    component.set_complete(
                        value
                            .get("exitCode")
                            .and_then(serde_json::Value::as_i64)
                            .map(|c| c as i32),
                        value
                            .get("cancelled")
                            .and_then(serde_json::Value::as_bool)
                            .unwrap_or(false),
                    );
                    self.chat.borrow_mut().add_child(component);
                } else if value
                    .get("display")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false)
                {
                    let custom_type = value
                        .get("customType")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("custom")
                        .to_owned();
                    let text = value
                        .get("content")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_owned();
                    let mut component =
                        super::components::custom_message::CustomMessageComponent::from_text(
                            custom_type,
                            text,
                        );
                    component.set_expanded(self.tool_output_expanded);
                    self.chat.borrow_mut().add_child(component);
                }
            }
        }
    }
}

// Theme-change flag: the pi-tui theme registry exposes a single global
// listener; the mode mirrors it into a thread-local the loop can poll.
thread_local! {
    static THEME_CHANGE_FLAG: RefCell<Option<Arc<std::sync::atomic::AtomicBool>>> =
        const { RefCell::new(None) };
}

/// JS `Number.prototype.toLocaleString()` for the en-US default: thousands
/// separators.
fn group_thousands(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    let offset = digits.len() % 3;
    for (i, c) in digits.chars().enumerate() {
        if i != 0 && (i + digits.len() - offset).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    let _ = offset;
    out
}

/// Extract text from a user message (oracle `getUserMessageText`).
fn user_message_text(message: &AgentMessage) -> Option<String> {
    let AgentMessage::Standard(Message::User(user)) = message else {
        return None;
    };
    Some(match &user.content {
        pi_ai::UserContent::Text(text) => text.clone(),
        pi_ai::UserContent::Blocks(blocks) => blocks
            .iter()
            .filter_map(|c| match c {
                pi_ai::Content::Text(t) => Some(t.text.to_string()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
    })
}

/// Live footer stats from session state (oracle FooterDataProvider reads).
fn footer_stats(session: &AgentSession) -> FooterStats {
    let stats = session.get_session_stats();
    let usage = session.get_context_usage();
    let model = session.model();
    FooterStats {
        input: stats.tokens.input,
        output: stats.tokens.output,
        cache_read: stats.tokens.cache_read,
        cache_write: stats.tokens.cache_write,
        cost: stats.cost,
        context_percent: usage.as_ref().and_then(|u| u.percent),
        context_window: usage.as_ref().map(|u| u.context_window).unwrap_or(0),
        model: model.as_ref().map(|m| m.id.clone()),
        provider: model.as_ref().map(|m| m.provider.clone()),
        reasoning: session.supports_thinking(),
        thinking_level: Some(
            crate::session::thinking_level_str(session.thinking_level()).to_owned(),
        ),
        using_subscription: false,
        experimental: false,
    }
}
