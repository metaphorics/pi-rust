use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use pi_agent::{AgentThinkingLevel, AgentToolResult, ToolDefinition};
use pi_ai::{
    AssistantMessage, AssistantMessageEvent, Content, Message, Model, StopReason, TextContent,
    ToolCall, UserContent,
};
use pi_coding_agent::modes::interactive::interactive_mode::{
    InteractiveMode, InteractiveModeOptions,
};
use pi_coding_agent::session::runtime::{
    AgentSessionRuntime, CreateRuntimeFactory, CreateRuntimeOptions, CreateRuntimeResult,
};
use pi_coding_agent::session::services::{
    CreateAgentSessionServicesOptions, create_agent_session_services,
};
use pi_coding_agent::session::{AgentSession, AgentSessionConfig, SessionToolDefinition};
use pi_coding_agent::{
    AuthStorage, ExtensionBridge, ModelRegistry, RegisteredCommand, SessionManager, SourceInfo,
};
use pi_tui::terminal::ProcessTerminal;

/// Bridge that registers one extension command (`/ext`) so the tmux harness
/// can exercise the compaction-gate extension-command path.
struct SmokeBridge;

impl ExtensionBridge for SmokeBridge {
    fn needs_sidecar(&self) -> bool {
        false
    }

    fn discovered_paths(&self) -> &[std::path::PathBuf] {
        &[]
    }

    fn registered_commands(&self) -> Vec<RegisteredCommand> {
        vec![RegisteredCommand {
            invocation_name: "ext".to_owned(),
            description: None,
            source_info: SourceInfo::synthetic("smoke-ext", "smoke", None, None, None),
        }]
    }
}

