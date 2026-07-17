//! Wiring between a live [`AgentSession`] and the sidecar host (Phase 6
//! commit C6 "binding").
//!
//! Construction order is strict (assignment contract):
//! 1. Zero discovered extensions ⇒ [`bind_extensions`] returns `None` — no
//!    host, no detection, no Bun (I6).
//! 2. Otherwise the [`ExtensionHost`] is created with a `lifecycle/init`
//!    source that snapshots the CURRENT session + state, so the initial
//!    spawn, every respawn replay, and every reload re-init carry a coherent
//!    session/state mirror baseline.
//! 3. [`ExtensionBinding::start`] performs the lazy first spawn (handshake →
//!    init/load → initialized) and only then emits `session_start` — the
//!    sidecar never sees an event before its mirrors exist.
//! 4. Session events are forwarded through one serial queue; compaction
//!    hooks bind into the session; `action/*` is served against
//!    [`HostActions`].

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Weak};
use std::time::Duration;

use pi_agent::CancellationToken;
use pi_ai::Model;
use pi_ext_protocol::{
    AppendEntryParams, CancelledResult, CompactParams, Delivery, ExtensionMode, FlagValue,
    ForkParams, InitParams, NavigateTreeParams, NewSessionParams, Notification, RefreshToolsParams,
    Registrations, SendMessageParams, SendUserMessageParams, SetLabelParams,
    SetThinkingLevelParams, SwitchSessionParams, UserMessageDelivery,
};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::extension_bridge::{
    BeforeCompactDecision, BoxFuture, CompactHooks, ExtensionBridge, ExtensionUiHost, HookOutcome,
    RegisteredCommand, SessionLifecycleEvent, SessionStartReason,
};
use crate::session::runtime::AgentSessionRuntime;
use crate::session::{
    AgentSession, CompactionReason, CustomMessageDelivery, PromptOptions, SendCustomMessageOptions,
};
use crate::source_info::{SourceInfo, SourceOrigin, SourceScope};

use super::actions::{ActionServerConfig, HostActions, NotificationSink, spawn_action_server};
use super::events::{
    DEFAULT_HOOK_TIMEOUT, EventForwarder, ExtensionErrorSink, SharedSessionState, StateOverlay,
    StateSource, agent_thinking_level, session_state_block, state_block_all_tools,
    wire_thinking_level,
};
use super::provider::ExtensionProviders;
use super::tools::{ExtensionToolContext, ToolUpdateRouter, extension_tool_definitions};
use super::{
    ClientConfig, ExtensionHost, ExtensionHostConfig, ExtensionPathError, HostError,
    LauncherSource, SidecarTimeouts,
};

fn ready<T: Send + 'static>(value: T) -> BoxFuture<'static, T> {
    Box::pin(std::future::ready(value))
}

