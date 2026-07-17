//! Bridged extension frame compositor (Phase 6 commit C8).
//!
//! Extension components never cross the process boundary as objects: they
//! render to styled ANSI lines inside the Bun sidecar and arrive here as
//! versioned `ui/frame` notifications (plan §5). This module hosts them:
//!
//! - [`FrameHub`] — the `Send + Sync` ingestion side. It receives raw
//!   [`Notification`]s on the tokio sidecar task (through
//!   [`FrameHub::sink`], plugged into `BindOptions.fallback`) and retains
//!   ONLY the latest frame per slot (latest-wins coalescing: 100 rapid
//!   frames cost one parse). Stale frames (`version` ≤ last accepted) are
//!   dropped. Structural events (mount/dispose/done/overlay) and the UI
//!   state notifications keep their arrival order in a bounded queue the
//!   TUI thread drains once per pump tick.
//! - [`BridgedLeaf`] — the terminal-thread `pi_tui::Component` leaf. ANSI is
//!   parsed into [`Line`]s on the TUI thread at the ingestion boundary
//!   ([`BridgedLeaf::sync`]), never on the tokio side (`Line` is `!Send` by
//!   design). `render(width)` only serves the cached lines and reports
//!   [`RenderStatus::Unchanged`] on unchanged content, so the DirtySpans
//!   never-degrade contract holds; a width change enqueues a non-blocking
//!   `ui/render {slot,width}` request through [`UiOutboundSender`] (the
//!   render thread NEVER awaits the bridge, invariant I5).
//!
//! Out-of-order protection for `ui/render` responses: each issued request
//! carries the slot's frame `revision` and a per-slot request `generation`;
//! a response is applied only when both are still current (a newer frame or
//! a newer width request wins).

use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::Arc;

use pi_ext_protocol::{FrameParams, Notification, WidgetPlacement};
use pi_tui::component::{Component, Focusable, RenderStatus};
use pi_tui::line::{Line, lines_from_ansi};
use serde_json::Value;

use super::actions::NotificationSink;

/// Maximum FIFO edge-triggered events retained while the TUI pump is stalled.
pub const FRAME_HUB_EDGE_CAPACITY: usize = 1_024;
/// Maximum distinct coalesced state keys retained while the TUI pump is stalled.
pub const FRAME_HUB_STATE_CAPACITY: usize = 256;
/// Maximum distinct dirty slot keys retained before requesting reconciliation.
pub const FRAME_HUB_DIRTY_CAPACITY: usize = 1_024;
/// Maximum total queued events; frame content is coalesced separately.
pub const FRAME_HUB_EVENT_CAPACITY: usize = FRAME_HUB_EDGE_CAPACITY + FRAME_HUB_STATE_CAPACITY;
/// Maximum total buffered queue entries and dirty keys.
pub const FRAME_HUB_BUFFER_CAPACITY: usize = FRAME_HUB_EVENT_CAPACITY + FRAME_HUB_DIRTY_CAPACITY;

// ============================================================================
// Outbound (TUI thread → sidecar)
// ============================================================================

/// UI traffic the TUI thread emits toward the sidecar. Fire-and-forget from
/// the caller's perspective: the sender enqueues onto the tokio side.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UiOutbound {
    /// `ui/render {slot,width}` request. `revision`/`generation` guard the
    /// response application (see module docs).
    Render {
        slot: String,
        width: u16,
        revision: u64,
        generation: u64,
    },
    /// `ui/input {slot,data}` — key input for a focused bridged slot.
    Input { slot: String, data: String },
    /// `ui/focus {slot,focused}` — focus mirror so the sidecar component
    /// renders its cursor marker exactly like a locally focused component.
    Focus { slot: String, focused: bool },
    /// `ui/resize {width,height}` — mirror the host grid into headless
    /// pi-tui so responsive components/overlay predicates match the host.
    Resize { width: u16, height: u16 },
    /// `ui/dispose {slot}` — host-initiated slot teardown.
    Dispose { slot: String },
    /// `ui/editorSetText {text}` — host-driven text replacement for the
    /// bridged editor slot (submit-clear parity with the native editor).
    EditorSetText { text: String },
}

/// Sink for [`UiOutbound`] messages; implemented over the extension binding
/// (spawns onto the tokio runtime). MUST NOT block.
pub type UiOutboundSender = Arc<dyn Fn(UiOutbound) + Send + Sync>;

/// No-op outbound sender (tests, detached leaves).
#[must_use]
pub fn noop_outbound() -> UiOutboundSender {
    Arc::new(|_| {})
}

// ============================================================================
// Hub (tokio ingestion → TUI drain)
// ============================================================================

