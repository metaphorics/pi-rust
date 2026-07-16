//! RPC mode wire tests: byte-semantic envelopes (id echo/omission,
//! `data:null` vs omitted, key order), response/event ordering, unknown
//! command survival, EOF shutdown, extension UI passthrough, and the
//! oracle prompt-response semantics fixtures
//! (test/rpc-prompt-response-semantics.test.ts).

mod common;

use std::collections::VecDeque;
use std::sync::Arc;

use common::{
    CapturedOut, TestRuntimeOptions, UiCapturingBridge, assert_key_order, assistant_text_message,
    gated_stream_fn, make_runtime, wait_for_lines,
};
use parking_lot::Mutex;
use pi_coding_agent::extension_bridge::UiDialogOptions;
use pi_coding_agent::modes::rpc::{RpcModeOptions, run_rpc_mode_with_io};
use pi_coding_agent::session::PromptTemplate;
use pi_coding_agent::source_info::{SourceInfo, SourceScope};
use pi_coding_agent::system_prompt::Skill;
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;

struct Rpc {
    client: Option<tokio::io::DuplexStream>,
    out: CapturedOut,
    task: tokio::task::JoinHandle<i32>,
    harness: common::TestRuntime,
}

impl Rpc {
    async fn start(options: TestRuntimeOptions) -> Self {
        Self::start_with(options, RpcModeOptions::default()).await
    }

    async fn start_with(options: TestRuntimeOptions, mode_options: RpcModeOptions) -> Self {
        let harness = make_runtime(options).await;
        let (out, captured) = CapturedOut::new();
        let (client, server) = tokio::io::duplex(1 << 16);
        let task = tokio::spawn(run_rpc_mode_with_io(
            harness.runtime.clone(),
            server,
            out,
            mode_options,
            false,
        ));
        Rpc {
            client: Some(client),
            out: captured,
            task,
            harness,
        }
    }

    async fn send(&mut self, line: &str) {
        let client = self.client.as_mut().expect("input open");
        client
            .write_all(format!("{line}\n").as_bytes())
            .await
            .expect("write line");
    }

    /// Wait until `n` response lines exist; returns all lines.
    async fn wait_responses(&self, n: usize) -> Vec<String> {
        wait_for_lines(&self.out, 5000, |lines| {
            lines
                .iter()
                .filter(|l| l.contains("\"type\":\"response\""))
                .count()
                >= n
        })
        .await
    }

    /// Close stdin (EOF) and return the mode exit code.
    async fn finish(mut self) -> i32 {
        drop(self.client.take());
        self.task.await.expect("rpc task")
    }
}

fn response_lines(lines: &[String]) -> Vec<String> {
    lines
        .iter()
        .filter(|l| l.contains("\"type\":\"response\""))
        .cloned()
        .collect()
}

fn event_types(lines: &[String]) -> Vec<String> {
    lines
        .iter()
        .filter(|l| !l.contains("\"type\":\"response\""))
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter_map(|v| v.get("type").and_then(Value::as_str).map(str::to_string))
        .collect()
}

