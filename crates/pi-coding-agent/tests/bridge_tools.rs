//! Extension tool bridging (Phase 6 commit C7), integration-tested against
//! the REAL Bun sidecar (`sidecar/src/main.ts`) running pi's actual
//! loader/runner with UNMODIFIED corpus extensions
//! (`tests/fixtures/corpus/*`, vendored byte-identical from
//! `.references/pi/packages/coding-agent/examples/extensions/`).
//!
//! Covered contracts:
//! - registrations cross verbatim: name/label/description/parameters are the
//!   corpus strings/schemas, promptSnippet/promptGuidelines reach the
//!   session prompt metadata (I9);
//! - `terminate: true` from a corpus tool ends the batch without another
//!   provider call (structured-output);
//! - late registrations (session_start handlers, slash commands, and
//!   mid-tool-execution `pi.registerTool`) rebuild the host registry via
//!   the `action/refreshTools` snapshot; newly appeared tools become active;
//! - `pi.setActiveTools` during tool execution is observable on the SAME
//!   run's next provider call (barrier ordering) and additive growth lands
//!   on the tool result as `addedToolNames` (oracle regression 6162);
//! - streamed partials reach `tool_execution_update`; a throwing extension
//!   tool becomes an error tool result, not a crash;
//! - an extension tool shadows a built-in by name, in place.

#![cfg(unix)]

mod common;

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use pi_agent::StreamFn;
use pi_ai::{
    AssistantMessage, Content, Context, Model, StopReason, TextContent, ToolCall,
    create_assistant_message_event_stream,
};
use pi_coding_agent::extensions::binding::{BindOptions, ExtensionBinding, SessionHostActions};
use pi_coding_agent::extensions::{BunEnvironment, LauncherSource, SidecarLauncher, resolve_bun};
use pi_coding_agent::session::AgentSession;
use pi_coding_agent::session_types::SessionEntry;
use pi_ext_protocol::{CommandExecuteParams, ExtensionError, Request};
use serde_json::{Value, json};

use common::{TestRuntime, TestRuntimeOptions, assistant_text_message, make_runtime};

use pi_coding_agent::extension_bridge::SessionStartReason;

// ============================================================================
// Harness (same shape as bridge_events.rs)
// ============================================================================

fn sidecar_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../sidecar")
        .canonicalize()
        .expect("sidecar package present")
}

fn corpus(name: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/corpus")
        .join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("corpus {name}: {error}"))
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
    #[allow(dead_code)]
    runtime: TestRuntime,
    binding: Arc<ExtensionBinding>,
    #[allow(dead_code)]
    actions: Arc<SessionHostActions>,
    errors: Arc<Mutex<Vec<ExtensionError>>>,
}

impl Fixture {
    fn session(&self) -> AgentSession {
        self.binding.session()
    }

    fn entries(&self) -> Vec<SessionEntry> {
        self.session().with_session_manager(|sm| sm.get_entries())
    }

    /// Persisted toolResult messages as raw JSON values, in order.
    fn tool_results(&self) -> Vec<Value> {
        self.entries()
            .into_iter()
            .filter_map(|entry| match entry {
                SessionEntry::Message { message, .. }
                    if message.get("role").and_then(Value::as_str) == Some("toolResult") =>
                {
                    Some(message)
                }
                _ => None,
            })
            .collect()
    }

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

    /// Errors except the known C6 placeholder-theme one (default
    /// `StateOverlay.theme` is not a real theme JSON; interactive mode
    /// supplies it — dispatch report, handoff note).
    fn real_errors(&self) -> Vec<ExtensionError> {
        self.errors
            .lock()
            .iter()
            .filter(|error| error.event != "theme")
            .cloned()
            .collect()
    }
}

