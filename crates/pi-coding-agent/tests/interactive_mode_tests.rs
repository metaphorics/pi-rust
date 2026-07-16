mod common;

use std::sync::Arc;
use std::time::Duration;

use pi_agent::StreamFn;
use pi_ai::{AssistantMessage, AssistantMessageEvent, Content, StopReason, TextContent, ToolCall};
use pi_coding_agent::modes::interactive::dispatch::{
    BuiltinCommand, DispatchAction, DispatchContext, dispatch_input,
};
use pi_coding_agent::modes::interactive::interactive_mode::{
    InteractiveMode, InteractiveModeOptions,
};

use common::vt_terminal::{VtHandle, VtTerminal};
use common::{TestRuntimeOptions, assistant_text_message, make_runtime};

fn assert_no_flicker(handle: &VtHandle) {
    handle.screen(|screen| {
        assert_eq!(
            screen.cells_mutated_outside_sync(),
            0,
            "frame mutated cells outside CSI 2026 synchronization:\n{}",
            screen.serialize()
        );
        assert!(
            screen.sync_frames_completed() > 0,
            "no synchronized frame completed"
        );
        assert!(
            !screen.in_sync_update(),
            "frame left synchronized update open"
        );
    });
}

fn send(mode: &mut InteractiveMode, handle: &VtHandle, data: &str) {
    handle.send_input(data);
    mode.pump();
}