/// Ordered structural/UI events drained by the TUI thread.
///
/// Content updates deliberately do NOT ride this queue — they coalesce in
/// the slot table and surface through [`FrameHub::drain`]'s dirty list.
#[derive(Debug)]
pub enum HubEvent {
    /// First frame arrived for a slot that was not mounted.
    Mounted { slot: String },
    /// The sidecar disposed a slot (`ui/dispose`).
    Disposed { slot: String },
    /// `ui/done {slot,result}` — a `ui.custom` component resolved.
    Done { slot: String, result: Value },
    /// `ui/overlay {slot,options}` — overlay mount/state update.
    Overlay { slot: String, options: Value },
    /// Every other UI notification routed through the fallback sink.
    Ui(Notification),
    /// Events discarded to keep ingestion nonblocking and memory bounded.
    /// Required events already accepted by the queue are never evicted.
    Overflow { dropped: usize },
    /// Bounded dirty/state tracking overflowed; reconcile from authoritative
    /// slot snapshots rather than retaining more pending keys.
    ResyncAll,
}

/// Immutable snapshot of a slot's latest content (cloned out of the hub so
/// the TUI thread parses without holding the lock).
#[derive(Clone, Debug)]
pub struct SlotSnapshot {
    pub lines: Vec<String>,
    pub revision: u64,
    pub wants_key_release: bool,
    pub focusable: bool,
    pub placement: Option<WidgetPlacement>,
}

#[derive(Debug)]
struct QueuedEvent {
    sequence: u64,
    event: HubEvent,
}

#[derive(Debug, Default)]
struct SlotState {
    lines: Vec<String>,
    /// Last accepted wire version (monotonic per slot, sidecar-owned).
    version: u64,
    /// Local content revision: bumps on every accepted content change
    /// (frame or render response). What [`BridgedLeaf::sync`] compares.
    revision: u64,
    /// Latest issued `ui/render` request generation for this slot.
    render_generation: u64,
    wants_key_release: bool,
    focusable: bool,
    placement: Option<WidgetPlacement>,
}

#[derive(Default)]
struct HubInner {
    slots: HashMap<String, SlotState>,
    /// FIFO one-shot/structural traffic.
    edges: VecDeque<QueuedEvent>,
    /// Latest state per semantic kind (and per key/slot where applicable).
    states: VecDeque<QueuedEvent>,
    next_sequence: u64,
    /// Number of events discarded since the previous drain.
    dropped_events: usize,
    /// Slots with unseen content changes, deduplicated.
    dirty: Vec<String>,
    /// Dirty/state key overflow requires authoritative slot reconciliation.
    resync_all: bool,
}

impl HubInner {
    fn request_resync(&mut self) {
        self.dirty.clear();
        self.resync_all = true;
    }

    fn mark_dirty(&mut self, slot: &str) {
        if self.resync_all || self.dirty.iter().any(|s| s == slot) {
            return;
        }
        if self.dirty.len() == FRAME_HUB_DIRTY_CAPACITY {
            self.request_resync();
            self.dropped_events = self.dropped_events.saturating_add(1);
            return;
        }
        self.dirty.push(slot.to_string());
    }

    fn enqueue(&mut self, event: HubEvent) {
        let sequence = self.next_sequence;
        self.next_sequence = self.next_sequence.saturating_add(1);

        if is_coalescing_state(&event) {
            if let Some(index) = self
                .states
                .iter()
                .position(|queued| same_state_slot(&queued.event, &event))
            {
                self.states.remove(index);
            } else if self.states.len() == FRAME_HUB_STATE_CAPACITY {
                self.states.pop_front();
                self.request_resync();
                self.dropped_events = self.dropped_events.saturating_add(1);
            }
            self.states.push_back(QueuedEvent { sequence, event });
            return;
        }

        if self.edges.len() == FRAME_HUB_EDGE_CAPACITY {
            if is_required_edge(&event) {
                if let Some(index) = self
                    .edges
                    .iter()
                    .position(|queued| !is_required_edge(&queued.event))
                {
                    self.edges.remove(index);
                    self.dropped_events = self.dropped_events.saturating_add(1);
                } else {
                    // An all-required FIFO cannot grow without bound. Keep
                    // every already-accepted completion/submit, reject the
                    // newest event, and surface the loss on the next drain.
                    self.dropped_events = self.dropped_events.saturating_add(1);
                    return;
                }
            } else {
                self.dropped_events = self.dropped_events.saturating_add(1);
                return;
            }
        }

        self.edges.push_back(QueuedEvent { sequence, event });
    }
}