/// Build a persisted-session runtime, write the extensions, bind the REAL
/// sidecar, and start it (spawn + init + session_start).
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
    let errors: Arc<Mutex<Vec<ExtensionError>>> = Arc::default();
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
    bind_options.runtime = Some(runtime.runtime.clone());
    let binding = pi_coding_agent::extensions::binding::bind_extensions(&session, bind_options)
        .expect("canonical extension paths")
        .expect("extensions discovered");
    actions.attach(&binding);
    actions.attach_runtime(runtime.runtime.clone());
    binding
        .start(SessionStartReason::Startup)
        .await
        .expect("sidecar boots");

    Fixture {
        runtime,
        binding,
        actions,
        errors,
    }
}

fn tool_call_message(model: &Model, id: &str, name: &str, args: Value) -> AssistantMessage {
    let mut message = assistant_text_message(model, "");
    let arguments = match args {
        Value::Object(map) => map,
        _ => serde_json::Map::new(),
    };
    message.content = vec![Content::ToolCall(ToolCall {
        id: id.to_string(),
        name: name.to_string(),
        arguments,
        thought_signature: None,
    })];
    message.stop_reason = StopReason::ToolUse;
    message
}

/// Scripted stream fn that also records each provider call's tool names
/// (sorted), like pi's regression harness.
fn capturing_stream_fn(
    script: Arc<Mutex<VecDeque<AssistantMessage>>>,
    tool_names_per_call: Arc<Mutex<Vec<Vec<String>>>>,
) -> StreamFn {
    Arc::new(move |model: Model, context: Context, _options| {
        let script = script.clone();
        let tool_names_per_call = tool_names_per_call.clone();
        Box::pin(async move {
            let mut names: Vec<String> =
                context.tools.iter().map(|tool| tool.name.clone()).collect();
            names.sort();
            tool_names_per_call.lock().push(names);
            let stream = create_assistant_message_event_stream();
            let message = script.lock().pop_front().unwrap_or_else(|| {
                let mut error = assistant_text_message(&model, "");
                error.stop_reason = StopReason::Error;
                error.error_message = Some("script exhausted".to_string());
                error
            });
            stream.push(pi_ai::AssistantMessageEvent::Done {
                reason: message.stop_reason,
                message,
            });
            stream
        })
    })
}