/// Resolves when `token` is cancelled (the house token is a bare flag, so
/// this polls — same cadence as the forwarder's blocking-emit cancel arm).
async fn cancelled(token: CancellationToken) {
    while !token.is_cancelled() {
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Inputs for [`bind_extensions`].
pub struct BindOptions {
    /// Discovered extension paths (resource loader). Empty ⇒ no binding.
    pub extension_paths: Vec<PathBuf>,
    pub launcher: LauncherSource,
    pub timeouts: SidecarTimeouts,
    pub client: ClientConfig,
    pub cwd: PathBuf,
    pub agent_dir: PathBuf,
    pub session_dir: PathBuf,
    pub mode: ExtensionMode,
    pub has_ui: bool,
    pub flag_values: BTreeMap<String, FlagValue>,
    /// Mode-owned state-block fields, updated in place by the mode (theme,
    /// editor text, commands, footer, trust).
    pub overlay: Arc<parking_lot::Mutex<StateOverlay>>,
    pub error_sink: ExtensionErrorSink,
    pub actions: Arc<dyn HostActions>,
    pub ui: Option<Arc<dyn ExtensionUiHost>>,
    /// C7/C8 seam for frames / tool updates / provider events.
    pub fallback: Option<NotificationSink>,
    /// Per-blocking-hook timeout ([`DEFAULT_HOOK_TIMEOUT`]).
    pub hook_timeout: Duration,
    /// Session runtime whose lifecycle path adopts the [`SidecarBridge`]
    /// during bind (session_shutdown + blocking before_switch/before_fork
    /// reach the sidecar instead of the initial `NoopExtensionBridge`).
    pub runtime: Option<Arc<AgentSessionRuntime>>,
    /// Extension provider directory (C7). Pass the instance whose
    /// [`extension_stream_fn`](super::provider::extension_stream_fn)
    /// wrapper went into the session's stream fn; `None` creates a fresh
    /// one (registry mutation still works; sidecar streaming needs the
    /// shared instance).
    pub providers: Option<Arc<ExtensionProviders>>,
}

impl BindOptions {
    pub fn new(
        extension_paths: Vec<PathBuf>,
        launcher: LauncherSource,
        cwd: PathBuf,
        agent_dir: PathBuf,
        session_dir: PathBuf,
        error_sink: ExtensionErrorSink,
        actions: Arc<dyn HostActions>,
    ) -> Self {
        Self {
            extension_paths,
            launcher,
            timeouts: SidecarTimeouts::default(),
            client: ClientConfig::default(),
            cwd,
            agent_dir,
            session_dir,
            mode: ExtensionMode::Rpc,
            has_ui: false,
            flag_values: BTreeMap::new(),
            overlay: Arc::new(parking_lot::Mutex::new(StateOverlay::default())),
            error_sink,
            actions,
            ui: None,
            fallback: None,
            hook_timeout: DEFAULT_HOOK_TIMEOUT,
            runtime: None,
            providers: None,
        }
    }
}

/// A live session ⇄ sidecar binding. Dropping it stops forwarding; call
/// [`shutdown`](ExtensionBinding::shutdown) for a graceful sidecar exit.
pub struct ExtensionBinding {
    host: Arc<ExtensionHost>,
    forwarder: Arc<EventForwarder>,
    shared: Arc<SharedSessionState>,
    config: Arc<ActionServerConfig>,
    paths: Vec<PathBuf>,
    queue_task: JoinHandle<()>,
    server_task: JoinHandle<()>,
    unsubscribe: parking_lot::Mutex<Option<Box<dyn FnOnce() + Send>>>,
    /// Shared execute-context (host handle + `tool/update` router) captured
    /// by every bridged tool definition (C7).
    tool_context: Arc<ExtensionToolContext>,
    /// Extension provider directory (registry mutation + sidecar streams).
    providers: Arc<ExtensionProviders>,
}

impl Drop for ExtensionBinding {
    fn drop(&mut self) {
        if let Some(unsubscribe) = self.unsubscribe.lock().take() {
            unsubscribe();
        }
        self.queue_task.abort();
        self.server_task.abort();
    }
}

/// Bind a session to the extension sidecar. `Ok(None)` when zero extensions
/// were discovered: no host is constructed and no Bun process will ever
/// exist (I6).
pub fn bind_extensions(
    session: &AgentSession,
    options: BindOptions,
) -> Result<Option<Arc<ExtensionBinding>>, ExtensionPathError> {
    if options.extension_paths.is_empty() {
        return Ok(None);
    }

    let shared = Arc::new(SharedSessionState::new(session.clone()));
    let overlay = options.overlay.clone();
    let state_source: StateSource = {
        let shared = shared.clone();
        let overlay = overlay.clone();
        Arc::new(move || {
            let session = shared.session();
            let overlay = overlay.lock().clone();
            session_state_block(&session, &overlay)
        })
    };

    let init_source = {
        let shared = shared.clone();
        let state_source = state_source.clone();
        let cwd = options.cwd.clone();
        let agent_dir = options.agent_dir.clone();
        let session_dir = options.session_dir.clone();
        let mode = options.mode;
        let has_ui = options.has_ui;
        let flag_values = options.flag_values.clone();
        let overlay = overlay.clone();
        Arc::new(move || {
            // Hoisted on purpose: a guard temporary inside the struct
            // literal would live until the whole expression ends and
            // deadlock against state_source's own overlay lock.
            let theme = overlay.lock().theme.clone();
            let session = shared.snapshot();
            let state = (state_source)();
            InitParams {
                cwd: cwd.to_string_lossy().into_owned(),
                agent_dir: agent_dir.to_string_lossy().into_owned(),
                session_dir: session_dir.to_string_lossy().into_owned(),
                configured_paths: Vec::new(), // overwritten by the host
                mode,
                has_ui,
                flag_values: flag_values.clone(),
                theme,
                session,
                state,
            }
        })
    };

    let host = Arc::new(ExtensionHost::new(ExtensionHostConfig {
        extension_paths: options.extension_paths.clone(),
        launcher: options.launcher,
        init: init_source,
        timeouts: options.timeouts,
        client: options.client,
    })?);
    let incoming = host
        .take_incoming()
        .expect("fresh host owns its incoming receiver");

    let (queue_tx, mut queue_rx) = mpsc::unbounded_channel();
    let forwarder = Arc::new(EventForwarder::new(
        host.clone(),
        shared.clone(),
        state_source,
        options.error_sink,
        options.hook_timeout,
        queue_tx,
    ));

    let queue_task = tokio::spawn({
        let forwarder = forwarder.clone();
        async move {
            while let Some(item) = queue_rx.recv().await {
                forwarder.process(item).await;
            }
        }
    });

    let tool_router = ToolUpdateRouter::new();
    let tool_context = Arc::new(ExtensionToolContext {
        host: Arc::downgrade(&host),
        router: tool_router.clone(),
        // Freshness patch pushed before every tool/execute: active tools,
        // registry, thinking level, and system prompt at call time.
        fresh_state: {
            let shared = shared.clone();
            Arc::new(move || {
                let session = shared.session();
                pi_ext_protocol::StateUpdate {
                    active_tools: Some(session.get_active_tool_names()),
                    all_tools: Some(state_block_all_tools(&session)),
                    thinking_level: Some(wire_thinking_level(session.thinking_level())),
                    system_prompt: Some(session.system_prompt()),
                    ..Default::default()
                }
            })
        },
    });

    let providers = options.providers.unwrap_or_default();
    providers.attach_host(&host);
    providers.attach_registry(session.model_registry());

    // Compose the notification fallback: `tool/update` routes to the
    // in-flight tool call, `provider/event` to its stream; everything else
    // keeps the caller's sink (C8).
    let fallback: Option<NotificationSink> = {
        let caller = options.fallback;
        let tool_router = tool_router.clone();
        let providers = providers.clone();
        Some(Arc::new(
            move |notification: Notification| match notification {
                Notification::ToolUpdate(params) => {
                    tool_router.dispatch(&params.tool_call_id, params.partial);
                }
                Notification::ProviderEvent(params) => {
                    providers.dispatch_event(&params.stream_id, params.event);
                }
                other => {
                    if let Some(caller) = &caller {
                        caller(other);
                    }
                }
            },
        ))
    };

    let config = Arc::new(ActionServerConfig {
        actions: options.actions,
        ui: parking_lot::Mutex::new(options.ui),
        fallback,
    });
    let server_task = spawn_action_server(forwarder.clone(), incoming, config.clone());

    let binding = Arc::new(ExtensionBinding {
        host,
        forwarder,
        shared,
        config,
        paths: options.extension_paths,
        queue_task,
        server_task,
        unsubscribe: parking_lot::Mutex::new(None),
        tool_context,
        providers,
    });
    // Registration snapshots (initial load, reload, respawn replay,
    // `refreshTools` payloads) rebuild the bound session's extension tool
    // registry. Installed BEFORE `start()` so the initial `initialized`
    // cannot be missed (forwarder contract).
    {
        let weak = Arc::downgrade(&binding);
        binding
            .forwarder
            .set_registrations_listener(Arc::new(move |registrations| {
                if let Some(binding) = weak.upgrade() {
                    binding.apply_registration_tools(registrations);
                    // Provider snapshot needs the async registry lock;
                    // reconciliation runs off-listener (registered providers
                    // become visible to the host catalog shortly after —
                    // pi's own registration effects are also async wrt the
                    // load promise).
                    let providers = registrations.providers.clone();
                    let generation = binding.providers().allocate_snapshot_generation();
                    let binding = binding.clone();
                    tokio::spawn(async move {
                        binding
                            .apply_provider_snapshot(generation, &providers)
                            .await;
                    });
                }
            }));
    }
    binding.attach_session(session);
    if let Some(runtime) = &options.runtime {
        runtime.set_bridge(Arc::new(SidecarBridge::new(&binding)));
    }
    Ok(Some(binding))
}

impl ExtensionBinding {
    pub fn host(&self) -> &Arc<ExtensionHost> {
        &self.host
    }

    pub fn forwarder(&self) -> &Arc<EventForwarder> {
        &self.forwarder
    }

    /// The currently bound session.
    pub fn session(&self) -> AgentSession {
        self.shared.session()
    }

    /// Latest sidecar registrations (tools/commands/shortcuts/flags/
    /// providers) — refreshed on load, `lifecycle/load`, and reload.
    pub fn registrations(&self) -> Registrations {
        self.forwarder.registrations()
    }

    /// Extension slash commands in the shape the modes consume.
    pub fn registered_commands(&self) -> Vec<RegisteredCommand> {
        self.registrations()
            .commands
            .into_iter()
            .map(|command| RegisteredCommand {
                invocation_name: command.name,
                description: command.description,
                source_info: wire_source_info(command.source_info),
            })
            .collect()
    }

    /// Bind the mode's UI host (dialogs + void setters).
    pub fn bind_ui(&self, ui: Arc<dyn ExtensionUiHost>) {
        *self.config.ui.lock() = Some(ui);
    }

    /// Strict startup order: spawn + handshake + `lifecycle/init` (session
    /// snapshot + state block + extension loading) first, `session_start`
    /// only after the sidecar is ready.
    pub async fn start(&self, reason: SessionStartReason) -> Result<(), HostError> {
        let previous_session_file = None;
        let connection = self.host.ensure_ready().await?;
        self.forwarder.adopt(&connection);
        self.forwarder
            .enqueue_lifecycle(SessionLifecycleEvent::SessionStart {
                reason,
                previous_session_file,
            });
        Ok(())
    }

    /// Re-point the binding at a replacement session (switch/new/fork/reload
    /// rebind): re-subscribes events + compact hooks; the next mirror sync
    /// is a full resync. The replacement session was constructed WITHOUT
    /// extension tools, so the current registration snapshot is re-applied
    /// (its first application is construction-equivalent: include-all).
    pub async fn rebind(self: &Arc<Self>, session: AgentSession) {
        self.forwarder.rebind_session(session.clone()).await;
        self.attach_session(&session);
        self.providers.attach_registry(session.model_registry());
        self.apply_registration_tools(&self.forwarder.registrations());
    }

    /// Rebuild the bound session's extension tool partition from a
    /// registration snapshot (C7; oracle `_refreshToolRegistry`).
    pub fn apply_registration_tools(&self, registrations: &Registrations) {
        let tools = extension_tool_definitions(&registrations.tools, &self.tool_context);
        self.shared.session().refresh_extension_tools(tools);
    }

    /// Extension provider directory (sidecar streaming lookups).
    pub fn providers(&self) -> &Arc<ExtensionProviders> {
        &self.providers
    }

    /// Reconcile a provider registration snapshot into the session's model
    /// registry (register listed, drop vanished).
    pub async fn apply_provider_snapshot(
        &self,
        generation: u64,
        providers: &[pi_ext_protocol::ProviderRegistration],
    ) {
        let registry = self.shared.session().model_registry();
        self.providers
            .apply_snapshot(
                generation,
                &registry,
                providers,
                self.forwarder.error_sink(),
            )
            .await;
    }

    /// Apply one runtime `provider/register` notification.
    pub async fn apply_provider_register(
        &self,
        registration: &pi_ext_protocol::ProviderRegistration,
    ) {
        let registry = self.shared.session().model_registry();
        self.providers
            .register(&registry, registration, self.forwarder.error_sink())
            .await;
    }

    /// Apply one runtime `provider/unregister` notification.
    pub async fn apply_provider_unregister(&self, name: &str) {
        let registry = self.shared.session().model_registry();
        self.providers.unregister(&registry, name).await;
    }

    /// Subscribe the session's event stream + compaction hooks.
    fn attach_session(self: &Arc<Self>, session: &AgentSession) {
        if let Some(unsubscribe) = self.unsubscribe.lock().take() {
            unsubscribe();
        }
        let forwarder = self.forwarder.clone();
        let unsubscribe = session.subscribe(Arc::new(move |event| {
            forwarder.enqueue_session_event(event.clone());
        }));
        *self.unsubscribe.lock() = Some(Box::new(unsubscribe));
        session.bind_compact_hooks(Some(Arc::new(ForwarderCompactHooks {
            forwarder: self.forwarder.clone(),
        })));
        session.bind_command_hooks(Some(Arc::new(BindingCommandHooks {
            binding: Arc::downgrade(self),
        })));
        session.bind_message_hooks(Some(Arc::new(ForwarderMessageHooks {
            forwarder: self.forwarder.clone(),
        })));
    }

    /// Registered extension CLI flags in the shape the CLI help consumes
    /// (provenance = registering extension path).
    pub fn registered_flags(&self) -> Vec<crate::cli::args::ExtensionFlag> {
        self.registrations()
            .flags
            .into_iter()
            .map(|flag| crate::cli::args::ExtensionFlag {
                name: flag.name,
                r#type: match flag.kind {
                    pi_ext_protocol::FlagKind::Boolean => "boolean".to_string(),
                    pi_ext_protocol::FlagKind::String => "string".to_string(),
                },
                description: flag.description,
                extension_path: flag.extension_path,
            })
            .collect()
    }

    /// Execute an extension slash command in the sidecar (oracle
    /// `command.handler(args, createCommandContext())`). A fresh state
    /// patch precedes the request so command-context sync getters resolve
    /// at call time.
    pub async fn execute_command(&self, name: &str, args: &str) -> Result<(), String> {
        let Some(connection) = self.host.current_connection().await else {
            return Err("extension sidecar is not running".to_string());
        };
        let patch = (self.tool_context.fresh_state)();
        if connection
            .notify(pi_ext_protocol::Notification::StateUpdate(Box::new(patch)))
            .await
            .is_err()
        {
            return Err("extension sidecar is not running".to_string());
        }
        connection
            .request(pi_ext_protocol::Request::CommandExecute(
                pi_ext_protocol::CommandExecuteParams {
                    name: name.to_string(),
                    args: args.to_string(),
                },
            ))
            .await
            .map(|_| ())
            .map_err(|error| match error {
                super::ClientError::Remote(remote) => remote.message,
                other => other.to_string(),
            })
    }

    /// Invoke an extension keyboard shortcut in the sidecar.
    pub async fn invoke_shortcut(&self, key_id: &str) -> Result<(), String> {
        let Some(connection) = self.host.current_connection().await else {
            return Err("extension sidecar is not running".to_string());
        };
        let patch = (self.tool_context.fresh_state)();
        let _ = connection
            .notify(pi_ext_protocol::Notification::StateUpdate(Box::new(patch)))
            .await;
        connection
            .request(pi_ext_protocol::Request::ShortcutInvoke(
                pi_ext_protocol::ShortcutInvokeParams {
                    key_id: key_id.to_string(),
                },
            ))
            .await
            .map(|_| ())
            .map_err(|error| match error {
                super::ClientError::Remote(remote) => remote.message,
                other => other.to_string(),
            })
    }

    /// `ui/terminal_input` round trip with the 50ms reply budget (plan §2,
    /// deviation R4). Timeout / dead sidecar ⇒ not-consumed.
    pub async fn terminal_input(&self, data: &str) -> pi_ext_protocol::TerminalInputResult {
        let Some(connection) = self.host.current_connection().await else {
            return pi_ext_protocol::TerminalInputResult::default();
        };
        let request =
            pi_ext_protocol::Request::UiTerminalInput(pi_ext_protocol::TerminalInputParams {
                data: data.to_string(),
            });
        match tokio::time::timeout(Duration::from_millis(50), connection.request(request)).await {
            Ok(Ok(value)) => serde_json::from_value(value).unwrap_or_default(),
            _ => pi_ext_protocol::TerminalInputResult::default(),
        }
    }

    /// Push a `state/update` patch (theme changes, footer data, ...).
    pub async fn notify_state(&self, patch: pi_ext_protocol::StateUpdate) {
        if let Some(connection) = self.host.current_connection().await {
            let _ = connection
                .notify(pi_ext_protocol::Notification::StateUpdate(Box::new(patch)))
                .await;
        }
    }

    /// Build the TUI-thread outbound sender for bridged frames (C8).
    ///
    /// Messages are relayed through one ordered queue task (key input MUST
    /// NOT reorder); `ui/render` responses land back in `hub` guarded by
    /// revision + request generation. MUST be called from within a tokio
    /// runtime context; the sender itself never blocks and is safe to call
    /// from the TUI thread.
    pub fn ui_outbound(
        self: &Arc<Self>,
        hub: &Arc<super::frames::FrameHub>,
    ) -> super::frames::UiOutboundSender {
        use super::frames::UiOutbound;
        let (tx, mut rx) = mpsc::unbounded_channel::<UiOutbound>();
        let binding = Arc::downgrade(self);
        let hub = hub.clone();
        tokio::spawn(async move {
            while let Some(message) = rx.recv().await {
                let Some(binding) = binding.upgrade() else {
                    return;
                };
                let Some(connection) = binding.host.current_connection().await else {
                    continue; // Sidecar down; frames are gone anyway.
                };
                match message {
                    UiOutbound::Render {
                        slot,
                        width,
                        revision,
                        generation,
                    } => {
                        // Detached: a slow render must not stall key input
                        // behind it. Concurrent responses for one slot are
                        // resolved by the revision+generation guard.
                        let hub = hub.clone();
                        let connection = connection.clone();
                        tokio::spawn(async move {
                            let request =
                                pi_ext_protocol::Request::UiRender(pi_ext_protocol::RenderParams {
                                    slot: slot.clone(),
                                    width,
                                });
                            if let Ok(value) = connection.request(request).await
                                && let Ok(lines) = serde_json::from_value::<Vec<String>>(value)
                            {
                                hub.apply_render_response(&slot, revision, generation, lines);
                            }
                        });
                    }
                    UiOutbound::Input { slot, data } => {
                        let _ = connection
                            .notify(pi_ext_protocol::Notification::UiComponentInput(
                                pi_ext_protocol::ComponentInputParams { slot, data },
                            ))
                            .await;
                    }
                    UiOutbound::Focus { slot, focused } => {
                        let _ = connection
                            .notify(pi_ext_protocol::Notification::UiFocus(
                                pi_ext_protocol::FocusParams { slot, focused },
                            ))
                            .await;
                    }
                    UiOutbound::Dispose { slot } => {
                        let _ = connection
                            .notify(pi_ext_protocol::Notification::UiDispose(
                                pi_ext_protocol::SlotParams { slot },
                            ))
                            .await;
                    }
                    UiOutbound::EditorSetText { text } => {
                        let _ = connection
                            .notify(pi_ext_protocol::Notification::UiSetEditorText(
                                pi_ext_protocol::TextParams { text },
                            ))
                            .await;
                    }
                }
            }
        });
        Arc::new(move |message| {
            let _ = tx.send(message);
        })
    }

    /// Graceful teardown: flush the queue, then bounded sidecar shutdown.
    pub async fn shutdown(&self) {
        self.forwarder.flush().await;
        self.host.shutdown().await;
    }
}

pub(super) fn wire_source_info(info: pi_ext_protocol::SourceInfo) -> SourceInfo {
    SourceInfo {
        path: info.path,
        source: info.source,
        scope: match info.scope {
            pi_ext_protocol::SourceScope::User => SourceScope::User,
            pi_ext_protocol::SourceScope::Project => SourceScope::Project,
            pi_ext_protocol::SourceScope::Temporary => SourceScope::Temporary,
        },
        origin: match info.origin {
            pi_ext_protocol::SourceOrigin::Package => SourceOrigin::Package,
            pi_ext_protocol::SourceOrigin::TopLevel => SourceOrigin::TopLevel,
        },
        base_dir: info.base_dir,
    }
}

/// [`ExtensionCommandHooks`] routed to the sidecar (`command/execute`).
/// Weak: the session owns these hooks; a strong binding handle would cycle
/// through session → hooks → binding → shared session.
struct BindingCommandHooks {
    binding: Weak<ExtensionBinding>,
}

impl crate::extension_bridge::ExtensionCommandHooks for BindingCommandHooks {
    fn has_command(&self, name: &str) -> bool {
        self.binding.upgrade().is_some_and(|binding| {
            binding
                .registrations()
                .commands
                .iter()
                .any(|command| command.name == name)
        })
    }

    fn execute(&self, name: String, args: String) -> BoxFuture<'static, ()> {
        let Some(binding) = self.binding.upgrade() else {
            return ready(());
        };
        Box::pin(async move {
            if let Err(error) = binding.execute_command(&name, &args).await {
                // Oracle parity: handler failures surface as extension
                // errors (`command:<name>`), never to the prompt caller.
                (binding.forwarder().error_sink())(pi_ext_protocol::ExtensionError {
                    extension_path: format!("command:{name}"),
                    event: "command".to_string(),
                    error,
                    stack: None,
                });
            }
        })
    }
}