async fn pump_until(
    mode: &mut InteractiveMode,
    handle: &VtHandle,
    timeout: Duration,
    predicate: impl Fn(&str) -> bool,
) -> String {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        mode.pump();
        let screen = handle.screen(|screen| screen.serialize());
        if predicate(&screen) {
            return screen;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for screen state:\n{screen}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn tool_call_message(model: &pi_ai::Model, command: &str) -> AssistantMessage {
    let mut arguments = serde_json::Map::new();
    arguments.insert(
        "command".to_owned(),
        serde_json::Value::String(command.to_owned()),
    );
    AssistantMessage {
        content: vec![Content::ToolCall(ToolCall {
            id: "tool-1".to_owned(),
            name: "bash".to_owned(),
            arguments,
            thought_signature: None,
        })],
        api: model.api.clone(),
        provider: model.provider.clone(),
        model: model.id.clone(),
        response_model: None,
        response_id: None,
        diagnostics: None,
        usage: pi_ai::Usage::default(),
        stop_reason: StopReason::ToolUse,
        error_message: None,
        timestamp: 1_700_000_000_000,
    }
}

fn delayed_text_stream(text: &'static str, delay: Duration) -> StreamFn {
    Arc::new(move |model, _context, options| {
        Box::pin(async move {
            let stream = pi_ai::create_assistant_message_event_stream();
            let mut partial = assistant_text_message(&model, "");
            partial.stop_reason = StopReason::Stop;
            stream.push(AssistantMessageEvent::Start {
                partial: partial.clone(),
            });
            partial.content = vec![Content::Text(TextContent {
                text: text.into(),
                text_signature: None,
            })];
            stream.push(AssistantMessageEvent::TextStart {
                content_index: 0,
                partial: partial.clone(),
            });
            stream.push(AssistantMessageEvent::TextDelta {
                content_index: 0,
                delta: text.into(),
                partial: partial.clone(),
            });
            let producer = stream.clone();
            tokio::spawn(async move {
                let deadline = tokio::time::Instant::now() + delay;
                while tokio::time::Instant::now() < deadline {
                    if options
                        .cancel
                        .as_ref()
                        .is_some_and(|cancel| cancel.is_cancelled())
                    {
                        let mut aborted = partial;
                        aborted.stop_reason = StopReason::Aborted;
                        producer.push(AssistantMessageEvent::Error {
                            reason: StopReason::Aborted,
                            error: aborted,
                        });
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                let mut final_message = partial;
                final_message.content = vec![Content::Text(TextContent {
                    text: format!("{text} never-shown-final").into(),
                    text_signature: None,
                })];
                producer.push(AssistantMessageEvent::Done {
                    reason: StopReason::Stop,
                    message: final_message,
                });
            });
            stream
        })
    })
}

#[tokio::test(flavor = "current_thread")]
async fn text_prompt_streams_into_the_screen_grid_without_flicker() {
    let seed = make_runtime(TestRuntimeOptions {
        with_auth: true,
        ..Default::default()
    })
    .await;
    let response = assistant_text_message(&seed.model, "streamed row one\nstreamed row two");
    drop(seed);
    let test = make_runtime(TestRuntimeOptions {
        with_auth: true,
        script: vec![response],
        ..Default::default()
    })
    .await;
    let (terminal, handle) = VtTerminal::new(80, 24);
    let mut mode = InteractiveMode::new(
        test.runtime.clone(),
        terminal,
        InteractiveModeOptions::default(),
    );
    mode.init();

    send(&mut mode, &handle, "hello from grid");
    send(&mut mode, &handle, "\r");
    let screen = pump_until(&mut mode, &handle, Duration::from_secs(2), |screen| {
        screen.contains("streamed row one") && screen.contains("streamed row two")
    })
    .await;

    assert!(screen.contains("hello from grid"));
    assert_no_flicker(&handle);
}

#[tokio::test(flavor = "current_thread")]
async fn tool_call_changes_from_pending_to_success_background() {
    let seed = make_runtime(TestRuntimeOptions {
        with_auth: true,
        ..Default::default()
    })
    .await;
    let tool = tool_call_message(&seed.model, "sleep 0.25; printf tool-result");
    let final_message = assistant_text_message(&seed.model, "tool turn finished");
    drop(seed);
    let test = make_runtime(TestRuntimeOptions {
        with_auth: true,
        script: vec![tool, final_message],
        ..Default::default()
    })
    .await;
    let (terminal, handle) = VtTerminal::new(96, 30);
    let mut mode = InteractiveMode::new(
        test.runtime.clone(),
        terminal,
        InteractiveModeOptions::default(),
    );
    mode.init();

    send(&mut mode, &handle, "run the tool");
    send(&mut mode, &handle, "\r");
    pump_until(&mut mode, &handle, Duration::from_secs(2), |screen| {
        screen.contains("sleep 0.25")
    })
    .await;
    let pending_bg = handle.screen(|screen| {
        let row = screen.find_row("sleep 0.25").expect("pending tool row");
        screen.cell(1, row).expect("pending tool cell").style.bg
    });

    let finished = pump_until(&mut mode, &handle, Duration::from_secs(3), |screen| {
        screen.contains("tool-result") && screen.contains("tool turn finished")
    })
    .await;
    let success_bg = handle.screen(|screen| {
        let row = screen.find_row("sleep 0.25").expect("success tool row");
        screen.cell(1, row).expect("success tool cell").style.bg
    });

    assert!(finished.contains("tool-result"));
    assert_ne!(
        pending_bg, success_bg,
        "tool background did not reflect state transition"
    );
    assert_no_flicker(&handle);
}

#[tokio::test(flavor = "current_thread")]
async fn slash_autocomplete_lists_builtin_commands() {
    let test = make_runtime(TestRuntimeOptions::default()).await;
    let (terminal, handle) = VtTerminal::new(80, 24);
    let mut mode = InteractiveMode::new(test.runtime, terminal, InteractiveModeOptions::default());
    mode.init();

    send(&mut mode, &handle, "/");
    let _screen = pump_until(&mut mode, &handle, Duration::from_secs(1), |screen| {
        screen.contains("Open settings menu")
    })
    .await;
    assert!(_screen.contains("model"), "{_screen}");
    assert_no_flicker(&handle);
}

#[tokio::test(flavor = "current_thread")]
async fn settings_command_mounts_real_selector_and_applies_changes() {
    let test = make_runtime(TestRuntimeOptions::default()).await;
    let session = test.runtime.session();
    let (terminal, handle) = VtTerminal::new(100, 30);
    let mut mode = InteractiveMode::new(test.runtime, terminal, InteractiveModeOptions::default());
    mode.init();

    send(&mut mode, &handle, "/settings");
    send(&mut mode, &handle, "\r");
    pump_until(&mut mode, &handle, Duration::from_secs(1), |screen| {
        screen.contains("Auto-compact") && screen.contains("Type to search")
    })
    .await;
    assert!(session.auto_compaction_enabled());
    send(&mut mode, &handle, "\r");
    mode.pump();
    assert!(!session.auto_compaction_enabled());
    assert_no_flicker(&handle);
}

#[tokio::test(flavor = "current_thread")]
async fn login_api_key_flow_persists_credential_and_reports_success() {
    let test = make_runtime(TestRuntimeOptions::default()).await;
    let provider = test.model.provider.clone();
    let auth = test.runtime.services().auth_storage.clone();
    let (terminal, handle) = VtTerminal::new(100, 30);
    let mut mode = InteractiveMode::new(test.runtime, terminal, InteractiveModeOptions::default());
    mode.init();

    send(&mut mode, &handle, &format!("/login {provider}"));
    send(&mut mode, &handle, "\r");
    pump_until(&mut mode, &handle, Duration::from_secs(1), |screen| {
        screen.contains("Select provider to configure:")
    })
    .await;
    send(&mut mode, &handle, "\x1b[B");
    send(&mut mode, &handle, "\r");
    pump_until(&mut mode, &handle, Duration::from_secs(1), |screen| {
        screen.contains("Enter API key:")
    })
    .await;
    send(&mut mode, &handle, "persisted-test-key");
    send(&mut mode, &handle, "\r");
    pump_until(&mut mode, &handle, Duration::from_secs(2), |screen| {
        screen.contains("Saved API key for")
    })
    .await;

    let credential = auth.get(&provider).await.expect("read credential");
    match credential {
        Some(pi_ai::auth::Credential::ApiKey(api_key)) => {
            assert_eq!(api_key.key.as_deref(), Some("persisted-test-key"));
        }
        other => panic!("unexpected credential: {other:?}"),
    }
    send(&mut mode, &handle, "/logout");
    send(&mut mode, &handle, "\r");
    pump_until(&mut mode, &handle, Duration::from_secs(1), |screen| {
        screen.contains("Select provider to logout:")
    })
    .await;
    send(&mut mode, &handle, "\r");
    pump_until(&mut mode, &handle, Duration::from_secs(2), |screen| {
        screen.contains("Removed stored API key for")
    })
    .await;
    assert!(
        auth.get(&provider)
            .await
            .expect("read removed credential")
            .is_none()
    );

    assert_no_flicker(&handle);
}

#[tokio::test(flavor = "current_thread")]
async fn theme_command_swaps_editor_for_selector_and_escape_restores_it() {
    let test = make_runtime(TestRuntimeOptions::default()).await;
    let (terminal, handle) = VtTerminal::new(80, 24);
    let mut mode = InteractiveMode::new(test.runtime, terminal, InteractiveModeOptions::default());
    mode.init();

    send(&mut mode, &handle, "/theme");
    send(&mut mode, &handle, "\r");
    let selector = pump_until(&mut mode, &handle, Duration::from_secs(1), |screen| {
        screen.contains("dark") && screen.contains("light")
    })
    .await;
    assert!(
        !selector.contains("/theme"),
        "editor was not replaced by selector"
    );

    send(&mut mode, &handle, "\x1b");
    let restored = pump_until(&mut mode, &handle, Duration::from_secs(1), |screen| {
        !screen.contains("Built-in themes") && !screen.contains("Custom themes")
    })
    .await;
    assert!(
        !restored.contains("/theme"),
        "submitted editor text was not cleared"
    );
    assert_no_flicker(&handle);
}

#[tokio::test(flavor = "current_thread")]
async fn model_command_mounts_selector_after_a_completed_run() {
    let seed = make_runtime(TestRuntimeOptions {
        with_auth: true,
        ..Default::default()
    })
    .await;
    let response = assistant_text_message(&seed.model, "first run complete");
    drop(seed);
    let test = make_runtime(TestRuntimeOptions {
        with_auth: true,
        script: vec![response],
        ..Default::default()
    })
    .await;
    let (terminal, handle) = VtTerminal::new(100, 30);
    let mut mode = InteractiveMode::new(test.runtime, terminal, InteractiveModeOptions::default());
    mode.init();
    send(&mut mode, &handle, "first");
    send(&mut mode, &handle, "\r");
    pump_until(&mut mode, &handle, Duration::from_secs(2), |screen| {
        screen.contains("first run complete")
    })
    .await;

    send(&mut mode, &handle, "/model");
    send(&mut mode, &handle, "\r");
    pump_until(&mut mode, &handle, Duration::from_secs(2), |screen| {
        screen.contains("Only showing models from configured providers")
    })
    .await;
    assert_no_flicker(&handle);
}

#[tokio::test(flavor = "current_thread")]
async fn resize_reflows_transcript_without_stale_rows() {
    let seed = make_runtime(TestRuntimeOptions {
        with_auth: true,
        ..Default::default()
    })
    .await;
    let marker = "RESIZE-END-MARKER";
    let response = assistant_text_message(
        &seed.model,
        &format!(
            "A deliberately long assistant line that must wrap differently when the terminal width shrinks and never leave old wide rows behind. {marker}"
        ),
    );
    drop(seed);
    let test = make_runtime(TestRuntimeOptions {
        with_auth: true,
        script: vec![response],
        ..Default::default()
    })
    .await;
    let (terminal, handle) = VtTerminal::new(100, 26);
    let mut mode = InteractiveMode::new(test.runtime, terminal, InteractiveModeOptions::default());
    mode.init();
    send(&mut mode, &handle, "resize this");
    send(&mut mode, &handle, "\r");
    pump_until(&mut mode, &handle, Duration::from_secs(2), |screen| {
        screen.contains(marker)
    })
    .await;

    handle.resize(44, 26);
    let narrow = pump_until(&mut mode, &handle, Duration::from_secs(1), |screen| {
        screen.contains(marker)
    })
    .await;
    handle.screen(|screen| {
        assert_eq!(screen.rows().len(), 26);
        assert!(screen.rows().iter().all(|row| row.chars().count() <= 44));
        assert_eq!(screen.serialize(), narrow);
    });
    assert_no_flicker(&handle);
}

#[tokio::test(flavor = "current_thread")]
async fn escape_aborts_a_mid_stream_run_and_clears_working_status() {
    let test = make_runtime(TestRuntimeOptions {
        with_auth: true,
        stream_fn: Some(delayed_text_stream(
            "partial-stream",
            Duration::from_secs(5),
        )),
        ..Default::default()
    })
    .await;
    let session = test.runtime.session();
    let (terminal, handle) = VtTerminal::new(80, 24);
    let mut mode = InteractiveMode::new(test.runtime, terminal, InteractiveModeOptions::default());
    mode.init();
    send(&mut mode, &handle, "abort this");
    send(&mut mode, &handle, "\r");
    pump_until(&mut mode, &handle, Duration::from_secs(2), |screen| {
        screen.contains("partial-stream")
    })
    .await;
    assert!(session.is_streaming());

    send(&mut mode, &handle, "\x1b");
    let screen = pump_until(&mut mode, &handle, Duration::from_secs(2), |screen| {
        !session.is_streaming() && !screen.contains("Working...")
    })
    .await;
    assert!(!screen.contains("never-shown-final"));
    assert!(!screen.contains("Working..."));
    assert_no_flicker(&handle);
}

#[test]
fn submit_dispatch_preserves_oracle_precedence() {
    let extension = vec!["ext".to_owned()];
    let compacting = DispatchContext {
        is_compacting: true,
        is_streaming: true,
        is_bash_running: false,
        extension_commands: extension,
    };
    assert_eq!(
        dispatch_input("/ext keep order", &compacting),
        DispatchAction::ExtensionDuringCompaction {
            text: "/ext keep order".to_owned()
        }
    );
    assert_eq!(
        dispatch_input("ordinary", &compacting),
        DispatchAction::QueueCompaction {
            text: "ordinary".to_owned()
        }
    );
    assert_eq!(
        dispatch_input("/theme", &DispatchContext::default()),
        DispatchAction::Builtin(BuiltinCommand::Theme)
    );
    assert_eq!(
        dispatch_input("!! printf x", &DispatchContext::default()),
        DispatchAction::Bash {
            command: "printf x".to_owned(),
            excluded: true
        }
    );
}