fn text_of(message: &Value) -> String {
    message["content"]
        .as_array()
        .map(|blocks| {
            blocks
                .iter()
                .filter_map(|block| block.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default()
}

// ============================================================================
// Inline fixture extensions (unmodified pi extension API)
// ============================================================================

/// Oracle regression 6162 fixture, verbatim semantics: additive growth via
/// `pi.setActiveTools` during execute + a replacement variant.
const LOAD_MORE: &str = r#"
import { Type } from "typebox";
export default function (pi) {
    pi.registerTool({
        name: "load_more_tools",
        label: "Load More Tools",
        description: "Load more tools",
        parameters: Type.Object({}),
        execute: async () => {
            pi.setActiveTools([...pi.getActiveTools(), "after_load"]);
            return { content: [{ type: "text", text: "loaded" }], details: {} };
        },
    });
    pi.registerTool({
        name: "switch_tools",
        label: "Switch Tools",
        description: "Switch the active extension tool set",
        parameters: Type.Object({}),
        execute: async () => {
            pi.setActiveTools(["after_load"]);
            return { content: [{ type: "text", text: "switched" }], details: {} };
        },
    });
    pi.registerTool({
        name: "after_load",
        label: "After Load",
        description: "Tool available after loading",
        parameters: Type.Object({}),
        execute: async () => ({ content: [{ type: "text", text: "after" }], details: {} }),
    });
}
"#;

/// Streams two partials, then finishes; plus an always-throwing tool.
const STREAMER: &str = r#"
import { Type } from "typebox";
export default function (pi) {
    pi.registerTool({
        name: "streamer",
        label: "Streamer",
        description: "Streams partial output",
        parameters: Type.Object({}),
        execute: async (_id, _params, _signal, onUpdate) => {
            onUpdate?.({ content: [{ type: "text", text: "one" }], details: {} });
            onUpdate?.({ content: [{ type: "text", text: "one two" }], details: {} });
            return { content: [{ type: "text", text: "one two three" }], details: {} };
        },
    });
    pi.registerTool({
        name: "exploder",
        label: "Exploder",
        description: "Always throws",
        parameters: Type.Object({}),
        execute: async () => { throw new Error("boom from extension"); },
    });
}
"#;

/// Shadows the built-in `read` tool by name.
const SHADOW_READ: &str = r#"
import { Type } from "typebox";
export default function (pi) {
    pi.registerTool({
        name: "read",
        label: "Read Override",
        description: "Extension-owned read",
        parameters: Type.Object({ path: Type.String() }),
        execute: async () => ({ content: [{ type: "text", text: "shadowed" }], details: {} }),
    });
}
"#;

/// Sleeps until its AbortSignal fires; records the abort as an entry.
const SLEEPER: &str = r#"
import { Type } from "typebox";
export default function (pi) {
    pi.registerTool({
        name: "sleeper",
        label: "Sleeper",
        description: "Sleeps until aborted",
        parameters: Type.Object({}),
        execute: async (_id, _params, signal) => {
            await new Promise((resolve, reject) => {
                const timer = setTimeout(resolve, 30000);
                signal?.addEventListener("abort", () => {
                    clearTimeout(timer);
                    pi.appendEntry("aborted", {});
                    reject(new Error("sleeper aborted"));
                });
            });
            return { content: [{ type: "text", text: "slept" }], details: {} };
        },
    });
}
"#;

// ============================================================================
// Tests
// ============================================================================

/// Corpus structured-output.ts: registration crosses verbatim; `terminate:
/// true` ends the batch after ONE provider call.
#[tokio::test]
async fn corpus_structured_output_registers_verbatim_and_terminates() {
    let script: Arc<Mutex<VecDeque<AssistantMessage>>> = Arc::default();
    let calls: Arc<Mutex<Vec<Vec<String>>>> = Arc::default();
    let stream_fn = capturing_stream_fn(script.clone(), calls.clone());
    let structured = corpus("structured-output.ts");
    let fx = fixture(
        &[("structured-output.ts", &structured)],
        TestRuntimeOptions {
            stream_fn: Some(stream_fn),
            ..Default::default()
        },
    )
    .await;

    // Registration parity: exact corpus strings + schema (I9).
    let registrations = fx.binding.registrations();
    let tool = registrations
        .tools
        .iter()
        .find(|tool| tool.name == "structured_output")
        .expect("corpus tool registered");
    assert_eq!(tool.label, "Structured Output");
    assert_eq!(
        tool.description,
        "Return a final structured answer. Use this as your last action when the user asks for structured output or a machine-readable summary.",
    );
    assert_eq!(
        tool.prompt_snippet.as_deref(),
        Some("Emit a final structured answer as a terminating tool result"),
    );
    assert_eq!(
        tool.parameters["properties"]["headline"]["description"],
        json!("Short title for the result"),
    );
    assert_eq!(
        tool.parameters["required"],
        json!(["headline", "summary", "actionItems"]),
    );
    assert!(tool.has_render_result && !tool.has_render_call);

    // Session registry: registered, active (first snapshot = include-all),
    // prompt metadata attached.
    let session = fx.session();
    let all = session.get_all_tools();
    let info = all
        .iter()
        .find(|info| info.name == "structured_output")
        .expect("in session registry");
    assert_eq!(info.source, "extension");
    assert_eq!(
        info.prompt_guidelines,
        vec![
            "Use structured_output as your final action when the user asks for structured output, JSON-like output, or a machine-readable summary.".to_string(),
            "After calling structured_output, do not emit another assistant response in the same turn.".to_string(),
        ],
    );
    assert!(
        session
            .get_active_tool_names()
            .contains(&"structured_output".to_string()),
    );

    // One scripted response ONLY: terminate must prevent a follow-up call.
    script.lock().push_back(tool_call_message(
        &fx.runtime.model,
        "call-1",
        "structured_output",
        json!({
            "headline": "Ship it",
            "summary": "All green.",
            "actionItems": ["merge", "tag"],
        }),
    ));
    session
        .prompt("give me structured output", Default::default())
        .await
        .expect("prompt runs");

    assert_eq!(calls.lock().len(), 1, "terminate stops the batch");
    let results = fx.tool_results();
    assert_eq!(results.len(), 1);
    assert_eq!(text_of(&results[0]), "Saved structured output: Ship it");
    assert_eq!(results[0]["details"]["headline"], json!("Ship it"));
    assert_eq!(
        results[0]["details"]["actionItems"],
        json!(["merge", "tag"])
    );
    assert_eq!(results[0]["isError"], json!(false));
    assert!(fx.real_errors().is_empty(), "{:?}", fx.real_errors());
}

/// Corpus dynamic-tools.ts: a session_start registration and a slash-command
/// registration both reach the host registry and become active; the loop can
/// call them.
#[tokio::test]
async fn corpus_dynamic_tools_late_registrations_become_active() {
    let script: Arc<Mutex<VecDeque<AssistantMessage>>> = Arc::default();
    let calls: Arc<Mutex<Vec<Vec<String>>>> = Arc::default();
    let stream_fn = capturing_stream_fn(script.clone(), calls.clone());
    let dynamic = corpus("dynamic-tools.ts");
    let fx = fixture(
        &[("dynamic-tools.ts", &dynamic)],
        TestRuntimeOptions {
            stream_fn: Some(stream_fn),
            ..Default::default()
        },
    )
    .await;
    let session = fx.session();

    // session_start handler registers echo_session AFTER `initialized`:
    // arrives via the refreshTools registration snapshot.
    fx.wait_until("echo_session registered", |fx| {
        fx.session()
            .get_all_tools()
            .iter()
            .any(|tool| tool.name == "echo_session")
    })
    .await;
    assert!(
        session
            .get_active_tool_names()
            .contains(&"echo_session".to_string()),
        "newly appeared tool becomes active: {:?}",
        session.get_active_tool_names(),
    );

    // Slash-command registration (/add-echo-tool echo_two).
    fx.execute_command("add-echo-tool", "echo_two")
        .await
        .expect("command runs");
    fx.wait_until("echo_two registered", |fx| {
        fx.session()
            .get_all_tools()
            .iter()
            .any(|tool| tool.name == "echo_two")
    })
    .await;
    assert!(
        session
            .get_active_tool_names()
            .contains(&"echo_two".to_string()),
    );

    // Drive both echo tools through the real loop.
    {
        let mut script = script.lock();
        script.push_back(tool_call_message(
            &fx.runtime.model,
            "call-1",
            "echo_session",
            json!({"message": "hi"}),
        ));
        script.push_back(tool_call_message(
            &fx.runtime.model,
            "call-2",
            "echo_two",
            json!({"message": "again"}),
        ));
        script.push_back(assistant_text_message(&fx.runtime.model, "done"));
    }
    session
        .prompt("echo twice", Default::default())
        .await
        .expect("prompt runs");

    let results = fx.tool_results();
    assert_eq!(results.len(), 2, "{results:?}");
    assert_eq!(text_of(&results[0]), "[session] hi");
    assert_eq!(text_of(&results[1]), "[echo_two] again");
    assert_eq!(
        results[0]["details"],
        json!({"tool": "echo_session", "prefix": "[session] "})
    );
    assert!(fx.real_errors().is_empty(), "{:?}", fx.real_errors());
}

/// Oracle regression 6162: `pi.setActiveTools` growth during execute lands
/// as `addedToolNames` on the tool result, the added tool is active, and the
/// SAME run's next provider call sees it (barrier ordering). The replacement
/// variant swaps the active set for the next call without addedToolNames.
#[tokio::test]
async fn mid_execution_active_tool_changes_reach_the_same_run() {
    let script: Arc<Mutex<VecDeque<AssistantMessage>>> = Arc::default();
    let calls: Arc<Mutex<Vec<Vec<String>>>> = Arc::default();
    let stream_fn = capturing_stream_fn(script.clone(), calls.clone());
    let fx = fixture(
        &[("load-more.ts", LOAD_MORE)],
        TestRuntimeOptions {
            stream_fn: Some(stream_fn),
            ..Default::default()
        },
    )
    .await;
    let session = fx.session();

    // Additive growth: active = load_more_tools only, tool adds after_load.
    session.set_active_tools_by_name(vec!["load_more_tools".to_string()]);
    assert_eq!(session.get_active_tool_names(), vec!["load_more_tools"]);
    {
        let mut script = script.lock();
        script.push_back(tool_call_message(
            &fx.runtime.model,
            "call-1",
            "load_more_tools",
            json!({}),
        ));
        script.push_back(assistant_text_message(&fx.runtime.model, "done"));
    }
    session
        .prompt("load", Default::default())
        .await
        .expect("prompt runs");

    assert_eq!(
        session.get_active_tool_names(),
        vec!["load_more_tools", "after_load"],
    );
    {
        let calls = calls.lock();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0], vec!["load_more_tools"]);
        assert_eq!(
            calls[1],
            vec!["after_load", "load_more_tools"],
            "the SAME run's next provider call sees the grown tool set",
        );
    }
    let results = fx.tool_results();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["addedToolNames"], json!(["after_load"]));

    // Replacement variant: no addedToolNames, next call sees the swap.
    session.set_active_tools_by_name(vec!["switch_tools".to_string()]);
    calls.lock().clear();
    {
        let mut script = script.lock();
        script.push_back(tool_call_message(
            &fx.runtime.model,
            "call-2",
            "switch_tools",
            json!({}),
        ));
        script.push_back(assistant_text_message(&fx.runtime.model, "done"));
    }
    session
        .prompt("switch", Default::default())
        .await
        .expect("prompt runs");

    assert_eq!(session.get_active_tool_names(), vec!["after_load"]);
    {
        let calls = calls.lock();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0], vec!["switch_tools"]);
        assert_eq!(calls[1], vec!["after_load"]);
    }
    let results = fx.tool_results();
    assert_eq!(results.len(), 2);
    assert!(
        results[1].get("addedToolNames").is_none(),
        "replacement is not additive: {:?}",
        results[1],
    );
    assert!(fx.real_errors().is_empty(), "{:?}", fx.real_errors());
}