/// [`CompactHooks`] routed through the forwarder.
struct ForwarderCompactHooks {
    forwarder: Arc<EventForwarder>,
}

impl CompactHooks for ForwarderCompactHooks {
    fn session_before_compact(
        &self,
        preparation: Value,
        branch_entries: Vec<Value>,
        custom_instructions: Option<String>,
        reason: CompactionReason,
        will_retry: bool,
        signal: pi_agent::CancellationToken,
    ) -> BoxFuture<'static, BeforeCompactDecision> {
        let forwarder = self.forwarder.clone();
        Box::pin(async move {
            forwarder
                .session_before_compact(
                    preparation,
                    branch_entries,
                    custom_instructions,
                    reason,
                    will_retry,
                    signal,
                )
                .await
        })
    }

    fn session_compact(
        &self,
        compaction_entry: Value,
        from_extension: bool,
        reason: CompactionReason,
        will_retry: bool,
    ) -> BoxFuture<'static, ()> {
        let forwarder = self.forwarder.clone();
        Box::pin(async move {
            forwarder
                .session_compact(compaction_entry, from_extension, reason, will_retry)
                .await;
        })
    }
}

/// [`crate::extension_bridge::MessageHooks`] routed through the forwarder
/// (F10: blocking `message_end` with replacement application).
struct ForwarderMessageHooks {
    forwarder: Arc<EventForwarder>,
}

