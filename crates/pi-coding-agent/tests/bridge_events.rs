//! Event dispatch + action server + session sync (Phase 6 commit C6),
//! integration-tested against the REAL Bun sidecar (`sidecar/src/main.ts`)
//! loading unmodified pi-API fixture extensions.
//!
//! Covered contracts:
//! - strict init order: spawn → handshake → init (session snapshot + state)
//!   → initialized → only then `session_start`;
//! - subscribed events forwarded serially, observed order preserved;
//! - blocking results resolved by pi's real runner in extension load order,
//!   with null-vs-`{}` semantics intact;
//! - `action/appendEntry` → real session file → `session/sync` (epoch+1)
//!   → mirror reconciliation (optimistic duplicates collapse);
//! - `ctx.compact()` callback correlation: success via a forwarded manual
//!   `session_compact`, cancellation via a self-observed cancel, and forced
//!   forwarding while a compact is pending even without subscriptions;
//! - `ctx.reload()`: in-place sidecar re-init, refreshed command
//!   registrations, session recreate with reason `reload`;
//! - crash: mid-turn events never respawn, the next turn boundary respawns
//!   exactly once (full replay).

#![cfg(unix)]

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use pi_agent::CancellationToken;
use pi_coding_agent::HostActions;
use pi_coding_agent::extension_bridge::{
    BoxFuture, ExtensionUiHost, NotifyType, SessionStartReason, UiDialogOptions, WidgetPlacement,
};
use pi_coding_agent::extensions::binding::{BindOptions, ExtensionBinding, SessionHostActions};
use pi_coding_agent::extensions::{
    BridgeState, BunEnvironment, LauncherSource, SidecarLauncher, resolve_bun,
};
use pi_coding_agent::session::AgentSession;
use pi_coding_agent::session::events::AgentSessionEvent;
use pi_coding_agent::session_types::SessionEntry;
use pi_ext_protocol::{
    CommandExecuteParams, ExtensionError, ExtensionEvent, Request, SwitchSessionParams,
};
use serde_json::{Value, json};

use common::{
    TestRuntime, TestRuntimeOptions, assistant_text_message, gated_stream_fn, make_runtime,
};

// ============================================================================
// Fixture extensions (unmodified pi extension API, no imports needed)
// ============================================================================

/// Records subscribed events + serves mirror reports; `user_bash` contributes
/// only for commands starting with `first`.
const RECORDER: &str = r#"
export default function (pi) {
    const record = (e, data = {}) => pi.appendEntry("obs", { e, ...data });
    pi.on("session_start", (ev) => record("session_start", { reason: ev.reason }));
    pi.on("agent_start", () => record("agent_start"));
    pi.on("message_start", () => record("message_start"));
    pi.on("message_end", () => record("message_end"));
    pi.on("agent_end", () => record("agent_end"));
    pi.on("user_bash", (ev) => {
        if (ev.command.startsWith("first")) {
            return { command: "recorder-wins" };
        }
        return undefined;
    });
    pi.registerCommand("mirror-report", {
        description: "report mirror state",
        handler: async (_args, ctx) => {
            record("mirror", {
                count: ctx.sessionManager.getEntries().length,
                name: ctx.sessionManager.getSessionName() ?? null,
            });
        },
    });
}
"#;

/// Second-in-load-order extension: only consulted when the first declined.
const SECOND: &str = r#"
export default function (pi) {
    pi.on("user_bash", () => ({ command: "second-wins", excludeFromContext: true }));
}
"#;

/// Compact driver WITH session_before_compact handlers (cancel + override).
const COMPACTOR: &str = r#"
export default function (pi) {
    pi.registerCommand("trigger-compact", {
        description: "queue a compact with correlated callbacks",
        handler: async (args, ctx) => {
            ctx.compact({
                ...(args !== "" ? { customInstructions: args } : {}),
                onComplete: (result) => pi.appendEntry("compact-complete", {
                    tag: args, summary: result.summary,
                }),
                onError: (error) => pi.appendEntry("compact-error", {
                    tag: args, message: error.message,
                }),
            });
        },
    });
    pi.on("session_before_compact", (event, ctx) => {
        if (event.customInstructions === "cancel-me") {
            return { cancel: true };
        }
        if (event.customInstructions === "override") {
            const entries = ctx.sessionManager.getEntries();
            const lastUser = [...entries].reverse().find(
                (entry) => entry.type === "message" && entry.message.role === "user",
            );
            return { compaction: {
                summary: "ext-summary",
                firstKeptEntryId: lastUser.id,
                tokensBefore: 7,
            } };
        }
        return undefined;
    });
    pi.on("session_compact", () => {});
}
"#;

/// Compact driver WITHOUT any compact subscriptions: callbacks must still
/// settle because pending compacts force manual session_compact forwarding.
const BLIND_COMPACTOR: &str = r#"
export default function (pi) {
    pi.registerCommand("trigger-compact", {
        description: "queue a compact",
        handler: async (args, ctx) => {
            ctx.compact({
                onComplete: (result) => pi.appendEntry("compact-complete", {
                    tag: args, summary: result.summary,
                }),
                onError: (error) => pi.appendEntry("compact-error", {
                    tag: args, message: error.message,
                }),
            });
        },
    });
}
"#;

