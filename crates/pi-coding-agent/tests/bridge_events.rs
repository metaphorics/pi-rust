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
use std::time::{Duration, Instant};

use pi_coding_agent::extension_bridge::SessionStartReason;
use pi_coding_agent::extensions::binding::{BindOptions, ExtensionBinding, SessionHostActions};
use pi_coding_agent::extensions::{
    BridgeState, BunEnvironment, LauncherSource, SidecarLauncher, resolve_bun,
};
use pi_coding_agent::session::AgentSession;
use pi_coding_agent::session::events::AgentSessionEvent;
use pi_coding_agent::session_types::SessionEntry;
use pi_ext_protocol::{CommandExecuteParams, ExtensionError, ExtensionEvent, Request};
use serde_json::{Value, json};

use common::{TestRuntime, TestRuntimeOptions, assistant_text_message, make_runtime};

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
    pi.on("session_start", (ev) => pi.appendEntry("start", { reason: ev.reason, gen }));
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
    let binding = pi_coding_agent::extensions::binding::bind_extensions(
        &session,
        BindOptions::new(
            paths,
            LauncherSource::Resolved(real_launcher()),
            cwd,
            agent_dir,
            session_dir,
            Arc::new(move |error| sink_errors.lock().push(error)),
            actions.clone(),
        ),
    )
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
    let forwarder = fx.binding.forwarder();
    forwarder.enqueue_session_event(AgentSessionEvent::AgentStart);
    forwarder.enqueue_session_event(AgentSessionEvent::MessageStart {
        message: message_event("user", "hi"),
    });
    forwarder.enqueue_session_event(AgentSessionEvent::MessageEnd {
        message: message_event("user", "hi"),
    });
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