/// The mode binds the UI host inside the spawned loop; poll until it lands.
async fn wait_for_ui(bridge: &Arc<UiCapturingBridge>) -> Arc<dyn pi_coding_agent::ExtensionUiHost> {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(5000);
    loop {
        if let Some(ui) = bridge.ui() {
            return ui;
        }
        assert!(tokio::time::Instant::now() < deadline, "UI never bound");
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
}

// ============================================================================
// Envelope byte-semantics
// ============================================================================

#[tokio::test(flavor = "multi_thread")]
async fn get_state_wire_shape_id_echo_and_omissions() {
    let mut rpc = Rpc::start(TestRuntimeOptions {
        with_auth: true,
        ..Default::default()
    })
    .await;

    rpc.send(r#"{"id":"1","type":"get_state"}"#).await;
    let lines = rpc.wait_responses(1).await;
    let line = &response_lines(&lines)[0];

    // Envelope order: id, type, command, success, data.
    assert!(
        line.starts_with(
            r#"{"id":"1","type":"response","command":"get_state","success":true,"data":{"#
        ),
        "envelope mismatch: {line}"
    );
    // Wire key order of RpcSessionState.
    assert_key_order(
        line,
        &[
            "model",
            "thinkingLevel",
            "isStreaming",
            "isCompacting",
            "steeringMode",
            "followUpMode",
            "sessionId",
            "autoCompactionEnabled",
            "messageCount",
            "pendingMessageCount",
        ],
    );
    // In-memory unnamed session: sessionFile/sessionName are omitted, not null.
    assert!(!line.contains("sessionFile"), "{line}");
    assert!(!line.contains("sessionName"), "{line}");

    let parsed: Value = serde_json::from_str(line).unwrap();
    assert_eq!(parsed["data"]["thinkingLevel"], "off");
    assert_eq!(parsed["data"]["isStreaming"], false);
    assert_eq!(parsed["data"]["messageCount"], 0);
    // Oracle default (settings-manager.ts:703): "one-at-a-time".
    assert_eq!(parsed["data"]["steeringMode"], "one-at-a-time");
    assert_eq!(parsed["data"]["model"]["provider"], "anthropic");

    assert_eq!(rpc.finish().await, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn response_id_omitted_when_command_has_none() {
    let mut rpc = Rpc::start(TestRuntimeOptions {
        with_auth: true,
        ..Default::default()
    })
    .await;

    rpc.send(r#"{"type":"get_state"}"#).await;
    let lines = rpc.wait_responses(1).await;
    let line = &response_lines(&lines)[0];
    assert!(
        line.starts_with(r#"{"type":"response","command":"get_state","success":true"#),
        "id must be omitted: {line}"
    );

    assert_eq!(rpc.finish().await, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn unknown_command_error_is_byte_exact_and_loop_survives() {
    let mut rpc = Rpc::start(TestRuntimeOptions {
        with_auth: true,
        ..Default::default()
    })
    .await;

    rpc.send(r#"{"id":"9","type":"bogus"}"#).await;
    let lines = rpc.wait_responses(1).await;
    assert_eq!(
        response_lines(&lines)[0],
        r#"{"id":"9","type":"response","command":"bogus","success":false,"error":"Unknown command: bogus"}"#
    );

    // The loop survives and keeps handling commands.
    rpc.send(r#"{"id":"10","type":"get_state"}"#).await;
    let lines = rpc.wait_responses(2).await;
    assert!(response_lines(&lines)[1].contains(r#""command":"get_state","success":true"#));

    assert_eq!(rpc.finish().await, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn malformed_json_produces_parse_error_envelope_without_id() {
    let mut rpc = Rpc::start(TestRuntimeOptions {
        with_auth: true,
        ..Default::default()
    })
    .await;

    rpc.send("{oops").await;
    let lines = rpc.wait_responses(1).await;
    let line = &response_lines(&lines)[0];
    assert!(
        line.starts_with(
            r#"{"type":"response","command":"parse","success":false,"error":"Failed to parse command: "#
        ),
        "parse envelope mismatch: {line}"
    );

    assert_eq!(rpc.finish().await, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn missing_and_non_string_type_survive_like_oracle() {
    let mut rpc = Rpc::start(TestRuntimeOptions {
        with_auth: true,
        ..Default::default()
    })
    .await;

    // Missing type: command key omitted, error stringifies `undefined`.
    rpc.send(r#"{"id":"x"}"#).await;
    let lines = rpc.wait_responses(1).await;
    assert_eq!(
        response_lines(&lines)[0],
        r#"{"id":"x","type":"response","success":false,"error":"Unknown command: undefined"}"#
    );

    // Non-string type: echoed as-is.
    rpc.send(r#"{"type":5}"#).await;
    let lines = rpc.wait_responses(2).await;
    assert_eq!(
        response_lines(&lines)[1],
        r#"{"type":"response","command":5,"success":false,"error":"Unknown command: 5"}"#
    );

    // Non-object line survives too.
    rpc.send("42").await;
    let lines = rpc.wait_responses(3).await;
    assert_eq!(
        response_lines(&lines)[2],
        r#"{"type":"response","success":false,"error":"Unknown command: undefined"}"#
    );

    assert_eq!(rpc.finish().await, 0);
}

// ============================================================================
// Prompt response semantics (oracle rpc-prompt-response-semantics.test.ts)
// ============================================================================

#[tokio::test(flavor = "multi_thread")]
async fn prompt_success_response_precedes_all_session_events() {
    let harness_model = {
        // scripted response text "done"
        let mut opts = TestRuntimeOptions {
            with_auth: true,
            ..Default::default()
        };
        let tmp_auth = Arc::new(pi_coding_agent::AuthStorage::new(
            std::env::temp_dir().join("unused-auth.json"),
        ));
        let model = common::test_model(tmp_auth);
        opts.script = vec![assistant_text_message(&model, "done")];
        opts
    };
    let mut rpc = Rpc::start(harness_model).await;

    rpc.send(r#"{"id":"b2","type":"prompt","message":"Hello"}"#)
        .await;
    let lines = wait_for_lines(&rpc.out, 5000, |lines| {
        lines
            .iter()
            .any(|l| l.contains(r#""type":"agent_settled""#))
    })
    .await;

    // Exactly one prompt response.
    let prompt_responses: Vec<&String> = lines
        .iter()
        .filter(|l| l.contains(r#""command":"prompt""#))
        .collect();
    assert_eq!(prompt_responses.len(), 1, "lines: {}", lines.join("\n"));
    assert_eq!(
        prompt_responses[0].as_str(),
        r#"{"id":"b2","type":"response","command":"prompt","success":true}"#
    );

    // The response precedes EVERY session event of the run.
    let response_index = lines
        .iter()
        .position(|l| l.contains(r#""command":"prompt""#))
        .unwrap();
    let first_event_index = lines
        .iter()
        .position(|l| !l.contains(r#""type":"response""#))
        .unwrap();
    assert!(
        response_index < first_event_index,
        "prompt response must precede events:\n{}",
        lines.join("\n")
    );

    // Event stream shape: starts with agent_start, ends with agent_settled.
    let events = event_types(&lines);
    assert_eq!(events.first().map(String::as_str), Some("agent_start"));
    assert_eq!(events.last().map(String::as_str), Some("agent_settled"));
    assert!(events.iter().filter(|t| *t == "message_end").count() >= 2);

    assert_eq!(rpc.finish().await, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn prompt_preflight_failure_emits_one_error_response_and_no_events() {
    // No API key configured: preflight rejects with the verbatim guidance.
    let mut rpc = Rpc::start(TestRuntimeOptions {
        with_auth: false,
        ..Default::default()
    })
    .await;

    rpc.send(r#"{"id":"b1","type":"prompt","message":"Hello"}"#)
        .await;
    let lines = rpc.wait_responses(1).await;
    let responses = response_lines(&lines);
    assert_eq!(responses.len(), 1);
    let parsed: Value = serde_json::from_str(&responses[0]).unwrap();
    assert_eq!(parsed["id"], "b1");
    assert_eq!(parsed["command"], "prompt");
    assert_eq!(parsed["success"], false);
    let error = parsed["error"].as_str().unwrap();
    assert!(
        error.starts_with(
            "No API key found for anthropic.\n\nUse /login to log into a provider via OAuth or API key. See:"
        ),
        "verbatim preflight error mismatch: {error}"
    );
    // No session events were emitted.
    assert!(event_types(&lines).is_empty(), "{}", lines.join("\n"));

    assert_eq!(rpc.finish().await, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn prompt_queued_during_streaming_succeeds_and_active_run_is_preserved() {
    let script = Arc::new(Mutex::new(VecDeque::new()));
    let gate = Arc::new(tokio::sync::Notify::new());
    let stream_fn = gated_stream_fn(script.clone(), gate.clone());
    let mut rpc = Rpc::start(TestRuntimeOptions {
        with_auth: true,
        stream_fn: Some(stream_fn),
        ..Default::default()
    })
    .await;
    {
        let model = rpc.harness.model.clone();
        let mut script = script.lock();
        script.push_back(assistant_text_message(&model, "first"));
        script.push_back(assistant_text_message(&model, "second"));
    }

    rpc.send(r#"{"id":"b3-start","type":"prompt","message":"Start"}"#)
        .await;
    let lines = rpc.wait_responses(1).await;
    assert_eq!(
        response_lines(&lines)[0],
        r#"{"id":"b3-start","type":"response","command":"prompt","success":true}"#
    );

    // Queued prompt with streamingBehavior also counts as success (one line).
    rpc.send(
        r#"{"id":"b3","type":"prompt","message":"Queue this","streamingBehavior":"followUp"}"#,
    )
    .await;
    let lines = rpc.wait_responses(2).await;
    assert_eq!(
        response_lines(&lines)[1],
        r#"{"id":"b3","type":"response","command":"prompt","success":true}"#
    );

    // A prompt with NO streamingBehavior is rejected with the verbatim error
    // (one active run preserved).
    rpc.send(r#"{"id":"b4","type":"prompt","message":"No behavior"}"#)
        .await;
    let lines = rpc.wait_responses(3).await;
    assert_eq!(
        response_lines(&lines)[2],
        r#"{"id":"b4","type":"response","command":"prompt","success":false,"error":"Agent is already processing. Specify streamingBehavior ('steer' or 'followUp') to queue the message."}"#
    );

    // Release the gated stream; the queued follow-up drains in the same run.
    gate.notify_one();
    let lines = wait_for_lines(&rpc.out, 5000, |lines| {
        lines
            .iter()
            .any(|l| l.contains(r#""type":"agent_settled""#))
    })
    .await;
    let events = event_types(&lines);
    assert!(events.contains(&"queue_update".to_string()));

    assert_eq!(rpc.finish().await, 0);
}

// ============================================================================
// State / queue-mode / thinking commands
// ============================================================================

#[tokio::test(flavor = "multi_thread")]
async fn queue_modes_round_trip_through_state() {
    let mut rpc = Rpc::start(TestRuntimeOptions {
        with_auth: true,
        ..Default::default()
    })
    .await;

    rpc.send(r#"{"id":"1","type":"set_steering_mode","mode":"all"}"#)
        .await;
    rpc.send(r#"{"id":"2","type":"set_follow_up_mode","mode":"all"}"#)
        .await;
    rpc.send(r#"{"id":"3","type":"get_state"}"#).await;
    let lines = rpc.wait_responses(3).await;
    let responses = response_lines(&lines);
    assert_eq!(
        responses[0],
        r#"{"id":"1","type":"response","command":"set_steering_mode","success":true}"#
    );
    let parsed: Value = serde_json::from_str(&responses[2]).unwrap();
    assert_eq!(parsed["data"]["steeringMode"], "all");
    assert_eq!(parsed["data"]["followUpMode"], "all");

    assert_eq!(rpc.finish().await, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn thinking_level_set_and_cycle() {
    let mut rpc = Rpc::start(TestRuntimeOptions {
        with_auth: true,
        ..Default::default()
    })
    .await;

    rpc.send(r#"{"id":"1","type":"set_thinking_level","level":"high"}"#)
        .await;
    rpc.send(r#"{"id":"2","type":"get_state"}"#).await;
    let lines = rpc.wait_responses(2).await;
    let parsed: Value = serde_json::from_str(&response_lines(&lines)[1]).unwrap();
    assert_eq!(parsed["data"]["thinkingLevel"], "high");

    rpc.send(r#"{"id":"3","type":"cycle_thinking_level"}"#)
        .await;
    let lines = rpc.wait_responses(3).await;
    let parsed: Value = serde_json::from_str(&response_lines(&lines)[2]).unwrap();
    assert_eq!(parsed["command"], "cycle_thinking_level");
    assert!(parsed["data"]["level"].is_string());

    assert_eq!(rpc.finish().await, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn cycle_model_without_available_models_returns_data_null() {
    let mut rpc = Rpc::start(TestRuntimeOptions {
        with_auth: false,
        ..Default::default()
    })
    .await;

    rpc.send(r#"{"id":"1","type":"cycle_model"}"#).await;
    let lines = rpc.wait_responses(1).await;
    assert_eq!(
        response_lines(&lines)[0],
        r#"{"id":"1","type":"response","command":"cycle_model","success":true,"data":null}"#
    );

    assert_eq!(rpc.finish().await, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn set_model_not_found_error_is_verbatim() {
    let mut rpc = Rpc::start(TestRuntimeOptions {
        with_auth: false,
        ..Default::default()
    })
    .await;

    rpc.send(r#"{"id":"1","type":"set_model","provider":"nope","modelId":"missing"}"#)
        .await;
    let lines = rpc.wait_responses(1).await;
    assert_eq!(
        response_lines(&lines)[0],
        r#"{"id":"1","type":"response","command":"set_model","success":false,"error":"Model not found: nope/missing"}"#
    );

    assert_eq!(rpc.finish().await, 0);
}

// ============================================================================
// Bash
// ============================================================================

#[tokio::test(flavor = "multi_thread")]
async fn bash_executes_and_reports_wire_shape() {
    let mut rpc = Rpc::start(TestRuntimeOptions {
        with_auth: true,
        ..Default::default()
    })
    .await;

    rpc.send(r#"{"id":"1","type":"bash","command":"echo hello"}"#)
        .await;
    let lines = rpc.wait_responses(1).await;
    let line = &response_lines(&lines)[0];
    assert_key_order(line, &["output", "exitCode", "cancelled", "truncated"]);
    let parsed: Value = serde_json::from_str(line).unwrap();
    assert_eq!(parsed["data"]["output"].as_str().unwrap().trim(), "hello");
    assert_eq!(parsed["data"]["exitCode"], 0);
    assert_eq!(parsed["data"]["cancelled"], false);

    assert_eq!(rpc.finish().await, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn abort_bash_stays_dispatchable_while_bash_runs() {
    let mut rpc = Rpc::start(TestRuntimeOptions {
        with_auth: true,
        ..Default::default()
    })
    .await;

    rpc.send(r#"{"id":"long","type":"bash","command":"sleep 30"}"#)
        .await;
    // Wait until the bash task actually started (poll state).
    wait_for_lines(&rpc.out, 5000, |_| {
        rpc.harness.runtime.session().is_bash_running()
    })
    .await;

    rpc.send(r#"{"id":"stop","type":"abort_bash"}"#).await;
    let lines = rpc.wait_responses(2).await;
    let responses = response_lines(&lines);
    // abort_bash responds first (bash is detached), then the cancelled bash.
    assert_eq!(
        responses[0],
        r#"{"id":"stop","type":"response","command":"abort_bash","success":true}"#
    );
    let parsed: Value = serde_json::from_str(&responses[1]).unwrap();
    assert_eq!(parsed["id"], "long");
    assert_eq!(parsed["command"], "bash");
    assert_eq!(parsed["data"]["cancelled"], true);

    assert_eq!(rpc.finish().await, 0);
}

// ============================================================================
// Session commands
// ============================================================================

#[tokio::test(flavor = "multi_thread")]
async fn get_tree_and_last_assistant_text_serialize_null_fields() {
    let mut rpc = Rpc::start(TestRuntimeOptions {
        with_auth: true,
        ..Default::default()
    })
    .await;

    rpc.send(r#"{"id":"t","type":"get_tree"}"#).await;
    let lines = rpc.wait_responses(1).await;
    assert_eq!(
        response_lines(&lines)[0],
        r#"{"id":"t","type":"response","command":"get_tree","success":true,"data":{"tree":[],"leafId":null}}"#
    );

    rpc.send(r#"{"id":"a","type":"get_last_assistant_text"}"#)
        .await;
    let lines = rpc.wait_responses(2).await;
    assert_eq!(
        response_lines(&lines)[1],
        r#"{"id":"a","type":"response","command":"get_last_assistant_text","success":true,"data":{"text":null}}"#
    );

    assert_eq!(rpc.finish().await, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn get_entries_since_cursor_and_not_found() {
    let model_probe = Arc::new(pi_coding_agent::AuthStorage::new(
        std::env::temp_dir().join("unused-auth2.json"),
    ));
    let model = common::test_model(model_probe);
    let mut rpc = Rpc::start(TestRuntimeOptions {
        with_auth: true,
        persisted: true,
        script: vec![assistant_text_message(&model, "ok")],
        ..Default::default()
    })
    .await;

    rpc.send(r#"{"id":"p","type":"prompt","message":"Reply with just ok"}"#)
        .await;
    wait_for_lines(&rpc.out, 5000, |lines| {
        lines
            .iter()
            .any(|l| l.contains(r#""type":"agent_settled""#))
    })
    .await;

    rpc.send(r#"{"id":"e1","type":"get_entries"}"#).await;
    let lines = rpc.wait_responses(2).await;
    let all: Value = serde_json::from_str(&response_lines(&lines)[1]).unwrap();
    let entries = all["data"]["entries"].as_array().unwrap();
    assert!(entries.len() >= 2, "user + assistant expected");
    let leaf = all["data"]["leafId"].as_str().unwrap();
    assert_eq!(leaf, entries.last().unwrap()["id"].as_str().unwrap());

    // since cursor returns only entries strictly after the given id.
    let first_id = entries[0]["id"].as_str().unwrap();
    rpc.send(&format!(
        r#"{{"id":"e2","type":"get_entries","since":"{first_id}"}}"#
    ))
    .await;
    let lines = rpc.wait_responses(3).await;
    let since: Value = serde_json::from_str(&response_lines(&lines)[2]).unwrap();
    let since_entries = since["data"]["entries"].as_array().unwrap();
    assert_eq!(since_entries.len(), entries.len() - 1);
    assert_eq!(since["data"]["leafId"].as_str().unwrap(), leaf);

    // Unknown since id is an error response (verbatim).
    rpc.send(r#"{"id":"e3","type":"get_entries","since":"nonexistent-id"}"#)
        .await;
    let lines = rpc.wait_responses(4).await;
    assert_eq!(
        response_lines(&lines)[3],
        r#"{"id":"e3","type":"response","command":"get_entries","success":false,"error":"Entry not found: nonexistent-id"}"#
    );

    assert_eq!(rpc.finish().await, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn set_session_name_validates_and_updates_state() {
    let mut rpc = Rpc::start(TestRuntimeOptions {
        with_auth: true,
        ..Default::default()
    })
    .await;

    rpc.send(r#"{"id":"1","type":"set_session_name","name":"   "}"#)
        .await;
    let lines = rpc.wait_responses(1).await;
    assert_eq!(
        response_lines(&lines)[0],
        r#"{"id":"1","type":"response","command":"set_session_name","success":false,"error":"Session name cannot be empty"}"#
    );

    rpc.send(r#"{"id":"2","type":"set_session_name","name":"my-test-session"}"#)
        .await;
    rpc.send(r#"{"id":"3","type":"get_state"}"#).await;
    let lines = rpc.wait_responses(3).await;
    let responses = response_lines(&lines);
    assert_eq!(
        responses[1],
        r#"{"id":"2","type":"response","command":"set_session_name","success":true}"#
    );
    let parsed: Value = serde_json::from_str(&responses[2]).unwrap();
    assert_eq!(parsed["data"]["sessionName"], "my-test-session");

    assert_eq!(rpc.finish().await, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn export_html_without_wired_handler_fails_and_with_handler_succeeds() {
    // Without a handler: error envelope.
    let mut rpc = Rpc::start(TestRuntimeOptions {
        with_auth: true,
        ..Default::default()
    })
    .await;
    rpc.send(r#"{"id":"1","type":"export_html"}"#).await;
    let lines = rpc.wait_responses(1).await;
    assert_eq!(
        response_lines(&lines)[0],
        r#"{"id":"1","type":"response","command":"export_html","success":false,"error":"export_html is not implemented"}"#
    );
    assert_eq!(rpc.finish().await, 0);

    // With an injected handler: oracle success envelope { path }.
    let mode_options = RpcModeOptions {
        export_html: Some(Arc::new(|_session, output_path| {
            Box::pin(async move {
                Ok(output_path.unwrap_or_else(|| "/tmp/session-export.html".to_string()))
            })
        })),
    };
    let mut rpc = Rpc::start_with(
        TestRuntimeOptions {
            with_auth: true,
            ..Default::default()
        },
        mode_options,
    )
    .await;
    rpc.send(r#"{"id":"2","type":"export_html","outputPath":"/tmp/x.html"}"#)
        .await;
    let lines = rpc.wait_responses(1).await;
    assert_eq!(
        response_lines(&lines)[0],
        r#"{"id":"2","type":"response","command":"export_html","success":true,"data":{"path":"/tmp/x.html"}}"#
    );
    assert_eq!(rpc.finish().await, 0);
}

// ============================================================================
// get_commands
// ============================================================================

#[tokio::test(flavor = "multi_thread")]
async fn get_commands_reports_templates_and_skills_with_source_info() {
    let skill = Skill {
        name: "demo".to_string(),
        description: "Demo skill".to_string(),
        file_path: "/tmp/skills/demo/SKILL.md".into(),
        base_dir: "/tmp/skills/demo".into(),
        source_info: SourceInfo::synthetic(
            "/tmp/skills/demo/SKILL.md",
            "local",
            Some(SourceScope::User),
            None,
            Some("/tmp/skills/demo".to_string()),
        ),
        disable_model_invocation: false,
    };
    let template = PromptTemplate {
        name: "tpl".to_string(),
        description: String::new(),
        argument_hint: None,
        content: "body".to_string(),
        file_path: "/tmp/prompts/tpl.md".into(),
        source_info: SourceInfo::synthetic(
            "/tmp/prompts/tpl.md",
            "local",
            Some(SourceScope::Project),
            None,
            None,
        ),
    };
    let mut rpc = Rpc::start(TestRuntimeOptions {
        with_auth: true,
        skills: vec![skill],
        prompt_templates: vec![template],
        ..Default::default()
    })
    .await;

    rpc.send(r#"{"id":"1","type":"get_commands"}"#).await;
    let lines = rpc.wait_responses(1).await;
    let line = &response_lines(&lines)[0];
    let parsed: Value = serde_json::from_str(line).unwrap();
    assert_eq!(
        parsed["data"]["commands"],
        json!([
            {
                "name": "tpl",
                "source": "prompt",
                "sourceInfo": {
                    "path": "/tmp/prompts/tpl.md",
                    "source": "local",
                    "scope": "project",
                    "origin": "top-level"
                }
            },
            {
                "name": "skill:demo",
                "description": "Demo skill",
                "source": "skill",
                "sourceInfo": {
                    "path": "/tmp/skills/demo/SKILL.md",
                    "source": "local",
                    "scope": "user",
                    "origin": "top-level",
                    "baseDir": "/tmp/skills/demo"
                }
            }
        ])
    );
    // Wire key order: name, description?, source, sourceInfo{path,source,scope,origin,baseDir}.
    assert_key_order(
        line,
        &["name", "source", "sourceInfo", "path", "scope", "origin"],
    );

    assert_eq!(rpc.finish().await, 0);
}

// ============================================================================
// Extension UI passthrough
// ============================================================================

#[tokio::test(flavor = "multi_thread")]
async fn extension_ui_select_round_trips_through_wire() {
    let bridge = Arc::new(UiCapturingBridge::default());
    let mut rpc = Rpc::start(TestRuntimeOptions {
        with_auth: true,
        bridge: Some(bridge.clone()),
        ..Default::default()
    })
    .await;

    let ui = wait_for_ui(&bridge).await;
    let selection = ui.select(
        "Pick one".to_string(),
        vec!["a".to_string(), "b".to_string()],
        UiDialogOptions::default(),
    );

    // The request line is emitted with the oracle wire shape.
    let lines = wait_for_lines(&rpc.out, 5000, |lines| {
        lines
            .iter()
            .any(|l| l.contains(r#""type":"extension_ui_request""#))
    })
    .await;
    let request_line = lines
        .iter()
        .find(|l| l.contains(r#""type":"extension_ui_request""#))
        .unwrap();
    assert_key_order(request_line, &["type", "id", "method", "title", "options"]);
    let request: Value = serde_json::from_str(request_line).unwrap();
    assert_eq!(request["method"], "select");
    assert_eq!(request["title"], "Pick one");
    assert_eq!(request["options"], json!(["a", "b"]));
    let id = request["id"].as_str().unwrap().to_string();

    // Responding on stdin resolves the pending dialog.
    rpc.send(&format!(
        r#"{{"type":"extension_ui_response","id":"{id}","value":"b"}}"#
    ))
    .await;
    assert_eq!(selection.await, Some("b".to_string()));

    // Cancelled responses resolve with the cancel fallback.
    let cancelled = ui.select(
        "Pick again".to_string(),
        vec!["x".to_string()],
        UiDialogOptions::default(),
    );
    let lines = wait_for_lines(&rpc.out, 5000, |lines| {
        lines
            .iter()
            .filter(|l| l.contains(r#""type":"extension_ui_request""#))
            .count()
            >= 2
    })
    .await;
    let second: Value = lines
        .iter()
        .filter(|l| l.contains(r#""type":"extension_ui_request""#))
        .nth(1)
        .map(|l| serde_json::from_str(l).unwrap())
        .unwrap();
    let id2 = second["id"].as_str().unwrap();
    rpc.send(&format!(
        r#"{{"type":"extension_ui_response","id":"{id2}","cancelled":true}}"#
    ))
    .await;
    assert_eq!(cancelled.await, None);

    assert_eq!(rpc.finish().await, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn extension_ui_confirm_timeout_and_fire_and_forget_shapes() {
    let bridge = Arc::new(UiCapturingBridge::default());
    let rpc = Rpc::start(TestRuntimeOptions {
        with_auth: true,
        bridge: Some(bridge.clone()),
        ..Default::default()
    })
    .await;

    let ui = wait_for_ui(&bridge).await;

    // Timeout resolves with the cancel fallback (false).
    let confirmed = ui.confirm(
        "Sure?".to_string(),
        "Really?".to_string(),
        UiDialogOptions {
            timeout_ms: Some(50),
            signal: None,
        },
    );
    assert!(!confirmed.await);

    // Fire-and-forget shapes.
    ui.notify(
        "hello".to_string(),
        Some(pi_coding_agent::NotifyType::Warning),
    );
    ui.set_status("key1".to_string(), Some("busy".to_string()));
    ui.set_widget(
        "w1".to_string(),
        Some(vec!["line".to_string()]),
        Some(pi_coding_agent::WidgetPlacement::AboveEditor),
    );
    ui.set_title("Title".to_string());
    ui.set_editor_text("draft".to_string());

    let lines = wait_for_lines(&rpc.out, 5000, |lines| {
        lines
            .iter()
            .filter(|l| l.contains(r#""type":"extension_ui_request""#))
            .count()
            >= 6
    })
    .await;
    let requests: Vec<Value> = lines
        .iter()
        .filter(|l| l.contains(r#""type":"extension_ui_request""#))
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(requests[0]["method"], "confirm");
    assert_eq!(requests[0]["timeout"], 50);
    assert_eq!(requests[1]["method"], "notify");
    assert_eq!(requests[1]["notifyType"], "warning");
    assert_eq!(requests[2]["method"], "setStatus");
    assert_eq!(requests[2]["statusKey"], "key1");
    assert_eq!(requests[2]["statusText"], "busy");
    assert_eq!(requests[3]["method"], "setWidget");
    assert_eq!(requests[3]["widgetLines"], json!(["line"]));
    assert_eq!(requests[3]["widgetPlacement"], "aboveEditor");
    assert_eq!(requests[4]["method"], "setTitle");
    assert_eq!(requests[5]["method"], "set_editor_text");
    assert_eq!(requests[5]["text"], "draft");

    // Clearing a status omits the text key entirely (undefined, not null).
    ui.set_status("key1".to_string(), None);
    let lines = wait_for_lines(&rpc.out, 5000, |lines| {
        lines
            .iter()
            .filter(|l| l.contains(r#""method":"setStatus""#))
            .count()
            >= 2
    })
    .await;
    let clear_line = lines
        .iter()
        .filter(|l| l.contains(r#""method":"setStatus""#))
        .nth(1)
        .unwrap();
    assert!(!clear_line.contains("statusText"), "{clear_line}");

    assert_eq!(rpc.finish().await, 0);
}
