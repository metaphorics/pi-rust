//! Subscription-filtered event forwarding (Phase 6 commit C6, plan §2/§6).
//!
//! One [`EventForwarder`] per [`ExtensionHost`]. Invariants:
//! - I3: events reach the sidecar in order; a blocking hook's result is
//!   applied before the next event dispatches. All forwarding funnels
//!   through one async dispatch lock, and fire-and-forget events flow
//!   through one serial queue task.
//! - I4: only subscribed event kinds cross the boundary — with one forced
//!   exception: while an extension `ctx.compact()` is pending, manual
//!   `session_before_compact` / `session_compact` events are forwarded even
//!   when no extension subscribed (the sidecar's FIFO compact callbacks
//!   settle off them).
//! - I8: exactly one respawn attempt per death, and only at a turn boundary
//!   (the next `session_start`/`input`/`before_agent_start`/`agent_start`
//!   class event). Mid-turn events after a crash resolve to pass-through
//!   defaults without touching the process.
//! - Blocking hooks get a per-hook timeout (default 30s, brief §4 — a
//!   deliberate deviation from pi, which has none); on fire the bridge
//!   synthesizes an [`ExtensionError`] and applies the event's pass-through
//!   default, identical to the handler-throw path.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use pi_agent::{AgentThinkingLevel, CancellationToken};
use pi_ai::ModelThinkingLevel;
use pi_ext_protocol::{
    CompactReason, EventDispatch, ExtensionError, ExtensionEvent, ExtensionEventKind,
    InitializedParams, Notification, Registrations, Request, SessionStartReason as WireStartReason,
    ShutdownReason as WireShutdownReason, StateBlock, SwitchReason, ThemeDto,
};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

use crate::extension_bridge::{
    BeforeCompactDecision, CompactionOverride, HookOutcome, SessionLifecycleEvent,
    SessionShutdownReason, SessionStartReason,
};
use crate::session::AgentSession;
use crate::session::events::{AgentSessionEvent, CompactionReason};

use super::client::{ClientError, SidecarConnection};
use super::session_sync::SessionSync;
use super::{ExtensionHost, HostError};

/// Default per-blocking-hook timeout (brief §4; deviation-manifest entry).
pub const DEFAULT_HOOK_TIMEOUT: Duration = Duration::from_secs(30);

/// Produces a fresh [`StateBlock`] for every `event/emit` (plan §3: sync
/// getters resolve against event-dispatch-granularity state).
pub type StateSource = Arc<dyn Fn() -> StateBlock + Send + Sync>;

/// Where synthesized and sidecar-reported extension errors land (interactive
/// banner, RPC line, or stderr — mode-owned).
pub type ExtensionErrorSink = Arc<dyn Fn(ExtensionError) + Send + Sync>;

/// Mode-owned pieces of the state block the session cannot provide.
#[derive(Clone, Debug)]
pub struct StateOverlay {
    pub project_trusted: bool,
    pub commands: Vec<pi_ext_protocol::CommandInfo>,
    pub flag_values: BTreeMap<String, pi_ext_protocol::FlagValue>,
    pub editor_text: String,
    pub tools_expanded: bool,
    pub footer: Option<Value>,
    pub theme: ThemeDto,
}

impl Default for StateOverlay {
    fn default() -> Self {
        Self {
            project_trusted: true,
            commands: Vec::new(),
            flag_values: BTreeMap::new(),
            editor_text: String::new(),
            tools_expanded: false,
            footer: None,
            theme: ThemeDto {
                name: "dark".to_string(),
                json: Value::Object(serde_json::Map::new()),
            },
        }
    }
}

