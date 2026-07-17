//! Extension providers, slash-command dispatch, and CLI flags (Phase 6
//! commit C7, providers/commands half) — integration-tested against the
//! REAL Bun sidecar running pi's actual loader/runner with UNMODIFIED
//! corpus extensions (`tests/fixtures/corpus/*`).
//!
//! Covered contracts:
//! - `pi.registerProvider` mutates the HOST model catalog at runtime (F9):
//!   models, baseUrl, api id, cost — no Value loss — and auth (apiKey env
//!   template + custom headers) resolves through the host registry;
//! - a provider with `streamSimple` streams through the sidecar: the full
//!   agent loop consumes `provider/event` frames wire-identically and the
//!   final assistant message persists;
//! - corpus custom-provider-anthropic loads unmodified (its real
//!   `@anthropic-ai/sdk` dependency resolves), registers verbatim, and a
//!   pre-cancelled stream aborts cleanly without touching the network;
//! - `prompt("/name args")` executes extension commands (corpus
//!   commands.ts / dynamic-tools.ts) without starting an LLM turn; unknown
//!   slash text still reaches the provider; a throwing handler surfaces as
//!   `command:<name>` ExtensionError (oracle parity);
//! - `registerFlag` registrations carry provenance and `getFlag` serves the
//!   host-supplied CLI values.

#![cfg(unix)]

mod common;

use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use pi_agent::{CancellationToken, StreamFn};
use pi_ai::{AssistantMessage, Context, Model, StopReason, create_assistant_message_event_stream};
use pi_coding_agent::extension_bridge::SessionStartReason;
use pi_coding_agent::extensions::binding::{BindOptions, ExtensionBinding, SessionHostActions};
use pi_coding_agent::extensions::provider::{ExtensionProviders, extension_stream_fn};
use pi_coding_agent::extensions::{BunEnvironment, LauncherSource, SidecarLauncher, resolve_bun};
use pi_coding_agent::session::AgentSession;
use pi_coding_agent::session_types::SessionEntry;
use pi_ext_protocol::{ExtensionError, FlagValue, Request, ToolExecuteParams};
use serde_json::{Value, json};

use common::{TestRuntime, TestRuntimeOptions, assistant_text_message, make_runtime};

// ============================================================================
// Harness
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
    providers: Arc<ExtensionProviders>,
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

    fn real_errors(&self) -> Vec<ExtensionError> {
        self.errors
            .lock()
            .iter()
            .filter(|error| error.event != "theme")
            .cloned()
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
}
#[derive(Default)]
struct FixtureOptions {
    runtime_options: TestRuntimeOptions,
    flag_values: BTreeMap<String, FlagValue>,
    providers: Option<Arc<ExtensionProviders>>,
    envs: Vec<(String, String)>,
}

/// `extensions` entries: (file name, source) written to a temp dir, or an
/// absolute pre-existing path when the source is `None`.
async fn fixture(extensions: &[(&str, Option<&str>)], options: FixtureOptions) -> Fixture {
    let runtime = make_runtime(TestRuntimeOptions {
        persisted: true,
        with_auth: true,
        ..options.runtime_options
    })
    .await;
    let ext_dir = runtime.tmp.path().join("extensions");
    std::fs::create_dir_all(&ext_dir).expect("ext dir");
    let mut paths = Vec::new();
    for (name, source) in extensions {
        match source {
            Some(source) => {
                let path = ext_dir.join(name);
                std::fs::write(&path, source).expect("fixture extension");
                paths.push(path);
            }
            None => paths.push(PathBuf::from(name)),
        }
    }
    let errors: Arc<Mutex<Vec<ExtensionError>>> = Arc::default();
    let sink_errors = errors.clone();
    let actions = SessionHostActions::new();
    let session = runtime.runtime.session();
    let cwd = runtime.tmp.path().join("project");
    let agent_dir = runtime.tmp.path().join("agent");
    let session_dir = runtime.tmp.path().join("sessions");
    let mut launcher = real_launcher();
    launcher.envs = options
        .envs
        .into_iter()
        .map(|(key, value)| (key.into(), value.into()))
        .collect();
    let mut bind_options = BindOptions::new(
        paths,
        LauncherSource::Resolved(launcher),
        cwd,
        agent_dir,
        session_dir,
        Arc::new(move |error| sink_errors.lock().push(error)),
        actions.clone(),
    );
    bind_options.runtime = Some(runtime.runtime.clone());
    bind_options.flag_values = options.flag_values;
    let providers = options.providers.unwrap_or_default();
    bind_options.providers = Some(providers.clone());
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
        providers,
    }
}