/// Reload probe: a process-global generation counter names the registered
/// command, and session_start records the generation that observed it.
const RELOADER: &str = r#"
const g = globalThis;
export default function (pi) {
    g.__gen = (g.__gen ?? 0) + 1;
    const gen = g.__gen;
    pi.registerCommand(`gen-${gen}`, { description: "generation probe", handler: async () => {} });
    pi.registerCommand("do-reload", {
        description: "reload extensions",
        handler: async (_args, ctx) => { await ctx.reload(); },
    });
    pi.on("session_start", (ev) => pi.appendEntry("start", {
        reason: ev.reason, gen, prev: ev.previousSessionFile ?? null,
    }));
}
"#;

/// Lifecycle recorder + replacement drivers: a process-global array captures
/// the exact event order (shutdown/start survive session replacement).
const LIFECYCLE: &str = r#"
const g = globalThis;
export default function (pi) {
    g.__order ??= [];
    const push = (x) => g.__order.push(x);
    pi.on("session_shutdown", (ev) => push({
        e: "shutdown", reason: ev.reason, target: ev.targetSessionFile ?? null,
    }));
    pi.on("session_before_switch", (ev) => {
        push({ e: "before_switch", reason: ev.reason, target: ev.targetSessionFile ?? null });
        return undefined;
    });
    pi.on("session_before_fork", (ev) => {
        push({ e: "before_fork", entryId: ev.entryId });
        return undefined;
    });
    pi.on("session_start", (ev) => push({
        e: "start", reason: ev.reason, prev: ev.previousSessionFile ?? null,
    }));
    pi.registerCommand("do-new", {
        description: "new session",
        handler: async (_args, ctx) => {
            const r = await ctx.newSession();
            pi.appendEntry("done", { op: "new", cancelled: r.cancelled });
        },
    });
    pi.registerCommand("do-switch", {
        description: "switch session",
        handler: async (args, ctx) => {
            const r = await ctx.switchSession(args);
            pi.appendEntry("done", { op: "switch", cancelled: r.cancelled });
        },
    });
    pi.registerCommand("do-fork", {
        description: "fork",
        handler: async (args, ctx) => {
            const r = await ctx.fork(args);
            pi.appendEntry("done", { op: "fork", cancelled: r.cancelled });
        },
    });
    pi.registerCommand("report-order", {
        description: "dump recorded order",
        handler: async () => { pi.appendEntry("order", { order: g.__order }); },
    });
}
"#;

/// A before_switch hook that always cancels; shutdown observation proves (or
/// disproves) that a teardown happened anyway.
const CANCELLER: &str = r#"
export default function (pi) {
    pi.on("session_before_switch", () => ({ cancel: true }));
    pi.on("session_shutdown", () => pi.appendEntry("shut", {}));
    pi.registerCommand("try-switch", {
        description: "switch that the hook cancels",
        handler: async (args, ctx) => {
            const r = await ctx.switchSession(args);
            pi.appendEntry("done", { cancelled: r.cancelled });
        },
    });
}
"#;

/// A before_switch hook that stalls long enough for a cancel frame to win.
const SLOW_GATE: &str = r#"
export default function (pi) {
    pi.on("session_before_switch", async () => {
        await new Promise((resolve) => setTimeout(resolve, 8000));
        return undefined;
    });
    pi.on("session_shutdown", () => pi.appendEntry("shut", {}));
}
"#;

/// Aborts its own ui.select through an AbortController: the sidecar emits a
/// real `cancel` frame for the in-flight `ui/select` request.
const DIALOG_ABORTER: &str = r#"
export default function (pi) {
    pi.registerCommand("dialog-abort", {
        description: "abort a pending select",
        handler: async (_args, ctx) => {
            const controller = new AbortController();
            setTimeout(() => controller.abort(), 100);
            const choice = await ctx.ui.select("pick", ["a", "b"], { signal: controller.signal });
            pi.appendEntry("dialog", { choice: choice ?? null });
        },
    });
}
"#;

// ============================================================================
// Harness
// ============================================================================

fn sidecar_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../sidecar")
        .canonicalize()
        .expect("sidecar package present")
}

fn real_launcher() -> SidecarLauncher {
    let bun = resolve_bun(&BunEnvironment::from_env()).expect("bun installed (sidecar tests)");
    let dir = sidecar_dir();
    SidecarLauncher {
        bun,
        entry: dir.join("src/main.ts"),
        cwd: dir,
        envs: Vec::new(),
    }
}

struct Fixture {
    /// Keeps the temp dirs (and the runtime the reload test drives) alive.
    #[allow(dead_code)]
    runtime: TestRuntime,
    binding: Arc<ExtensionBinding>,
    #[allow(dead_code)]
    actions: Arc<SessionHostActions>,
    errors: Arc<parking_lot::Mutex<Vec<ExtensionError>>>,
}

