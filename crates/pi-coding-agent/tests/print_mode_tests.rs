//! Print/json mode tests — port of test/print-mode.test.ts plus the json
//! stdout contract (SessionHeader first line, then the event stream).

mod common;

use std::sync::Arc;

use common::{CapturedOut, TestRuntimeOptions, assistant_text_message, make_runtime, test_model};
use pi_ai::{ImageContent, StopReason};
use pi_coding_agent::AuthStorage;
use pi_coding_agent::modes::json::run_json_mode_with_out;
use pi_coding_agent::modes::print::{PrintModeOptions, PrintOutputMode, run_print_mode_with_out};
use serde_json::Value;

fn model() -> pi_ai::Model {
    test_model(Arc::new(AuthStorage::new(
        std::env::temp_dir().join("print-mode-unused-auth.json"),
    )))
}

#[tokio::test(flavor = "multi_thread")]
async fn text_mode_prints_final_assistant_text_only() {
    let harness = make_runtime(TestRuntimeOptions {
        with_auth: true,
        script: vec![assistant_text_message(&model(), "done")],
        ..Default::default()
    })
    .await;
    let (out, captured) = CapturedOut::new();

    let exit_code = run_print_mode_with_out(
        harness.runtime.clone(),
        PrintModeOptions {
            mode: PrintOutputMode::Text,
            initial_message: Some("Say done".to_string()),
            ..Default::default()
        },
        out,
        false,
    )
    .await;

    assert_eq!(exit_code, 0);
    // stdout purity: the final text block and nothing else.
    assert_eq!(captured.raw(), "done\n");
}

#[tokio::test(flavor = "multi_thread")]
async fn text_mode_sends_initial_images_and_sequential_messages() {
    let m = model();
    let harness = make_runtime(TestRuntimeOptions {
        with_auth: true,
        script: vec![
            assistant_text_message(&m, "first"),
            assistant_text_message(&m, "second"),
        ],
        ..Default::default()
    })
    .await;
    let (out, captured) = CapturedOut::new();

    let exit_code = run_print_mode_with_out(
        harness.runtime.clone(),
        PrintModeOptions {
            mode: PrintOutputMode::Text,
            initial_message: Some("Say first".to_string()),
            initial_images: vec![ImageContent {
                mime_type: "image/png".into(),
                data: "abc".into(),
            }],
            messages: vec!["Say second".to_string()],
        },
        out,
        false,
    )
    .await;

    assert_eq!(exit_code, 0);
    // Only the LAST assistant message prints (oracle inspects state tail).
    assert_eq!(captured.raw(), "second\n");
    // Both prompts ran; the image rode the first user message.
    let messages = harness.runtime.session().messages();
    assert_eq!(messages.len(), 4);
    let first_user = serde_json::to_value(&messages[0]).unwrap();
    assert!(
        first_user["content"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["type"] == "image"),
        "initial images must attach to the first prompt"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn text_mode_error_stop_reason_exits_one_with_empty_stdout() {
    let mut message = assistant_text_message(&model(), "");
    message.stop_reason = StopReason::Error;
    message.error_message = Some("provider failure".to_string());
    let harness = make_runtime(TestRuntimeOptions {
        with_auth: true,
        script: vec![message],
        ..Default::default()
    })
    .await;
    let (out, captured) = CapturedOut::new();

    let exit_code = run_print_mode_with_out(
        harness.runtime.clone(),
        PrintModeOptions {
            mode: PrintOutputMode::Text,
            initial_message: Some("boom".to_string()),
            ..Default::default()
        },
        out,
        false,
    )
    .await;

    assert_eq!(exit_code, 1);
    assert_eq!(captured.raw(), "", "stdout must stay clean on error");
}

#[tokio::test(flavor = "multi_thread")]
async fn json_mode_emits_header_first_then_event_stream() {
    let harness = make_runtime(TestRuntimeOptions {
        with_auth: true,
        persisted: true,
        script: vec![assistant_text_message(&model(), "done")],
        ..Default::default()
    })
    .await;
    let (out, captured) = CapturedOut::new();

    let exit_code = run_json_mode_with_out(
        harness.runtime.clone(),
        PrintModeOptions {
            initial_message: Some("hello".to_string()),
            ..Default::default()
        },
        out,
        false,
    )
    .await;

    assert_eq!(exit_code, 0);
    let lines = captured.lines();
    assert!(!lines.is_empty());

    // Every stdout line is strict JSON (stdout purity).
    let parsed: Vec<Value> = lines
        .iter()
        .map(|l| serde_json::from_str(l).expect("stdout line must be JSON"))
        .collect();

    // First line is the verbatim SessionHeader.
    assert_eq!(parsed[0]["type"], "session");
    assert!(parsed[0]["id"].is_string());
    assert!(parsed[0]["cwd"].is_string());

    // Then the session events, starting with agent_start and settling last.
    let types: Vec<&str> = parsed[1..]
        .iter()
        .map(|v| v["type"].as_str().unwrap())
        .collect();
    assert_eq!(types.first(), Some(&"agent_start"));
    assert_eq!(types.last(), Some(&"agent_settled"));
    assert!(types.iter().filter(|t| **t == "message_end").count() >= 2);
    // Ordered wire stream: message_start(user) precedes message_end(user)
    // precedes agent_end.
    let start = types.iter().position(|t| *t == "message_start").unwrap();
    let end = types.iter().position(|t| *t == "message_end").unwrap();
    let agent_end = types.iter().position(|t| *t == "agent_end").unwrap();
    assert!(start < end && end < agent_end);
}

#[tokio::test(flavor = "multi_thread")]
async fn json_mode_prompt_error_reports_exit_one() {
    // No API key: prompt preflight rejects; json mode exits 1.
    let harness = make_runtime(TestRuntimeOptions {
        with_auth: false,
        ..Default::default()
    })
    .await;
    let (out, captured) = CapturedOut::new();

    let exit_code = run_json_mode_with_out(
        harness.runtime.clone(),
        PrintModeOptions {
            initial_message: Some("hello".to_string()),
            ..Default::default()
        },
        out,
        false,
    )
    .await;

    assert_eq!(exit_code, 1);
    // Header still leads; no non-JSON noise on stdout.
    for line in captured.lines() {
        serde_json::from_str::<Value>(&line).expect("stdout line must be JSON");
    }
}
