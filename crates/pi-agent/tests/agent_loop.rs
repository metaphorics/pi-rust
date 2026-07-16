//! Behavior fixtures ported from packages/agent/test/agent-loop.test.ts.
//! Scripted mock streams only — no network.

use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

use parking_lot::Mutex;
use pi_agent::{
    AgentContext, AgentEvent, AgentLoopConfig, AgentMessage, AgentToolResult, CancellationToken,
    ToolDefinition, ToolExecutionMode, collecting_sink, identity_convert_to_llm_fn,
    run_agent_loop, run_agent_loop_continue, text_content,
};
use pi_ai::{
    Api, AssistantMessage, AssistantMessageEvent, Content, Context, Message, Model, ModelCost,
    ModelCostRates, ModelInput, StopReason, ToolCall, Usage, UserContent, UserMessage,
    create_assistant_message_event_stream,
};
use serde_json::{Value, json};

fn create_usage() -> Usage {
    Usage::default()
}

fn create_model() -> Model {
    Model {
        id: "mock".into(),
        name: "mock".into(),
        api: Api("openai-responses".into()),
        provider: "openai".into(),
        base_url: "https://example.invalid".into(),
        reasoning: false,
        thinking_level_map: None,
        input: vec![ModelInput::Text],
        cost: ModelCost {
            input: 0.0,
            output: 0.0,
            cache_read: 0.0,
            cache_write: 0.0,
            ..Default::default()
        },
        context_window: 8192,
        max_tokens: 2048,
        headers: None,
        compat: None,
    }
}

fn create_assistant_message(
    content: Vec<Content>,
    stop_reason: StopReason,
) -> AssistantMessage {
    AssistantMessage {
        content,
        api: Api("openai-responses".into()),
        provider: "openai".into(),
        model: "mock".into(),
        response_model: None,
        response_id: None,
        diagnostics: None,
        usage: create_usage(),
        stop_reason,
        error_message: None,
        timestamp: 1,
    }
}

fn create_user_message(text: &str) -> AgentMessage {
    AgentMessage::user(UserMessage {
        content: UserContent::Text(text.into()),
        timestamp: 1,
    })
}

fn tool_call(id: &str, name: &str, arguments: Value) -> Content {
    let map = arguments.as_object().cloned().unwrap_or_default();
    Content::ToolCall(ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: map,
        thought_signature: None,
    })
}

fn text_block(text: &str) -> Content {
    text_content(text)
}

fn scripted_stream_fn(
    scripts: Vec<Vec<AssistantMessageEvent>>,
) -> pi_agent::StreamFn {
    let call_index = Arc::new(AtomicUsize::new(0));
    let scripts = Arc::new(scripts);
    Arc::new(move |_model, _context, _options| {
        let idx = call_index.fetch_add(1, Ordering::SeqCst);
        let events = scripts
            .get(idx)
            .cloned()
            .unwrap_or_else(|| {
                vec![AssistantMessageEvent::Done {
                    reason: StopReason::Stop,
                    message: create_assistant_message(
                        vec![text_block("fallback")],
                        StopReason::Stop,
                    ),
                }]
            });
        Box::pin(async move {
            let stream = create_assistant_message_event_stream();
            let stream2 = stream.clone();
            tokio::spawn(async move {
                for event in events {
                    stream2.push(event);
                }
            });
            stream
        })
    })
}

fn done_message(message: AssistantMessage) -> AssistantMessageEvent {
    let reason = message.stop_reason;
    AssistantMessageEvent::Done { reason, message }
}

fn object_schema(props: Value, required: &[&str]) -> Value {
    json!({
        "type": "object",
        "properties": props,
        "required": required,
    })
}

fn echo_tool(
    executed: Arc<Mutex<Vec<String>>>,
) -> Arc<ToolDefinition> {
    let executed2 = executed.clone();
    Arc::new(ToolDefinition {
        name: "echo".into(),
        label: "Echo".into(),
        description: "Echo tool".into(),
        parameters: object_schema(json!({"value": {"type": "string"}}), &["value"]),
        execution_mode: None,
        prepare_arguments: None,
        execute: Arc::new(move |_id, params, _cancel, _update| {
            let executed = executed2.clone();
            Box::pin(async move {
                let value = params
                    .get("value")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                executed.lock().push(value.clone());
                Ok(AgentToolResult {
                    content: vec![text_content(format!("echoed: {value}"))],
                    details: json!({ "value": value }),
                    added_tool_names: None,
                    terminate: None,
                })
            })
        }),
        renderer: None,
    })
}

fn base_config() -> AgentLoopConfig {
    AgentLoopConfig::new(create_model(), identity_convert_to_llm_fn())
}

fn event_types(events: &[AgentEvent]) -> Vec<&'static str> {
    events.iter().map(AgentEvent::event_type).collect()
}

#[tokio::test]
async fn emits_events_with_agent_message_types() {
    let context = AgentContext {
        system_prompt: "You are helpful.".into(),
        messages: vec![],
        tools: vec![],
    };
    let user = create_user_message("Hello");
    let message = create_assistant_message(vec![text_block("Hi there!")], StopReason::Stop);
    let stream_fn = scripted_stream_fn(vec![vec![done_message(message)]]);
    let events = Arc::new(Mutex::new(Vec::new()));
    let messages = run_agent_loop(
        vec![user],
        context,
        base_config(),
        collecting_sink(events.clone()),
        None,
        stream_fn,
    )
    .await;

    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].role(), "user");
    assert_eq!(messages[1].role(), "assistant");

    let types = event_types(&events.lock());
    for expected in [
        "agent_start",
        "turn_start",
        "message_start",
        "message_end",
        "turn_end",
        "agent_end",
    ] {
        assert!(types.contains(&expected), "missing {expected} in {types:?}");
    }
}