impl Fixture {
    fn session(&self) -> AgentSession {
        self.binding.session()
    }

    fn entries(&self) -> Vec<SessionEntry> {
        self.session().with_session_manager(|sm| sm.get_entries())
    }

    fn obs(&self, custom_type: &str) -> Vec<Value> {
        self.entries()
            .into_iter()
            .filter_map(|entry| match entry {
                SessionEntry::Custom {
                    custom_type: kind,
                    data,
                    ..
                } if kind == custom_type => Some(data.unwrap_or(Value::Null)),
                _ => None,
            })
            .collect()
    }

    /// Poll until `pred` holds; panics with the session tail on timeout.
    async fn wait_until(&self, what: &str, pred: impl Fn(&Fixture) -> bool) {
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(20) {
            if pred(self) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!(
            "timed out waiting for {what}; errors={:?} entries tail={:?}",
            self.errors.lock(),
            self.entries().into_iter().rev().take(6).collect::<Vec<_>>(),
        );
    }

    async fn execute_command(&self, name: &str, args: &str) -> Result<Value, String> {
        let connection = self
            .binding
            .host()
            .current_connection()
            .await
            .expect("live sidecar");
        connection
            .request(Request::CommandExecute(CommandExecuteParams {
                name: name.to_string(),
                args: args.to_string(),
            }))
            .await
            .map_err(|error| error.to_string())
    }
}

/// Build a persisted-session runtime, write the fixture extensions, and bind
/// the REAL sidecar.
async fn fixture(extensions: &[(&str, &str)], options: TestRuntimeOptions) -> Fixture {
    fixture_with_ui(extensions, options, false).await
}

/// [`fixture`] with `hasUi` control: `true` makes the sidecar runner install
/// its RPC-backed `ctx.ui` (dialogs cross the wire).
async fn fixture_with_ui(
    extensions: &[(&str, &str)],
    options: TestRuntimeOptions,
    has_ui: bool,
) -> Fixture {
    let runtime = make_runtime(TestRuntimeOptions {
        persisted: true,
        with_auth: true,
        ..options
    })
    .await;
    let ext_dir = runtime.tmp.path().join("extensions");
    std::fs::create_dir_all(&ext_dir).expect("ext dir");
    let mut paths = Vec::new();
    for (name, source) in extensions {
        let path = ext_dir.join(name);
        std::fs::write(&path, source).expect("fixture extension");
        paths.push(path);
    }
    let errors: Arc<parking_lot::Mutex<Vec<ExtensionError>>> = Arc::default();
    let sink_errors = errors.clone();
    let actions = SessionHostActions::new();
    let session = runtime.runtime.session();
    let cwd = runtime.tmp.path().join("project");
    let agent_dir = runtime.tmp.path().join("agent");
    let session_dir = runtime.tmp.path().join("sessions");
    let mut bind_options = BindOptions::new(
        paths,
        LauncherSource::Resolved(real_launcher()),
        cwd,
        agent_dir,
        session_dir,
        Arc::new(move |error| sink_errors.lock().push(error)),
        actions.clone(),
    );
    // Fix P6-dispatch: bind installs the SidecarBridge into the runtime's
    // lifecycle path (session_shutdown + blocking hooks reach the sidecar).
    bind_options.runtime = Some(runtime.runtime.clone());
    bind_options.has_ui = has_ui;
    let binding = pi_coding_agent::extensions::binding::bind_extensions(&session, bind_options)
        .expect("canonical extension paths")
        .expect("extensions discovered");
    actions.attach(&binding);
    actions.attach_runtime(runtime.runtime.clone());

    Fixture {
        runtime,
        binding,
        actions,
        errors,
    }
}

/// Stream fn answering every summarization request with a canned summary.
fn endless_summary_stream_fn() -> pi_agent::StreamFn {
    Arc::new(move |model: pi_ai::Model, _context, _options| {
        Box::pin(async move {
            let stream = pi_ai::create_assistant_message_event_stream();
            let message = assistant_text_message(&model, "canned summary of the conversation");
            stream.push(pi_ai::AssistantMessageEvent::Done {
                reason: message.stop_reason,
                message,
            });
            stream
        })
    })
}

/// Seed the session with enough large user messages that
/// `prepare_compaction` finds a valid cut point.
fn seed_compactable_session(session: &AgentSession) {
    let blob = "lorem ipsum dolor sit amet ".repeat(2_000); // ~54k chars each
    session.with_session_manager_mut(|sm| {
        for index in 0..6 {
            let user = json!({
                "role": "user",
                "content": format!("{index}: {blob}"),
                "timestamp": 1_700_000_000_000_i64 + index,
            });
            sm.append_message(user).expect("seed user message");
            let assistant = json!({
                "role": "assistant",
                "content": [{"type": "text", "text": format!("ack {index}")}],
                "api": "anthropic-messages",
                "provider": "anthropic",
                "model": "claude-opus-4-8",
                "usage": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0,
                           "totalTokens": 0,
                           "cost": {"input": 0.0, "output": 0.0, "cacheRead": 0.0,
                                     "cacheWrite": 0.0, "total": 0.0}},
                "stopReason": "stop",
                "timestamp": 1_700_000_000_001_i64 + index,
            });
            sm.append_message(assistant)
                .expect("seed assistant message");
        }
    });
}

