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
    ForkParams, InitParams, NavigateTreeParams, NewSessionParams, Registrations, SendMessageParams,
    SendUserMessageParams, SetLabelParams, SetThinkingLevelParams, SwitchSessionParams,
    UserMessageDelivery,
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
    StateSource, agent_thinking_level, session_state_block,
};
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

    let config = Arc::new(ActionServerConfig {
        actions: options.actions,
        ui: parking_lot::Mutex::new(options.ui),
        fallback: options.fallback,
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
    });
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
    /// is a full resync.
    pub async fn rebind(self: &Arc<Self>, session: AgentSession) {
        self.forwarder.rebind_session(session.clone()).await;
        self.attach_session(&session);
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
    }

    /// Graceful teardown: flush the queue, then bounded sidecar shutdown.
    pub async fn shutdown(&self) {
        self.forwarder.flush().await;
        self.host.shutdown().await;
    }
}

fn wire_source_info(info: pi_ext_protocol::SourceInfo) -> SourceInfo {
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