#[tokio::test]
async fn convert_to_llm_filters_custom_messages() {
    let notification = AgentMessage::Custom(json!({
        "role": "notification",
        "text": "This is a notification",
        "timestamp": 1,
    }));
    let context = AgentContext {
        system_prompt: "You are helpful.".into(),
        messages: vec![notification],
        tools: vec![],
    };
    let converted = Arc::new(Mutex::new(Vec::<Message>::new()));
    let converted2 = converted.clone();
    let config = AgentLoopConfig {
        convert_to_llm: Arc::new(move |messages| {
            let converted2 = converted2.clone();
            Box::pin(async move {
                let llm: Vec<Message> = messages
                    .into_iter()
                    .filter_map(|m| {
                        if m.role() == "notification" {
                            None
                        } else {
                            m.into_message()
                        }
                    })
                    .collect();
                *converted2.lock() = llm.clone();
                llm
            })
        }),
        ..base_config()
    };
    let message = create_assistant_message(vec![text_block("Response")], StopReason::Stop);
    let stream_fn = scripted_stream_fn(vec![vec![done_message(message)]]);
    let events = Arc::new(Mutex::new(Vec::new()));
    let _ = run_agent_loop(
        vec![create_user_message("Hello")],
        context,
        config,
        collecting_sink(events),
        None,
        stream_fn,
    )
    .await;

    let converted = converted.lock();
    assert_eq!(converted.len(), 1);
    assert!(matches!(converted[0], Message::User(_)));
}