fn message_event(role: &str, text: &str) -> pi_agent::AgentMessage {
    pi_agent::AgentMessage::Custom(json!({
        "role": role,
        "content": text,
        "timestamp": 1_700_000_000_000_i64,
    }))
}

// ============================================================================
// Tests
// ============================================================================

#[tokio::test]
async fn strict_init_order_event_order_and_blocking_results() {
    let fx = fixture(
        &[("a-recorder.ts", RECORDER), ("b-second.ts", SECOND)],
        TestRuntimeOptions::default(),
    )
    .await;

    // Strict order: nothing spawned yet; start() = ensure_ready + session_start.
    assert!(matches!(
        fx.binding.host().state().await,
        BridgeState::Detected
    ));
    fx.binding
        .start(SessionStartReason::Startup)
        .await
        .expect("sidecar boots");
    assert!(matches!(
        fx.binding.host().state().await,
        BridgeState::Ready
    ));

    // Registrations reported by the load phase.
    let commands = fx.binding.registered_commands();
    assert!(
        commands
            .iter()
            .any(|command| command.invocation_name == "mirror-report"),
        "extension command registered: {commands:?}"
    );

    // Forward a run's worth of session events (serially, via the queue).
    // message_end no longer rides the observer queue: it is a blocking hook
    // at the session's persistence boundary (F10); the session flushes the
    // queue first, exactly like this.
    let forwarder = fx.binding.forwarder();
    forwarder.enqueue_session_event(AgentSessionEvent::AgentStart);
    forwarder.enqueue_session_event(AgentSessionEvent::MessageStart {
        message: message_event("user", "hi"),
    });
    forwarder.flush().await;
    let _ = forwarder
        .emit_blocking_or_default(
            ExtensionEvent::MessageEnd {
                message: pi_ext_protocol::AgentMessage::Custom(json!({
                    "role": "user",
                    "content": [{"type": "text", "text": "hi"}],
                    "timestamp": 1,
                })),
            },
            None,
        )
        .await;
    forwarder.enqueue_session_event(AgentSessionEvent::AgentEnd {
        messages: Vec::new(),
        will_retry: false,
    });
    forwarder.flush().await;

    fx.wait_until("recorded event order", |fx| fx.obs("obs").len() >= 5)
        .await;
    let observed: Vec<String> = fx
        .obs("obs")
        .iter()
        .map(|data| data["e"].as_str().unwrap_or_default().to_string())
        .collect();
    assert_eq!(
        observed,
        [
            "session_start",
            "agent_start",
            "message_start",
            "message_end",
            "agent_end"
        ],
        "subscribed events arrive in emission order"
    );
    let start = &fx.obs("obs")[0];
    assert_eq!(start["reason"], "startup");

    // Blocking result, first contributing extension wins (load order).
    let result = forwarder
        .emit_blocking_or_default(
            ExtensionEvent::UserBash {
                command: "first things first".into(),
                exclude_from_context: false,
                cwd: "/tmp".into(),
            },
            None,
        )
        .await
        .expect("recorder contributed");
    assert_eq!(result["command"], "recorder-wins");
    assert!(result.get("excludeFromContext").is_none());

    // First extension declines -> second-in-load-order result is used.
    let result = forwarder
        .emit_blocking_or_default(
            ExtensionEvent::UserBash {
                command: "other".into(),
                exclude_from_context: false,
                cwd: "/tmp".into(),
            },
            None,
        )
        .await
        .expect("second contributed");
    assert_eq!(result["command"], "second-wins");
    assert_eq!(result["excludeFromContext"], true);

    // Null-vs-{}: an unsubscribed blocking kind never crosses the wire and
    // resolves to the pass-through default (None).
    let none = forwarder
        .emit_blocking_or_default(
            ExtensionEvent::Context {
                messages: Vec::new(),
            },
            None,
        )
        .await;
    assert!(none.is_none(), "unsubscribed context hook defaults");

    // Session mirror: host appends + sidecar optimistic appends reconcile.
    let host_count_before = fx.entries().len();
    fx.session().set_session_name("synced-name");
    // The name append rides the next dispatched event's session/sync.
    forwarder.enqueue_session_event(AgentSessionEvent::SessionInfoChanged {
        name: Some("synced-name".to_string()),
    });
    forwarder.flush().await;
    fx.execute_command("mirror-report", "")
        .await
        .expect("mirror-report runs");
    fx.wait_until("mirror report entry", |fx| {
        fx.obs("obs").iter().any(|data| data["e"] == "mirror")
    })
    .await;
    let report = fx
        .obs("obs")
        .into_iter()
        .rev()
        .find(|data| data["e"] == "mirror")
        .expect("mirror report");
    assert_eq!(report["name"], "synced-name");
    // host_count_before + session_info entry = entries visible to the
    // handler (its own obs entry lands after the report).
    assert_eq!(
        report["count"].as_u64().expect("count"),
        (host_count_before + 1) as u64,
        "mirror entry count matches the host session at report time"
    );

    fx.binding.shutdown().await;
}