impl ForwarderMessageHooks {
    /// Parse + normalize a `message_end` result. Untyped extension handlers
    /// can return messages with null/missing content; the oracle normalizes
    /// to `[]` before it enters state or history (agent-session.ts:716-725).
    fn parse_replacement(
        &self,
        original_role: &str,
        result: Value,
    ) -> Option<pi_agent::AgentMessage> {
        let mut message = result.get("message")?.clone();
        if let Some(object) = message.as_object_mut() {
            let role = object.get("role").and_then(Value::as_str).unwrap_or("");
            if matches!(role, "user" | "assistant" | "toolResult" | "custom")
                && object.get("content").is_none_or(Value::is_null)
            {
                object.insert("content".to_string(), Value::Array(Vec::new()));
            }
        }
        let replacement: pi_agent::AgentMessage = match serde_json::from_value(message) {
            Ok(replacement) => replacement,
            Err(error) => {
                (self.forwarder.error_sink())(pi_ext_protocol::ExtensionError {
                    extension_path: "<bridge>".to_string(),
                    event: "message_end".to_string(),
                    error: format!("malformed replacement message: {error}"),
                    stack: None,
                });
                return None;
            }
        };
        // The sidecar runner already rejects per-handler role changes
        // (runner.ts:804); this guards the aggregate result.
        if replacement.role() != original_role {
            (self.forwarder.error_sink())(pi_ext_protocol::ExtensionError {
                extension_path: "<bridge>".to_string(),
                event: "message_end".to_string(),
                error: "message_end handlers must return a message with the same role".to_string(),
                stack: None,
            });
            return None;
        }
        Some(replacement)
    }
}