#[tokio::test]
async fn transform_context_runs_before_convert_to_llm() {
    let context = AgentContext {
        system_prompt: "You are helpful.".into(),
        messages: vec![
            create_user_message("old message 1"),
            AgentMessage::assistant(create_assistant_message(
                vec![text_block("old response 1")],
                StopReason::Stop,
            )),
            create_user_message("old message 2"),
            AgentMessage::assistant(create_assistant_message(
                vec![text_block("old response 2")],
                StopReason::Stop,
            )),
        ],
        tools: vec![],
    };
    let transformed_len = Arc::new(AtomicUsize::new(0));
    let converted_len = Arc::new(AtomicUsize::new(0));
    let tlen = transformed_len.clone();
    let clen = converted_len.clone();
    let config = AgentLoopConfig {
        transform_context: Some(Arc::new(move |messages, _| {
            let tlen = tlen.clone();
            Box::pin(async move {
                let pruned: Vec<_> = messages.into_iter().rev().take(2).collect::<Vec<_>>();
                let pruned: Vec<_> = pruned.into_iter().rev().collect();
                tlen.store(pruned.len(), Ordering::SeqCst);
                pruned
            })
        })),
        convert_to_llm: Arc::new(move |messages| {
            let clen = clen.clone();
            Box::pin(async move {
                let llm = pi_agent::identity_convert_to_llm(messages);
                clen.store(llm.len(), Ordering::SeqCst);
                llm
            })
        }),
        ..base_config()
    };
    let stream_fn = scripted_stream_fn(vec![vec![done_message(create_assistant_message(
        vec![text_block("Response")],
        StopReason::Stop,
    ))]]);
    let events = Arc::new(Mutex::new(Vec::new()));
    let _ = run_agent_loop(
        vec![create_user_message("new message")],
        context,
        config,
        collecting_sink(events),
        None,
        stream_fn,
    )
    .await;
    assert_eq!(transformed_len.load(Ordering::SeqCst), 2);
    assert_eq!(converted_len.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn handles_tool_calls_and_results() {
    let executed = Arc::new(Mutex::new(Vec::new()));
    let context = AgentContext {
        system_prompt: String::new(),
        messages: vec![],
        tools: vec![echo_tool(executed.clone())],
    };
    let first = create_assistant_message(
        vec![tool_call("tool-1", "echo", json!({"value": "hello"}))],
        StopReason::ToolUse,
    );
    let second = create_assistant_message(vec![text_block("done")], StopReason::Stop);
    let stream_fn = scripted_stream_fn(vec![vec![done_message(first)], vec![done_message(second)]]);
    let events = Arc::new(Mutex::new(Vec::new()));
    let _ = run_agent_loop(
        vec![create_user_message("echo something")],
        context,
        base_config(),
        collecting_sink(events.clone()),
        None,
        stream_fn,
    )
    .await;

    assert_eq!(*executed.lock(), vec!["hello".to_string()]);
    let events = events.lock();
    assert!(events.iter().any(|e| matches!(e, AgentEvent::ToolExecutionStart { .. })));
    let tool_end = events
        .iter()
        .find(|e| matches!(e, AgentEvent::ToolExecutionEnd { .. }));
    match tool_end {
        Some(AgentEvent::ToolExecutionEnd { is_error, .. }) => assert!(!*is_error),
        _ => panic!("missing tool_execution_end"),
    }
}

#[tokio::test]
async fn does_not_execute_tool_calls_from_length_truncated_message() {
    let executed = Arc::new(Mutex::new(Vec::new()));
    let context = AgentContext {
        system_prompt: String::new(),
        messages: vec![],
        tools: vec![echo_tool(executed.clone())],
    };
    let first = create_assistant_message(
        vec![tool_call("tool-1", "echo", json!({"value": "hel"}))],
        StopReason::Length,
    );
    let second = create_assistant_message(vec![text_block("done")], StopReason::Stop);
    let stream_fn = scripted_stream_fn(vec![vec![done_message(first)], vec![done_message(second)]]);
    let events = Arc::new(Mutex::new(Vec::new()));
    let messages = run_agent_loop(
        vec![create_user_message("echo something")],
        context,
        base_config(),
        collecting_sink(events.clone()),
        None,
        stream_fn,
    )
    .await;

    assert!(executed.lock().is_empty());
    let events = events.lock();
    match events
        .iter()
        .find(|e| matches!(e, AgentEvent::ToolExecutionEnd { .. }))
    {
        Some(AgentEvent::ToolExecutionEnd {
            is_error, result, ..
        }) => {
            assert!(*is_error);
            let text = result
                .content
                .iter()
                .find_map(|c| match c {
                    Content::Text(t) => Some(t.text.as_string()),
                    _ => None,
                })
                .unwrap_or_default();
            assert!(text.contains("output token limit"));
        }
        _ => panic!("missing tool end"),
    }
    assert_eq!(messages.last().map(AgentMessage::role), Some("assistant"));
}

#[tokio::test]
async fn executes_mutated_before_tool_call_args() {
    let config = AgentLoopConfig {
        before_tool_call: Some(Arc::new(|ctx, _| {
            Box::pin(async move {
                if let Value::Object(map) = &mut *ctx.args.lock() {
                    map.insert("value".into(), json!(123));
                }
                None
            })
        })),
        ..base_config()
    };
    // echo tool expects string; validation already passed with "hello". Mutation to 123
    // is intentionally not revalidated (matches TS).
    let executed_any = Arc::new(Mutex::new(Vec::<Value>::new()));
    let executed_any2 = executed_any.clone();
    let tool = Arc::new(ToolDefinition {
        name: "echo".into(),
        label: "Echo".into(),
        description: "Echo tool".into(),
        parameters: object_schema(json!({"value": {"type": "string"}}), &["value"]),
        execution_mode: None,
        prepare_arguments: None,
        execute: Arc::new(move |_id, params, _c, _u| {
            let executed_any2 = executed_any2.clone();
            Box::pin(async move {
                executed_any2
                    .lock()
                    .push(params.get("value").cloned().unwrap_or(Value::Null));
                Ok(AgentToolResult::text("ok"))
            })
        }),
        renderer: None,
    });
    let context = AgentContext {
        system_prompt: String::new(),
        messages: vec![],
        tools: vec![tool],
    };
    let first = create_assistant_message(
        vec![tool_call("tool-1", "echo", json!({"value": "hello"}))],
        StopReason::ToolUse,
    );
    let second = create_assistant_message(vec![text_block("done")], StopReason::Stop);
    let stream_fn = scripted_stream_fn(vec![vec![done_message(first)], vec![done_message(second)]]);
    let events = Arc::new(Mutex::new(Vec::new()));
    let _ = run_agent_loop(
        vec![create_user_message("echo something")],
        context,
        config,
        collecting_sink(events),
        None,
        stream_fn,
    )
    .await;
    assert_eq!(*executed_any.lock(), vec![json!(123)]);
}

#[tokio::test]
async fn prepare_arguments_before_validation() {
    let executed = Arc::new(Mutex::new(Vec::<Vec<Value>>::new()));
    let executed2 = executed.clone();
    let tool = Arc::new(ToolDefinition {
        name: "edit".into(),
        label: "Edit".into(),
        description: "Edit tool".into(),
        parameters: object_schema(
            json!({
                "edits": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "oldText": {"type": "string"},
                            "newText": {"type": "string"}
                        },
                        "required": ["oldText", "newText"]
                    }
                }
            }),
            &["edits"],
        ),
        execution_mode: None,
        prepare_arguments: Some(Arc::new(|args| {
            let obj = args.as_object().cloned().unwrap_or_default();
            if obj.get("oldText").and_then(Value::as_str).is_some()
                && obj.get("newText").and_then(Value::as_str).is_some()
            {
                let mut edits = obj
                    .get("edits")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                edits.push(json!({
                    "oldText": obj["oldText"],
                    "newText": obj["newText"],
                }));
                return json!({ "edits": edits });
            }
            args
        })),
        execute: Arc::new(move |_id, params, _c, _u| {
            let executed2 = executed2.clone();
            Box::pin(async move {
                let edits = params
                    .get("edits")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                executed2.lock().push(edits.clone());
                Ok(AgentToolResult {
                    content: vec![text_content(format!("edited {}", edits.len()))],
                    details: json!({ "count": edits.len() }),
                    added_tool_names: None,
                    terminate: None,
                })
            })
        }),
        renderer: None,
    });
    let context = AgentContext {
        system_prompt: String::new(),
        messages: vec![],
        tools: vec![tool],
    };
    let first = create_assistant_message(
        vec![tool_call(
            "tool-1",
            "edit",
            json!({"oldText": "before", "newText": "after"}),
        )],
        StopReason::ToolUse,
    );
    let second = create_assistant_message(vec![text_block("done")], StopReason::Stop);
    let stream_fn = scripted_stream_fn(vec![vec![done_message(first)], vec![done_message(second)]]);
    let events = Arc::new(Mutex::new(Vec::new()));
    let _ = run_agent_loop(
        vec![create_user_message("edit something")],
        context,
        base_config(),
        collecting_sink(events),
        None,
        stream_fn,
    )
    .await;
    assert_eq!(
        *executed.lock(),
        vec![vec![json!({"oldText": "before", "newText": "after"})]]
    );
}