async fn make_runtime(
    tmp: &std::path::Path,
    was_tool_call: Arc<AtomicBool>,
) -> Arc<AgentSessionRuntime> {
    let cwd = tmp.join("project");
    std::fs::create_dir_all(&cwd).expect("cwd");
    let agent_dir = tmp.join("agent");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");
    // Tiny keep-recent budget so /compact has something to compact after a
    // couple of short smoke exchanges.
    std::fs::write(
        agent_dir.join("settings.json"),
        r#"{"compaction":{"keepRecentTokens":1}}"#,
    )
    .expect("seed settings");

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
        let was_tool_call = was_tool_call.clone();
        Box::pin(async move {
            let services = create_agent_session_services(CreateAgentSessionServicesOptions {
                cwd: options.cwd,
                agent_dir: Some(options.agent_dir),
                auth_storage: Some(auth),
                model_registry: Some(registry),
                ..Default::default()
            });

            // Register custom tool definition
            let custom_tool = SessionToolDefinition {
                definition: Arc::new(ToolDefinition {
                    name: "progress_demo".to_string(),
                    label: "Progress Demo".to_string(),
                    description: "Streams demo progress".to_string(),
                    parameters: serde_json::json!({
                        "type": "object",
                        "properties": {}
                    }),
                    execution_mode: None,
                    prepare_arguments: None,
                    execute: Arc::new(move |_id, _params, _cancellation, on_update| {
                        Box::pin(async move {
                            for i in 1..=5 {
                                tokio::time::sleep(Duration::from_millis(200)).await;
                                if let Some(cb) = &on_update {
                                    cb(AgentToolResult::text(format!("progress {}/5", i)));
                                }
                            }
                            Ok(AgentToolResult::text("progress complete"))
                        })
                    }),
                    renderer: None,
                }),
                prompt_snippet: None,
                prompt_guidelines: Vec::new(),
                source: "sdk",
                source_info: None,
            };

            let was_tool_call_stream = was_tool_call.clone();
            let stream_fn: pi_agent::StreamFn = Arc::new(move |model: Model, ctx, opts| {
                let was_tool_call = was_tool_call_stream.clone();
                let cancel = opts.cancel.clone();
                Box::pin(async move {
                    let stream = pi_ai::create_assistant_message_event_stream();
                    let s = stream.clone();
                    let model_id = model.id.clone();
                    let api = model.api.clone();
                    let provider = model.provider.clone();

                    let is_after_tool = was_tool_call.swap(false, Ordering::SeqCst);

                    // Extract latest user message
                    let latest_user_msg = ctx.messages.iter().rev().find_map(|m| match m {
                        Message::User(u) => match &u.content {
                            UserContent::Text(t) => Some(t.clone()),
                            UserContent::Blocks(blocks) => {
                                let mut text = String::new();
                                for block in blocks {
                                    if let Content::Text(tc) = block {
                                        text.push_str(&tc.text.to_string());
                                    }
                                }
                                Some(text)
                            }
                        },
                        _ => None,
                    });

                    let has_tool = latest_user_msg
                        .as_ref()
                        .is_some_and(|t| t.to_lowercase().contains("tool"));
                    // "slowly" prompts stream ~6s so the tmux harness has a
                    // wide window to queue/dequeue follow-ups mid-stream.
                    let has_slow = latest_user_msg
                        .as_ref()
                        .is_some_and(|t| t.to_lowercase().contains("slowly"));
                    // Compaction summarization requests wrap the transcript
                    // in <conversation> tags: stall them (cancel-aware) so
                    // the harness can act while `is_compacting()` is true.
                    let is_summary = latest_user_msg
                        .as_ref()
                        .is_some_and(|t| t.starts_with("<conversation>"));

                    tokio::spawn(async move {
                        let mut partial_msg = AssistantMessage {
                            content: vec![],
                            api: api.clone(),
                            provider: provider.clone(),
                            model: model_id.clone(),
                            response_model: None,
                            response_id: None,
                            diagnostics: None,
                            usage: pi_ai::Usage::default(),
                            stop_reason: StopReason::Stop,
                            error_message: None,
                            timestamp: 1_700_000_000_000,
                        };
                        s.push(AssistantMessageEvent::Start {
                            partial: partial_msg.clone(),
                        });

                        if is_summary {
                            loop {
                                tokio::time::sleep(Duration::from_millis(50)).await;
                                if cancel.as_ref().is_some_and(|c| c.is_cancelled()) {
                                    partial_msg.stop_reason = StopReason::Aborted;
                                    partial_msg.error_message =
                                        Some("Operation aborted".to_string());
                                    s.push(AssistantMessageEvent::Done {
                                        reason: StopReason::Aborted,
                                        message: partial_msg,
                                    });
                                    return;
                                }
                            }
                        } else if is_after_tool {
                            s.push(AssistantMessageEvent::TextStart {
                                content_index: 0,
                                partial: partial_msg.clone(),
                            });
                            let text = "SMOKE-TOOL-DONE";
                            partial_msg.content = vec![Content::Text(TextContent {
                                text: text.to_string().into(),
                                text_signature: None,
                            })];
                            s.push(AssistantMessageEvent::TextDelta {
                                content_index: 0,
                                delta: text.to_string(),
                                partial: partial_msg.clone(),
                            });
                            s.push(AssistantMessageEvent::TextEnd {
                                content_index: 0,
                                content: text.to_string(),
                                partial: partial_msg.clone(),
                            });
                            s.push(AssistantMessageEvent::Done {
                                reason: StopReason::Stop,
                                message: partial_msg,
                            });
                        } else if has_tool {
                            was_tool_call.store(true, Ordering::SeqCst);
                            s.push(AssistantMessageEvent::ToolcallStart {
                                content_index: 0,
                                partial: partial_msg.clone(),
                            });

                            // sleep a little bit
                            tokio::time::sleep(Duration::from_millis(50)).await;

                            let tc = ToolCall {
                                id: "call_demo_id".to_string(),
                                name: "progress_demo".to_string(),
                                arguments: serde_json::Map::new(),
                                thought_signature: None,
                            };

                            partial_msg.content = vec![Content::ToolCall(tc.clone())];
                            partial_msg.stop_reason = StopReason::ToolUse;

                            s.push(AssistantMessageEvent::ToolcallEnd {
                                content_index: 0,
                                tool_call: tc.clone(),
                                partial: partial_msg.clone(),
                            });

                            tokio::time::sleep(Duration::from_millis(50)).await;

                            s.push(AssistantMessageEvent::Done {
                                reason: StopReason::ToolUse,
                                message: partial_msg,
                            });
                        } else {
                            s.push(AssistantMessageEvent::TextStart {
                                content_index: 0,
                                partial: partial_msg.clone(),
                            });
                            let mut accumulated_text = String::new();
                            let reply_text: String = if has_slow {
                                (0..60)
                                    .map(|i| format!("SLOW-SMOKE-BODY-{i}"))
                                    .collect::<Vec<_>>()
                                    .join(" ")
                            } else {
                                "Here is the response: SMOKE-REPLY. This is some dummy text to make the stream long enough to allow testing escape cancellation.".to_string()
                            };
                            let chunk_delay =
                                Duration::from_millis(if has_slow { 100 } else { 50 });
                            let chunks: Vec<&str> = reply_text.split_whitespace().collect();
                            let mut cancelled = false;
                            for chunk in chunks {
                                tokio::time::sleep(chunk_delay).await;
                                if cancel.as_ref().is_some_and(|c| c.is_cancelled()) {
                                    cancelled = true;
                                    break;
                                }
                                accumulated_text.push_str(chunk);
                                accumulated_text.push(' ');

                                partial_msg.content = vec![Content::Text(TextContent {
                                    text: accumulated_text.clone().into(),
                                    text_signature: None,
                                })];
                                s.push(AssistantMessageEvent::TextDelta {
                                    content_index: 0,
                                    delta: format!("{chunk} "),
                                    partial: partial_msg.clone(),
                                });
                            }

                            if cancelled {
                                partial_msg.stop_reason = StopReason::Aborted;
                                partial_msg.error_message = Some("Operation aborted".to_string());
                                s.push(AssistantMessageEvent::Done {
                                    reason: StopReason::Aborted,
                                    message: partial_msg,
                                });
                            } else {
                                s.push(AssistantMessageEvent::TextEnd {
                                    content_index: 0,
                                    content: accumulated_text.clone(),
                                    partial: partial_msg.clone(),
                                });
                                partial_msg.stop_reason = StopReason::Stop;
                                s.push(AssistantMessageEvent::Done {
                                    reason: StopReason::Stop,
                                    message: partial_msg,
                                });
                            }
                        }
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
                custom_tools: vec![custom_tool],
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
            Arc::new(SmokeBridge),
        )
        .await
        .expect("runtime"),
    )
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let was_tool_call = Arc::new(AtomicBool::new(false));
    let runtime = make_runtime(tmp.path(), was_tool_call).await;

    let terminal = ProcessTerminal::new();
    let mode = InteractiveMode::new(
        runtime,
        terminal,
        InteractiveModeOptions {
            handle_signals: true,
            ..Default::default()
        },
    );

    let outcome = mode.run().await;
    if let Some(farewell) = outcome.farewell {
        println!("{}", farewell);
    }
    std::process::exit(outcome.exit_code);
}
