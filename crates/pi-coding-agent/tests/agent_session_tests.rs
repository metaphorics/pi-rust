//! AgentSession fixture tests: wire-event JSON/order, queue transitions,
//! active-run rejection, session entries, tool registry, services order,
//! runtime switch/fork, and verbatim prompt/context strings.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use parking_lot::Mutex;
use serde_json::{Value, json};

use pi_agent::{AgentMessage, AgentThinkingLevel, AgentToolResult, StreamFn, ToolDefinition};
use pi_ai::{
    AssistantMessage, AssistantMessageEvent, Content, Message, Model, StopReason, TextContent,
    UserContent, UserMessage,
};
use pi_coding_agent::extension_bridge::{
    ExtensionBridge, ForkPosition, HookOutcome, SessionLifecycleEvent,
};
use pi_coding_agent::session::runtime::{
    AgentSessionRuntime, CreateRuntimeFactory, CreateRuntimeOptions, CreateRuntimeResult,
};
use pi_coding_agent::session::services::{
    CreateAgentSessionServicesOptions, create_agent_session_services,
};
use pi_coding_agent::session::{
    AgentSession, AgentSessionConfig, AgentSessionEvent, CompactionReason, CompactionResult,
    PromptOptions, PromptTemplate, SessionToolDefinition, StreamingBehavior, convert_to_llm,
};
use pi_coding_agent::system_prompt::Skill;
use pi_coding_agent::{AuthStorage, ModelRegistry, SessionManager, Settings, SettingsManager};

// ============================================================================
// Fixtures
// ============================================================================