/// Wire thinking level for an agent-level one (identical value sets).
pub fn wire_thinking_level(level: AgentThinkingLevel) -> ModelThinkingLevel {
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

/// Agent-level thinking level for a wire one.
pub fn agent_thinking_level(level: ModelThinkingLevel) -> AgentThinkingLevel {
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

/// Build the session-derived portion of a [`StateBlock`] (oracle: runner
/// state resolved at call time, runner.ts:634 — here at event granularity).
pub fn session_state_block(session: &AgentSession, overlay: &StateOverlay) -> StateBlock {
    let all_tools = session
        .get_all_tools()
        .into_iter()
        .map(|tool| pi_ext_protocol::ToolInfo {
            source_info: pi_ext_protocol::SourceInfo {
                path: tool.name.clone(),
                source: tool.source.to_string(),
                scope: pi_ext_protocol::SourceScope::Temporary,
                origin: pi_ext_protocol::SourceOrigin::TopLevel,
                base_dir: None,
            },
            name: tool.name,
            description: tool.description,
            parameters: tool.parameters,
            prompt_guidelines: if tool.prompt_guidelines.is_empty() {
                None
            } else {
                Some(tool.prompt_guidelines.join("\n"))
            },
        })
        .collect();
    StateBlock {
        session_name: session.session_name(),
        model: session.model(),
        idle: session.is_idle(),
        project_trusted: overlay.project_trusted,
        pending_messages: session.pending_message_count() > 0,
        active_tools: session.get_active_tool_names(),
        all_tools,
        commands: overlay.commands.clone(),
        thinking_level: wire_thinking_level(session.thinking_level()),
        context_usage: session
            .get_context_usage()
            .map(|usage| pi_ext_protocol::ContextUsageDto {
                tokens: usage.tokens,
                context_window: usage.context_window,
                percent: usage.percent,
            }),
        system_prompt: session.system_prompt(),
        system_prompt_options: None,
        flag_values: overlay.flag_values.clone(),
        editor_text: overlay.editor_text.clone(),
        tools_expanded: overlay.tools_expanded,
        footer: overlay.footer.clone(),
        theme: overlay.theme.clone(),
    }
}

/// Items on the serial fire-and-forget queue.
#[allow(clippy::large_enum_variant)] // Session events dominate the traffic.
pub(super) enum QueueItem {
    Session(AgentSessionEvent),
    Lifecycle(SessionLifecycleEvent),
    /// Barrier: resolves once everything enqueued before it was dispatched.
    Flush(oneshot::Sender<()>),
}

/// Session handle + mirror bookkeeping shared between the forwarder and the
/// host's `lifecycle/init` source (a respawn or reload replay re-baselines
/// the mirror through [`SharedSessionState::snapshot`]).
///
/// Lock order: `sync` before the session's internal state lock.
pub struct SharedSessionState {
    session: parking_lot::Mutex<AgentSession>,
    sync: parking_lot::Mutex<SessionSync>,
}

impl SharedSessionState {
    pub fn new(session: AgentSession) -> Self {
        Self {
            session: parking_lot::Mutex::new(session),
            sync: parking_lot::Mutex::new(SessionSync::new()),
        }
    }

    /// The currently bound session.
    pub fn session(&self) -> AgentSession {
        self.session.lock().clone()
    }

    /// Full mirror snapshot (consumes an epoch); used by `lifecycle/init`.
    pub fn snapshot(&self) -> pi_ext_protocol::SessionSnapshot {
        let session = self.session();
        let mut sync = self.sync.lock();
        session.with_session_manager(|sm| sync.snapshot(sm))
    }

    fn delta(&self) -> Option<pi_ext_protocol::SessionSyncParams> {
        let session = self.session();
        let mut sync = self.sync.lock();
        session.with_session_manager(|sm| sync.delta(sm))
    }
}

pub struct EventForwarder {
    host: Arc<ExtensionHost>,
    state_source: StateSource,
    error_sink: ExtensionErrorSink,
    hook_timeout: Duration,
    shared: Arc<SharedSessionState>,
    /// I3 serialization point: one dispatch at a time, blocking hooks hold
    /// it across the await.
    dispatch: tokio::sync::Mutex<()>,
    subscribed: parking_lot::RwLock<Vec<ExtensionEventKind>>,
    registrations: parking_lot::Mutex<Registrations>,
    /// Extension `ctx.compact()` calls not yet settled by a manual
    /// compaction outcome (mirrors the sidecar FIFO).
    pending_compacts: AtomicUsize,
    /// Connection generation last adopted (subscriptions/registrations
    /// refreshed when a new connection appears).
    adopted: parking_lot::Mutex<Option<usize>>,
    queue_tx: mpsc::UnboundedSender<QueueItem>,
    /// Monotonic turn index fed into `turn_start`/`turn_end` wire events.
    turn_index: AtomicU64,
    last_thinking: parking_lot::Mutex<AgentThinkingLevel>,
}

/// Outcome of one blocking emit.
#[derive(Debug)]
pub enum EmitError {
    /// No sidecar available (dead, disabled, draining, or never needed).
    Unavailable,
    /// The hook timed out (already reported through the error sink).
    Timeout,
    /// The sidecar answered with an error frame (pi's `emitToolCall`
    /// no-catch asymmetry surfaces here — callers decide propagation).
    Remote(pi_ext_protocol::ProtocolError),
    /// Transport failure (already folded into the state machine).
    Transport(String),
}

impl EventForwarder {
    pub(super) fn new(
        host: Arc<ExtensionHost>,
        shared: Arc<SharedSessionState>,
        state_source: StateSource,
        error_sink: ExtensionErrorSink,
        hook_timeout: Duration,
        queue_tx: mpsc::UnboundedSender<QueueItem>,
    ) -> Self {
        let last_thinking = shared.session().thinking_level();
        Self {
            host,
            state_source,
            error_sink,
            hook_timeout,
            shared,
            dispatch: tokio::sync::Mutex::new(()),
            subscribed: parking_lot::RwLock::new(Vec::new()),
            registrations: parking_lot::Mutex::new(Registrations::default()),
            pending_compacts: AtomicUsize::new(0),
            adopted: parking_lot::Mutex::new(None),
            queue_tx,
            turn_index: AtomicU64::new(0),
            last_thinking: parking_lot::Mutex::new(last_thinking),
        }
    }

    pub fn host(&self) -> &Arc<ExtensionHost> {
        &self.host
    }

    pub fn error_sink(&self) -> &ExtensionErrorSink {
        &self.error_sink
    }

    /// Latest registrations reported by the sidecar (load, reload, or
    /// `lifecycle/load`).
    pub fn registrations(&self) -> Registrations {
        self.registrations.lock().clone()
    }

    /// Point the forwarder at a replacement session (session switch/new/fork
    /// rebind). The next delta produces a full resync.
    pub async fn rebind_session(&self, session: AgentSession) {
        let _dispatch = self.dispatch.lock().await;
        *self.last_thinking.lock() = session.thinking_level();
        *self.shared.session.lock() = session;
    }

    /// Number of unsettled extension `ctx.compact()` calls.
    pub fn pending_compacts(&self) -> usize {
        self.pending_compacts.load(Ordering::SeqCst)
    }

    /// Record an `action/compact` arrival (sidecar queued one FIFO pending).
    pub fn note_compact_requested(&self) {
        self.pending_compacts.fetch_add(1, Ordering::SeqCst);
    }

    fn note_compact_settled(&self) {
        let _ = self
            .pending_compacts
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1));
    }

    fn subscribed_to(&self, kind: ExtensionEventKind) -> bool {
        self.subscribed.read().contains(&kind)
    }

    fn report(&self, event: &str, error: String) {
        (self.error_sink)(ExtensionError {
            extension_path: "<bridge>".to_string(),
            event: event.to_string(),
            error,
            stack: None,
        });
    }

    /// Adopt a (possibly new) ready connection: refresh the subscription
    /// union and registrations from its `lifecycle/initialized`.
    pub(super) fn adopt(&self, connection: &Arc<SidecarConnection>) {
        let generation = Arc::as_ptr(connection) as usize;
        {
            let mut adopted = self.adopted.lock();
            if *adopted == Some(generation) {
                return;
            }
            *adopted = Some(generation);
        }
        if let Some(initialized) = connection.initialized() {
            self.apply_initialized(&initialized);
        }
    }

    /// Refresh subscriptions/registrations from a load phase (initial, load,
    /// or reload re-init).
    pub fn apply_initialized(&self, initialized: &InitializedParams) {
        *self.subscribed.write() = initialized.subscribed_events.clone();
        *self.registrations.lock() = initialized.registrations.clone();
        for error in &initialized.errors {
            (self.error_sink)(error.clone());
        }
    }

    /// Resolve a live connection, honoring the respawn gate (I8): a dead
    /// sidecar respawns only when `boundary` is true.
    async fn connection(&self, boundary: bool) -> Option<Arc<SidecarConnection>> {
        use super::state::{BridgeState, DeadReason};
        match self.host.state().await {
            BridgeState::Ready | BridgeState::Detected => {}
            BridgeState::Dead(DeadReason::Shutdown) => return None,
            BridgeState::Dead(_) => {
                if !boundary {
                    return None;
                }
            }
            _ => return None,
        }
        match self.host.ensure_ready().await {
            Ok(connection) => {
                self.adopt(&connection);
                Some(connection)
            }
            Err(HostError::NotNeeded | HostError::ShutDown) => None,
            Err(error) => {
                self.report("spawn", error.to_string());
                None
            }
        }
    }

    /// Ship pending session-mirror deltas. Caller holds the dispatch lock.
    async fn sync_session(&self, connection: &Arc<SidecarConnection>) {
        if let Some(params) = self.shared.delta()
            && let Err(error) = connection.notify(Notification::SessionSync(params)).await
        {
            self.report("session_sync", error.to_string());
        }
    }

    fn dispatch_payload(&self, event: ExtensionEvent) -> EventDispatch {
        EventDispatch {
            event,
            state: (self.state_source)(),
        }
    }

    /// Fire-and-forget forwarding of one wire event (`event/notify`).
    async fn notify_wire(&self, event: ExtensionEvent, boundary: bool, force: bool) {
        let kind = event.kind();
        let Some(connection) = self.connection(boundary).await else {
            return;
        };
        let _dispatch = self.dispatch.lock().await;
        self.sync_session(&connection).await;
        if !force && !self.subscribed_to(kind) {
            return;
        }
        let payload = self.dispatch_payload(event);
        if let Err(error) = connection
            .notify(Notification::EventNotify(Box::new(payload)))
            .await
        {
            self.report(wire_kind_str(kind), error.to_string());
        }
    }

    /// Blocking emit (`event/emit`) with the per-hook timeout and optional
    /// cancellation. `Ok(None)` = no handler contributed a result (`null` on
    /// the wire); `Ok(Some(Value::Object(..)))` includes the empty `{}`
    /// result — the null-vs-`{}` distinction is preserved.
    pub async fn emit_blocking(
        &self,
        event: ExtensionEvent,
        signal: Option<CancellationToken>,
    ) -> Result<Option<Value>, EmitError> {
        let kind = event.kind();
        let force = matches!(
            kind,
            ExtensionEventKind::SessionBeforeCompact | ExtensionEventKind::SessionCompact
        ) && self.pending_compacts() > 0;
        let boundary = is_turn_boundary(kind);
        let Some(connection) = self.connection(boundary).await else {
            return Err(EmitError::Unavailable);
        };
        let _dispatch = self.dispatch.lock().await;
        self.sync_session(&connection).await;
        if !force && !self.subscribed_to(kind) {
            return Ok(None);
        }
        let payload = self.dispatch_payload(event);
        let request = Request::EventEmit(Box::new(payload));

        // The dispatch lock is HELD across the await: a blocking hook's
        // result must land before the next event dispatches (I3).
        let mut pending = match connection.begin_request(request).await {
            Ok(pending) => pending,
            Err(error) => return Err(self.transport_error(kind, error)),
        };
        let deadline = tokio::time::sleep(self.hook_timeout);
        tokio::pin!(deadline);
        let cancelled = async {
            match &signal {
                Some(token) => {
                    while !token.is_cancelled() {
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                }
                None => std::future::pending::<()>().await,
            }
        };
        tokio::pin!(cancelled);
        tokio::select! {
            result = &mut pending => {
                match result {
                    Ok(Value::Null) => Ok(None),
                    Ok(value) => Ok(Some(value)),
                    Err(ClientError::Remote(err)) => Err(EmitError::Remote(err)),
                    Err(error) => Err(self.transport_error(kind, error)),
                }
            }
            _ = &mut deadline => {
                pending.try_cancel();
                self.report(
                    wire_kind_str(kind),
                    format!("blocking hook timed out after {:?}", self.hook_timeout),
                );
                Err(EmitError::Timeout)
            }
            _ = &mut cancelled => {
                pending.try_cancel();
                Err(EmitError::Unavailable)
            }
        }
    }

    fn transport_error(&self, kind: ExtensionEventKind, error: ClientError) -> EmitError {
        self.report(wire_kind_str(kind), error.to_string());
        EmitError::Transport(error.to_string())
    }

    /// Blocking emit collapsed to the pass-through default on every failure
    /// except a remote error (which the sink already carries for caught
    /// paths; `tool_call` callers use [`emit_blocking`] directly to keep the
    /// uncaught propagation, I10).
    pub async fn emit_blocking_or_default(
        &self,
        event: ExtensionEvent,
        signal: Option<CancellationToken>,
    ) -> Option<Value> {
        let kind = event.kind();
        match self.emit_blocking(event, signal).await {
            Ok(value) => value,
            Err(EmitError::Remote(err)) => {
                self.report(wire_kind_str(kind), err.message);
                None
            }
            Err(_) => None,
        }
    }

    /// Reload the extension runtime in place (`ctx.reload()`): re-run
    /// `lifecycle/init` on the live connection (the sidecar re-discovers and
    /// re-loads extensions — pi's runner-replacement reload, no process
    /// restart) and refresh subscriptions/registrations/commands/flags.
    pub async fn reload(&self) -> Result<InitializedParams, HostError> {
        // Barrier: everything enqueued before the reload (e.g. the
        // session_shutdown of a teardown) must reach the old runner first.
        self.flush().await;
        let initialized = {
            let _dispatch = self.dispatch.lock().await;
            // The host's init source re-snapshots through SharedSessionState,
            // re-baselining the mirror inline.
            self.host.reinit().await?
        };
        self.apply_initialized(&initialized);
        Ok(initialized)
    }

    /// Wait until every previously enqueued fire-and-forget item dispatched.
    pub async fn flush(&self) {
        let (tx, rx) = oneshot::channel();
        if self.queue_tx.send(QueueItem::Flush(tx)).is_ok() {
            let _ = rx.await;
        }
    }

    /// Enqueue a lifecycle notification (session_start / session_shutdown).
    /// Sync and infallible by contract (`ExtensionBridge::emit_lifecycle`).
    pub fn enqueue_lifecycle(&self, event: SessionLifecycleEvent) {
        let _ = self.queue_tx.send(QueueItem::Lifecycle(event));
    }

    /// Enqueue a session event (the [`AgentSession`] listener calls this).
    pub fn enqueue_session_event(&self, event: AgentSessionEvent) {
        let _ = self.queue_tx.send(QueueItem::Session(event));
    }

    /// Blocking lifecycle hooks (before_switch / before_fork).
    pub async fn emit_lifecycle_blocking(&self, event: SessionLifecycleEvent) -> HookOutcome {
        self.flush().await;
        let wire = match event {
            SessionLifecycleEvent::SessionBeforeSwitch {
                reason,
                target_session_file,
            } => ExtensionEvent::SessionBeforeSwitch {
                reason: match reason {
                    SessionStartReason::New => SwitchReason::New,
                    _ => SwitchReason::Resume,
                },
                target_session_file: target_session_file
                    .map(|path| path.to_string_lossy().into_owned()),
            },
            SessionLifecycleEvent::SessionBeforeFork { entry_id, position } => {
                ExtensionEvent::SessionBeforeFork {
                    entry_id,
                    position: match position {
                        crate::extension_bridge::ForkPosition::Before => {
                            pi_ext_protocol::ForkPosition::Before
                        }
                        crate::extension_bridge::ForkPosition::At => {
                            pi_ext_protocol::ForkPosition::At
                        }
                    },
                }
            }
            _ => return HookOutcome::Continue,
        };
        let result = self.emit_blocking_or_default(wire, None).await;
        let cancel = result
            .as_ref()
            .and_then(|value| value.get("cancel"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if cancel {
            HookOutcome::Cancel
        } else {
            HookOutcome::Continue
        }
    }

    /// One queue item (called by the queue task, in order).
    pub(super) async fn process(&self, item: QueueItem) {
        match item {
            QueueItem::Flush(tx) => {
                let _ = tx.send(());
            }
            QueueItem::Lifecycle(event) => {
                let wire = match event {
                    SessionLifecycleEvent::SessionStart {
                        reason,
                        previous_session_file,
                    } => ExtensionEvent::SessionStart {
                        reason: match reason {
                            SessionStartReason::Startup => WireStartReason::Startup,
                            SessionStartReason::New => WireStartReason::New,
                            SessionStartReason::Resume => WireStartReason::Resume,
                            SessionStartReason::Fork => WireStartReason::Fork,
                            SessionStartReason::Reload => WireStartReason::Reload,
                        },
                        previous_session_file: previous_session_file
                            .map(|path| path.to_string_lossy().into_owned()),
                    },
                    SessionLifecycleEvent::SessionShutdown {
                        reason,
                        target_session_file,
                    } => ExtensionEvent::SessionShutdown {
                        reason: match reason {
                            SessionShutdownReason::New => WireShutdownReason::New,
                            SessionShutdownReason::Resume => WireShutdownReason::Resume,
                            SessionShutdownReason::Fork => WireShutdownReason::Fork,
                            SessionShutdownReason::Reload => WireShutdownReason::Reload,
                            SessionShutdownReason::Quit => WireShutdownReason::Quit,
                        },
                        target_session_file: target_session_file
                            .map(|path| path.to_string_lossy().into_owned()),
                    },
                    // Blocking lifecycle kinds never ride the queue.
                    _ => return,
                };
                let boundary = matches!(wire, ExtensionEvent::SessionStart { .. });
                self.notify_wire(wire, boundary, false).await;
            }
            QueueItem::Session(event) => self.forward_session_event(event).await,
        }
    }

    /// Map one session event onto the wire and forward it.
    async fn forward_session_event(&self, event: AgentSessionEvent) {
        let wire = match event {
            AgentSessionEvent::AgentStart => Some(ExtensionEvent::AgentStart {}),
            AgentSessionEvent::AgentEnd { messages, .. } => Some(ExtensionEvent::AgentEnd {
                messages: messages.into_iter().map(wire_message).collect(),
            }),
            AgentSessionEvent::AgentSettled => Some(ExtensionEvent::AgentSettled {}),
            AgentSessionEvent::TurnStart => {
                let index = self.turn_index.fetch_add(1, Ordering::SeqCst);
                Some(ExtensionEvent::TurnStart {
                    turn_index: index,
                    timestamp: jiff::Timestamp::now().as_millisecond().max(0) as u64,
                })
            }
            AgentSessionEvent::TurnEnd {
                message,
                tool_results,
            } => Some(ExtensionEvent::TurnEnd {
                turn_index: self.turn_index.load(Ordering::SeqCst).saturating_sub(1),
                message: wire_message(message),
                tool_results,
            }),
            AgentSessionEvent::MessageStart { message } => Some(ExtensionEvent::MessageStart {
                message: wire_message(message),
            }),
            AgentSessionEvent::MessageUpdate {
                message,
                assistant_message_event,
            } => Some(ExtensionEvent::MessageUpdate {
                message: wire_message(message),
                assistant_message_event: Box::new(assistant_message_event),
            }),
            AgentSessionEvent::MessageEnd { message } => Some(ExtensionEvent::MessageEnd {
                message: wire_message(message),
            }),
            AgentSessionEvent::ToolExecutionStart {
                tool_call_id,
                tool_name,
                args,
            } => Some(ExtensionEvent::ToolExecutionStart {
                tool_call_id,
                tool_name,
                args,
            }),
            AgentSessionEvent::ToolExecutionUpdate {
                tool_call_id,
                tool_name,
                args,
                partial_result,
            } => Some(ExtensionEvent::ToolExecutionUpdate {
                tool_call_id,
                tool_name,
                args,
                partial_result: serde_json::to_value(partial_result).unwrap_or(Value::Null),
            }),
            AgentSessionEvent::ToolExecutionEnd {
                tool_call_id,
                tool_name,
                result,
                is_error,
            } => Some(ExtensionEvent::ToolExecutionEnd {
                tool_call_id,
                tool_name,
                result: serde_json::to_value(result).unwrap_or(Value::Null),
                is_error,
            }),
            AgentSessionEvent::SessionInfoChanged { name } => {
                Some(ExtensionEvent::SessionInfoChanged { name })
            }
            AgentSessionEvent::ThinkingLevelChanged { level } => {
                let previous = {
                    let mut last = self.last_thinking.lock();
                    std::mem::replace(&mut *last, level)
                };
                Some(ExtensionEvent::ThinkingLevelSelect {
                    level: wire_thinking_level(level),
                    previous_level: wire_thinking_level(previous),
                })
            }
            // No wire counterpart: compaction_start/end are host UI events
            // (the extension-visible compact events flow through the
            // blocking CompactHooks), queue_update / entry_appended /
            // auto_retry_* are host-only. They still drive a mirror sync.
            AgentSessionEvent::CompactionStart { .. }
            | AgentSessionEvent::CompactionEnd { .. }
            | AgentSessionEvent::QueueUpdate { .. }
            | AgentSessionEvent::EntryAppended { .. }
            | AgentSessionEvent::AutoRetryStart { .. }
            | AgentSessionEvent::AutoRetryEnd { .. } => None,
        };

        match wire {
            Some(wire) => {
                let boundary = matches!(wire, ExtensionEvent::AgentStart {});
                if wire.is_blocking() {
                    // message_end is the only blocking kind reaching this
                    // path; its result application lives at the P5 call
                    // sites (F10) — forwarding preserves order + timeout.
                    let _ = self.emit_blocking_or_default(wire, None).await;
                } else {
                    self.notify_wire(wire, boundary, false).await;
                }
            }
            None => {
                // Keep the mirror fresh even when nothing is dispatched.
                if let Some(connection) = self.connection(false).await {
                    let _dispatch = self.dispatch.lock().await;
                    self.sync_session(&connection).await;
                }
            }
        }
    }

    // =====================================================================
    // Compact hooks (blocking, oracle agent-session.ts:1765/:2032)
    // =====================================================================

    pub(super) async fn session_before_compact(
        &self,
        preparation: Value,
        branch_entries: Vec<Value>,
        custom_instructions: Option<String>,
        reason: CompactionReason,
        will_retry: bool,
        signal: CancellationToken,
    ) -> BeforeCompactDecision {
        self.flush().await;
        let manual = reason == CompactionReason::Manual;
        let event = ExtensionEvent::SessionBeforeCompact {
            preparation,
            branch_entries,
            custom_instructions,
            reason: wire_compact_reason(reason),
            will_retry,
        };
        let Some(result) = self.emit_blocking_or_default(event, Some(signal)).await else {
            return BeforeCompactDecision::Proceed;
        };
        if result
            .get("cancel")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            if manual {
                // The sidecar consumes one pending ctx.compact() when it
                // observes a manual cancel; mirror its FIFO.
                self.note_compact_settled();
            }
            return BeforeCompactDecision::Cancel;
        }
        let Some(compaction) = result.get("compaction") else {
            return BeforeCompactDecision::Proceed;
        };
        match parse_compaction_override(compaction) {
            Some(over) => BeforeCompactDecision::Replace(over),
            None => {
                self.report(
                    "session_before_compact",
                    "handler returned a malformed compaction override".to_string(),
                );
                BeforeCompactDecision::Proceed
            }
        }
    }

    pub(super) async fn session_compact(
        &self,
        compaction_entry: Value,
        from_extension: bool,
        reason: CompactionReason,
        will_retry: bool,
    ) {
        self.flush().await;
        let manual = reason == CompactionReason::Manual;
        let event = ExtensionEvent::SessionCompact {
            compaction_entry,
            from_extension,
            reason: wire_compact_reason(reason),
            will_retry,
        };
        // session_compact is a blocking emit in pi (awaited before compact()
        // resolves) even though it returns no result: pending ctx.compact()
        // callbacks fire inside the sidecar dispatch.
        let _ = self.emit_blocking_or_default(event, None).await;
        if manual {
            self.note_compact_settled();
        }
    }
}

fn parse_compaction_override(value: &Value) -> Option<CompactionOverride> {
    let object = value.as_object()?;
    Some(CompactionOverride {
        summary: object.get("summary")?.as_str()?.to_string(),
        first_kept_entry_id: object.get("firstKeptEntryId")?.as_str()?.to_string(),
        tokens_before: object.get("tokensBefore").and_then(Value::as_u64)?,
        details: object.get("details").cloned(),
    })
}

fn wire_compact_reason(reason: CompactionReason) -> CompactReason {
    match reason {
        CompactionReason::Manual => CompactReason::Manual,
        CompactionReason::Threshold => CompactReason::Threshold,
        CompactionReason::Overflow => CompactReason::Overflow,
    }
}

fn wire_message(message: pi_agent::AgentMessage) -> pi_ext_protocol::AgentMessage {
    match message {
        pi_agent::AgentMessage::Standard(message) => {
            pi_ext_protocol::AgentMessage::Standard(Box::new(message))
        }
        pi_agent::AgentMessage::Custom(value) => pi_ext_protocol::AgentMessage::Custom(value),
    }
}

/// Events marking the start of new work: after a crash, these (and only
/// these) may trigger the single respawn attempt (plan §4 "next turn").
fn is_turn_boundary(kind: ExtensionEventKind) -> bool {
    matches!(
        kind,
        ExtensionEventKind::SessionStart
            | ExtensionEventKind::AgentStart
            | ExtensionEventKind::BeforeAgentStart
            | ExtensionEventKind::Input
            | ExtensionEventKind::UserBash
            | ExtensionEventKind::ProjectTrust
            | ExtensionEventKind::ResourcesDiscover
    )
}

fn wire_kind_str(kind: ExtensionEventKind) -> &'static str {
    // The wire tag (snake_case) for diagnostics; mirrors the serde rename.
    match kind {
        ExtensionEventKind::ProjectTrust => "project_trust",
        ExtensionEventKind::ResourcesDiscover => "resources_discover",
        ExtensionEventKind::SessionStart => "session_start",
        ExtensionEventKind::SessionInfoChanged => "session_info_changed",
        ExtensionEventKind::SessionBeforeSwitch => "session_before_switch",
        ExtensionEventKind::SessionBeforeFork => "session_before_fork",
        ExtensionEventKind::SessionBeforeCompact => "session_before_compact",
        ExtensionEventKind::SessionCompact => "session_compact",
        ExtensionEventKind::SessionShutdown => "session_shutdown",
        ExtensionEventKind::SessionBeforeTree => "session_before_tree",
        ExtensionEventKind::SessionTree => "session_tree",
        ExtensionEventKind::Context => "context",
        ExtensionEventKind::BeforeProviderRequest => "before_provider_request",
        ExtensionEventKind::BeforeProviderHeaders => "before_provider_headers",
        ExtensionEventKind::AfterProviderResponse => "after_provider_response",
        ExtensionEventKind::BeforeAgentStart => "before_agent_start",
        ExtensionEventKind::AgentStart => "agent_start",
        ExtensionEventKind::AgentEnd => "agent_end",
        ExtensionEventKind::AgentSettled => "agent_settled",
        ExtensionEventKind::TurnStart => "turn_start",
        ExtensionEventKind::TurnEnd => "turn_end",
        ExtensionEventKind::MessageStart => "message_start",
        ExtensionEventKind::MessageUpdate => "message_update",
        ExtensionEventKind::MessageEnd => "message_end",
        ExtensionEventKind::ToolExecutionStart => "tool_execution_start",
        ExtensionEventKind::ToolExecutionUpdate => "tool_execution_update",
        ExtensionEventKind::ToolExecutionEnd => "tool_execution_end",
        ExtensionEventKind::ModelSelect => "model_select",
        ExtensionEventKind::ThinkingLevelSelect => "thinking_level_select",
        ExtensionEventKind::UserBash => "user_bash",
        ExtensionEventKind::Input => "input",
        ExtensionEventKind::ToolCall => "tool_call",
        ExtensionEventKind::ToolResult => "tool_result",
    }
}