/// Streamed partials surface as tool_execution_update events; a throwing
/// extension tool becomes an error tool result.
#[tokio::test]
async fn streamed_updates_and_thrown_errors() {
    let script: Arc<Mutex<VecDeque<AssistantMessage>>> = Arc::default();
    let calls: Arc<Mutex<Vec<Vec<String>>>> = Arc::default();
    let stream_fn = capturing_stream_fn(script.clone(), calls.clone());
    let fx = fixture(
        &[("streamer.ts", STREAMER)],
        TestRuntimeOptions {
            stream_fn: Some(stream_fn),
            ..Default::default()
        },
    )
    .await;
    let session = fx.session();

    let updates: Arc<Mutex<Vec<String>>> = Arc::default();
    let _unsubscribe = session.subscribe(Arc::new({
        let updates = updates.clone();
        move |event| {
            if let pi_coding_agent::session::events::AgentSessionEvent::ToolExecutionUpdate {
                partial_result,
                ..
            } = event
            {
                let text = partial_result
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        Content::Text(TextContent { text, .. }) => Some(text.to_string()),
                        _ => None,
                    })
                    .collect::<String>();
                updates.lock().push(text);
            }
        }
    }));

    {
        let mut script = script.lock();
        script.push_back(tool_call_message(
            &fx.runtime.model,
            "call-1",
            "streamer",
            json!({}),
        ));
        script.push_back(tool_call_message(
            &fx.runtime.model,
            "call-2",
            "exploder",
            json!({}),
        ));
        script.push_back(assistant_text_message(&fx.runtime.model, "done"));
    }
    session
        .prompt("stream then explode", Default::default())
        .await
        .expect("prompt runs");

    assert_eq!(
        *updates.lock(),
        vec!["one".to_string(), "one two".to_string()]
    );
    let results = fx.tool_results();
    assert_eq!(results.len(), 2, "{results:?}");
    assert_eq!(text_of(&results[0]), "one two three");
    assert_eq!(results[0]["isError"], json!(false));
    assert_eq!(results[1]["isError"], json!(true));
    assert!(
        text_of(&results[1]).contains("boom from extension"),
        "thrown message relayed: {:?}",
        results[1],
    );
}