#[tokio::test]
async fn compact_callbacks_override_and_cancel() {
    let fx = fixture(
        &[("compactor.ts", COMPACTOR)],
        TestRuntimeOptions::default(),
    )
    .await;
    seed_compactable_session(&fx.session());
    fx.binding
        .start(SessionStartReason::Startup)
        .await
        .expect("sidecar boots");

    // Cancelling handler: compact() rejects, callback fails with pi's exact
    // "Compaction cancelled" error.
    fx.execute_command("trigger-compact", "cancel-me")
        .await
        .expect("command runs");
    fx.wait_until("cancelled compact error", |fx| {
        !fx.obs("compact-error").is_empty()
    })
    .await;
    let error = fx.obs("compact-error").pop().expect("error callback");
    assert_eq!(error["message"], "Compaction cancelled");
    assert_eq!(error["tag"], "cancel-me");
    assert_eq!(fx.binding.forwarder().pending_compacts(), 0);

    // Extension-supplied compaction: no LLM call, fromExtension recorded,
    // pending ctx.compact() settles through the forwarded session_compact.
    fx.execute_command("trigger-compact", "override")
        .await
        .expect("command runs");
    fx.wait_until("override compact completion", |fx| {
        !fx.obs("compact-complete").is_empty()
    })
    .await;
    let done = fx.obs("compact-complete").pop().expect("completion");
    assert_eq!(done["summary"], "ext-summary");
    assert_eq!(done["tag"], "override");
    let compaction = fx
        .entries()
        .into_iter()
        .find_map(|entry| match entry {
            SessionEntry::Compaction {
                summary, from_hook, ..
            } => Some((summary, from_hook)),
            _ => None,
        })
        .expect("compaction entry persisted");
    assert_eq!(compaction.0, "ext-summary");
    assert_eq!(compaction.1, Some(true), "fromExtension recorded");
    assert_eq!(fx.binding.forwarder().pending_compacts(), 0);

    fx.binding.shutdown().await;
}

#[tokio::test]
async fn pending_compact_forces_forwarding_without_subscriptions() {
    // The summarization LLM call is canned; the extension subscribes to
    // NO compact events, yet its ctx.compact() callback must settle.
    let fx = fixture(
        &[("blind.ts", BLIND_COMPACTOR)],
        TestRuntimeOptions {
            stream_fn: Some(endless_summary_stream_fn()),
            ..Default::default()
        },
    )
    .await;
    seed_compactable_session(&fx.session());
    fx.binding
        .start(SessionStartReason::Startup)
        .await
        .expect("sidecar boots");

    fx.execute_command("trigger-compact", "blind")
        .await
        .expect("command runs");
    fx.wait_until("blind compact settles", |fx| {
        !fx.obs("compact-complete").is_empty() || !fx.obs("compact-error").is_empty()
    })
    .await;
    let completions = fx.obs("compact-complete");
    assert_eq!(
        completions.len(),
        1,
        "onComplete fired despite zero compact subscriptions: {:?}",
        fx.errors.lock()
    );
    assert_eq!(completions[0]["tag"], "blind");
    assert_eq!(fx.binding.forwarder().pending_compacts(), 0);

    fx.binding.shutdown().await;
}

#[tokio::test]
async fn reload_reinits_in_place_and_recreates_the_session() {
    let fx = fixture(&[("reloader.ts", RELOADER)], TestRuntimeOptions::default()).await;
    fx.binding
        .start(SessionStartReason::Startup)
        .await
        .expect("sidecar boots");
    fx.wait_until("initial session_start", |fx| !fx.obs("start").is_empty())
        .await;
    assert_eq!(fx.obs("start")[0]["gen"], 1);
    assert!(
        fx.binding
            .registered_commands()
            .iter()
            .any(|command| command.invocation_name == "gen-1")
    );
    let session_before = fx.session();
    let connection_before = fx
        .binding
        .host()
        .current_connection()
        .await
        .expect("live connection");

    fx.execute_command("do-reload", "").await.expect("reload");

    fx.wait_until("reload session_start", |fx| {
        fx.obs("start")
            .iter()
            .any(|data| data["reason"] == "reload")
    })
    .await;
    let reload_start = fx
        .obs("start")
        .into_iter()
        .find(|data| data["reason"] == "reload")
        .expect("reload start entry");
    assert_eq!(reload_start["gen"], 2, "factory re-ran in place");
    assert_eq!(
        reload_start["prev"],
        Value::Null,
        "reload session_start omits previousSessionFile (oracle types.ts:552)"
    );

    // Same process (in-place re-init), fresh registrations, new session.
    let connection_after = fx
        .binding
        .host()
        .current_connection()
        .await
        .expect("still the same connection");
    assert!(
        Arc::ptr_eq(&connection_before, &connection_after),
        "reload never restarts the sidecar process"
    );
    let commands: Vec<String> = fx
        .binding
        .registered_commands()
        .into_iter()
        .map(|command| command.invocation_name)
        .collect();
    assert!(commands.contains(&"gen-2".to_string()), "{commands:?}");
    assert!(
        !commands.contains(&"gen-1".to_string()),
        "stale registrations dropped: {commands:?}"
    );
    let session_after = fx.session();
    assert!(
        !session_before.ptr_eq(&session_after),
        "session recreated on reload"
    );

    fx.binding.shutdown().await;
}