impl crate::extension_bridge::MessageHooks for ForwarderMessageHooks {
    fn on_message_end(
        &self,
        message: pi_agent::AgentMessage,
    ) -> BoxFuture<'static, Option<pi_agent::AgentMessage>> {
        let forwarder = self.forwarder.clone();
        let hooks = ForwarderMessageHooks {
            forwarder: self.forwarder.clone(),
        };
        Box::pin(async move {
            // Order: everything enqueued before this message dispatches
            // first (same barrier as the compact hooks).
            forwarder.flush().await;
            let original_role = message.role().to_string();
            let event = pi_ext_protocol::ExtensionEvent::MessageEnd {
                message: super::events::wire_message(message),
            };
            let result = forwarder.emit_blocking_or_default(event, None).await?;
            hooks.parse_replacement(&original_role, result)
        })
    }
}

/// [`ExtensionBridge`] implementation backed by a live binding (replaces
/// `NoopExtensionBridge` when extensions exist).
///
/// Holds the binding weakly: the binding's action config owns the host
/// actions, which own the runtime, which owns this bridge — a strong handle
/// here would cycle. When the binding is gone the bridge degrades to the
/// no-op behavior (dropping the binding stops forwarding by contract).
pub struct SidecarBridge {
    binding: Weak<ExtensionBinding>,
    paths: Vec<PathBuf>,
}