/// An extension tool shadows a built-in by name: same registry position, the
/// extension definition wins, and execution routes to the sidecar.
#[tokio::test]
async fn extension_tool_shadows_builtin_in_place() {
    let script: Arc<Mutex<VecDeque<AssistantMessage>>> = Arc::default();
    let calls: Arc<Mutex<Vec<Vec<String>>>> = Arc::default();
    let stream_fn = capturing_stream_fn(script.clone(), calls.clone());
    let fx = fixture(
        &[("shadow-read.ts", SHADOW_READ)],
        TestRuntimeOptions {
            stream_fn: Some(stream_fn),
            ..Default::default()
        },
    )
    .await;
    let session = fx.session();

    let all = session.get_all_tools();
    let read_positions: Vec<usize> = all
        .iter()
        .enumerate()
        .filter(|(_, tool)| tool.name == "read")
        .map(|(index, _)| index)
        .collect();
    assert_eq!(read_positions, vec![0], "one read, at the built-in slot");
    assert_eq!(all[0].source, "extension");
    assert_eq!(all[0].description, "Extension-owned read");

    script.lock().push_back(tool_call_message(
        &fx.runtime.model,
        "call-1",
        "read",
        json!({"path": "whatever.txt"}),
    ));
    script
        .lock()
        .push_back(assistant_text_message(&fx.runtime.model, "done"));
    session
        .prompt("read something", Default::default())
        .await
        .expect("prompt runs");

    let results = fx.tool_results();
    assert_eq!(results.len(), 1);
    assert_eq!(text_of(&results[0]), "shadowed");
    assert!(fx.real_errors().is_empty(), "{:?}", fx.real_errors());
}