#[tokio::test]
async fn crash_defers_the_single_respawn_to_the_next_turn_boundary() {
    let fx = fixture(
        &[("a-recorder.ts", RECORDER)],
        TestRuntimeOptions::default(),
    )
    .await;
    fx.binding
        .start(SessionStartReason::Startup)
        .await
        .expect("sidecar boots");

    let connection = fx
        .binding
        .host()
        .current_connection()
        .await
        .expect("live connection");
    connection.kill();
    connection.wait_exit().await;

    // Mid-turn events after the crash: dropped, no respawn.
    let forwarder = fx.binding.forwarder();
    forwarder.enqueue_session_event(AgentSessionEvent::MessageStart {
        message: message_event("user", "mid-turn"),
    });
    forwarder.enqueue_session_event(AgentSessionEvent::MessageEnd {
        message: message_event("user", "mid-turn"),
    });
    forwarder.flush().await;
    assert!(
        matches!(fx.binding.host().state().await, BridgeState::Dead(_)),
        "mid-turn events never respawn"
    );

    // A blocking non-boundary hook resolves to its pass-through default.
    let default = forwarder
        .emit_blocking_or_default(
            ExtensionEvent::MessageEnd {
                message: pi_ext_protocol::AgentMessage::Custom(json!({
                    "role": "user", "content": "x", "timestamp": 1,
                })),
            },
            None,
        )
        .await;
    assert!(default.is_none());
    assert!(matches!(
        fx.binding.host().state().await,
        BridgeState::Dead(_)
    ));

    // Next turn boundary: exactly one respawn, full replay (fresh load).
    forwarder.enqueue_session_event(AgentSessionEvent::AgentStart);
    forwarder.flush().await;
    assert!(
        matches!(fx.binding.host().state().await, BridgeState::Ready),
        "turn boundary respawned the sidecar"
    );
    let respawned = fx
        .binding
        .host()
        .current_connection()
        .await
        .expect("respawned connection");
    assert!(
        !Arc::ptr_eq(&connection, &respawned),
        "a fresh process serves the new turn"
    );
    // The replayed load re-reported registrations.
    assert!(
        fx.binding
            .registered_commands()
            .iter()
            .any(|command| command.invocation_name == "mirror-report")
    );

    fx.binding.shutdown().await;
}

// ============================================================================
// Lifecycle order + cancellation (P6-dispatch fixes)
// ============================================================================

/// Seed one user + one assistant message (the assistant append flushes the
/// persisted file so `switchSession` can re-open it); returns the user id.
fn seed_user_and_assistant(session: &AgentSession) -> String {
    session.with_session_manager_mut(|sm| {
        let user_id = sm
            .append_message(json!({
                "role": "user",
                "content": [{"type": "text", "text": "hello"}],
                "timestamp": 1
            }))
            .expect("seed user");
        sm.append_message(json!({
            "role": "assistant",
            "content": [{"type": "text", "text": "world"}],
            "api": "anthropic-messages",
            "provider": "anthropic",
            "model": "m",
            "usage": {"input": 1, "output": 1, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 2,
                       "cost": {"input": 0.0, "output": 0.0, "cacheRead": 0.0, "cacheWrite": 0.0, "total": 0.0}},
            "stopReason": "stop",
            "timestamp": 2
        }))
        .expect("seed assistant");
        user_id
    })
}

fn path_string(path: &std::path::Path) -> String {
    path.to_string_lossy().into_owned()
}