fn is_required_edge(event: &HubEvent) -> bool {
    matches!(
        event,
        HubEvent::Done { .. }
            | HubEvent::Ui(
                Notification::UiEditorSubmit(_)
                    | Notification::UiFocus(_)
                    | Notification::UiComponentInput(_)
            )
    )
}

fn is_coalescing_state(event: &HubEvent) -> bool {
    matches!(
        event,
        HubEvent::Mounted { .. }
            | HubEvent::Disposed { .. }
            | HubEvent::Overlay { .. }
            | HubEvent::Ui(
                Notification::UiSetStatus(_)
                    | Notification::UiSetWorkingMessage(_)
                    | Notification::UiSetWorkingVisible(_)
                    | Notification::UiSetWorkingIndicator(_)
                    | Notification::UiSetHiddenThinkingLabel(_)
                    | Notification::UiSetTitle(_)
                    | Notification::UiSetEditorText(_)
                    | Notification::UiSetTheme(_)
                    | Notification::UiSetToolsExpanded(_)
                    | Notification::UiResize(_)
                    | Notification::UiEditorChange(_)
                    | Notification::UiTerminalInputActive(_)
            )
    )
}

fn same_state_slot(left: &HubEvent, right: &HubEvent) -> bool {
    match (left, right) {
        (
            HubEvent::Mounted { slot: left } | HubEvent::Disposed { slot: left },
            HubEvent::Mounted { slot: right } | HubEvent::Disposed { slot: right },
        ) => left == right,
        (HubEvent::Overlay { slot: left, .. }, HubEvent::Overlay { slot: right, .. }) => {
            left == right
        }
        (
            HubEvent::Ui(Notification::UiSetStatus(left)),
            HubEvent::Ui(Notification::UiSetStatus(right)),
        ) => left.key == right.key,
        (HubEvent::Ui(left), HubEvent::Ui(right)) => matches!(
            (left, right),
            (
                Notification::UiSetWorkingMessage(_),
                Notification::UiSetWorkingMessage(_)
            ) | (
                Notification::UiSetWorkingVisible(_),
                Notification::UiSetWorkingVisible(_)
            ) | (
                Notification::UiSetWorkingIndicator(_),
                Notification::UiSetWorkingIndicator(_)
            ) | (
                Notification::UiSetHiddenThinkingLabel(_),
                Notification::UiSetHiddenThinkingLabel(_)
            ) | (Notification::UiSetTitle(_), Notification::UiSetTitle(_))
                | (
                    Notification::UiSetEditorText(_),
                    Notification::UiSetEditorText(_)
                )
                | (Notification::UiSetTheme(_), Notification::UiSetTheme(_))
                | (
                    Notification::UiSetToolsExpanded(_),
                    Notification::UiSetToolsExpanded(_)
                )
                | (Notification::UiResize(_), Notification::UiResize(_))
                | (
                    Notification::UiEditorChange(_),
                    Notification::UiEditorChange(_)
                )
                | (
                    Notification::UiTerminalInputActive(_),
                    Notification::UiTerminalInputActive(_)
                )
        ),
        _ => false,
    }
}

/// Send-side frame store + event queue. One per extension binding.
#[derive(Default)]
pub struct FrameHub {
    inner: parking_lot::Mutex<HubInner>,
}

impl FrameHub {
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Notification sink for `BindOptions.fallback`. Routes frame traffic
    /// into the hub; everything else queues as [`HubEvent::Ui`].
    #[must_use]
    pub fn sink(self: &Arc<Self>) -> NotificationSink {
        let hub = self.clone();
        Arc::new(move |notification| hub.apply(notification))
    }

    /// Ingest one notification (tokio side).
    pub fn apply(&self, notification: Notification) {
        let mut inner = self.inner.lock();
        match notification {
            Notification::UiFrame(params) => apply_frame(&mut inner, params),
            Notification::UiDispose(params) => {
                inner.slots.remove(&params.slot);
                inner.dirty.retain(|s| *s != params.slot);
                inner.enqueue(HubEvent::Disposed { slot: params.slot });
            }
            Notification::UiDone(params) => inner.enqueue(HubEvent::Done {
                slot: params.slot,
                result: params.result,
            }),
            Notification::UiOverlay(params) => inner.enqueue(HubEvent::Overlay {
                slot: params.slot,
                options: params.options,
            }),
            other => inner.enqueue(HubEvent::Ui(other)),
        }
    }

