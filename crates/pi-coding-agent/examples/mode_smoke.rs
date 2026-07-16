//! Process-level smoke driver for the wire modes (binary wiring lands with
//! the CLI unit). Runs the REAL mode handlers over real stdin/stdout with a
//! scripted provider, so framing, signals, and exit codes can be exercised
//! from a shell:
//!
//! ```sh
//! printf '{"id":"1","type":"get_state"}\n' | cargo run --example mode_smoke rpc
//! cargo run --example mode_smoke print "hello"
//! cargo run --example mode_smoke json "hello"
//! ```

use std::collections::VecDeque;
use std::sync::Arc;

use parking_lot::Mutex;
use pi_agent::AgentThinkingLevel;
use pi_ai::{AssistantMessage, AssistantMessageEvent, Content, Model, StopReason, TextContent};
use pi_coding_agent::modes::json::run_json_mode;
use pi_coding_agent::modes::print::{PrintModeOptions, PrintOutputMode, run_print_mode};
use pi_coding_agent::modes::rpc::{RpcModeOptions, run_rpc_mode};
use pi_coding_agent::session::runtime::{
    AgentSessionRuntime, CreateRuntimeFactory, CreateRuntimeOptions, CreateRuntimeResult,
};
use pi_coding_agent::session::services::{
    CreateAgentSessionServicesOptions, create_agent_session_services,
};
use pi_coding_agent::session::{AgentSession, AgentSessionConfig};
use pi_coding_agent::{AuthStorage, ModelRegistry, NoopExtensionBridge, SessionManager};

fn scripted_message(model: &Model) -> AssistantMessage {
    AssistantMessage {
        content: vec![Content::Text(TextContent {
            text: "done".into(),
            text_signature: None,
        })],
        api: model.api.clone(),
        provider: model.provider.clone(),
        model: model.id.clone(),
        response_model: None,
        response_id: None,
        diagnostics: None,
        usage: pi_ai::Usage::default(),
        stop_reason: StopReason::Stop,
        error_message: None,
        timestamp: 1_700_000_000_000,
    }
}

async fn make_runtime(tmp: &std::path::Path) -> Arc<AgentSessionRuntime> {
    let cwd = tmp.join("project");
    std::fs::create_dir_all(&cwd).expect("cwd");
    let agent_dir = tmp.join("agent");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");

    let auth = Arc::new(AuthStorage::new(agent_dir.join("auth.json")));
    auth.set_runtime_api_key("anthropic".to_string(), "smoke-key".to_string());
    let registry = Arc::new(tokio::sync::RwLock::new(ModelRegistry::in_memory(
        auth.clone(),
    )));
    let model = registry
        .read()
        .await
        .find("anthropic", "claude-opus-4-8")
        .expect("builtin model")
        .clone();
    let session_manager =
        SessionManager::in_memory(Some(&cwd.to_string_lossy()), None).expect("session manager");

    let factory: CreateRuntimeFactory = Arc::new(move |options: CreateRuntimeOptions| {
        let auth = auth.clone();
        let registry = registry.clone();
        let model = model.clone();
        Box::pin(async move {
            let services = create_agent_session_services(CreateAgentSessionServicesOptions {
                cwd: options.cwd,
                agent_dir: Some(options.agent_dir),
                auth_storage: Some(auth),
                model_registry: Some(registry),
                ..Default::default()
            });
            let script = Arc::new(Mutex::new(VecDeque::from([scripted_message(&model)])));
            let stream_fn: pi_agent::StreamFn = Arc::new(move |model: Model, _ctx, _opts| {
                let script = script.clone();
                Box::pin(async move {
                    let stream = pi_ai::create_assistant_message_event_stream();
                    let message = script
                        .lock()
                        .pop_front()
                        .unwrap_or_else(|| scripted_message(&model));
                    stream.push(AssistantMessageEvent::Done {
                        reason: message.stop_reason,
                        message,
                    });
                    stream
                })
            });
            let session = AgentSession::new(AgentSessionConfig {
                session_manager: options.session_manager,
                settings_manager: services.settings_manager.clone(),
                model_registry: services.model_registry.clone(),
                cwd: services.cwd.clone(),
                stream_fn: Some(stream_fn),
                model: Some(model),
                thinking_level: AgentThinkingLevel::Off,
                scoped_models: Vec::new(),
                custom_tools: Vec::new(),
                initial_active_tool_names: None,
                allowed_tool_names: None,
                excluded_tool_names: None,
                skills: Vec::new(),
                prompt_templates: Vec::new(),
                context_files: Vec::new(),
                custom_system_prompt: None,
                append_system_prompt: None,
            });
            Ok(CreateRuntimeResult {
                session,
                services,
                diagnostics: Vec::new(),
                model_fallback_message: None,
            })
        })
    });

    let session_dir = tmp.join("project");
    Arc::new(
        AgentSessionRuntime::create(
            factory,
            CreateRuntimeOptions {
                cwd: session_dir,
                agent_dir,
                session_manager,
                session_start_reason: pi_coding_agent::SessionStartReason::Startup,
                previous_session_file: None,
            },
            Arc::new(NoopExtensionBridge::default()),
        )
        .await
        .expect("runtime"),
    )
}

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let mode = args.next().unwrap_or_else(|| "rpc".to_string());
    let message = args.next();

    let tmp = tempfile::tempdir().expect("tempdir");
    let runtime = make_runtime(tmp.path()).await;

    let exit_code = match mode.as_str() {
        "rpc" => run_rpc_mode(runtime, RpcModeOptions::default()).await,
        "print" => {
            run_print_mode(
                runtime,
                PrintModeOptions {
                    mode: PrintOutputMode::Text,
                    initial_message: message,
                    ..Default::default()
                },
            )
            .await
        }
        "json" => {
            run_json_mode(
                runtime,
                PrintModeOptions {
                    initial_message: message,
                    ..Default::default()
                },
            )
            .await
        }
        other => {
            eprintln!("unknown smoke mode: {other} (expected rpc|print|json)");
            2
        }
    };
    std::process::exit(exit_code);
}