impl SidecarBridge {
    pub fn new(binding: &Arc<ExtensionBinding>) -> Self {
        Self {
            binding: Arc::downgrade(binding),
            paths: binding.paths.clone(),
        }
    }

    pub fn binding(&self) -> Option<Arc<ExtensionBinding>> {
        self.binding.upgrade()
    }
}

impl ExtensionBridge for SidecarBridge {
    fn needs_sidecar(&self) -> bool {
        true
    }

    fn discovered_paths(&self) -> &[PathBuf] {
        &self.paths
    }

    fn emit_lifecycle(
        &self,
        event: SessionLifecycleEvent,
        signal: Option<CancellationToken>,
    ) -> BoxFuture<'static, HookOutcome> {
        let Some(binding) = self.binding.upgrade() else {
            return ready(HookOutcome::Continue);
        };
        match event {
            SessionLifecycleEvent::SessionStart { .. }
            | SessionLifecycleEvent::SessionShutdown { .. } => {
                // Enqueued BEFORE returning (trait contract): a sync caller
                // may drop the future.
                binding.forwarder.enqueue_lifecycle(event);
                ready(HookOutcome::Continue)
            }
            blocking => {
                let forwarder = binding.forwarder.clone();
                Box::pin(async move { forwarder.emit_lifecycle_blocking(blocking, signal).await })
            }
        }
    }

    fn registered_commands(&self) -> Vec<RegisteredCommand> {
        self.binding
            .upgrade()
            .map(|binding| binding.registered_commands())
            .unwrap_or_default()
    }

    fn bind_ui(&self, ui: Arc<dyn ExtensionUiHost>) {
        if let Some(binding) = self.binding.upgrade() {
            binding.bind_ui(ui);
        }
    }
}

/// [`HostActions`] bound to a live [`AgentSession`] (and optionally an
/// [`AgentSessionRuntime`] for session-replacement ops). This is the default
/// binding the modes wrap; anything a mode cannot serve keeps the trait
/// default.
pub struct SessionHostActions {
    binding: parking_lot::Mutex<Weak<ExtensionBinding>>,
    runtime: parking_lot::Mutex<Option<Arc<AgentSessionRuntime>>>,
}

impl SessionHostActions {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            binding: parking_lot::Mutex::new(Weak::new()),
            runtime: parking_lot::Mutex::new(None),
        })
    }

    /// Wire the binding after [`bind_extensions`] (the actions object is
    /// constructed first; runtime input, hence the late set).
    pub fn attach(&self, binding: &Arc<ExtensionBinding>) {
        *self.binding.lock() = Arc::downgrade(binding);
    }

    pub fn attach_runtime(&self, runtime: Arc<AgentSessionRuntime>) {
        *self.runtime.lock() = Some(runtime);
    }

    fn session(&self) -> Option<AgentSession> {
        self.binding
            .lock()
            .upgrade()
            .map(|binding| binding.session())
    }

    fn binding(&self) -> Option<Arc<ExtensionBinding>> {
        self.binding.lock().upgrade()
    }

    fn runtime(&self) -> Option<Arc<AgentSessionRuntime>> {
        self.runtime.lock().clone()
    }
}