    /// Drain pending structural events and the dirty-slot list (TUI thread,
    /// once per pump tick).
    #[must_use]
    pub fn drain(&self) -> (Vec<HubEvent>, Vec<String>) {
        let mut inner = self.inner.lock();
        let mut edges = std::mem::take(&mut inner.edges).into_iter().peekable();
        let mut states = std::mem::take(&mut inner.states).into_iter().peekable();
        let mut events = Vec::with_capacity(edges.len() + states.len() + 1);
        while edges.peek().is_some() || states.peek().is_some() {
            let take_edge = match (edges.peek(), states.peek()) {
                (Some(edge), Some(state)) => edge.sequence < state.sequence,
                (Some(_), None) => true,
                (None, Some(_)) => false,
                (None, None) => break,
            };
            let queued = if take_edge {
                edges.next().expect("peeked edge")
            } else {
                states.next().expect("peeked state")
            };
            events.push(queued.event);
        }
        let dropped = std::mem::take(&mut inner.dropped_events);
        if std::mem::take(&mut inner.resync_all) {
            events.push(HubEvent::ResyncAll);
        }
        if dropped > 0 {
            events.push(HubEvent::Overflow { dropped });
        }
        (events, std::mem::take(&mut inner.dirty))
    }

    /// Number of buffered events and dirty keys, excluding the
    /// allocation-free overflow/reconciliation flags.
    #[must_use]
    pub fn pending_event_count(&self) -> usize {
        let inner = self.inner.lock();
        inner.edges.len() + inner.states.len() + inner.dirty.len()
    }

    /// Snapshot the authoritative set of live frame slots. Used only after a
    /// bounded-buffer overflow requests full reconciliation.
    #[must_use]
    pub fn slot_names(&self) -> Vec<String> {
        self.inner.lock().slots.keys().cloned().collect()
    }

    /// True when any event, overflow report, or dirty slot is pending.
    #[must_use]
    pub fn has_pending(&self) -> bool {
        let inner = self.inner.lock();
        !inner.edges.is_empty()
            || !inner.states.is_empty()
            || inner.dropped_events > 0
            || inner.resync_all
            || !inner.dirty.is_empty()
    }

    /// Latest content snapshot for a slot.
    #[must_use]
    pub fn snapshot(&self, slot: &str) -> Option<SlotSnapshot> {
        let inner = self.inner.lock();
        inner.slots.get(slot).map(|state| SlotSnapshot {
            lines: state.lines.clone(),
            revision: state.revision,
            wants_key_release: state.wants_key_release,
            focusable: state.focusable,
            placement: state.placement,
        })
    }

    /// Mint the request generation for a `ui/render` issue (TUI thread; the
    /// leaf passes it into [`UiOutbound::Render`]).
    #[must_use]
    pub fn begin_render_request(&self, slot: &str) -> u64 {
        let mut inner = self.inner.lock();
        let state = inner.slots.entry(slot.to_string()).or_default();
        state.render_generation += 1;
        state.render_generation
    }

    /// Apply a `ui/render` response (tokio side). Dropped when a newer frame
    /// (revision moved) or a newer render request (generation moved) exists.
    pub fn apply_render_response(
        &self,
        slot: &str,
        revision: u64,
        generation: u64,
        lines: Vec<String>,
    ) {
        let mut inner = self.inner.lock();
        let Some(state) = inner.slots.get_mut(slot) else {
            return; // Disposed while the request was in flight.
        };
        if state.revision != revision || state.render_generation != generation {
            return;
        }
        state.lines = lines;
        state.revision += 1;
        inner.mark_dirty(slot);
    }

    /// Remove a slot locally (host-initiated teardown; pair with
    /// [`UiOutbound::Dispose`] when the sidecar must drop its component).
    pub fn remove(&self, slot: &str) {
        let mut inner = self.inner.lock();
        inner.slots.remove(slot);
        inner.dirty.retain(|s| s != slot);
    }

    /// Drop every slot and pending event (session teardown / respawn).
    pub fn clear(&self) {
        let mut inner = self.inner.lock();
        inner.slots.clear();
        inner.edges.clear();
        inner.states.clear();
        inner.dropped_events = 0;
        inner.resync_all = false;
        inner.dirty.clear();
    }
}

fn apply_frame(inner: &mut HubInner, params: FrameParams) {
    let is_new = !inner.slots.contains_key(&params.slot);
    let state = inner.slots.entry(params.slot.clone()).or_default();
    // Latest-wins: drop stale versions from a respawn-free stream. A
    // version RESET below the high-water mark (sidecar respawn re-mounts
    // slots from version 1) is accepted as new content.
    if !is_new && params.version <= state.version && params.version != 1 {
        return;
    }
    state.version = params.version;
    state.lines = params.lines;
    state.revision += 1;
    state.wants_key_release = params.wants_key_release;
    state.focusable = params.focusable;
    if params.placement.is_some() {
        state.placement = params.placement;
    }
    if is_new {
        inner.enqueue(HubEvent::Mounted {
            slot: params.slot.clone(),
        });
    }
    inner.mark_dirty(&params.slot);
}