/// Scripted inner stream fn counting its calls (models NOT owned by a
/// sidecar provider land here).
fn counting_inner(
    script: Arc<Mutex<VecDeque<AssistantMessage>>>,
    calls: Arc<Mutex<usize>>,
) -> StreamFn {
    Arc::new(move |model: Model, _context, _options| {
        let script = script.clone();
        let calls = calls.clone();
        Box::pin(async move {
            *calls.lock() += 1;
            let stream = create_assistant_message_event_stream();
            let message = script.lock().pop_front().unwrap_or_else(|| {
                let mut error = assistant_text_message(&model, "");
                error.stop_reason = StopReason::Error;
                error.error_message = Some("inner script exhausted".to_string());
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

// ============================================================================
// Inline fixture extensions (unmodified pi extension API)
// ============================================================================

/// Registers a data+streamSimple provider whose stream is fully scripted
/// (no network): events cross the bridge wire-identically.
const SCRIPTED_PROVIDER: &str = r#"
import { createAssistantMessageEventStream } from "@earendil-works/pi-ai/compat";
export default function (pi) {
    pi.registerProvider("scripted", {
        baseUrl: "https://scripted.invalid",
        apiKey: "$SCRIPTED_KEY",
        api: "scripted-api",
        headers: { "x-scripted": "yes" },
        models: [
            {
                id: "scripted-1",
                name: "Scripted One",
                reasoning: false,
                input: ["text"],
                cost: { input: 1.5, output: 6, cacheRead: 0.25, cacheWrite: 2 },
                contextWindow: 100000,
                maxTokens: 4096,
            },
        ],
        streamSimple: (model, context, options) => {
            const stream = createAssistantMessageEventStream();
            const usage = {
                input: 3, output: 5, cacheRead: 0, cacheWrite: 0, totalTokens: 8,
                cost: { input: 0.1, output: 0.2, cacheRead: 0, cacheWrite: 0, total: 0.3 },
            };
            const base = {
                role: "assistant",
                api: model.api,
                provider: model.provider,
                model: model.id,
                usage,
                stopReason: "stop",
                timestamp: 1700000000000,
            };
            const echoed = (options?.apiKey ?? "<none>");
            const text = `scripted reply key=${echoed}`;
            stream.push({ type: "start", partial: { ...base, content: [] } });
            stream.push({
                type: "text_delta",
                contentIndex: 0,
                delta: text,
                partial: { ...base, content: [{ type: "text", text }] },
            });
            stream.push({
                type: "done",
                reason: "stop",
                message: { ...base, content: [{ type: "text", text }] },
            });
            return stream;
        },
    });
}
"#;

/// A slash command whose handler always throws.
const BOOM_CMD: &str = r#"
export default function (pi) {
    pi.registerCommand("boom", {
        description: "always throws",
        handler: async () => { throw new Error("boom handler failed"); },
    });
}
"#;

/// Registers a CLI flag and records the value the host supplied.
const FLAGGED: &str = r#"
export default function (pi) {
    pi.registerFlag("demo-flag", { description: "demo flag", type: "string" });
    pi.on("session_start", () => {
        pi.appendEntry("flag", { value: pi.getFlag("demo-flag") ?? null });
    });
}
"#;

// ============================================================================
// Tests
// ============================================================================

/// `pi.registerProvider` with `streamSimple`: host catalog + auth mutate at
/// runtime, and the FULL agent loop streams through the sidecar.
#[tokio::test]
async fn custom_provider_streams_through_the_agent_loop() {
    // SAFETY: test-process env; the value is constant across threads.
    unsafe { std::env::set_var("SCRIPTED_KEY", "sk-scripted") };
    let script: Arc<Mutex<VecDeque<AssistantMessage>>> = Arc::default();
    let inner_calls: Arc<Mutex<usize>> = Arc::default();
    let providers = ExtensionProviders::new();
    let stream_fn = extension_stream_fn(
        providers.clone(),
        counting_inner(script.clone(), inner_calls.clone()),
    );
    let fx = fixture(
        &[("scripted-provider.ts", Some(SCRIPTED_PROVIDER))],
        FixtureOptions {
            runtime_options: TestRuntimeOptions {
                stream_fn: Some(stream_fn),
                ..Default::default()
            },
            providers: Some(providers.clone()),
            ..Default::default()
        },
    )
    .await;
    let session = fx.session();

    // Registration parity: reported with the streamSimple marker.
    let registration = fx
        .binding
        .registrations()
        .providers
        .into_iter()
        .find(|provider| provider.name == "scripted")
        .expect("provider registered");
    assert!(registration.has_stream_simple);
    assert_eq!(
        registration.config_dto["baseUrl"],
        json!("https://scripted.invalid")
    );

    // Runtime catalog mutation (F9): the model is findable with every data
    // field intact (no Value loss).
    let registry = session.model_registry();
    let model = {
        let start = Instant::now();
        loop {
            let found = {
                let registry = registry.read().await;
                registry.find("scripted", "scripted-1").cloned()
            };
            if let Some(model) = found {
                break model;
            }
            assert!(
                start.elapsed() < Duration::from_secs(20),
                "provider model never reached the catalog; errors={:?}",
                fx.errors.lock(),
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    };
    assert_eq!(model.base_url, "https://scripted.invalid");
    assert_eq!(model.api.as_ref(), "scripted-api");
    assert_eq!(model.name, "Scripted One");
    assert_eq!(model.cost.input, 1.5);
    assert_eq!(model.context_window, 100000);
    assert!(fx.providers.is_sidecar_streaming("scripted"));

    // Host-side auth: env template + custom headers resolve.
    {
        let registry = registry.read().await;
        let auth = registry.get_api_key_and_headers(&model).await;
        assert!(auth.ok, "{:?}", auth.error);
        assert_eq!(auth.api_key.as_deref(), Some("sk-scripted"));
        assert_eq!(
            auth.headers.as_ref().and_then(|h| h.get("x-scripted")),
            Some(&"yes".to_string()),
        );
    }

    // Full loop: the sidecar streamSimple answers the prompt; the resolved
    // api key crossed in the wire options (echoed into the reply).
    session.set_model(model.clone()).await.expect("model set");
    session
        .prompt("hello scripted", Default::default())
        .await
        .expect("prompt runs");

    let assistant_texts: Vec<String> = fx
        .entries()
        .into_iter()
        .filter_map(|entry| match entry {
            SessionEntry::Message { message, .. }
                if message.get("role").and_then(Value::as_str) == Some("assistant") =>
            {
                message["content"][0]["text"].as_str().map(str::to_string)
            }
            _ => None,
        })
        .collect();
    assert_eq!(assistant_texts, vec!["scripted reply key=sk-scripted"]);
    assert_eq!(*inner_calls.lock(), 0, "inner stream fn never touched");
    assert!(fx.real_errors().is_empty(), "{:?}", fx.real_errors());
}

/// Corpus custom-provider-anthropic loads UNMODIFIED (real @anthropic-ai/sdk
/// import), registers verbatim, and a pre-cancelled stream aborts cleanly
/// with zero network traffic.
#[tokio::test]
async fn corpus_custom_provider_anthropic_registers_and_aborts() {
    // SAFETY: test-process env; constant value.
    unsafe { std::env::set_var("CUSTOM_ANTHROPIC_API_KEY", "sk-ant-test") };
    // The corpus file imports @anthropic-ai/sdk (a real transitive dep of
    // the pinned npm pi-coding-agent): it must live under sidecar/ so node
    // resolution finds sidecar/node_modules. Unique dir per run; cleaned up.
    let host_dir = sidecar_dir().join(format!(".test-ext-{}", std::process::id()));
    std::fs::create_dir_all(&host_dir).expect("test ext dir");
    let ext_path = host_dir.join("custom-provider-anthropic.ts");
    std::fs::write(&ext_path, corpus("custom-provider-anthropic.ts")).expect("copy corpus");
    let _cleanup = scopeguard(move || {
        let _ = std::fs::remove_dir_all(&host_dir);
    });

    let providers = ExtensionProviders::new();
    let script: Arc<Mutex<VecDeque<AssistantMessage>>> = Arc::default();
    let inner_calls: Arc<Mutex<usize>> = Arc::default();
    let fx = fixture(
        &[(ext_path.to_str().expect("utf8 path"), None)],
        FixtureOptions {
            providers: Some(providers.clone()),
            ..Default::default()
        },
    )
    .await;
    let session = fx.session();

    // Registration parity (corpus values verbatim).
    let registration = fx
        .binding
        .registrations()
        .providers
        .into_iter()
        .find(|provider| provider.name == "custom-anthropic")
        .expect("corpus provider registered");
    assert!(registration.has_stream_simple);
    assert_eq!(
        registration.config_dto["baseUrl"],
        json!("https://api.anthropic.com")
    );
    assert_eq!(
        registration.config_dto["apiKey"],
        json!("$CUSTOM_ANTHROPIC_API_KEY")
    );
    assert_eq!(
        registration.config_dto["api"],
        json!("custom-anthropic-api")
    );

    // Catalog + auth (env interpolation) on the host.
    let registry = session.model_registry();
    let model = {
        let start = Instant::now();
        loop {
            let models: Vec<Model> = {
                let registry = registry.read().await;
                registry
                    .get_available()
                    .await
                    .into_iter()
                    .filter(|m| m.provider == "custom-anthropic")
                    .collect()
            };
            if !models.is_empty() {
                assert_eq!(models.len(), 2, "corpus defines two models");
                break models[0].clone();
            }
            assert!(
                start.elapsed() < Duration::from_secs(20),
                "corpus models never reached the catalog; errors={:?}",
                fx.errors.lock(),
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    };
    assert_eq!(model.api.as_ref(), "custom-anthropic-api");
    assert_eq!(model.base_url, "https://api.anthropic.com");
    {
        let registry = registry.read().await;
        let auth = registry.get_api_key_and_headers(&model).await;
        assert!(auth.ok, "{:?}", auth.error);
        assert_eq!(auth.api_key.as_deref(), Some("sk-ant-test"));
    }

    // Pre-cancelled stream: the real streamSimple is invoked and aborted
    // via a wire cancel frame before any network I/O completes.
    assert!(fx.providers.is_sidecar_streaming("custom-anthropic"));
    let cancel = CancellationToken::new();
    cancel.cancel();
    let stream = (extension_stream_fn(
        providers.clone(),
        counting_inner(script, inner_calls.clone()),
    ))(
        model.clone(),
        Context {
            messages: vec![pi_ai::Message::User(pi_ai::UserMessage {
                content: pi_ai::UserContent::Text("hi".into()),
                timestamp: 1_700_000_000_000,
            })],
            ..Default::default()
        },
        pi_agent::StreamCallOptions {
            cancel: Some(cancel),
            ..Default::default()
        },
    )
    .await;
    let result = tokio::time::timeout(Duration::from_secs(15), stream.result())
        .await
        .expect("aborted stream settles");
    assert_eq!(result.stop_reason, StopReason::Aborted);
    assert_eq!(*inner_calls.lock(), 0);

    // Sidecar still responsive: a real round trip after the abort.
    let connection = fx
        .binding
        .host()
        .current_connection()
        .await
        .expect("sidecar alive");
    let err = connection
        .request(Request::ToolExecute(ToolExecuteParams {
            tool_call_id: "probe".into(),
            name: "no_such_tool".into(),
            args: json!({}),
        }))
        .await
        .expect_err("unknown tool errors");
    assert!(err.to_string().contains("extension tool not found"));
}

/// `prompt("/name args")` dispatches extension commands (corpus commands.ts
/// + dynamic-tools.ts) without an LLM turn; unknown slash text still starts
/// a turn; a throwing handler reports `command:<name>`.
#[tokio::test]
async fn prompt_dispatches_extension_commands() {
    let script: Arc<Mutex<VecDeque<AssistantMessage>>> = Arc::default();
    let inner_calls: Arc<Mutex<usize>> = Arc::default();
    let commands_src = corpus("commands.ts");
    let dynamic_src = corpus("dynamic-tools.ts");
    let fx = fixture(
        &[
            ("commands.ts", Some(&commands_src)),
            ("dynamic-tools.ts", Some(&dynamic_src)),
            ("boom.ts", Some(BOOM_CMD)),
        ],
        FixtureOptions {
            runtime_options: TestRuntimeOptions {
                stream_fn: Some(counting_inner(script.clone(), inner_calls.clone())),
                ..Default::default()
            },
            ..Default::default()
        },
    )
    .await;
    let session = fx.session();

    // Corpus registrations with provenance.
    let commands = fx.binding.registered_commands();
    let listing = commands
        .iter()
        .find(|command| command.invocation_name == "commands")
        .expect("corpus /commands registered");
    assert_eq!(
        listing.description.as_deref(),
        Some("List available slash commands")
    );
    assert!(
        listing.source_info.path.ends_with("commands.ts"),
        "{:?}",
        listing.source_info,
    );
    assert!(
        commands
            .iter()
            .any(|command| command.invocation_name == "add-echo-tool"),
    );

    // Extension command: executes in the sidecar, NO provider call. The
    // corpus handler walks pi.getCommands() and drives ctx.ui.select (no-op
    // UI without hasUi) to completion.
    session
        .prompt("/commands", Default::default())
        .await
        .expect("command dispatch");
    assert_eq!(*inner_calls.lock(), 0, "no LLM turn for /commands");

    // Command with args: registers a new tool at runtime.
    session
        .prompt("/add-echo-tool prompt_echo", Default::default())
        .await
        .expect("command with args");
    fx.wait_until("prompt_echo registered", |fx| {
        fx.session()
            .get_all_tools()
            .iter()
            .any(|tool| tool.name == "prompt_echo")
    })
    .await;
    assert_eq!(*inner_calls.lock(), 0);
    assert!(fx.real_errors().is_empty(), "{:?}", fx.real_errors());

    // Throwing handler: consumed as a command, surfaced as command:<name>.
    session
        .prompt("/boom", Default::default())
        .await
        .expect("throwing command still counts as handled");
    let errors = fx.real_errors();
    assert!(
        errors
            .iter()
            .any(|error| error.extension_path == "command:boom"
                && error.error.contains("boom handler failed")),
        "{errors:?}",
    );
    assert_eq!(*inner_calls.lock(), 0);

    // Unknown slash text falls through to a normal turn.
    script
        .lock()
        .push_back(assistant_text_message(&fx.runtime.model, "not a command"));
    session
        .prompt("/definitely-not-registered", Default::default())
        .await
        .expect("prompt runs");
    assert_eq!(*inner_calls.lock(), 1, "unknown slash text reaches the LLM");
}

/// `registerFlag` provenance + `getFlag` served from host CLI values.
#[tokio::test]
async fn flags_register_with_provenance_and_resolve_values() {
    let mut flag_values = BTreeMap::new();
    flag_values.insert(
        "demo-flag".to_string(),
        FlagValue::String("hello".to_string()),
    );
    let fx = fixture(
        &[("flagged.ts", Some(FLAGGED))],
        FixtureOptions {
            flag_values,
            ..Default::default()
        },
    )
    .await;

    let flags = fx.binding.registrations().flags;
    assert_eq!(flags.len(), 1, "{flags:?}");
    assert_eq!(flags[0].name, "demo-flag");
    assert_eq!(flags[0].kind, pi_ext_protocol::FlagKind::String);
    assert_eq!(flags[0].description.as_deref(), Some("demo flag"));
    assert!(
        flags[0].extension_path.ends_with("flagged.ts"),
        "provenance: {:?}",
        flags[0],
    );

    // CLI-help shape (mode surface).
    let cli_flags = fx.binding.registered_flags();
    assert_eq!(cli_flags.len(), 1);
    assert_eq!(cli_flags[0].name, "demo-flag");
    assert_eq!(cli_flags[0].r#type, "string");

    // getFlag resolves the host-supplied value inside the extension.
    fx.wait_until("flag value recorded", |fx| !fx.obs("flag").is_empty())
        .await;
    assert_eq!(fx.obs("flag")[0]["value"], json!("hello"));
    assert!(fx.real_errors().is_empty(), "{:?}", fx.real_errors());
}

/// Minimal drop guard (avoid a scopeguard dependency).
fn scopeguard<F: FnOnce()>(f: F) -> impl Drop {
    struct Guard<F: FnOnce()>(Option<F>);
    impl<F: FnOnce()> Drop for Guard<F> {
        fn drop(&mut self) {
            if let Some(f) = self.0.take() {
                f();
            }
        }
    }
    Guard(Some(f))
}
