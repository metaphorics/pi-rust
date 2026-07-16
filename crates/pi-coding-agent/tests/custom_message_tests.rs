//! AgentSession::send_custom_message fixtures (oracle sendCustomMessage,
//! agent-session.ts:1388): idle append + events, nextTurn injection, and
//! streaming steer/followUp queueing.

mod common;

use std::sync::Arc;

use common::{
    TestRuntimeOptions, assistant_text_message, gated_stream_fn, make_runtime, test_model,
};
use parking_lot::Mutex;
use pi_coding_agent::session::{
    AgentSessionEvent, CustomMessageDelivery, PromptOptions, SendCustomMessageOptions,
};
use serde_json::{Value, json};

fn model() -> pi_ai::Model {
    test_model(Arc::new(pi_coding_agent::AuthStorage::new(
        std::env::temp_dir().join("custom-msg-unused-auth.json"),
    )))
}

#[tokio::test(flavor = "multi_thread")]
async fn idle_append_persists_entry_and_emits_message_events() {
    let harness = make_runtime(TestRuntimeOptions {
        with_auth: true,
        persisted: true,
        ..Default::default()
    })
    .await;
    let session = harness.runtime.session();

    let events: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    let unsubscribe = session.subscribe(Arc::new(move |event: &AgentSessionEvent| {
        sink.lock().push(serde_json::to_value(event).unwrap());
    }));

    session
        .send_custom_message(
            "my-ext",
            Some(json!([{ "type": "text", "text": "note" }])),
            true,
            Some(json!({ "k": 1 })),
            SendCustomMessageOptions::default(),
        )
        .await;

    // message_start + message_end with the custom message, in order.
    let events = events.lock().clone();
    let types: Vec<&str> = events.iter().map(|e| e["type"].as_str().unwrap()).collect();
    assert_eq!(types, vec!["message_start", "message_end"]);
    let message = &events[0]["message"];
    assert_eq!(message["role"], "custom");
    assert_eq!(message["customType"], "my-ext");
    assert_eq!(message["display"], true);
    assert_eq!(message["details"], json!({ "k": 1 }));

    // In-memory state contains it; the session file has the entry with the
    // oracle wire shape.
    assert_eq!(session.messages().len(), 1);
    let entry = session.with_session_manager(|sm| {
        sm.get_entries().into_iter().find_map(|e| match e {
            pi_coding_agent::SessionEntry::CustomMessage {
                custom_type,
                content,
                display,
                ..
            } => Some((custom_type, content, display)),
            _ => None,
        })
    });
    let (custom_type, content, display) = entry.expect("custom_message entry persisted");
    assert_eq!(custom_type, "my-ext");
    assert_eq!(content, json!([{ "type": "text", "text": "note" }]));
    assert!(display);

    unsubscribe();
}

#[tokio::test(flavor = "multi_thread")]
async fn null_content_normalizes_to_empty_array() {
    let harness = make_runtime(TestRuntimeOptions {
        with_auth: true,
        ..Default::default()
    })
    .await;
    let session = harness.runtime.session();

    session
        .send_custom_message(
            "my-ext",
            None,
            false,
            None,
            SendCustomMessageOptions::default(),
        )
        .await;

    let message = serde_json::to_value(&session.messages()[0]).unwrap();
    assert_eq!(message["content"], json!([]));
    // details omitted entirely (undefined, not null).
    assert!(message.get("details").is_none(), "{message}");
}

#[tokio::test(flavor = "multi_thread")]
async fn next_turn_messages_ride_the_next_prompt() {
    let m = model();
    let harness = make_runtime(TestRuntimeOptions {
        with_auth: true,
        script: vec![assistant_text_message(&m, "done")],
        ..Default::default()
    })
    .await;
    let session = harness.runtime.session();

    session
        .send_custom_message(
            "aside",
            Some(json!([{ "type": "text", "text": "context" }])),
            false,
            None,
            SendCustomMessageOptions {
                trigger_turn: false,
                deliver_as: Some(CustomMessageDelivery::NextTurn),
            },
        )
        .await;

    // Not in state yet — queued for the next turn.
    assert!(session.messages().is_empty());

    session
        .prompt("go", PromptOptions::default())
        .await
        .expect("prompt");

    // The custom message rides alongside the user message.
    let messages = session.messages();
    let roles: Vec<String> = messages
        .iter()
        .map(|m| {
            serde_json::to_value(m).unwrap()["role"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();
    assert_eq!(roles, vec!["user", "custom", "assistant"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn streaming_delivery_queues_via_steer_or_follow_up() {
    let script = Arc::new(Mutex::new(std::collections::VecDeque::new()));
    let gate = Arc::new(tokio::sync::Notify::new());
    let harness = make_runtime(TestRuntimeOptions {
        with_auth: true,
        stream_fn: Some(gated_stream_fn(script.clone(), gate.clone())),
        ..Default::default()
    })
    .await;
    let session = harness.runtime.session();
    {
        let m = harness.model.clone();
        let mut script = script.lock();
        script.push_back(assistant_text_message(&m, "first"));
        script.push_back(assistant_text_message(&m, "second"));
    }

    let run = tokio::spawn({
        let session = session.clone();
        async move { session.prompt("start", PromptOptions::default()).await }
    });
    // Wait until the run is active (gated stream holds it open).
    while !session.is_streaming() {
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }

    session
        .send_custom_message(
            "mid-run",
            Some(json!([{ "type": "text", "text": "steered" }])),
            false,
            None,
            SendCustomMessageOptions::default(),
        )
        .await;

    gate.notify_one();
    run.await.expect("join").expect("prompt");
    session.wait_for_idle().await;

    // The steered custom message entered the transcript during the run.
    let roles: Vec<String> = session
        .messages()
        .iter()
        .map(|m| {
            serde_json::to_value(m).unwrap()["role"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();
    assert!(
        roles.contains(&"custom".to_string()),
        "steered custom message must drain into the run: {roles:?}"
    );
}