// ============================================================================
// BridgedLeaf (TUI thread)
// ============================================================================

/// A sidecar-rendered component hosted as a plain pi-tui leaf.
///
/// Content flows hub → [`sync`](Self::sync) (parse on the TUI thread) →
/// cached [`Line`]s; input flows [`handle_input`](Component::handle_input) →
/// [`UiOutbound::Input`]. Nothing here blocks or awaits.
pub struct BridgedLeaf {
    hub: Arc<FrameHub>,
    outbound: UiOutboundSender,
    slot: String,
    lines: Vec<Line>,
    revision: u64,
    /// Width last sent to the sidecar (dedup for `ui/render` requests).
    sent_width: Option<u16>,
    /// Content changed since the last `render` (consumed into `status`).
    pending_changed: bool,
    /// Status of the last `render` (DirtySpans input).
    status: RenderStatus,
    focused: bool,
    focusable: bool,
    wants_key_release: bool,
}

impl BridgedLeaf {
    #[must_use]
    pub fn new(hub: Arc<FrameHub>, outbound: UiOutboundSender, slot: impl Into<String>) -> Self {
        let slot = slot.into();
        let mut leaf = Self {
            hub,
            outbound,
            slot,
            lines: Vec::new(),
            revision: 0,
            sent_width: None,
            pending_changed: true,
            status: RenderStatus::Changed,
            focused: false,
            focusable: false,
            wants_key_release: false,
        };
        leaf.sync();
        leaf
    }

    #[must_use]
    pub fn slot(&self) -> &str {
        &self.slot
    }

    /// Pull the latest hub content; parses ANSI on this (TUI) thread.
    /// Returns `true` when the cached lines changed.
    pub fn sync(&mut self) -> bool {
        let Some(snapshot) = self.hub.snapshot(&self.slot) else {
            return false;
        };
        self.wants_key_release = snapshot.wants_key_release;
        self.focusable = snapshot.focusable;
        if snapshot.revision == self.revision {
            return false;
        }
        self.revision = snapshot.revision;
        self.lines = lines_from_ansi(&snapshot.lines);
        self.pending_changed = true;
        // Adapters such as interactive `Shared<T>` consult
        // `last_render_status` before calling `render`. Make the newly
        // ingested generation observable immediately; `render` consumes
        // `pending_changed` back to `Unchanged` on the following pass.
        self.status = RenderStatus::Changed;
        true
    }

    /// Ask the sidecar to re-render at the last known width (pi-tui
    /// `invalidate` semantics for a remote component).
    fn request_render(&mut self, width: u16) {
        self.sent_width = Some(width);
        let generation = self.hub.begin_render_request(&self.slot);
        (self.outbound)(UiOutbound::Render {
            slot: self.slot.clone(),
            width,
            revision: self.revision,
            generation,
        });
    }
}

impl Component for BridgedLeaf {
    fn render(&mut self, width: u16) -> &[Line] {
        // Late content pickup: harmless double-check after the pump drain
        // (first render after mount happens before any drain).
        self.sync();
        if self.sent_width != Some(width) {
            // Non-blocking: the re-rendered frame arrives as new content.
            self.request_render(width);
        }
        self.status = if std::mem::take(&mut self.pending_changed) {
            RenderStatus::Changed
        } else {
            RenderStatus::Unchanged
        };
        &self.lines
    }

    fn invalidate(&mut self) {
        if let Some(width) = self.sent_width {
            self.request_render(width);
        }
    }

    fn handle_input(&mut self, data: &str) {
        (self.outbound)(UiOutbound::Input {
            slot: self.slot.clone(),
            data: data.to_string(),
        });
    }

    fn wants_key_release(&self) -> bool {
        self.wants_key_release
    }

    fn last_render_status(&self) -> RenderStatus {
        self.status
    }

    fn as_focusable(&mut self) -> Option<&mut dyn Focusable> {
        // Only focus-eligible slots (editor / custom dialogs) accept focus;
        // headers, footers, and widgets stay input-transparent.
        if self.focusable { Some(self) } else { None }
    }
}

impl Focusable for BridgedLeaf {
    fn focused(&self) -> bool {
        self.focused
    }