#[tokio::test]
async fn parallel_tool_execution_end_order_and_result_source_order() {
    let first_resolved = Arc::new(AtomicBool::new(false));
    let parallel_observed = Arc::new(AtomicBool::new(false));
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let release = Arc::new(Mutex::new(Some(tx)));
    let rx = Arc::new(Mutex::new(Some(rx)));

    let first_resolved2 = first_resolved.clone();
    let parallel_observed2 = parallel_observed.clone();
    let rx2 = rx.clone();
    let tool = Arc::new(ToolDefinition {
        name: "echo".into(),
        label: "Echo".into(),
        description: "Echo tool".into(),
        parameters: object_schema(json!({"value": {"type": "string"}}), &["value"]),
        execution_mode: None,
        prepare_arguments: None,
        execute: Arc::new(move |_id, params, _c, _u| {
            let first_resolved2 = first_resolved2.clone();
            let parallel_observed2 = parallel_observed2.clone();
            let rx2 = rx2.clone();
            Box::pin(async move {
                let value = params
                    .get("value")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                if value == "first" {
                    let rx = rx2.lock().take();
                    if let Some(rx) = rx {
                        let _ = rx.await;
                        first_resolved2.store(true, Ordering::SeqCst);
                    }
                }
                if value == "second" && !first_resolved2.load(Ordering::SeqCst) {
                    parallel_observed2.store(true, Ordering::SeqCst);
                }
                Ok(AgentToolResult {
                    content: vec![text_content(format!("echoed: {value}"))],
                    details: json!({ "value": value }),
                    added_tool_names: None,
                    terminate: None,
                })
            })
        }),
        renderer: None,
    });

    let context = AgentContext {
        system_prompt: String::new(),
        messages: vec![],
        tools: vec![tool],
    };
    let mut config = base_config();
    config.tool_execution = ToolExecutionMode::Parallel;

    let first = create_assistant_message(
        vec![
            tool_call("tool-1", "echo", json!({"value": "first"})),
            tool_call("tool-2", "echo", json!({"value": "second"})),
        ],
        StopReason::ToolUse,
    );
    let second = create_assistant_message(vec![text_block("done")], StopReason::Stop);
    let stream_fn = scripted_stream_fn(vec![vec![done_message(first)], vec![done_message(second)]]);
    let events = Arc::new(Mutex::new(Vec::new()));

    let run = tokio::spawn({
        let events = events.clone();
        async move {
            run_agent_loop(
                vec![create_user_message("echo both")],
                context,
                config,
                collecting_sink(events),
                None,
                stream_fn,
            )
            .await
        }
    });

    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    if let Some(tx) = release.lock().take() {
        let _ = tx.send(());
    }
    let _ = run.await.unwrap();

    assert!(parallel_observed.load(Ordering::SeqCst));
    let events = events.lock();
    let tool_end_ids: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ToolExecutionEnd { tool_call_id, .. } => Some(tool_call_id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(tool_end_ids, ["tool-2", "tool-1"]);

    let tool_result_ids: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::MessageEnd {
                message: AgentMessage::Standard(Message::ToolResult(tr)),
            } => Some(tr.tool_call_id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(tool_result_ids, ["tool-1", "tool-2"]);
}

#[tokio::test]
async fn sequential_when_tool_execution_mode_sequential() {
    let first_resolved = Arc::new(AtomicBool::new(false));
    let parallel_observed = Arc::new(AtomicBool::new(false));
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let release = Arc::new(Mutex::new(Some(tx)));
    let rx = Arc::new(Mutex::new(Some(rx)));

    let first_resolved2 = first_resolved.clone();
    let parallel_observed2 = parallel_observed.clone();
    let rx2 = rx.clone();
    let tool = Arc::new(ToolDefinition {
        name: "slow".into(),
        label: "Slow".into(),
        description: "Slow tool".into(),
        parameters: object_schema(json!({"value": {"type": "string"}}), &["value"]),
        execution_mode: Some(ToolExecutionMode::Sequential),
        prepare_arguments: None,
        execute: Arc::new(move |_id, params, _c, _u| {
            let first_resolved2 = first_resolved2.clone();
            let parallel_observed2 = parallel_observed2.clone();
            let rx2 = rx2.clone();
            Box::pin(async move {
                let value = params
                    .get("value")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                if value == "first" {
                    let rx = rx2.lock().take();
                    if let Some(rx) = rx {
                        let _ = rx.await;
                        first_resolved2.store(true, Ordering::SeqCst);
                    }
                }
                if value == "second" && !first_resolved2.load(Ordering::SeqCst) {
                    parallel_observed2.store(true, Ordering::SeqCst);
                }
                Ok(AgentToolResult {
                    content: vec![text_content(format!("slow: {value}"))],
                    details: json!({ "value": value }),
                    added_tool_names: None,
                    terminate: None,
                })
            })
        }),
        renderer: None,
    });

    let context = AgentContext {
        system_prompt: String::new(),
        messages: vec![],
        tools: vec![tool],
    };
    // default config is parallel, but tool forces sequential
    let first = create_assistant_message(
        vec![
            tool_call("tool-1", "slow", json!({"value": "first"})),
            tool_call("tool-2", "slow", json!({"value": "second"})),
        ],
        StopReason::ToolUse,
    );
    let second = create_assistant_message(vec![text_block("done")], StopReason::Stop);
    let stream_fn = scripted_stream_fn(vec![vec![done_message(first)], vec![done_message(second)]]);
    let events = Arc::new(Mutex::new(Vec::new()));
    let run = tokio::spawn({
        let events = events.clone();
        async move {
            run_agent_loop(
                vec![create_user_message("run both")],
                context,
                base_config(),
                collecting_sink(events),
                None,
                stream_fn,
            )
            .await
        }
    });
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    if let Some(tx) = release.lock().take() {
        let _ = tx.send(());
    }
    let _ = run.await.unwrap();
    assert!(!parallel_observed.load(Ordering::SeqCst));
}

#[tokio::test]
async fn terminate_true_stops_after_tool_batch() {
    let tool = Arc::new(ToolDefinition {
        name: "echo".into(),
        label: "Echo".into(),
        description: "Echo tool".into(),
        parameters: object_schema(json!({"value": {"type": "string"}}), &["value"]),
        execution_mode: None,
        prepare_arguments: None,
        execute: Arc::new(move |_id, params, _c, _u| {
            Box::pin(async move {
                let value = params
                    .get("value")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                Ok(AgentToolResult {
                    content: vec![text_content(format!("echoed: {value}"))],
                    details: json!({ "value": value }),
                    added_tool_names: None,
                    terminate: Some(true),
                })
            })
        }),
        renderer: None,
    });
    let context = AgentContext {
        system_prompt: String::new(),
        messages: vec![],
        tools: vec![tool],
    };
    let llm_calls = Arc::new(AtomicUsize::new(0));
    let llm_calls2 = llm_calls.clone();
    let stream_fn: pi_agent::StreamFn = Arc::new(move |_model, _ctx, _opt| {
        llm_calls2.fetch_add(1, Ordering::SeqCst);
        let message = create_assistant_message(
            vec![tool_call("tool-1", "echo", json!({"value": "hello"}))],
            StopReason::ToolUse,
        );
        Box::pin(async move {
            let stream = create_assistant_message_event_stream();
            let s = stream.clone();
            tokio::spawn(async move {
                s.push(done_message(message));
            });
            stream
        })
    });
    let events = Arc::new(Mutex::new(Vec::new()));
    let messages = run_agent_loop(
        vec![create_user_message("echo something")],
        context,
        base_config(),
        collecting_sink(events.clone()),
        None,
        stream_fn,
    )
    .await;
    assert_eq!(llm_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        messages
            .iter()
            .map(AgentMessage::role)
            .collect::<Vec<_>>(),
        vec!["user", "assistant", "toolResult"]
    );
    assert_eq!(
        events
            .lock()
            .iter()
            .filter(|e| matches!(e, AgentEvent::TurnEnd { .. }))
            .count(),
        1
    );
}

#[tokio::test]
async fn after_tool_call_can_mark_terminate() {
    let tool = Arc::new(ToolDefinition {
        name: "echo".into(),
        label: "Echo".into(),
        description: "Echo tool".into(),
        parameters: object_schema(json!({"value": {"type": "string"}}), &["value"]),
        execution_mode: None,
        prepare_arguments: None,
        execute: Arc::new(move |_id, params, _c, _u| {
            Box::pin(async move {
                let value = params
                    .get("value")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                Ok(AgentToolResult {
                    content: vec![text_content(format!("echoed: {value}"))],
                    details: json!({ "value": value }),
                    added_tool_names: None,
                    terminate: None,
                })
            })
        }),
        renderer: None,
    });
    let context = AgentContext {
        system_prompt: String::new(),
        messages: vec![],
        tools: vec![tool],
    };
    let config = AgentLoopConfig {
        after_tool_call: Some(Arc::new(|_ctx, _| {
            Box::pin(async move {
                Some(pi_agent::AfterToolCallResult {
                    terminate: Some(true),
                    ..Default::default()
                })
            })
        })),
        ..base_config()
    };
    let llm_calls = Arc::new(AtomicUsize::new(0));
    let llm_calls2 = llm_calls.clone();
    let stream_fn: pi_agent::StreamFn = Arc::new(move |_model, _ctx, _opt| {
        llm_calls2.fetch_add(1, Ordering::SeqCst);
        let message = create_assistant_message(
            vec![tool_call("tool-1", "echo", json!({"value": "hello"}))],
            StopReason::ToolUse,
        );
        Box::pin(async move {
            let stream = create_assistant_message_event_stream();
            let s = stream.clone();
            tokio::spawn(async move {
                s.push(done_message(message));
            });
            stream
        })
    });
    let events = Arc::new(Mutex::new(Vec::new()));
    let _ = run_agent_loop(
        vec![create_user_message("echo something")],
        context,
        config,
        collecting_sink(events),
        None,
        stream_fn,
    )
    .await;
    assert_eq!(llm_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn should_stop_after_turn_exits_before_follow_up() {
    let executed = Arc::new(Mutex::new(Vec::new()));
    let context = AgentContext {
        system_prompt: String::new(),
        messages: vec![],
        tools: vec![echo_tool(executed.clone())],
    };
    let steering_polls = Arc::new(AtomicUsize::new(0));
    let follow_up_polls = Arc::new(AtomicUsize::new(0));
    let sp = steering_polls.clone();
    let fp = follow_up_polls.clone();
    let config = AgentLoopConfig {
        get_steering_messages: Some(Arc::new(move || {
            let sp = sp.clone();
            Box::pin(async move {
                sp.fetch_add(1, Ordering::SeqCst);
                vec![]
            })
        })),
        get_follow_up_messages: Some(Arc::new(move || {
            let fp = fp.clone();
            Box::pin(async move {
                fp.fetch_add(1, Ordering::SeqCst);
                vec![create_user_message("follow up should stay queued")]
            })
        })),
        should_stop_after_turn: Some(Arc::new(|_ctx| Box::pin(async move { true }))),
        ..base_config()
    };
    let llm_calls = Arc::new(AtomicUsize::new(0));
    let llm_calls2 = llm_calls.clone();
    let stream_fn: pi_agent::StreamFn = Arc::new(move |_model, _ctx, _opt| {
        let n = llm_calls2.fetch_add(1, Ordering::SeqCst) + 1;
        let message = if n == 1 {
            create_assistant_message(
                vec![tool_call("tool-1", "echo", json!({"value": "hello"}))],
                StopReason::ToolUse,
            )
        } else {
            create_assistant_message(vec![text_block("should not run")], StopReason::Stop)
        };
        Box::pin(async move {
            let stream = create_assistant_message_event_stream();
            let s = stream.clone();
            tokio::spawn(async move {
                s.push(done_message(message));
            });
            stream
        })
    });
    let events = Arc::new(Mutex::new(Vec::new()));
    let messages = run_agent_loop(
        vec![create_user_message("echo something")],
        context,
        config,
        collecting_sink(events.clone()),
        None,
        stream_fn,
    )
    .await;
    assert_eq!(llm_calls.load(Ordering::SeqCst), 1);
    assert_eq!(*executed.lock(), vec!["hello".to_string()]);
    assert_eq!(steering_polls.load(Ordering::SeqCst), 1);
    assert_eq!(follow_up_polls.load(Ordering::SeqCst), 0);
    assert_eq!(
        messages
            .iter()
            .map(AgentMessage::role)
            .collect::<Vec<_>>(),
        vec!["user", "assistant", "toolResult"]
    );
    assert_eq!(
        event_types(&events.lock()),
        vec![
            "agent_start",
            "turn_start",
            "message_start",
            "message_end",
            "message_start",
            "message_end",
            "tool_execution_start",
            "tool_execution_end",
            "message_start",
            "message_end",
            "turn_end",
            "agent_end",
        ]
    );
}

#[tokio::test]
async fn abort_mid_stream_propagates_cancel_token() {
    let cancel = CancellationToken::new();
    let saw_cancel = Arc::new(AtomicBool::new(false));
    let saw_cancel2 = saw_cancel.clone();
    let cancel_for_stream = cancel.clone();
    let stream_fn: pi_agent::StreamFn = Arc::new(move |model, _ctx, options| {
        let saw_cancel2 = saw_cancel2.clone();
        let cancel_for_stream = cancel_for_stream.clone();
        let token = options.cancel.clone();
        Box::pin(async move {
            let stream = create_assistant_message_event_stream();
            let s = stream.clone();
            tokio::spawn(async move {
                let partial = AssistantMessage {
                    content: vec![text_block("")],
                    api: model.api.clone(),
                    provider: model.provider.clone(),
                    model: model.id.clone(),
                    response_model: None,
                    response_id: None,
                    diagnostics: None,
                    usage: create_usage(),
                    stop_reason: StopReason::Stop,
                    error_message: None,
                    timestamp: 1,
                };
                s.push(AssistantMessageEvent::Start {
                    partial: partial.clone(),
                });
                // Wait until cancelled, then emit aborted error.
                for _ in 0..100 {
                    if token.as_ref().is_some_and(CancellationToken::is_cancelled)
                        || cancel_for_stream.is_cancelled()
                    {
                        saw_cancel2.store(true, Ordering::SeqCst);
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
                let mut aborted = partial;
                aborted.stop_reason = StopReason::Aborted;
                aborted.error_message = Some("aborted".into());
                s.push(AssistantMessageEvent::Error {
                    reason: StopReason::Aborted,
                    error: aborted,
                });
            });
            stream
        })
    });
    let context = AgentContext {
        system_prompt: String::new(),
        messages: vec![],
        tools: vec![],
    };
    let events = Arc::new(Mutex::new(Vec::new()));
    let cancel2 = cancel.clone();
    let run = tokio::spawn(async move {
        run_agent_loop(
            vec![create_user_message("hello")],
            context,
            base_config(),
            collecting_sink(events.clone()),
            Some(cancel2),
            stream_fn,
        )
        .await
    });
    tokio::time::sleep(std::time::Duration::from_millis(15)).await;
    cancel.cancel();
    let messages = run.await.unwrap();
    assert!(saw_cancel.load(Ordering::SeqCst));
    assert_eq!(messages.last().map(AgentMessage::role), Some("assistant"));
    match messages.last() {
        Some(AgentMessage::Standard(Message::Assistant(a))) => {
            assert_eq!(a.stop_reason, StopReason::Aborted);
        }
        _ => panic!("expected aborted assistant"),
    }
}

#[tokio::test]
async fn continue_rejects_empty_context() {
    let context = AgentContext {
        system_prompt: "You are helpful.".into(),
        messages: vec![],
        tools: vec![],
    };
    let events = Arc::new(Mutex::new(Vec::new()));
    let err = run_agent_loop_continue(
        context,
        base_config(),
        collecting_sink(events),
        None,
        scripted_stream_fn(vec![]),
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("no messages"));
}

#[tokio::test]
async fn continue_from_existing_context_without_user_events() {
    let user = create_user_message("Hello");
    let context = AgentContext {
        system_prompt: "You are helpful.".into(),
        messages: vec![user],
        tools: vec![],
    };
    let stream_fn = scripted_stream_fn(vec![vec![done_message(create_assistant_message(
        vec![text_block("Response")],
        StopReason::Stop,
    ))]]);
    let events = Arc::new(Mutex::new(Vec::new()));
    let messages = run_agent_loop_continue(
        context,
        base_config(),
        collecting_sink(events.clone()),
        None,
        stream_fn,
    )
    .await
    .unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].role(), "assistant");
    let message_ends: Vec<_> = events
        .lock()
        .iter()
        .filter_map(|e| match e {
            AgentEvent::MessageEnd { message } => Some(message.role().to_owned()),
            _ => None,
        })
        .collect();
    assert_eq!(message_ends, vec!["assistant".to_string()]);
}

#[tokio::test]
async fn injects_steering_messages_after_tools() {
    let executed = Arc::new(Mutex::new(Vec::new()));
    let context = AgentContext {
        system_prompt: String::new(),
        messages: vec![],
        tools: vec![echo_tool(executed.clone())],
    };
    let delivered = Arc::new(AtomicBool::new(false));
    let delivered2 = delivered.clone();
    let executed2 = executed.clone();
    let saw_interrupt = Arc::new(AtomicBool::new(false));
    let saw_interrupt2 = saw_interrupt.clone();
    let config = AgentLoopConfig {
        tool_execution: ToolExecutionMode::Sequential,
        get_steering_messages: Some(Arc::new(move || {
            let delivered2 = delivered2.clone();
            let executed2 = executed2.clone();
            Box::pin(async move {
                if !executed2.lock().is_empty() && !delivered2.swap(true, Ordering::SeqCst) {
                    vec![create_user_message("interrupt")]
                } else {
                    vec![]
                }
            })
        })),
        ..base_config()
    };
    let call_index = Arc::new(AtomicUsize::new(0));
    let call_index2 = call_index.clone();
    let stream_fn: pi_agent::StreamFn = Arc::new(move |_model, ctx: Context, _opt| {
        let n = call_index2.fetch_add(1, Ordering::SeqCst);
        if n == 1 {
            let has = ctx.messages.iter().any(|m| match m {
                Message::User(u) => match &u.content {
                    UserContent::Text(t) => t == "interrupt",
                    _ => false,
                },
                _ => false,
            });
            if has {
                saw_interrupt2.store(true, Ordering::SeqCst);
            }
        }
        let message = if n == 0 {
            create_assistant_message(
                vec![
                    tool_call("tool-1", "echo", json!({"value": "first"})),
                    tool_call("tool-2", "echo", json!({"value": "second"})),
                ],
                StopReason::ToolUse,
            )
        } else {
            create_assistant_message(vec![text_block("done")], StopReason::Stop)
        };
        Box::pin(async move {
            let stream = create_assistant_message_event_stream();
            let s = stream.clone();
            tokio::spawn(async move {
                s.push(done_message(message));
            });
            stream
        })
    });
    let events = Arc::new(Mutex::new(Vec::new()));
    let _ = run_agent_loop(
        vec![create_user_message("start")],
        context,
        config,
        collecting_sink(events.clone()),
        None,
        stream_fn,
    )
    .await;
    assert_eq!(
        *executed.lock(),
        vec!["first".to_string(), "second".to_string()]
    );
    assert!(saw_interrupt.load(Ordering::SeqCst));
}

/// Progress tool: emits mid-execute updates via the on_update callback, then
/// optionally delays so a missing Promise.all-style barrier would let
/// tool_execution_end race ahead of the update emits.
fn progress_tool(
    name: &str,
    updates: usize,
    hold_ms: u64,
) -> Arc<ToolDefinition> {
    let name_owned = name.to_owned();
    Arc::new(ToolDefinition {
        name: name_owned.clone(),
        label: name_owned.clone(),
        description: format!("{name_owned} with progress"),
        parameters: object_schema(json!({"value": {"type": "string"}}), &["value"]),
        execution_mode: None,
        prepare_arguments: None,
        execute: Arc::new(move |_id, params, _cancel, on_update| {
            let hold_ms = hold_ms;
            let updates = updates;
            Box::pin(async move {
                let value = params
                    .get("value")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                if let Some(on_update) = on_update {
                    for i in 0..updates {
                        on_update(AgentToolResult {
                            content: vec![text_content(format!("progress {i}: {value}"))],
                            details: json!({ "step": i, "value": value }),
                            added_tool_names: None,
                            terminate: None,
                        });
                    }
                }
                if hold_ms > 0 {
                    tokio::time::sleep(std::time::Duration::from_millis(hold_ms)).await;
                }
                Ok(AgentToolResult {
                    content: vec![text_content(format!("done: {value}"))],
                    details: json!({ "value": value }),
                    added_tool_names: None,
                    terminate: None,
                })
            })
        }),
        renderer: None,
    })
}

/// Sink that artificially delays tool_execution_update emits so a missing
/// post-execute join barrier would surface as update-after-end ordering.
fn delayed_update_sink(
    events: Arc<Mutex<Vec<AgentEvent>>>,
    update_delay: std::time::Duration,
) -> pi_agent::AgentEventSink {
    Arc::new(move |event| {
        let events = events.clone();
        let delay = update_delay;
        Box::pin(async move {
            if matches!(event, AgentEvent::ToolExecutionUpdate { .. }) {
                tokio::time::sleep(delay).await;
            }
            events.lock().push(event);
        })
    })
}

/// For each tool_call_id, every tool_execution_update must appear before that
/// tool's tool_execution_end (oracle: Promise.all(updateEvents) before end).
fn assert_updates_precede_end_per_tool(events: &[AgentEvent]) {
    use std::collections::HashMap;
    let mut end_index: HashMap<&str, usize> = HashMap::new();
    for (i, e) in events.iter().enumerate() {
        if let AgentEvent::ToolExecutionEnd { tool_call_id, .. } = e {
            end_index.insert(tool_call_id.as_str(), i);
        }
    }
    assert!(
        !end_index.is_empty(),
        "expected at least one tool_execution_end, got {events:?}"
    );
    let mut saw_update = false;
    for (i, e) in events.iter().enumerate() {
        if let AgentEvent::ToolExecutionUpdate { tool_call_id, .. } = e {
            saw_update = true;
            let end_i = end_index
                .get(tool_call_id.as_str())
                .unwrap_or_else(|| panic!("update for {tool_call_id} without end"));
            assert!(
                i < *end_i,
                "tool_execution_update for {tool_call_id} at index {i} must precede \
                 tool_execution_end at {end_i}; types={:?}",
                event_types(events)
            );
        }
    }
    assert!(saw_update, "expected at least one tool_execution_update");
}

/// After any tool_execution_end, no later tool_execution_update may belong to
/// that same tool_call_id (no cross-tool interleave of a finished tool's updates).
fn assert_no_update_after_own_end(events: &[AgentEvent]) {
    use std::collections::HashSet;
    let mut ended: HashSet<&str> = HashSet::new();
    for e in events {
        match e {
            AgentEvent::ToolExecutionEnd { tool_call_id, .. } => {
                ended.insert(tool_call_id.as_str());
            }
            AgentEvent::ToolExecutionUpdate { tool_call_id, .. } => {
                assert!(
                    !ended.contains(tool_call_id.as_str()),
                    "tool_execution_update for {tool_call_id} after its tool_execution_end; \
                     types={:?}",
                    event_types(events)
                );
            }
            _ => {}
        }
    }
}

#[tokio::test]
async fn tool_progress_updates_precede_execution_end() {
    let tool = progress_tool("progress", 3, 0);
    let context = AgentContext {
        system_prompt: String::new(),
        messages: vec![],
        tools: vec![tool],
    };
    let mut config = base_config();
    config.tool_execution = ToolExecutionMode::Sequential;

    let first = create_assistant_message(
        vec![tool_call("tool-p", "progress", json!({"value": "mid"}))],
        StopReason::ToolUse,
    );
    let second = create_assistant_message(vec![text_block("done")], StopReason::Stop);
    let stream_fn = scripted_stream_fn(vec![vec![done_message(first)], vec![done_message(second)]]);
    let events = Arc::new(Mutex::new(Vec::new()));

    let _ = run_agent_loop(
        vec![create_user_message("progress once")],
        context,
        config,
        // Delay update emits so a missing join barrier races end ahead of updates.
        delayed_update_sink(events.clone(), std::time::Duration::from_millis(15)),
        None,
        stream_fn,
    )
    .await;

    let events = events.lock();
    let updates: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::ToolExecutionUpdate { .. }))
        .collect();
    assert_eq!(updates.len(), 3, "expected 3 mid-execute progress updates");
    assert_updates_precede_end_per_tool(&events);
    assert_no_update_after_own_end(&events);
}

#[tokio::test]
async fn parallel_tool_progress_updates_precede_each_end() {
    // Two tools both emit progress; staggered completion + delayed update sink
    // stress-tests that no tool's updates appear after its own end (and that
    // the barrier is per-execute, not a global best-effort yield).
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let release = Arc::new(Mutex::new(Some(tx)));
    let rx = Arc::new(Mutex::new(Some(rx)));

    let rx2 = rx.clone();
    let slow = Arc::new(ToolDefinition {
        name: "slow".into(),
        label: "Slow".into(),
        description: "Slow tool with progress".into(),
        parameters: object_schema(json!({"value": {"type": "string"}}), &["value"]),
        execution_mode: None,
        prepare_arguments: None,
        execute: Arc::new(move |_id, params, _c, on_update| {
            let rx2 = rx2.clone();
            Box::pin(async move {
                let value = params
                    .get("value")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                if let Some(on_update) = on_update {
                    on_update(AgentToolResult {
                        content: vec![text_content(format!("slow-progress: {value}"))],
                        details: json!({ "value": value, "step": 0 }),
                        added_tool_names: None,
                        terminate: None,
                    });
                    on_update(AgentToolResult {
                        content: vec![text_content(format!("slow-progress-2: {value}"))],
                        details: json!({ "value": value, "step": 1 }),
                        added_tool_names: None,
                        terminate: None,
                    });
                }
                let rx = rx2.lock().take();
                if let Some(rx) = rx {
                    let _ = rx.await;
                }
                Ok(AgentToolResult {
                    content: vec![text_content(format!("slow-done: {value}"))],
                    details: json!({ "value": value }),
                    added_tool_names: None,
                    terminate: None,
                })
            })
        }),
        renderer: None,
    });

    let fast = progress_tool("fast", 2, 0);

    let context = AgentContext {
        system_prompt: String::new(),
        messages: vec![],
        tools: vec![slow, fast],
    };
    let mut config = base_config();
    config.tool_execution = ToolExecutionMode::Parallel;

    let first = create_assistant_message(
        vec![
            tool_call("tool-slow", "slow", json!({"value": "a"})),
            tool_call("tool-fast", "fast", json!({"value": "b"})),
        ],
        StopReason::ToolUse,
    );
    let second = create_assistant_message(vec![text_block("done")], StopReason::Stop);
    let stream_fn = scripted_stream_fn(vec![vec![done_message(first)], vec![done_message(second)]]);
    let events = Arc::new(Mutex::new(Vec::new()));

    let run = tokio::spawn({
        let events = events.clone();
        async move {
            run_agent_loop(
                vec![create_user_message("parallel progress")],
                context,
                config,
                delayed_update_sink(events, std::time::Duration::from_millis(20)),
                None,
                stream_fn,
            )
            .await
        }
    });

    // Let fast finish (and emit end) while slow is still held; without a join
    // barrier, delayed slow updates could interleave after fast's end — or
    // worse, after slow's own end once released.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    if let Some(tx) = release.lock().take() {
        let _ = tx.send(());
    }
    let _ = run.await.unwrap();

    let events = events.lock();
    let update_ids: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ToolExecutionUpdate { tool_call_id, .. } => Some(tool_call_id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        update_ids.iter().filter(|id| **id == "tool-slow").count(),
        2,
        "slow tool updates: {update_ids:?}"
    );
    assert_eq!(
        update_ids.iter().filter(|id| **id == "tool-fast").count(),
        2,
        "fast tool updates: {update_ids:?}"
    );

    assert_updates_precede_end_per_tool(&events);
    assert_no_update_after_own_end(&events);

    // Cross-tool: once a tool ends, later events must not include that tool's
    // updates (already covered), and each end is preceded by its own updates.
    let types = event_types(&events);
    assert!(
        types.contains(&"tool_execution_update"),
        "missing updates in {types:?}"
    );
    assert!(
        types.contains(&"tool_execution_end"),
        "missing ends in {types:?}"
    );
}

// Silence unused import in create_model if ModelCostRates not needed.
const _: ModelCostRates = ModelCostRates {
    input: 0.0,
    output: 0.0,
    cache_read: 0.0,
    cache_write: 0.0,
};