/// Aborting the run cancels the in-flight `tool/execute` via a wire cancel
/// frame: the extension tool's AbortSignal fires, the call fails as an
/// error tool result, and nothing hangs.
#[tokio::test]
async fn abort_cancels_an_in_flight_extension_tool() {
    let script: Arc<Mutex<VecDeque<AssistantMessage>>> = Arc::default();
    let calls: Arc<Mutex<Vec<Vec<String>>>> = Arc::default();
    let stream_fn = capturing_stream_fn(script.clone(), calls.clone());
    let fx = fixture(
        &[("sleeper.ts", SLEEPER)],
        TestRuntimeOptions {
            stream_fn: Some(stream_fn),
            ..Default::default()
        },
    )
    .await;
    let session = fx.session();

    script.lock().push_back(tool_call_message(
        &fx.runtime.model,
        "call-1",
        "sleeper",
        json!({}),
    ));
    let prompt_task = tokio::spawn({
        let session = session.clone();
        async move { session.prompt("sleep", Default::default()).await }
    });

    // Wait until the tool call reached the sidecar (execution started).
    fx.wait_until("tool execution started", |fx| {
        fx.entries().iter().any(|entry| match entry {
            SessionEntry::Message { message, .. } => {
                message.get("role").and_then(Value::as_str) == Some("assistant")
            }
            _ => false,
        })
    })
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    session.abort().await;
    let result = tokio::time::timeout(Duration::from_secs(10), prompt_task)
        .await
        .expect("abort unblocks the prompt")
        .expect("prompt task not panicked");
    result.expect("aborted prompt resolves without error");

    // The sidecar observed the abort (extension recorded it).
    fx.wait_until("extension observed the abort", |fx| {
        fx.entries().iter().any(|entry| {
            matches!(entry, SessionEntry::Custom { custom_type, .. } if custom_type == "aborted")
        })
    })
    .await;
    // Sidecar still alive: a follow-up command round-trips.
    assert!(fx.binding.host().current_connection().await.is_some());
}