    fn set_focused(&mut self, focused: bool) {
        if self.focused == focused {
            return;
        }
        self.focused = focused;
        (self.outbound)(UiOutbound::Focus {
            slot: self.slot.clone(),
            focused,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_ext_protocol::{
        ActiveParams, ComponentInputParams, DoneParams, FrameParams, OptionalTextParams,
        OverlayParams, SlotParams, TextParams, ThemeSelectionParams, VisibleParams,
    };
    use serde_json::json;

    fn frame(slot: &str, version: u64, lines: &[&str]) -> Notification {
        Notification::UiFrame(FrameParams {
            slot: slot.to_string(),
            lines: lines.iter().map(|s| (*s).to_string()).collect(),
            version,
            wants_key_release: false,
            focusable: false,
            placement: None,
        })
    }

    #[test]
    fn hundred_rapid_frames_coalesce_to_latest() {
        let hub = FrameHub::new();
        for v in 1..=100u64 {
            hub.apply(frame("widget:w", v, &[&format!("frame {v}")]));
        }
        let (events, dirty) = hub.drain();
        // One mount event, one dirty entry — not 100.
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], HubEvent::Mounted { slot } if slot == "widget:w"));
        assert_eq!(dirty, vec!["widget:w".to_string()]);
        let snapshot = hub.snapshot("widget:w").unwrap();
        assert_eq!(snapshot.lines, vec!["frame 100".to_string()]);
    }