/// Extract the pieces of a wire custom message (`{role:"custom", customType,
/// content, display?, details?}`).
fn custom_message_parts(message: &Value) -> Option<(String, Option<Value>, bool, Option<Value>)> {
    let custom_type = message.get("customType")?.as_str()?.to_string();
    Some((
        custom_type,
        message.get("content").cloned(),
        message
            .get("display")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        message.get("details").cloned(),
    ))
}

fn text_of_user_content(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
            .map(|block| {
                block
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

impl HostActions for SessionHostActions {
    fn send_message(&self, params: SendMessageParams) -> BoxFuture<'static, ()> {
        let Some(session) = self.session() else {
            return ready(());
        };
        Box::pin(async move {
            let Some((custom_type, content, display, details)) =
                custom_message_parts(&params.message)
            else {
                return;
            };
            let deliver_as = params.deliver_as.map(|delivery| match delivery {
                Delivery::Steer => CustomMessageDelivery::Steer,
                Delivery::FollowUp => CustomMessageDelivery::FollowUp,
                Delivery::NextTurn => CustomMessageDelivery::NextTurn,
            });
            let options = SendCustomMessageOptions {
                trigger_turn: params.trigger_turn.unwrap_or(false),
                deliver_as,
            };
            // A triggered turn runs the whole agent loop; never block the
            // action stream on it.
            tokio::spawn(async move {
                session
                    .send_custom_message(&custom_type, content, display, details, options)
                    .await;
            });
        })
    }

    fn send_user_message(&self, params: SendUserMessageParams) -> BoxFuture<'static, ()> {
        let Some(session) = self.session() else {
            return ready(());
        };
        Box::pin(async move {
            let text = text_of_user_content(&params.content);
            if text.is_empty() {
                return;
            }
            if session.is_streaming() {
                match params.deliver_as {
                    Some(UserMessageDelivery::FollowUp) => session.follow_up(&text, Vec::new()),
                    _ => session.steer(&text, Vec::new()),
                }
                return;
            }
            tokio::spawn(async move {
                let _ = session.prompt(&text, PromptOptions::default()).await;
            });
        })
    }

    fn append_entry(&self, params: AppendEntryParams) -> BoxFuture<'static, ()> {
        if let Some(session) = self.session() {
            session.with_session_manager_mut(|sm| {
                let _ = sm.append_custom_entry(params.custom_type, params.data);
            });
        }
        ready(())
    }

    fn set_session_name(&self, name: String) -> BoxFuture<'static, ()> {
        if let Some(session) = self.session() {
            session.set_session_name(&name);
        }
        ready(())
    }

    fn set_label(&self, params: SetLabelParams) -> BoxFuture<'static, ()> {
        if let Some(session) = self.session() {
            session.with_session_manager_mut(|sm| {
                let _ = sm.append_label_change(params.entry_id, params.label);
            });
        }
        ready(())
    }

    fn set_active_tools(&self, tool_names: Vec<String>) -> BoxFuture<'static, ()> {
        if let Some(session) = self.session() {
            session.set_active_tools_by_name(tool_names);
        }
        ready(())
    }

    /// `refreshTools()` — a late `pi.registerTool` shipped its registration
    /// snapshot; store it (the forwarder notifies the binding's listener,
    /// which rebuilds the session's extension tool registry).
    fn refresh_tools(&self, params: RefreshToolsParams) -> BoxFuture<'static, ()> {
        if let (Some(binding), Some(registrations)) = (self.binding(), params.registrations) {
            binding.forwarder().update_registrations(registrations);
        }
        ready(())
    }

    /// Runtime `pi.registerProvider` — model catalog mutation (F9), ordered
    /// on the serve loop.
    fn provider_register(
        &self,
        registration: pi_ext_protocol::ProviderRegistration,
    ) -> BoxFuture<'static, ()> {
        let Some(binding) = self.binding() else {
            return ready(());
        };
        Box::pin(async move {
            binding.apply_provider_register(&registration).await;
        })
    }

    fn provider_unregister(&self, name: String) -> BoxFuture<'static, ()> {
        let Some(binding) = self.binding() else {
            return ready(());
        };
        Box::pin(async move {
            binding.apply_provider_unregister(&name).await;
        })
    }

    fn set_thinking_level(&self, params: SetThinkingLevelParams) -> BoxFuture<'static, ()> {
        if let Some(session) = self.session() {
            session.set_thinking_level(agent_thinking_level(params.level));
        }
        ready(())
    }

    fn abort(&self) -> BoxFuture<'static, ()> {
        let Some(session) = self.session() else {
            return ready(());
        };
        Box::pin(async move {
            // Abort awaits idle; keep the action stream moving.
            tokio::spawn(async move { session.abort().await });
        })
    }

    fn compact(&self, params: CompactParams) -> BoxFuture<'static, ()> {
        let Some(session) = self.session() else {
            return ready(());
        };
        Box::pin(async move {
            let custom_instructions = params
                .options
                .as_ref()
                .and_then(|options| options.get("customInstructions"))
                .and_then(Value::as_str)
                .map(str::to_string);
            // Manual compaction; the session's CompactHooks forward
            // session_before_compact/session_compact so the sidecar's FIFO
            // pending settles (success via session_compact, cancellation via
            // a self-observed cancel).
            tokio::spawn(async move {
                let _ = session.compact(custom_instructions).await;
            });
        })
    }

    fn set_model(&self, model: Model, signal: CancellationToken) -> BoxFuture<'static, bool> {
        let Some(session) = self.session() else {
            return ready(false);
        };
        Box::pin(async move {
            tokio::select! {
                applied = session.set_model(model) => applied.is_ok(),
                _ = cancelled(signal) => false,
            }
        })
    }

    fn wait_for_idle(&self, signal: CancellationToken) -> BoxFuture<'static, ()> {
        let Some(session) = self.session() else {
            return ready(());
        };
        Box::pin(async move {
            tokio::select! {
                _ = session.wait_for_idle() => {}
                _ = cancelled(signal) => {}
            }
        })
    }

    fn new_session(
        &self,
        params: NewSessionParams,
        signal: CancellationToken,
    ) -> BoxFuture<'static, Result<CancelledResult, String>> {
        let Some(runtime) = self.runtime() else {
            return ready(Err("newSession requires a session runtime".to_string()));
        };
        let binding = self.binding();
        Box::pin(async move {
            // Setup/withSession callbacks (`session/setup`) are a P5-B/C7
            // integration (they need the replaced-session op fence).
            if params.setup_token.is_some() || params.with_session_token.is_some() {
                return Err("newSession setup callbacks are not supported yet".to_string());
            }
            let result = runtime.new_session(params.parent_session, &signal).await?;
            if let Some(binding) = binding {
                binding.rebind(runtime.session()).await;
                if !result.cancelled {
                    // Oracle order: extensions rebind to the replacement
                    // session, then session_start fires with the reason and
                    // the replaced session's file (types.ts:548).
                    binding
                        .forwarder()
                        .enqueue_lifecycle(SessionLifecycleEvent::SessionStart {
                            reason: SessionStartReason::New,
                            previous_session_file: result.previous_session_file.clone(),
                        });
                }
            }
            Ok(CancelledResult {
                cancelled: result.cancelled,
            })
        })
    }

    fn fork(
        &self,
        params: ForkParams,
        signal: CancellationToken,
    ) -> BoxFuture<'static, Result<CancelledResult, String>> {
        let Some(runtime) = self.runtime() else {
            return ready(Err("fork requires a session runtime".to_string()));
        };
        let binding = self.binding();
        Box::pin(async move {
            if params.with_session_token.is_some() {
                return Err("fork withSession callbacks are not supported yet".to_string());
            }
            let position = match params.position {
                Some(pi_ext_protocol::ForkPosition::At) => {
                    crate::extension_bridge::ForkPosition::At
                }
                _ => crate::extension_bridge::ForkPosition::Before,
            };
            let result = runtime.fork(&params.entry_id, position, &signal).await?;
            if let Some(binding) = binding {
                binding.rebind(runtime.session()).await;
                if !result.cancelled {
                    binding
                        .forwarder()
                        .enqueue_lifecycle(SessionLifecycleEvent::SessionStart {
                            reason: SessionStartReason::Fork,
                            previous_session_file: result.previous_session_file.clone(),
                        });
                }
            }
            Ok(CancelledResult {
                cancelled: result.cancelled,
            })
        })
    }

    fn switch_session(
        &self,
        params: SwitchSessionParams,
        signal: CancellationToken,
    ) -> BoxFuture<'static, Result<CancelledResult, String>> {
        let Some(runtime) = self.runtime() else {
            return ready(Err("switchSession requires a session runtime".to_string()));
        };
        let binding = self.binding();
        Box::pin(async move {
            if params.with_session_token.is_some() {
                return Err("switchSession withSession callbacks are not supported yet".to_string());
            }
            let result = runtime
                .switch_session(std::path::Path::new(&params.session_path), None, &signal)
                .await?;
            if let Some(binding) = binding {
                binding.rebind(runtime.session()).await;
                if !result.cancelled {
                    binding
                        .forwarder()
                        .enqueue_lifecycle(SessionLifecycleEvent::SessionStart {
                            reason: SessionStartReason::Resume,
                            previous_session_file: result.previous_session_file.clone(),
                        });
                }
            }
            Ok(CancelledResult {
                cancelled: result.cancelled,
            })
        })
    }

    fn navigate_tree(
        &self,
        _params: NavigateTreeParams,
    ) -> BoxFuture<'static, Result<CancelledResult, String>> {
        ready(Err(
            "navigateTree binding lands with the tree hooks (C7)".to_string()
        ))
    }

    /// `ctx.reload()` (oracle reload-runtime.ts order): session_shutdown to
    /// the OLD extensions → in-place sidecar re-init (fresh discovery/load,
    /// refreshed commands/flags) → session recreate → session_start(reload).
    ///
    /// A cancel frame is honored only BEFORE the teardown starts; past that
    /// the reload commits (a half-reloaded runtime is worse than a late one).
    fn reload(&self, signal: CancellationToken) -> BoxFuture<'static, Result<(), String>> {
        let Some(runtime) = self.runtime() else {
            return ready(Err("reload requires a session runtime".to_string()));
        };
        let Some(binding) = self.binding() else {
            return ready(Err("reload requires a live extension binding".to_string()));
        };
        Box::pin(async move {
            if signal.is_cancelled() {
                return Ok(());
            }
            let forwarder = binding.forwarder().clone();
            runtime
                .reload_session(async {
                    forwarder
                        .reload()
                        .await
                        .map(|_| ())
                        .map_err(|error| error.to_string())
                })
                .await?;
            binding.rebind(runtime.session()).await;
            // Oracle (types.ts:552): previousSessionFile is present only for
            // "new", "resume", and "fork" — a reload start omits it.
            binding
                .forwarder()
                .enqueue_lifecycle(SessionLifecycleEvent::SessionStart {
                    reason: SessionStartReason::Reload,
                    previous_session_file: None,
                });
            Ok(())
        })
    }
}