/// Real-sidecar lifecycle order for new/switch/fork driven through pi's own
/// command context actions: before-hook → session_shutdown (exact reason +
/// targetSessionFile) → session_start (exact reason + previousSessionFile),
/// observed by the extension across every replacement.
#[tokio::test]
async fn replacements_emit_shutdown_then_start_with_previous_file() {
    let fx = fixture(
        &[("lifecycle.ts", LIFECYCLE)],
        TestRuntimeOptions::default(),
    )
    .await;
    let user_id = seed_user_and_assistant(&fx.session());
    let f0 = fx.session().session_file().expect("persisted session file");
    fx.binding
        .start(SessionStartReason::Startup)
        .await
        .expect("sidecar boots");

    fx.execute_command("do-new", "").await.expect("do-new");
    let f1 = fx.session().session_file().expect("replacement file");
    assert_ne!(f0, f1, "new_session replaced the session");

    fx.execute_command("do-switch", &path_string(&f0))
        .await
        .expect("do-switch");
    assert_eq!(fx.session().session_file().as_deref(), Some(f0.as_path()));

    fx.execute_command("do-fork", &user_id)
        .await
        .expect("do-fork");
    let f2 = fx.session().session_file().expect("fork file");
    assert_ne!(f2, f0, "fork replaced the session");

    // Dump the global order once everything (including the fork start) landed.
    let expected = json!([
        { "e": "start", "reason": "startup", "prev": null },
        { "e": "before_switch", "reason": "new", "target": null },
        { "e": "shutdown", "reason": "new", "target": path_string(&f1) },
        { "e": "start", "reason": "new", "prev": path_string(&f0) },
        { "e": "before_switch", "reason": "resume", "target": path_string(&f0) },
        { "e": "shutdown", "reason": "resume", "target": path_string(&f0) },
        { "e": "start", "reason": "resume", "prev": path_string(&f1) },
        { "e": "before_fork", "entryId": user_id },
        { "e": "shutdown", "reason": "fork", "target": path_string(&f2) },
        { "e": "start", "reason": "fork", "prev": path_string(&f0) },
    ]);
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        fx.execute_command("report-order", "")
            .await
            .expect("report-order");
        fx.wait_until("order report entry", |fx| !fx.obs("order").is_empty())
            .await;
        let order = fx.obs("order").pop().expect("order dump")["order"].clone();
        if order == expected {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "lifecycle order mismatch;\n  got {order:#}\n  want {expected:#}\n  errors={:?}",
            fx.errors.lock(),
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    fx.binding.shutdown().await;
}

/// A cancelling before_switch hook (real extension, real sidecar) aborts the
/// replacement: `cancelled: true` returns to the extension, no
/// session_shutdown fires, and the session is untouched.
#[tokio::test]
async fn before_switch_cancel_blocks_the_replacement() {
    let fx = fixture(
        &[("canceller.ts", CANCELLER)],
        TestRuntimeOptions::default(),
    )
    .await;
    seed_user_and_assistant(&fx.session());
    let f0 = fx.session().session_file().expect("persisted session file");
    let session_before = fx.session();
    fx.binding
        .start(SessionStartReason::Startup)
        .await
        .expect("sidecar boots");

    fx.execute_command("try-switch", &path_string(&f0))
        .await
        .expect("try-switch");
    fx.wait_until("cancelled switch result", |fx| !fx.obs("done").is_empty())
        .await;
    assert_eq!(fx.obs("done").pop().expect("done")["cancelled"], true);
    assert!(
        fx.obs("shut").is_empty(),
        "no session_shutdown after a hook-cancelled switch"
    );
    assert!(
        fx.session().ptr_eq(&session_before),
        "session untouched after a hook-cancelled switch"
    );

    fx.binding.shutdown().await;
}

/// A cancel token fired while `waitForIdle` blocks on a streaming turn stops
/// the wait immediately; the turn itself keeps running.
#[tokio::test]
async fn wait_for_idle_stops_on_request_cancellation() {
    // Empty script: the gated stream fn answers with a fallback message once
    // the gate opens; the turn stays open until then.
    let script = Arc::new(parking_lot::Mutex::new(std::collections::VecDeque::new()));
    let gate = Arc::new(tokio::sync::Notify::new());
    let fx = fixture(
        &[("lifecycle.ts", LIFECYCLE)],
        TestRuntimeOptions {
            stream_fn: Some(gated_stream_fn(script, gate.clone())),
            ..Default::default()
        },
    )
    .await;
    fx.binding
        .start(SessionStartReason::Startup)
        .await
        .expect("sidecar boots");

    // Hold a turn open behind the gate.
    let session = fx.session();
    let turn = tokio::spawn({
        let session = session.clone();
        async move {
            let _ = session
                .prompt("hi", pi_coding_agent::session::PromptOptions::default())
                .await;
        }
    });
    let busy_deadline = Instant::now() + Duration::from_secs(10);
    while !session.is_streaming() {
        assert!(Instant::now() < busy_deadline, "turn never started");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // waitForIdle blocks while streaming...
    let token = CancellationToken::new();
    let wait = fx.actions.wait_for_idle(token.clone());
    tokio::pin!(wait);
    assert!(
        tokio::time::timeout(Duration::from_millis(300), wait.as_mut())
            .await
            .is_err(),
        "waitForIdle blocks while a turn streams"
    );
    // ...and a cancel frame's token stops it while the turn is still live.
    token.cancel();
    tokio::time::timeout(Duration::from_secs(2), wait)
        .await
        .expect("cancelled waitForIdle returns promptly");
    assert!(session.is_streaming(), "the turn itself keeps running");

    gate.notify_one();
    turn.await.expect("turn completes");
    fx.binding.shutdown().await;
}

/// A cancel token fired while the before_switch hook is still pending on the
/// real sidecar aborts the replacement (`cancelled: true`) without tearing
/// anything down — no partial lifecycle.
#[tokio::test]
async fn replacement_cancelled_mid_hook_leaves_the_session_intact() {
    let fx = fixture(&[("slow.ts", SLOW_GATE)], TestRuntimeOptions::default()).await;
    seed_user_and_assistant(&fx.session());
    let f0 = fx.session().session_file().expect("persisted session file");
    let session_before = fx.session();
    fx.binding
        .start(SessionStartReason::Startup)
        .await
        .expect("sidecar boots");

    let token = CancellationToken::new();
    let switch = fx.actions.switch_session(
        SwitchSessionParams {
            session_path: path_string(&f0),
            with_session_token: None,
        },
        token.clone(),
    );
    let started = Instant::now();
    let switch = tokio::spawn(switch);
    tokio::time::sleep(Duration::from_millis(300)).await;
    token.cancel();
    let result = tokio::time::timeout(Duration::from_secs(4), switch)
        .await
        .expect("cancel beats the 8s hook")
        .expect("switch task")
        .expect("switch result");
    assert!(result.cancelled, "cancelled replacement reports cancelled");
    assert!(
        started.elapsed() < Duration::from_secs(6),
        "cancel aborted the in-flight hook wait"
    );
    assert!(
        fx.session().ptr_eq(&session_before),
        "session untouched by a cancelled replacement"
    );
    assert!(fx.obs("shut").is_empty(), "no session_shutdown fired");

    // A pre-cancelled token stops the replacement before the hook even fires.
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    let result = fx
        .actions
        .new_session(pi_ext_protocol::NewSessionParams::default(), cancelled)
        .await
        .expect("pre-cancelled newSession");
    assert!(result.cancelled);
    assert!(fx.session().ptr_eq(&session_before));
    assert!(fx.obs("shut").is_empty());

    fx.binding.shutdown().await;
}

/// Host dialog provider that resolves only when its request token cancels.
#[derive(Default)]
struct HangingUi {
    saw_cancel: Arc<AtomicBool>,
}

impl ExtensionUiHost for HangingUi {
    fn select(
        &self,
        _title: String,
        _options: Vec<String>,
        opts: UiDialogOptions,
    ) -> BoxFuture<'static, Option<String>> {
        let saw_cancel = self.saw_cancel.clone();
        let signal = opts.signal;
        Box::pin(async move {
            let deadline = Instant::now() + Duration::from_secs(15);
            loop {
                if signal.as_ref().is_some_and(CancellationToken::is_cancelled) {
                    saw_cancel.store(true, Ordering::SeqCst);
                    return None;
                }
                if Instant::now() > deadline {
                    return Some("timed-out".to_string());
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
    }
    fn confirm(
        &self,
        _title: String,
        _message: String,
        _opts: UiDialogOptions,
    ) -> BoxFuture<'static, bool> {
        Box::pin(std::future::ready(false))
    }
    fn input(
        &self,
        _title: String,
        _placeholder: Option<String>,
        _opts: UiDialogOptions,
    ) -> BoxFuture<'static, Option<String>> {
        Box::pin(std::future::ready(None))
    }
    fn editor(
        &self,
        _title: String,
        _prefill: Option<String>,
    ) -> BoxFuture<'static, Option<String>> {
        Box::pin(std::future::ready(None))
    }
    fn notify(&self, _message: String, _notify_type: Option<NotifyType>) {}
    fn set_status(&self, _key: String, _text: Option<String>) {}
    fn set_widget(
        &self,
        _key: String,
        _lines: Option<Vec<String>>,
        _placement: Option<WidgetPlacement>,
    ) {
    }
    fn set_title(&self, _title: String) {}
    fn set_editor_text(&self, _text: String) {}
}

/// End-to-end cancel FRAME plumbing: the sidecar aborts its own in-flight
/// `ui/select` (AbortController → wire `cancel` frame) and the host's
/// per-request token fires.
#[tokio::test]
async fn sidecar_cancel_frame_cancels_the_host_request_token() {
    let fx = fixture_with_ui(
        &[("aborter.ts", DIALOG_ABORTER)],
        TestRuntimeOptions::default(),
        true,
    )
    .await;
    let ui = Arc::new(HangingUi::default());
    fx.binding.bind_ui(ui.clone());
    fx.binding
        .start(SessionStartReason::Startup)
        .await
        .expect("sidecar boots");

    fx.execute_command("dialog-abort", "")
        .await
        .expect("dialog-abort");
    fx.wait_until("aborted dialog result", |fx| !fx.obs("dialog").is_empty())
        .await;
    assert_eq!(
        fx.obs("dialog").pop().expect("dialog entry")["choice"],
        Value::Null,
        "extension observed the aborted select as undefined"
    );
    let cancel_deadline = Instant::now() + Duration::from_secs(5);
    while !ui.saw_cancel.load(Ordering::SeqCst) {
        assert!(
            Instant::now() < cancel_deadline,
            "the wire cancel frame never cancelled the host request token"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    fx.binding.shutdown().await;
}