    #[test]
    fn stalled_pump_keeps_mixed_ui_traffic_strictly_bounded() {
        let hub = FrameHub::new();
        let mut submitted = Vec::new();

        for sequence in 1..=100_001_u64 {
            hub.apply(Notification::UiSetWorkingMessage(OptionalTextParams {
                text: Some(format!("working-{sequence}")),
            }));
            hub.apply(Notification::UiSetToolsExpanded(VisibleParams {
                visible: sequence % 2 == 0,
            }));
            hub.apply(Notification::UiTerminalInputActive(ActiveParams {
                active: sequence % 3 == 0,
            }));
            hub.apply(Notification::UiSetTheme(ThemeSelectionParams {
                theme: format!("theme-{sequence}"),
            }));
            hub.apply(Notification::UiEditorChange(TextParams {
                text: format!("draft-{sequence}"),
            }));
            hub.apply(Notification::UiPasteToEditor(TextParams {
                text: format!("paste-{sequence}"),
            }));
            hub.apply(frame(
                "widget:stress",
                sequence,
                &[&format!("frame-{sequence}")],
            ));

            if sequence % 10_000 == 0 {
                let text = format!("submit-{sequence}");
                submitted.push(text.clone());
                hub.apply(Notification::UiEditorSubmit(TextParams { text }));
            }
        }

        // A delayed pre-stress frame must not roll the retained slot backward.
        hub.apply(frame("widget:stress", 99_999, &["stale"]));

        assert!(
            hub.pending_event_count() <= FRAME_HUB_BUFFER_CAPACITY,
            "stalled pump retained {} buffered items (capacity {})",
            hub.pending_event_count(),
            FRAME_HUB_BUFFER_CAPACITY
        );

        let (events, dirty) = hub.drain();
        assert_eq!(dirty, vec!["widget:stress".to_string()]);
        assert_eq!(
            hub.snapshot("widget:stress").unwrap().lines,
            vec!["frame-100001".to_string()]
        );

        let delivered_submits: Vec<String> = events
            .iter()
            .filter_map(|event| match event {
                HubEvent::Ui(Notification::UiEditorSubmit(params)) => Some(params.text.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            delivered_submits, submitted,
            "submit order changed under pressure"
        );

        assert!(events.iter().any(|event| matches!(
            event,
            HubEvent::Ui(Notification::UiSetWorkingMessage(params))
                if params.text.as_deref() == Some("working-100001")
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            HubEvent::Ui(Notification::UiSetToolsExpanded(params)) if !params.visible
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            HubEvent::Ui(Notification::UiTerminalInputActive(params)) if !params.active
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            HubEvent::Ui(Notification::UiSetTheme(params)) if params.theme == "theme-100001"
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            HubEvent::Ui(Notification::UiEditorChange(params)) if params.text == "draft-100001"
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            HubEvent::Overflow { dropped } if *dropped > 0
        )));
    }

    #[test]
    fn overflow_never_evicts_accepted_submit_or_dismiss_events() {
        let hub = FrameHub::new();
        for index in 0..FRAME_HUB_EDGE_CAPACITY {
            hub.apply(Notification::UiPasteToEditor(TextParams {
                text: format!("paste-{index}"),
            }));
        }
        hub.apply(Notification::UiEditorSubmit(TextParams {
            text: "required-submit".to_string(),
        }));
        hub.apply(Notification::UiDone(DoneParams {
            slot: "custom:required".to_string(),
            result: json!(null),
        }));

        assert_eq!(hub.pending_event_count(), FRAME_HUB_EDGE_CAPACITY);
        let (events, _) = hub.drain();
        assert!(events.iter().any(|event| matches!(
            event,
            HubEvent::Ui(Notification::UiEditorSubmit(params)) if params.text == "required-submit"
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            HubEvent::Done { slot, .. } if slot == "custom:required"
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            HubEvent::Overflow { dropped } if *dropped == 2
        )));
    }

    #[test]
    fn required_edge_saturation_cannot_displace_latest_state() {
        let hub = FrameHub::new();
        for index in 0..FRAME_HUB_EDGE_CAPACITY {
            hub.apply(Notification::UiEditorSubmit(TextParams {
                text: format!("submit-{index}"),
            }));
        }

        hub.apply(Notification::UiSetTheme(ThemeSelectionParams {
            theme: "latest-theme".to_string(),
        }));
        hub.apply(Notification::UiEditorChange(TextParams {
            text: "latest-editor".to_string(),
        }));
        hub.apply(Notification::UiTerminalInputActive(ActiveParams {
            active: true,
        }));
        hub.apply(Notification::UiOverlay(OverlayParams {
            slot: "custom:latest".to_string(),
            options: json!({"anchor": "center"}),
        }));
        hub.apply(frame("custom:latest", 1, &["latest-frame"]));

        assert_eq!(
            hub.pending_event_count(),
            FRAME_HUB_EDGE_CAPACITY + 6,
            "state and dirty keys must use independent bounded storage"
        );
        let (events, dirty) = hub.drain();
        assert_eq!(dirty, vec!["custom:latest".to_string()]);
        assert!(events.iter().any(|event| matches!(
            event,
            HubEvent::Ui(Notification::UiSetTheme(params)) if params.theme == "latest-theme"
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            HubEvent::Ui(Notification::UiEditorChange(params)) if params.text == "latest-editor"
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            HubEvent::Ui(Notification::UiTerminalInputActive(params)) if params.active
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            HubEvent::Overlay { slot, .. } if slot == "custom:latest"
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            HubEvent::Mounted { slot } if slot == "custom:latest"
        )));
    }

    #[test]
    fn unique_slot_flood_switches_to_bounded_authoritative_resync() {
        let hub = FrameHub::new();
        for sequence in 1..=100_001_u64 {
            let slot = format!("widget:{sequence}");
            hub.apply(frame(&slot, 1, &["frame"]));
        }

        assert!(
            hub.pending_event_count() <= FRAME_HUB_BUFFER_CAPACITY,
            "unique slots retained {} buffered items (capacity {})",
            hub.pending_event_count(),
            FRAME_HUB_BUFFER_CAPACITY
        );
        let (events, dirty) = hub.drain();
        assert!(dirty.is_empty(), "resync supersedes individual dirty keys");
        assert!(
            events
                .iter()
                .any(|event| matches!(event, HubEvent::ResyncAll))
        );
        assert!(events.iter().any(|event| matches!(
            event,
            HubEvent::Overflow { dropped } if *dropped > 0
        )));
        assert_eq!(hub.slot_names().len(), 100_001);
        assert_eq!(
            hub.snapshot("widget:100001").unwrap().lines,
            vec!["frame".to_string()]
        );
    }

    #[test]
    fn stale_versions_drop() {
        let hub = FrameHub::new();
        hub.apply(frame("s", 5, &["new"]));
        hub.apply(frame("s", 4, &["old"]));
        assert_eq!(hub.snapshot("s").unwrap().lines, vec!["new".to_string()]);
        // Version reset (respawn) is accepted.
        hub.apply(frame("s", 1, &["respawned"]));
        assert_eq!(
            hub.snapshot("s").unwrap().lines,
            vec!["respawned".to_string()]
        );
    }

    #[test]
    fn dispose_removes_slot_and_orders_event() {
        let hub = FrameHub::new();
        hub.apply(frame("s", 1, &["x"]));
        hub.apply(Notification::UiDispose(SlotParams { slot: "s".into() }));
        let (events, dirty) = hub.drain();
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], HubEvent::Disposed { slot } if slot == "s"));
        assert!(dirty.is_empty());
        assert!(hub.snapshot("s").is_none());
    }

    #[test]
    fn done_and_overlay_ride_the_ordered_queue() {
        let hub = FrameHub::new();
        hub.apply(Notification::UiOverlay(OverlayParams {
            slot: "custom:1".into(),
            options: json!({"anchor": "center"}),
        }));
        hub.apply(frame("custom:1", 1, &["body"]));
        hub.apply(Notification::UiDone(DoneParams {
            slot: "custom:1".into(),
            result: json!("picked"),
        }));
        let (events, _) = hub.drain();
        assert!(matches!(&events[0], HubEvent::Overlay { .. }));
        assert!(matches!(&events[1], HubEvent::Mounted { .. }));
        assert!(matches!(&events[2], HubEvent::Done { result, .. } if result == "picked"));
    }

    #[test]
    fn render_response_guards_against_stale_revision_and_generation() {
        let hub = FrameHub::new();
        hub.apply(frame("s", 1, &["v1"]));
        let revision = hub.snapshot("s").unwrap().revision;
        let g1 = hub.begin_render_request("s");
        let g2 = hub.begin_render_request("s");
        // Older request's response loses to the newer generation.
        hub.apply_render_response("s", revision, g1, vec!["old-width".into()]);
        assert_eq!(hub.snapshot("s").unwrap().lines, vec!["v1".to_string()]);
        // Newer generation applies.
        hub.apply_render_response("s", revision, g2, vec!["new-width".into()]);
        assert_eq!(
            hub.snapshot("s").unwrap().lines,
            vec!["new-width".to_string()]
        );
        // A frame arriving after issue invalidates by revision.
        let g3 = hub.begin_render_request("s");
        let revision = hub.snapshot("s").unwrap().revision;
        hub.apply(frame("s", 2, &["v2"]));
        hub.apply_render_response(
            "s",
            revision,
            g3,
            vec!["late"].into_iter().map(String::from).collect(),
        );
        assert_eq!(hub.snapshot("s").unwrap().lines, vec!["v2".to_string()]);
    }

    #[test]
    fn leaf_parses_on_sync_and_reports_dirty_once() {
        let hub = FrameHub::new();
        let sent: Arc<parking_lot::Mutex<Vec<UiOutbound>>> = Arc::default();
        let sink = sent.clone();
        let outbound: UiOutboundSender = Arc::new(move |msg| sink.lock().push(msg));
        hub.apply(frame("w", 1, &["\u{1b}[31mred\u{1b}[0m"]));
        let mut leaf = BridgedLeaf::new(hub.clone(), outbound, "w");

        let lines = leaf.render(40);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].plain_text(), "red");
        assert_eq!(leaf.last_render_status(), RenderStatus::Changed);
        // Width request issued once for 40.
        assert!(matches!(
            sent.lock()[0],
            UiOutbound::Render { width: 40, .. }
        ));

        // Unchanged content → Unchanged status, no new request.
        let _ = leaf.render(40);
        assert_eq!(leaf.last_render_status(), RenderStatus::Unchanged);
        assert_eq!(sent.lock().len(), 1);

        // New frame → Changed after sync.
        hub.apply(frame("w", 2, &["blue"]));
        assert!(leaf.sync());
        // `Shared<T>` consults this before deciding to call `render`.
        assert_eq!(leaf.last_render_status(), RenderStatus::Changed);
        let lines = leaf.render(40);
        assert_eq!(lines[0].plain_text(), "blue");
        assert_eq!(leaf.last_render_status(), RenderStatus::Changed);

        // Resize → new render request at the new width.
        let _ = leaf.render(60);
        assert!(matches!(
            sent.lock().last().unwrap(),
            UiOutbound::Render { width: 60, .. }
        ));
    }

    #[test]
    fn leaf_forwards_input_and_focus() {
        let hub = FrameHub::new();
        let sent: Arc<parking_lot::Mutex<Vec<UiOutbound>>> = Arc::default();
        let sink = sent.clone();
        let outbound: UiOutboundSender = Arc::new(move |msg| sink.lock().push(msg));
        hub.apply(Notification::UiFrame(FrameParams {
            slot: "editor".into(),
            lines: vec!["> ".into()],
            version: 1,
            wants_key_release: true,
            focusable: true,
            placement: None,
        }));
        let mut leaf = BridgedLeaf::new(hub, outbound, "editor");
        assert!(leaf.wants_key_release());

        leaf.handle_input("x");
        leaf.set_focused(true);
        leaf.set_focused(true); // dedup
        leaf.set_focused(false);
        let sent = sent.lock();
        assert_eq!(
            *sent,
            vec![
                UiOutbound::Input {
                    slot: "editor".into(),
                    data: "x".into()
                },
                UiOutbound::Focus {
                    slot: "editor".into(),
                    focused: true
                },
                UiOutbound::Focus {
                    slot: "editor".into(),
                    focused: false
                },
            ]
        );
        let _ = Notification::UiComponentInput(ComponentInputParams {
            slot: "editor".into(),
            data: "x".into(),
        });
    }
}