fn assistant_text_message(model: &Model, text: &str) -> AssistantMessage {
    AssistantMessage {
        content: vec![Content::Text(TextContent {
            text: text.into(),
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

/// Stream fn returning scripted responses in order (fails when exhausted).
fn scripted_stream_fn(script: Arc<Mutex<VecDeque<AssistantMessage>>>) -> StreamFn {
    Arc::new(move |model: Model, _context, _options| {
        let script = script.clone();
        Box::pin(async move {
            let stream = pi_ai::create_assistant_message_event_stream();
            let message = script.lock().pop_front().unwrap_or_else(|| {
                let mut error = assistant_text_message(&model, "");
                error.stop_reason = StopReason::Error;
                error.error_message = Some("script exhausted".to_string());
                error
            });
            stream.push(AssistantMessageEvent::Done {
                reason: message.stop_reason,
                message,
            });
            stream
        })
    })
}

/// Stream fn that blocks the FIRST call until `gate` is notified, then
/// behaves like `scripted_stream_fn`.
fn gated_stream_fn(
    script: Arc<Mutex<VecDeque<AssistantMessage>>>,
    gate: Arc<tokio::sync::Notify>,
    calls: Arc<AtomicUsize>,
) -> StreamFn {
    Arc::new(move |model: Model, _context, _options| {
        let script = script.clone();
        let gate = gate.clone();
        let calls = calls.clone();
        Box::pin(async move {
            let call_index = calls.fetch_add(1, Ordering::SeqCst);
            if call_index == 0 {
                gate.notified().await;
            }
            let stream = pi_ai::create_assistant_message_event_stream();
            let message = script
                .lock()
                .pop_front()
                .unwrap_or_else(|| assistant_text_message(&model, "fallback"));
            stream.push(AssistantMessageEvent::Done {
                reason: message.stop_reason,
                message,
            });
            stream
        })
    })
}

struct Fixture {
    session: AgentSession,
    events: Arc<Mutex<Vec<Value>>>,
    settings: Arc<Mutex<SettingsManager>>,
    model: Model,
    _tmp: tempfile::TempDir,
}

#[derive(Default)]
struct FixtureOptions {
    stream_fn: Option<StreamFn>,
    allowed_tool_names: Option<Vec<String>>,
    excluded_tool_names: Option<Vec<String>>,
    custom_tools: Vec<SessionToolDefinition>,
    skills: Vec<Skill>,
    prompt_templates: Vec<PromptTemplate>,
}

fn make_fixture(script: Vec<AssistantMessage>, options: FixtureOptions) -> Fixture {
    let tmp = tempfile::tempdir().expect("tempdir");
    let cwd = tmp.path().join("project");
    std::fs::create_dir_all(&cwd).expect("cwd");

    let auth = Arc::new(AuthStorage::new(tmp.path().join("auth.json")));
    auth.set_runtime_api_key("anthropic".to_string(), "test-key".to_string());
    let registry = Arc::new(tokio::sync::RwLock::new(ModelRegistry::in_memory(
        auth.clone(),
    )));
    let model = {
        let registry = registry.blocking_read();
        registry
            .find("anthropic", "claude-opus-4-8")
            .expect("builtin model")
            .clone()
    };
    let settings = Arc::new(Mutex::new(SettingsManager::in_memory(
        Settings::new(),
        true,
    )));
    let session_manager =
        SessionManager::in_memory(Some(&cwd.to_string_lossy()), None).expect("session manager");

    let script = Arc::new(Mutex::new(VecDeque::from(script)));
    let stream_fn = options
        .stream_fn
        .unwrap_or_else(|| scripted_stream_fn(script));

    let session = AgentSession::new(AgentSessionConfig {
        session_manager,
        settings_manager: settings.clone(),
        model_registry: registry.clone(),
        cwd: cwd.clone(),
        stream_fn: Some(stream_fn),
        model: Some(model.clone()),
        thinking_level: AgentThinkingLevel::Off,
        scoped_models: Vec::new(),
        custom_tools: options.custom_tools,
        initial_active_tool_names: None,
        allowed_tool_names: options.allowed_tool_names,
        excluded_tool_names: options.excluded_tool_names,
        skills: options.skills,
        prompt_templates: options.prompt_templates,
        context_files: Vec::new(),
        custom_system_prompt: None,
        append_system_prompt: None,
    });

    let events: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    // Keep listener registered for the fixture lifetime.
    let _unsubscribe = session.subscribe(Arc::new(move |event: &AgentSessionEvent| {
        sink.lock()
            .push(serde_json::to_value(event).expect("serialize event"));
    }));
    std::mem::forget(_unsubscribe);

    Fixture {
        session,
        events,
        settings,
        model,
        _tmp: tmp,
    }
}

fn event_types(events: &[Value]) -> Vec<String> {
    events
        .iter()
        .map(|e| e["type"].as_str().unwrap_or("?").to_string())
        .collect()
}

// ============================================================================
// 1. Event JSON + order over a full prompt run
// ============================================================================

#[tokio::test(flavor = "multi_thread")]
async fn prompt_emits_wire_events_in_order_and_persists_entries() {
    // Build a probe fixture to learn the model, then the real one with a
    // scripted response.
    let model =
        tokio::task::spawn_blocking(|| make_fixture(vec![], FixtureOptions::default()).model)
            .await
            .expect("model");
    let reply = assistant_text_message(&model, "Hello back");
    let fixture2 =
        tokio::task::spawn_blocking(move || make_fixture(vec![reply], FixtureOptions::default()))
            .await
            .expect("fixture");
    let session = fixture2.session.clone();

    session
        .prompt("Hello there", PromptOptions::default())
        .await
        .expect("prompt");

    let events = fixture2.events.lock().clone();
    assert_eq!(
        event_types(&events),
        vec![
            "agent_start",
            "turn_start",
            "message_start",
            "message_end",
            "message_start",
            "message_end",
            "turn_end",
            "agent_end",
            "agent_settled",
        ]
    );

    // User message payload shape (camelCase, block content).
    let user_start = &events[2];
    assert_eq!(user_start["message"]["role"], "user");
    assert_eq!(user_start["message"]["content"][0]["type"], "text");
    assert_eq!(user_start["message"]["content"][0]["text"], "Hello there");

    // Assistant end carries the wire assistant shape.
    let assistant_end = &events[5];
    assert_eq!(assistant_end["message"]["role"], "assistant");
    assert_eq!(assistant_end["message"]["stopReason"], "stop");

    // turn_end/agent_end payload key spellings.
    assert!(events[6]["toolResults"].as_array().is_some());
    assert_eq!(events[7]["willRetry"], Value::Bool(false));
    assert!(events[7]["messages"].as_array().is_some());

    // Session persistence: user + assistant message entries, in order.
    let entries = session.with_session_manager(|sm| sm.get_entries());
    assert_eq!(entries.len(), 2);
    let roles: Vec<String> = entries
        .iter()
        .map(|e| match e {
            pi_coding_agent::SessionEntry::Message { message, .. } => {
                message["role"].as_str().unwrap_or("?").to_string()
            }
            other => panic!("unexpected entry {other:?}"),
        })
        .collect();
    assert_eq!(roles, vec!["user", "assistant"]);

    // Agent state mirrors the transcript.
    assert_eq!(session.messages().len(), 2);
    assert!(session.is_idle());
    assert_eq!(
        session.get_last_assistant_text().as_deref(),
        Some("Hello back")
    );
}

// ============================================================================
// 2. Serde fixtures for the 19-variant wire surface
// ============================================================================

#[test]
fn wire_event_serde_matches_fixtures() {
    let user = AgentMessage::user(UserMessage {
        content: UserContent::Text("hi".to_string()),
        timestamp: 7,
    });
    let entry: pi_coding_agent::SessionEntry = serde_json::from_value(json!({
        "type": "custom",
        "customType": "marker",
        "id": "abc",
        "parentId": null,
        "timestamp": "2026-01-01T00:00:00Z"
    }))
    .expect("entry");

    let cases: Vec<(AgentSessionEvent, Value)> = vec![
        (
            AgentSessionEvent::AgentStart,
            json!({"type": "agent_start"}),
        ),
        (
            AgentSessionEvent::AgentEnd {
                messages: vec![user.clone()],
                will_retry: true,
            },
            json!({"type": "agent_end", "messages": [{"role": "user", "content": "hi", "timestamp": 7}], "willRetry": true}),
        ),
        (AgentSessionEvent::TurnStart, json!({"type": "turn_start"})),
        (
            AgentSessionEvent::TurnEnd {
                message: user.clone(),
                tool_results: vec![],
            },
            json!({"type": "turn_end", "message": {"role": "user", "content": "hi", "timestamp": 7}, "toolResults": []}),
        ),
        (
            AgentSessionEvent::MessageStart {
                message: user.clone(),
            },
            json!({"type": "message_start", "message": {"role": "user", "content": "hi", "timestamp": 7}}),
        ),
        (
            AgentSessionEvent::MessageEnd {
                message: user.clone(),
            },
            json!({"type": "message_end", "message": {"role": "user", "content": "hi", "timestamp": 7}}),
        ),
        (
            AgentSessionEvent::ToolExecutionStart {
                tool_call_id: "t1".into(),
                tool_name: "read".into(),
                args: json!({"path": "x"}),
            },
            json!({"type": "tool_execution_start", "toolCallId": "t1", "toolName": "read", "args": {"path": "x"}}),
        ),
        (
            AgentSessionEvent::ToolExecutionUpdate {
                tool_call_id: "t1".into(),
                tool_name: "read".into(),
                args: json!({}),
                partial_result: AgentToolResult::text("partial"),
            },
            json!({"type": "tool_execution_update", "toolCallId": "t1", "toolName": "read", "args": {}, "partialResult": {"content": [{"type": "text", "text": "partial"}], "details": {}}}),
        ),
        (
            AgentSessionEvent::ToolExecutionEnd {
                tool_call_id: "t1".into(),
                tool_name: "read".into(),
                result: AgentToolResult::text("done"),
                is_error: false,
            },
            json!({"type": "tool_execution_end", "toolCallId": "t1", "toolName": "read", "result": {"content": [{"type": "text", "text": "done"}], "details": {}}, "isError": false}),
        ),
        (
            AgentSessionEvent::AgentSettled,
            json!({"type": "agent_settled"}),
        ),
        (
            AgentSessionEvent::QueueUpdate {
                steering: vec!["a".into()],
                follow_up: vec![],
            },
            json!({"type": "queue_update", "steering": ["a"], "followUp": []}),
        ),
        (
            AgentSessionEvent::CompactionStart {
                reason: CompactionReason::Threshold,
            },
            json!({"type": "compaction_start", "reason": "threshold"}),
        ),
        (
            AgentSessionEvent::CompactionEnd {
                reason: CompactionReason::Manual,
                result: Some(CompactionResult {
                    summary: "s".into(),
                    first_kept_entry_id: "e1".into(),
                    tokens_before: 42,
                    estimated_tokens_after: Some(7),
                    details: None,
                }),
                aborted: false,
                will_retry: false,
                error_message: None,
            },
            json!({"type": "compaction_end", "reason": "manual", "result": {"summary": "s", "firstKeptEntryId": "e1", "tokensBefore": 42, "estimatedTokensAfter": 7}, "aborted": false, "willRetry": false}),
        ),
        (
            AgentSessionEvent::CompactionEnd {
                reason: CompactionReason::Overflow,
                result: None,
                aborted: false,
                will_retry: false,
                error_message: Some("Auto-compaction failed: x".into()),
            },
            json!({"type": "compaction_end", "reason": "overflow", "aborted": false, "willRetry": false, "errorMessage": "Auto-compaction failed: x"}),
        ),
        (
            AgentSessionEvent::EntryAppended {
                entry: entry.clone(),
            },
            json!({"type": "entry_appended", "entry": {"type": "custom", "customType": "marker", "id": "abc", "parentId": null, "timestamp": "2026-01-01T00:00:00Z"}}),
        ),
        (
            AgentSessionEvent::SessionInfoChanged {
                name: Some("named".into()),
            },
            json!({"type": "session_info_changed", "name": "named"}),
        ),
        (
            AgentSessionEvent::SessionInfoChanged { name: None },
            json!({"type": "session_info_changed"}),
        ),
        (
            AgentSessionEvent::ThinkingLevelChanged {
                level: AgentThinkingLevel::High,
            },
            json!({"type": "thinking_level_changed", "level": "high"}),
        ),
        (
            AgentSessionEvent::AutoRetryStart {
                attempt: 1,
                max_attempts: 3,
                delay_ms: 2000,
                error_message: "overloaded".into(),
            },
            json!({"type": "auto_retry_start", "attempt": 1, "maxAttempts": 3, "delayMs": 2000, "errorMessage": "overloaded"}),
        ),
        (
            AgentSessionEvent::AutoRetryEnd {
                success: false,
                attempt: 2,
                final_error: Some("Retry cancelled".into()),
            },
            json!({"type": "auto_retry_end", "success": false, "attempt": 2, "finalError": "Retry cancelled"}),
        ),
        (
            AgentSessionEvent::AutoRetryEnd {
                success: true,
                attempt: 1,
                final_error: None,
            },
            json!({"type": "auto_retry_end", "success": true, "attempt": 1}),
        ),
    ];

    for (event, expected) in cases {
        let serialized = serde_json::to_value(&event).expect("serialize");
        assert_eq!(serialized, expected, "wire JSON for {}", event.event_type());
        let round_tripped: AgentSessionEvent =
            serde_json::from_value(expected.clone()).expect("deserialize");
        assert_eq!(round_tripped.event_type(), event.event_type());
    }
}

// ============================================================================
// 3. Active-run invariant + steering queue transitions
// ============================================================================

#[tokio::test(flavor = "multi_thread")]
async fn active_run_rejects_unqueued_prompts_and_steers() {
    let gate = Arc::new(tokio::sync::Notify::new());
    let calls = Arc::new(AtomicUsize::new(0));
    let fixture = {
        let gate = gate.clone();
        let calls = calls.clone();
        tokio::task::spawn_blocking(move || {
            // Script inside the fixture: capture via gated stream below.
            let tmp_model_holder: Vec<AssistantMessage> = vec![];
            let mut options = FixtureOptions::default();
            // Placeholder; replaced after model known.
            let script = Arc::new(Mutex::new(VecDeque::from(tmp_model_holder)));
            options.stream_fn = Some(gated_stream_fn(script.clone(), gate, calls));
            let fixture = make_fixture(vec![], options);
            // Two responses: initial prompt + steered continuation.
            script
                .lock()
                .push_back(assistant_text_message(&fixture.model, "first"));
            script
                .lock()
                .push_back(assistant_text_message(&fixture.model, "second"));
            fixture
        })
        .await
        .expect("fixture")
    };
    let session = fixture.session.clone();

    let run_session = session.clone();
    let run = tokio::spawn(async move {
        run_session
            .prompt("start", PromptOptions::default())
            .await
            .expect("prompt");
    });

    // Wait until the run is active (gated inside the first stream call).
    while !session.is_streaming() {
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }

    // Unqueued prompt during streaming: verbatim rejection.
    let error = session
        .prompt("nope", PromptOptions::default())
        .await
        .expect_err("must reject");
    assert_eq!(
        error,
        "Agent is already processing. Specify streamingBehavior ('steer' or 'followUp') to queue the message."
    );

    // Queued prompt (steer) is accepted and visible in the queue.
    session
        .prompt(
            "queued steer",
            PromptOptions {
                streaming_behavior: Some(StreamingBehavior::Steer),
                ..Default::default()
            },
        )
        .await
        .expect("steer");
    assert_eq!(session.get_steering_messages(), vec!["queued steer"]);

    // Release the gated first response; the loop drains the steering queue.
    gate.notify_one();
    run.await.expect("run");
    session.wait_for_idle().await;

    assert!(session.get_steering_messages().is_empty());
    assert_eq!(calls.load(Ordering::SeqCst), 2, "steered turn ran");

    // queue_update events: one add (with text) and one removal (empty).
    let events = fixture.events.lock().clone();
    let queue_updates: Vec<&Value> = events
        .iter()
        .filter(|e| e["type"] == "queue_update")
        .collect();
    assert!(
        queue_updates
            .iter()
            .any(|e| e["steering"] == json!(["queued steer"])),
        "queue add observed"
    );
    assert_eq!(
        queue_updates.last().map(|e| e["steering"].clone()),
        Some(json!([])),
        "queue drained"
    );

    // The steered user message reached the transcript before "second".
    let roles: Vec<(String, String)> = session
        .messages()
        .iter()
        .filter_map(|m| match m {
            AgentMessage::Standard(Message::User(u)) => Some((
                "user".to_string(),
                match &u.content {
                    UserContent::Text(t) => t.clone(),
                    UserContent::Blocks(blocks) => blocks
                        .iter()
                        .filter_map(|c| match c {
                            Content::Text(t) => Some(t.text.to_string()),
                            _ => None,
                        })
                        .collect(),
                },
            )),
            AgentMessage::Standard(Message::Assistant(a)) => Some((
                "assistant".to_string(),
                a.content
                    .iter()
                    .filter_map(|c| match c {
                        Content::Text(t) => Some(t.text.to_string()),
                        _ => None,
                    })
                    .collect(),
            )),
            _ => None,
        })
        .collect();
    assert_eq!(
        roles,
        vec![
            ("user".to_string(), "start".to_string()),
            ("assistant".to_string(), "first".to_string()),
            ("user".to_string(), "queued steer".to_string()),
            ("assistant".to_string(), "second".to_string()),
        ]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn queue_modes_persist_and_clear_queue_returns_texts() {
    let fixture = tokio::task::spawn_blocking(|| make_fixture(vec![], FixtureOptions::default()))
        .await
        .expect("fixture");
    let session = fixture.session.clone();

    assert_eq!(session.steering_mode(), "one-at-a-time");
    session.set_steering_mode("all");
    session.set_follow_up_mode("all");
    assert_eq!(session.steering_mode(), "all");
    assert_eq!(session.follow_up_mode(), "all");
    {
        let settings = fixture.settings.lock();
        assert_eq!(settings.get_steering_mode(), "all");
        assert_eq!(settings.get_follow_up_mode(), "all");
    }

    session.steer("one", Vec::new());
    session.follow_up("two", Vec::new());
    assert_eq!(session.pending_message_count(), 2);

    let (steering, follow_up) = session.clear_queue();
    assert_eq!(steering, vec!["one"]);
    assert_eq!(follow_up, vec!["two"]);
    assert_eq!(session.pending_message_count(), 0);

    let events = fixture.events.lock().clone();
    let last_queue_update = events
        .iter()
        .rfind(|e| e["type"] == "queue_update")
        .expect("queue_update");
    assert_eq!(last_queue_update["steering"], json!([]));
    assert_eq!(last_queue_update["followUp"], json!([]));
}

// ============================================================================
// 4. Session entries: session_info / thinking / model changes
// ============================================================================

#[tokio::test(flavor = "multi_thread")]
async fn session_mutations_append_expected_entries_and_events() {
    let fixture = tokio::task::spawn_blocking(|| make_fixture(vec![], FixtureOptions::default()))
        .await
        .expect("fixture");
    let session = fixture.session.clone();

    session.set_session_name("my session");
    assert_eq!(session.session_name().as_deref(), Some("my session"));

    session.set_thinking_level(AgentThinkingLevel::High);
    assert_eq!(session.thinking_level(), AgentThinkingLevel::High);
    // Same level again: no duplicate entry.
    session.set_thinking_level(AgentThinkingLevel::High);

    session
        .set_model(fixture.model.clone())
        .await
        .expect("set model");

    let entries = session.with_session_manager(|sm| sm.get_entries());
    let kinds: Vec<&'static str> = entries
        .iter()
        .map(|e| match e {
            pi_coding_agent::SessionEntry::SessionInfo { .. } => "session_info",
            pi_coding_agent::SessionEntry::ThinkingLevelChange { .. } => "thinking_level_change",
            pi_coding_agent::SessionEntry::ModelChange { .. } => "model_change",
            _ => "other",
        })
        .collect();
    assert_eq!(
        kinds,
        vec!["session_info", "thinking_level_change", "model_change"]
    );

    // Settings persistence side effects.
    {
        let settings = fixture.settings.lock();
        assert_eq!(settings.get_default_provider(), Some("anthropic"));
        assert_eq!(
            settings.get_default_model(),
            Some(fixture.model.id.as_str())
        );
        assert_eq!(settings.get_default_thinking_level(), Some("high"));
    }

    let events = fixture.events.lock().clone();
    let types = event_types(&events);
    assert!(types.contains(&"session_info_changed".to_string()));
    assert_eq!(
        types
            .iter()
            .filter(|t| *t == "thinking_level_changed")
            .count(),
        1,
        "no event when level unchanged"
    );
    let info_event = events
        .iter()
        .find(|e| e["type"] == "session_info_changed")
        .expect("session_info_changed");
    assert_eq!(info_event["name"], "my session");
}

// ============================================================================
// 5. Tool registry: defaults, allow/deny, override position, prompt strings
// ============================================================================

fn noop_tool(name: &str) -> SessionToolDefinition {
    SessionToolDefinition {
        definition: Arc::new(ToolDefinition {
            name: name.to_string(),
            label: name.to_string(),
            description: format!("custom {name}"),
            parameters: json!({"type": "object", "properties": {}}),
            execution_mode: None,
            prepare_arguments: None,
            renderer: None,
            execute: Arc::new(|_, _, _, _| Box::pin(async { Ok(AgentToolResult::text("ok")) })),
        }),
        prompt_snippet: Some(format!("custom {name} snippet")),
        prompt_guidelines: vec![],
        source: "sdk",
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn tool_registry_defaults_filters_and_override_position() {
    // Defaults: 7 builtins in oracle order, 4 active.
    let fixture = tokio::task::spawn_blocking(|| make_fixture(vec![], FixtureOptions::default()))
        .await
        .expect("fixture");
    let names: Vec<String> = fixture
        .session
        .get_all_tools()
        .into_iter()
        .map(|t| t.name)
        .collect();
    assert_eq!(
        names,
        vec!["read", "bash", "edit", "write", "grep", "find", "ls"]
    );
    assert_eq!(
        fixture.session.get_active_tool_names(),
        vec!["read", "bash", "edit", "write"]
    );

    // Denylist removes from registry and active set.
    let fixture = tokio::task::spawn_blocking(|| {
        make_fixture(
            vec![],
            FixtureOptions {
                excluded_tool_names: Some(vec!["bash".to_string()]),
                ..Default::default()
            },
        )
    })
    .await
    .expect("fixture");
    assert!(fixture.session.get_tool_definition("bash").is_none());
    assert_eq!(
        fixture.session.get_active_tool_names(),
        vec!["read", "edit", "write"]
    );
    assert!(!fixture.session.is_tool_allowed("bash"));

    // Allowlist keeps only listed tools.
    let fixture = tokio::task::spawn_blocking(|| {
        make_fixture(
            vec![],
            FixtureOptions {
                allowed_tool_names: Some(vec!["read".to_string(), "write".to_string()]),
                ..Default::default()
            },
        )
    })
    .await
    .expect("fixture");
    let names: Vec<String> = fixture
        .session
        .get_all_tools()
        .into_iter()
        .map(|t| t.name)
        .collect();
    assert_eq!(names, vec!["read", "write"]);
    assert_eq!(
        fixture.session.get_active_tool_names(),
        vec!["read", "write"]
    );

    // Custom tool overriding a builtin keeps its insertion position; new
    // custom tools append and become active (includeAllExtensionTools).
    let fixture = tokio::task::spawn_blocking(|| {
        make_fixture(
            vec![],
            FixtureOptions {
                custom_tools: vec![noop_tool("read"), noop_tool("lint")],
                ..Default::default()
            },
        )
    })
    .await
    .expect("fixture");
    let tools = fixture.session.get_all_tools();
    let names: Vec<String> = tools.iter().map(|t| t.name.clone()).collect();
    assert_eq!(
        names,
        vec![
            "read", "bash", "edit", "write", "grep", "find", "ls", "lint"
        ]
    );
    assert_eq!(tools[0].source, "sdk", "override replaces in place");
    assert_eq!(
        fixture.session.get_active_tool_names(),
        vec!["read", "bash", "edit", "write", "lint"]
    );

    // set_active_tools_by_name ignores unknown names and rebuilds the prompt.
    fixture
        .session
        .set_active_tools_by_name(vec!["write".to_string(), "ghost".to_string()]);
    assert_eq!(fixture.session.get_active_tool_names(), vec!["write"]);
    let prompt = fixture.session.system_prompt();
    assert!(prompt.contains("- write: Create or overwrite files"));
    assert!(!prompt.contains("- bash: "));
}

#[tokio::test(flavor = "multi_thread")]
async fn system_prompt_contains_verbatim_builtin_strings() {
    let fixture = tokio::task::spawn_blocking(|| make_fixture(vec![], FixtureOptions::default()))
        .await
        .expect("fixture");
    let prompt = fixture.session.system_prompt();
    assert!(prompt.starts_with(
        "You are an expert coding assistant operating inside pi, a coding agent harness."
    ));
    assert!(prompt.contains("Available tools:\n- read: Read file contents\n- bash: Execute bash commands (ls, grep, find, etc.)\n- edit: Make precise file edits with exact text replacement, including multiple disjoint edits in one call\n- write: Create or overwrite files"));
    assert!(prompt.contains("- Use read to examine files instead of cat or sed."));
    assert!(prompt.contains("- Be concise in your responses"));
    assert!(prompt.contains("- Show file paths clearly when working with files"));
    assert!(prompt.contains("\nCurrent working directory: "));
}

// ============================================================================
// 6. Skill + template expansion and convert_to_llm framing strings
// ============================================================================

#[tokio::test(flavor = "multi_thread")]
async fn skill_and_template_expansion_produce_verbatim_blocks() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let skill_dir = tmp.path().join("skills/demo");
    std::fs::create_dir_all(&skill_dir).expect("skill dir");
    let skill_path = skill_dir.join("SKILL.md");
    std::fs::write(
        &skill_path,
        "---\nname: demo\ndescription: demo skill\n---\nDo the demo thing.",
    )
    .expect("skill file");

    let skill = Skill {
        name: "demo".to_string(),
        description: "demo skill".to_string(),
        file_path: skill_path.clone(),
        base_dir: skill_dir.clone(),
        ..Default::default()
    };
    let template = PromptTemplate {
        name: "tpl".to_string(),
        description: String::new(),
        argument_hint: None,
        content: "X $1 and all: $ARGUMENTS (${2:-fallback})".to_string(),
        file_path: PathBuf::from("tpl.md"),
        source_info: Default::default(),
    };

    let fixture = tokio::task::spawn_blocking(move || {
        make_fixture(
            vec![],
            FixtureOptions {
                skills: vec![skill],
                prompt_templates: vec![template],
                ..Default::default()
            },
        )
    })
    .await
    .expect("fixture");
    let session = fixture.session.clone();

    session.steer("/skill:demo now go", Vec::new());
    let expected_block = format!(
        "<skill name=\"demo\" location=\"{}\">\nReferences are relative to {}.\n\nDo the demo thing.\n</skill>\n\nnow go",
        skill_path.display(),
        skill_dir.display()
    );
    assert_eq!(session.get_steering_messages(), vec![expected_block]);
    session.clear_queue();

    session.steer("/tpl alpha", Vec::new());
    assert_eq!(
        session.get_steering_messages(),
        vec!["X alpha and all: alpha (fallback)"]
    );
}

#[test]
fn convert_to_llm_frames_custom_messages_verbatim() {
    let bash = AgentMessage::Custom(json!({
        "role": "bashExecution",
        "command": "ls -la",
        "output": "total 0",
        "exitCode": 2,
        "cancelled": false,
        "truncated": true,
        "fullOutputPath": "/tmp/pi-bash-x.log",
        "timestamp": 5
    }));
    let excluded = AgentMessage::Custom(json!({
        "role": "bashExecution",
        "command": "secret",
        "output": "",
        "cancelled": false,
        "truncated": false,
        "timestamp": 6,
        "excludeFromContext": true
    }));
    let compaction = AgentMessage::Custom(json!({
        "role": "compactionSummary",
        "summary": "the summary",
        "tokensBefore": 10,
        "timestamp": 7
    }));
    let branch = AgentMessage::Custom(json!({
        "role": "branchSummary",
        "summary": "branch text",
        "fromId": "e9",
        "timestamp": 8
    }));

    let converted = convert_to_llm(vec![bash, excluded, compaction, branch]);
    assert_eq!(converted.len(), 3, "excluded bash dropped");

    let text_of = |message: &Message| -> String {
        match message {
            Message::User(user) => match &user.content {
                UserContent::Blocks(blocks) => blocks
                    .iter()
                    .filter_map(|c| match c {
                        Content::Text(t) => Some(t.text.to_string()),
                        _ => None,
                    })
                    .collect(),
                UserContent::Text(t) => t.clone(),
            },
            _ => panic!("expected user message"),
        }
    };

    assert_eq!(
        text_of(&converted[0]),
        "Ran `ls -la`\n```\ntotal 0\n```\n\nCommand exited with code 2\n\n[Output truncated. Full output: /tmp/pi-bash-x.log]"
    );
    assert_eq!(
        text_of(&converted[1]),
        "The conversation history before this point was compacted into the following summary:\n\n<summary>\nthe summary\n</summary>"
    );
    assert_eq!(
        text_of(&converted[2]),
        "The following is a summary of a branch that this conversation came back from:\n\n<summary>\nbranch text</summary>"
    );
}

// ============================================================================
// 7. Bash execution records a session entry
// ============================================================================

#[tokio::test(flavor = "multi_thread")]
async fn execute_bash_records_bash_execution_message() {
    let fixture = tokio::task::spawn_blocking(|| make_fixture(vec![], FixtureOptions::default()))
        .await
        .expect("fixture");
    let session = fixture.session.clone();

    let chunks: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = chunks.clone();
    let result = session
        .execute_bash(
            "printf 'hi there'; exit 3",
            Some(Arc::new(move |chunk: &str| {
                sink.lock().push(chunk.to_string());
            })),
            Some(true),
        )
        .await
        .expect("bash");

    assert_eq!(result.output, "hi there");
    assert_eq!(result.exit_code, Some(3));
    assert!(!result.cancelled);
    assert!(!result.truncated);
    assert_eq!(chunks.lock().join(""), "hi there");

    // Persisted as a bashExecution message with oracle field order.
    let entries = session.with_session_manager(|sm| sm.get_entries());
    assert_eq!(entries.len(), 1);
    let pi_coding_agent::SessionEntry::Message { message, .. } = &entries[0] else {
        panic!("expected message entry");
    };
    assert_eq!(message["role"], "bashExecution");
    assert_eq!(message["command"], "printf 'hi there'; exit 3");
    assert_eq!(message["output"], "hi there");
    assert_eq!(message["exitCode"], 3);
    assert_eq!(message["excludeFromContext"], true);
    let keys: Vec<&String> = message.as_object().expect("object").keys().collect();
    assert_eq!(
        keys,
        vec![
            "role",
            "command",
            "output",
            "exitCode",
            "cancelled",
            "truncated",
            "timestamp",
            "excludeFromContext"
        ]
    );
}

// ============================================================================
// 8. Services construction order / shared instances
// ============================================================================

#[tokio::test(flavor = "multi_thread")]
async fn services_share_one_auth_storage_and_bind_agent_dir() {
    let services = tokio::task::spawn_blocking(|| {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cwd = tmp.path().join("proj");
        std::fs::create_dir_all(&cwd).expect("cwd");
        let agent_dir = tmp.path().join("agent");
        std::fs::create_dir_all(&agent_dir).expect("agent dir");
        std::fs::write(
            agent_dir.join("settings.json"),
            r#"{"defaultProvider": "anthropic"}"#,
        )
        .expect("settings seed");
        let services = create_agent_session_services(CreateAgentSessionServicesOptions {
            cwd: cwd.clone(),
            agent_dir: Some(agent_dir.clone()),
            ..Default::default()
        });
        assert_eq!(services.cwd, cwd.canonicalize().unwrap_or(cwd));
        // Settings were loaded from the provided agent dir.
        assert_eq!(
            services.settings_manager.lock().get_default_provider(),
            Some("anthropic")
        );
        (tmp, services)
    })
    .await
    .expect("services");
    let (_tmp, services) = services;

    // Registry and services expose the SAME live AuthStorage.
    let registry = services.model_registry.read().await;
    assert!(Arc::ptr_eq(&services.auth_storage, &registry.auth_storage));
}

// ============================================================================
// 9. Runtime switch/fork with lifecycle hooks
// ============================================================================

#[derive(Default)]
struct RecordingBridge {
    paths: Vec<PathBuf>,
    log: Mutex<Vec<String>>,
    cancel_next: std::sync::atomic::AtomicBool,
}

impl ExtensionBridge for RecordingBridge {
    fn needs_sidecar(&self) -> bool {
        false
    }
    fn discovered_paths(&self) -> &[PathBuf] {
        &self.paths
    }
    fn emit_lifecycle(
        &self,
        event: SessionLifecycleEvent,
        _signal: Option<pi_agent::CancellationToken>,
    ) -> std::pin::Pin<Box<dyn Future<Output = HookOutcome> + Send + 'static>> {
        let tag = match event {
            SessionLifecycleEvent::SessionStart { .. } => "session_start",
            SessionLifecycleEvent::SessionBeforeSwitch { .. } => "session_before_switch",
            SessionLifecycleEvent::SessionBeforeFork { .. } => "session_before_fork",
            SessionLifecycleEvent::SessionShutdown { .. } => "session_shutdown",
        };
        self.log.lock().push(tag.to_string());
        let outcome = if self.cancel_next.swap(false, Ordering::SeqCst) {
            HookOutcome::Cancel
        } else {
            HookOutcome::Continue
        };
        Box::pin(std::future::ready(outcome))
    }
}

fn runtime_factory() -> CreateRuntimeFactory {
    Arc::new(move |options: CreateRuntimeOptions| {
        Box::pin(async move {
            let services = tokio::task::spawn_blocking({
                let cwd = options.cwd.clone();
                let agent_dir = options.agent_dir.clone();
                move || {
                    create_agent_session_services(CreateAgentSessionServicesOptions {
                        cwd,
                        agent_dir: Some(agent_dir),
                        ..Default::default()
                    })
                }
            })
            .await
            .map_err(|e| e.to_string())?;

            let session_manager = options.session_manager;
            let settings_manager = services.settings_manager.clone();
            let model_registry = services.model_registry.clone();
            let cwd = services.cwd.clone();
            let session = tokio::task::spawn_blocking(move || {
                AgentSession::new(AgentSessionConfig {
                    session_manager,
                    settings_manager,
                    model_registry,
                    cwd,
                    stream_fn: Some(scripted_stream_fn(Arc::new(Mutex::new(VecDeque::new())))),
                    model: None,
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
                })
            })
            .await
            .map_err(|e| e.to_string())?;

            Ok(CreateRuntimeResult {
                session,
                services,
                diagnostics: Vec::new(),
                model_fallback_message: None,
            })
        })
    })
}

fn seeded_persisted_manager(
    cwd: &std::path::Path,
    session_dir: &std::path::Path,
) -> (SessionManager, String, String) {
    let mut sm = SessionManager::create(cwd, Some(session_dir.to_path_buf()), None)
        .expect("session manager");
    let user_id = sm
        .append_message(json!({
            "role": "user",
            "content": [{"type": "text", "text": "hello"}],
            "timestamp": 1
        }))
        .expect("user entry");
    let assistant_id = sm
        .append_message(json!({
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
        .expect("assistant entry");
    (sm, user_id, assistant_id)
}

#[tokio::test(flavor = "multi_thread")]
async fn runtime_switch_new_and_fork_replace_sessions() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let cwd = tmp.path().join("proj");
    std::fs::create_dir_all(&cwd).expect("cwd");
    let agent_dir = tmp.path().join("agent");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");
    let session_dir = tmp.path().join("sessions");
    std::fs::create_dir_all(&session_dir).expect("session dir");

    let (manager, user_id, assistant_id) = {
        let cwd = cwd.clone();
        let session_dir = session_dir.clone();
        tokio::task::spawn_blocking(move || seeded_persisted_manager(&cwd, &session_dir))
            .await
            .expect("seed")
    };
    let original_file = manager
        .get_session_file()
        .map(PathBuf::from)
        .expect("persisted file");
    assert!(original_file.exists(), "assistant flushes the file");

    let bridge = Arc::new(RecordingBridge::default());
    let runtime = AgentSessionRuntime::create(
        runtime_factory(),
        CreateRuntimeOptions {
            cwd: cwd.clone(),
            agent_dir: agent_dir.clone(),
            session_manager: manager,
            session_start_reason: pi_coding_agent::SessionStartReason::Startup,
            previous_session_file: None,
        },
        bridge.clone(),
    )
    .await
    .expect("runtime");

    let original_session_id = runtime.session().session_id();
    let never = pi_agent::CancellationToken::new();

    // Fork BEFORE a user message returns its text and replaces the session.
    let fork = runtime
        .fork(&user_id, ForkPosition::Before, &never)
        .await
        .expect("fork");
    assert!(!fork.cancelled);
    assert_eq!(fork.selected_text.as_deref(), Some("hello"));
    assert_eq!(
        fork.previous_session_file.as_deref(),
        Some(original_file.as_path()),
        "fork reports the replaced session file"
    );
    let forked_session = runtime.session();
    assert_ne!(forked_session.session_id(), original_session_id);
    // user message had no parent → brand-new session parented on the old file.
    assert_eq!(
        forked_session.with_session_manager(|sm| sm.get_entries().len()),
        0
    );
    assert_eq!(
        bridge.log.lock().as_slice(),
        &["session_before_fork", "session_shutdown"],
        "hook order for fork"
    );
    bridge.log.lock().clear();

    // Switch back to the original session file.
    let switch = runtime
        .switch_session(&original_file, None, &never)
        .await
        .expect("switch");
    assert!(!switch.cancelled);
    assert_eq!(runtime.session().session_id(), original_session_id);
    assert_eq!(
        runtime
            .session()
            .with_session_manager(|sm| sm.get_entries().len()),
        2
    );
    assert_eq!(
        bridge.log.lock().as_slice(),
        &["session_before_switch", "session_shutdown"]
    );
    bridge.log.lock().clear();

    // Fork AT the assistant entry branches the file with re-chained entries.
    let fork = runtime
        .fork(&assistant_id, ForkPosition::At, &never)
        .await
        .expect("fork at");
    assert!(!fork.cancelled);
    assert_eq!(fork.selected_text, None);
    let branched = runtime.session();
    assert_ne!(branched.session_id(), original_session_id);
    let branched_file = branched.session_file().expect("branched file");
    assert_ne!(branched_file, original_file);
    assert!(branched_file.exists(), "branch with assistant flushes");
    let entries = branched.with_session_manager(|sm| sm.get_entries());
    assert_eq!(entries.len(), 2);
    assert!(
        entries[0].parent_id().as_option().is_none(),
        "re-chained root"
    );
    assert_eq!(
        entries[1].parent_id().as_option().map(String::as_str),
        entries[0].id()
    );
    bridge.log.lock().clear();

    // A cancelling hook aborts the switch and keeps the session.
    bridge.cancel_next.store(true, Ordering::SeqCst);
    let cancelled = runtime
        .switch_session(&original_file, None, &never)
        .await
        .expect("cancelled switch");
    assert!(cancelled.cancelled);
    assert_eq!(cancelled.previous_session_file, None);
    assert_eq!(runtime.session().session_id(), branched.session_id());
    bridge.log.lock().clear();

    // A pre-cancelled request token aborts the switch BEFORE any teardown:
    // no hooks fire and the session is untouched.
    let cancelled_token = pi_agent::CancellationToken::new();
    cancelled_token.cancel();
    let aborted = runtime
        .switch_session(&original_file, None, &cancelled_token)
        .await
        .expect("token-cancelled switch");
    assert!(aborted.cancelled);
    assert_eq!(runtime.session().session_id(), branched.session_id());
    assert!(
        bridge.log.lock().is_empty(),
        "a pre-cancelled token stops the replacement before any hook"
    );

    // new_session replaces with a fresh persisted session.
    let fresh = runtime.new_session(None, &never).await.expect("new session");
    assert!(!fresh.cancelled);
    assert_ne!(runtime.session().session_id(), branched.session_id());
    assert_eq!(
        runtime
            .session()
            .with_session_manager(|sm| sm.get_entries().len()),
        0
    );

    runtime.dispose();
}

// ============================================================================
// 9. Causal regressions: turn-history refresh, settle order, atomic run claim
// ============================================================================

/// Stream fn capturing the LLM `Context` of every provider call, then
/// replaying a script (error message when exhausted, like `scripted_stream_fn`).
fn capturing_stream_fn(
    script: Arc<Mutex<VecDeque<AssistantMessage>>>,
    contexts: Arc<Mutex<Vec<pi_ai::Context>>>,
) -> StreamFn {
    Arc::new(move |model: Model, context, _options| {
        let script = script.clone();
        let contexts = contexts.clone();
        Box::pin(async move {
            contexts.lock().push(context);
            let stream = pi_ai::create_assistant_message_event_stream();
            let message = script.lock().pop_front().unwrap_or_else(|| {
                let mut error = assistant_text_message(&model, "");
                error.stop_reason = StopReason::Error;
                error.error_message = Some("script exhausted".to_string());
                error
            });
            stream.push(AssistantMessageEvent::Done {
                reason: message.stop_reason,
                message,
            });
            stream
        })
    })
}

fn context_roles(context: &pi_ai::Context) -> Vec<&'static str> {
    context
        .messages
        .iter()
        .map(|m| match m {
            Message::User(_) => "user",
            Message::Assistant(_) => "assistant",
            _ => "toolResult",
        })
        .collect()
}

/// prepare_next_turn must preserve the turn's accumulated messages: the
/// second provider call of a tool loop sees user + assistant(toolCall) +
/// toolResult, not an empty history (agent-session.ts:483-489 spreads
/// `turn.context`).
#[tokio::test(flavor = "multi_thread")]
async fn tool_loop_second_provider_call_receives_turn_history() {
    let contexts: Arc<Mutex<Vec<pi_ai::Context>>> = Arc::new(Mutex::new(Vec::new()));
    let fixture = {
        let contexts = contexts.clone();
        tokio::task::spawn_blocking(move || {
            let script = Arc::new(Mutex::new(VecDeque::new()));
            let mut options = FixtureOptions {
                stream_fn: Some(capturing_stream_fn(script.clone(), contexts)),
                ..Default::default()
            };
            options.custom_tools = vec![noop_tool("echo")];
            let fixture = make_fixture(vec![], options);
            // Call 1: assistant issues a tool call; call 2: plain text.
            let mut tool_call_reply = assistant_text_message(&fixture.model, "calling echo");
            tool_call_reply
                .content
                .push(Content::ToolCall(pi_ai::ToolCall {
                    id: "call-1".to_string(),
                    name: "echo".to_string(),
                    arguments: serde_json::Map::new(),
                    thought_signature: None,
                }));
            tool_call_reply.stop_reason = StopReason::ToolUse;
            script.lock().push_back(tool_call_reply);
            script
                .lock()
                .push_back(assistant_text_message(&fixture.model, "done"));
            fixture
        })
        .await
        .expect("fixture")
    };
    let session = fixture.session.clone();

    session
        .prompt("use the tool", PromptOptions::default())
        .await
        .expect("prompt");

    let contexts = contexts.lock();
    assert_eq!(contexts.len(), 2, "tool loop makes two provider calls");
    assert_eq!(context_roles(&contexts[0]), vec!["user"]);
    // Mutation guard: a prepare_next_turn that wipes messages makes this [].
    assert_eq!(
        context_roles(&contexts[1]),
        vec!["user", "assistant", "toolResult"],
        "assistant + tool result history must reach provider call 2"
    );
    let Message::Assistant(assistant) = &contexts[1].messages[1] else {
        panic!("expected assistant message");
    };
    assert!(
        assistant.content.iter().any(|c| matches!(
            c,
            Content::ToolCall(tc) if tc.id == "call-1"
        )),
        "call-2 history carries the original tool call"
    );
    assert_eq!(session.get_last_assistant_text().as_deref(), Some("done"));
}

/// agent_settled listeners must observe an idle session (oracle
/// `_emitAgentSettled` clears the active flag before emitting,
/// agent-session.ts:534-541) and must be able to immediately start the next
/// prompt from the handler.
#[tokio::test(flavor = "multi_thread")]
async fn agent_settled_listener_observes_idle_session() {
    let fixture = tokio::task::spawn_blocking(|| {
        let script = Arc::new(Mutex::new(VecDeque::new()));
        let options = FixtureOptions {
            stream_fn: Some(scripted_stream_fn(script.clone())),
            ..Default::default()
        };
        let fixture = make_fixture(vec![], options);
        script
            .lock()
            .push_back(assistant_text_message(&fixture.model, "hi"));
        script
            .lock()
            .push_back(assistant_text_message(&fixture.model, "again"));
        fixture
    })
    .await
    .expect("fixture");
    let session = fixture.session.clone();

    let observed: Arc<Mutex<Vec<bool>>> = Arc::new(Mutex::new(Vec::new()));
    type PromptHandle = tokio::task::JoinHandle<Result<(), String>>;
    let listener_prompt: Arc<Mutex<Option<PromptHandle>>> = Arc::new(Mutex::new(None));
    let listener_session = session.clone();
    let listener_observed = observed.clone();
    let listener_prompt_slot = listener_prompt.clone();
    let fired = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let unsubscribe = session.subscribe(Arc::new(move |event: &AgentSessionEvent| {
        if matches!(event, AgentSessionEvent::AgentSettled) && !fired.swap(true, Ordering::SeqCst) {
            listener_observed.lock().push(listener_session.is_idle());
            // The user-visible contract: a listener can start the next
            // prompt the moment agent_settled fires.
            let prompt_session = listener_session.clone();
            listener_prompt_slot
                .lock()
                .replace(tokio::spawn(async move {
                    prompt_session
                        .prompt("from listener", PromptOptions::default())
                        .await
                }));
        }
    }));
    std::mem::forget(unsubscribe);

    session
        .prompt("hello", PromptOptions::default())
        .await
        .expect("prompt");

    // Mutation guard: emitting agent_settled before releasing the run makes
    // the listener observe a still-streaming session ([false]) and the
    // listener-issued prompt fail with the already-processing error.
    assert_eq!(
        observed.lock().as_slice(),
        &[true],
        "agent_settled listener must observe idle"
    );
    let handle = listener_prompt.lock().take().expect("listener prompted");
    handle
        .await
        .expect("join listener prompt")
        .expect("listener prompt succeeds");
    session.wait_for_idle().await;
    assert_eq!(session.get_last_assistant_text().as_deref(), Some("again"));
}

/// Prompt preflight and run claim are one atomic step: a second concurrent
/// prompt issued while the first is still inside preflight deterministically
/// gets the verbatim already-processing error and is never silently dropped.
/// The first prompt's `preflight_result` callback is the barrier: it fires
/// exactly in the historical gap between preflight and run start.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_prompt_in_preflight_window_gets_verbatim_error() {
    let fixture = tokio::task::spawn_blocking(|| {
        let script = Arc::new(Mutex::new(VecDeque::new()));
        let options = FixtureOptions {
            stream_fn: Some(scripted_stream_fn(script.clone())),
            ..Default::default()
        };
        let fixture = make_fixture(vec![], options);
        script
            .lock()
            .push_back(assistant_text_message(&fixture.model, "only"));
        fixture
    })
    .await
    .expect("fixture");
    let session = fixture.session.clone();

    let (entered_tx, entered_rx) = std::sync::mpsc::channel::<()>();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();

    let first_session = session.clone();
    let first = tokio::spawn(async move {
        first_session
            .prompt(
                "first",
                PromptOptions {
                    preflight_result: Some(Box::new(move |accepted| {
                        assert!(accepted, "first prompt passes preflight");
                        entered_tx.send(()).expect("signal entered");
                        // Hold the preflight→run window open until released.
                        release_rx.recv().expect("await release");
                    })),
                    ..Default::default()
                },
            )
            .await
    });

    // Barrier: first prompt has passed preflight and is parked pre-run.
    tokio::task::spawn_blocking(move || entered_rx.recv().expect("entered"))
        .await
        .expect("join");

    // Second prompt: deterministic verbatim rejection, never a silent drop.
    let error = session
        .prompt("second", PromptOptions::default())
        .await
        .expect_err("concurrent prompt must be rejected");
    assert_eq!(
        error,
        "Agent is already processing. Specify streamingBehavior ('steer' or 'followUp') to queue the message."
    );
    assert!(
        session.is_streaming(),
        "run claim is visible during preflight hold"
    );

    // Steer/follow-up queueing still works while the claim is held.
    session
        .prompt(
            "queued follow-up",
            PromptOptions {
                streaming_behavior: Some(StreamingBehavior::FollowUp),
                ..Default::default()
            },
        )
        .await
        .expect("follow-up queues");
    assert_eq!(session.get_follow_up_messages(), vec!["queued follow-up"]);
    session.clear_queue();

    release_tx.send(()).expect("release first prompt");
    first.await.expect("join first").expect("first prompt runs");
    session.wait_for_idle().await;

    // Exactly the first exchange landed; the rejected prompt left no trace.
    let events = fixture.events.lock().clone();
    let agent_starts = events.iter().filter(|e| e["type"] == "agent_start").count();
    assert_eq!(agent_starts, 1, "exactly one run started");
    assert_eq!(
        session.messages().len(),
        2,
        "user 'first' + assistant reply"
    );
}
