mod common;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use pi_agent::StreamFn;
use pi_ai::{
    AssistantMessage, AssistantMessageEvent, Content, Message, StopReason, TextContent, ToolCall,
    UserContent,
};
use pi_coding_agent::modes::interactive::dispatch::{
    BuiltinCommand, DispatchAction, DispatchContext, dispatch_input,
};
use pi_coding_agent::modes::interactive::interactive_mode::{
    InteractiveMode, InteractiveModeOptions,
};
use pi_coding_agent::{ExtensionBridge, RegisteredCommand, SourceInfo};

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

#[tokio::test(flavor = "current_thread")]
async fn double_escape_honors_window_and_action_setting() {
    use std::cell::Cell;
    use std::rc::Rc;

    // Deterministic clock: t0 + controllable offset.
    let t0 = std::time::Instant::now();
    let offset: Rc<Cell<Duration>> = Rc::new(Cell::new(Duration::ZERO));

    let seed = make_runtime(TestRuntimeOptions {
        with_auth: true,
        ..Default::default()
    })
    .await;
    let response = assistant_text_message(&seed.model, "tree seed reply");
    drop(seed);
    let test = make_runtime(TestRuntimeOptions {
        with_auth: true,
        script: vec![response],
        ..Default::default()
    })
    .await;
    let (terminal, handle) = VtTerminal::new(100, 30);
    let clock = {
        let offset = Rc::clone(&offset);
        Rc::new(move || t0 + offset.get())
    };
    let mut mode = InteractiveMode::new(
        test.runtime.clone(),
        terminal,
        InteractiveModeOptions {
            clock: Some(clock),
            ..Default::default()
        },
    );
    mode.init();

    // Seed the session so /tree has entries.
    send(&mut mode, &handle, "seed message");
    send(&mut mode, &handle, "\r");
    pump_until(&mut mode, &handle, Duration::from_secs(2), |screen| {
        screen.contains("tree seed reply")
    })
    .await;

    // Two escapes 600ms apart: outside the window, nothing opens.
    send(&mut mode, &handle, "\x1b");
    offset.set(offset.get() + Duration::from_millis(600));
    send(&mut mode, &handle, "\x1b");
    mode.pump();
    assert!(
        handle.screen(|s| !s.serialize().contains("Session Tree")),
        "600ms apart must not open the tree selector"
    );

    // Second press 499ms later: inside the window, tree selector opens.
    offset.set(offset.get() + Duration::from_millis(499));
    send(&mut mode, &handle, "\x1b");
    mode.pump();
    let screen = handle.screen(pi_vt::VtScreen::serialize);
    assert!(
        screen.contains("Session Tree"),
        "double escape within 500ms must open /tree:\n{screen}"
    );
    assert_no_flicker(&handle);
}

#[tokio::test(flavor = "current_thread")]
async fn double_escape_respects_fork_and_none_settings() {
    // action = "none": double escape does nothing.
    let test = make_runtime(TestRuntimeOptions {
        global_settings: Some(serde_json::json!({ "doubleEscapeAction": "none" })),
        ..Default::default()
    })
    .await;
    let (terminal, handle) = VtTerminal::new(100, 30);
    let mut mode = InteractiveMode::new(
        test.runtime.clone(),
        terminal,
        InteractiveModeOptions::default(),
    );
    mode.init();
    send(&mut mode, &handle, "\x1b");
    send(&mut mode, &handle, "\x1b");
    mode.pump();
    assert!(handle.screen(|s| {
        let screen = s.serialize();
        !screen.contains("Session Tree") && !screen.contains("No messages to fork from")
    }));

    // action = "fork": double escape routes to the fork selector path.
    let test = make_runtime(TestRuntimeOptions {
        global_settings: Some(serde_json::json!({ "doubleEscapeAction": "fork" })),
        ..Default::default()
    })
    .await;
    let (terminal, handle) = VtTerminal::new(100, 30);
    let mut mode = InteractiveMode::new(
        test.runtime.clone(),
        terminal,
        InteractiveModeOptions::default(),
    );
    mode.init();
    send(&mut mode, &handle, "\x1b");
    send(&mut mode, &handle, "\x1b");
    mode.pump();
    let screen = handle.screen(pi_vt::VtScreen::serialize);
    assert!(
        screen.contains("No messages to fork from"),
        "fork action on empty session must show the fork status:\n{screen}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn warnings_submenu_persists_anthropic_extra_usage_setting() {
    let test = make_runtime(TestRuntimeOptions::default()).await;
    let settings_path = test.tmp.path().join("agent/settings.json");
    let (terminal, handle) = VtTerminal::new(100, 30);
    let mut mode = InteractiveMode::new(
        test.runtime.clone(),
        terminal,
        InteractiveModeOptions::default(),
    );
    mode.init();

    send(&mut mode, &handle, "/settings");
    send(&mut mode, &handle, "\r");
    pump_until(&mut mode, &handle, Duration::from_secs(1), |screen| {
        screen.contains("Auto-compact")
    })
    .await;

    // Search for the Warnings item and enter its submenu.
    send(&mut mode, &handle, "warnings");
    pump_until(&mut mode, &handle, Duration::from_secs(1), |screen| {
        screen.contains("Enable or disable individual warnings")
    })
    .await;
    send(&mut mode, &handle, "\r");
    pump_until(&mut mode, &handle, Duration::from_secs(1), |screen| {
        screen.contains("Anthropic extra usage")
    })
    .await;

    // Toggle: default true -> false, persisted under warnings.anthropicExtraUsage.
    send(&mut mode, &handle, "\r");
    mode.pump();
    let raw: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&settings_path).expect("settings written"))
            .expect("settings json");
    assert_eq!(
        raw["warnings"]["anthropicExtraUsage"],
        serde_json::Value::Bool(false),
        "toggle must persist nested warnings settings: {raw}"
    );
    assert_no_flicker(&handle);
}

const OAUTH_ANTHROPIC: &str = r#"{
    "anthropic": {
        "type": "oauth",
        "access": "access-token",
        "refresh": "refresh-token",
        "expires": 1893456000000
    }
}"#;

#[tokio::test(flavor = "current_thread")]
async fn anthropic_subscription_warning_fires_once_for_oauth_auth() {
    let test = make_runtime(TestRuntimeOptions {
        with_auth: true,
        auth_json: Some(serde_json::from_str(OAUTH_ANTHROPIC).expect("auth json")),
        ..Default::default()
    })
    .await;
    let (terminal, handle) = VtTerminal::new(120, 40);
    let mut mode = InteractiveMode::new(
        test.runtime.clone(),
        terminal,
        InteractiveModeOptions::default(),
    );
    mode.init();
    mode.pump();
    let screen = handle.screen(pi_vt::VtScreen::serialize);
    assert!(
        screen.contains("Anthropic subscription auth is active."),
        "oauth-backed anthropic model must warn on startup:\n{screen}"
    );

    // Cycling the model re-invokes the check; the warning must not repeat.
    let session = test.runtime.session();
    let original_model = session.model().map(|m| m.id);
    send(&mut mode, &handle, "\x10");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while session.model().map(|m| m.id) == original_model {
        assert!(
            tokio::time::Instant::now() < deadline,
            "model cycle never completed"
        );
        mode.pump();
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    mode.pump();
    let screen = handle.screen(|s| {
        let mut all = s.scrollback().join("\n");
        all.push('\n');
        all.push_str(&s.serialize());
        all
    });
    assert_eq!(
        screen
            .matches("Anthropic subscription auth is active.")
            .count(),
        1,
        "warning must fire exactly once:\n{screen}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn anthropic_subscription_warning_respects_setting_and_key_prefix() {
    // Disabled via warnings.anthropicExtraUsage = false: no warning.
    let test = make_runtime(TestRuntimeOptions {
        with_auth: true,
        auth_json: Some(serde_json::from_str(OAUTH_ANTHROPIC).expect("auth json")),
        global_settings: Some(serde_json::json!({
            "warnings": { "anthropicExtraUsage": false }
        })),
        ..Default::default()
    })
    .await;
    let (terminal, handle) = VtTerminal::new(120, 40);
    let mut mode = InteractiveMode::new(
        test.runtime.clone(),
        terminal,
        InteractiveModeOptions::default(),
    );
    mode.init();
    mode.pump();
    assert!(
        handle.screen(|s| !s.serialize().contains("Anthropic subscription auth")),
        "disabled warning setting must suppress the banner"
    );

    // sk-ant-oat API key resolves through the async registry path and warns.
    let test = make_runtime(TestRuntimeOptions {
        auth_json: Some(serde_json::json!({
            "anthropic": { "type": "api_key", "key": "sk-ant-oat-0123" }
        })),
        ..Default::default()
    })
    .await;
    let (terminal, handle) = VtTerminal::new(120, 40);
    let mut mode = InteractiveMode::new(
        test.runtime.clone(),
        terminal,
        InteractiveModeOptions::default(),
    );
    mode.init();
    pump_until(&mut mode, &handle, Duration::from_secs(2), |screen| {
        screen.contains("Anthropic subscription auth is active.")
    })
    .await;

    // A plain API key must NOT warn.
    let test = make_runtime(TestRuntimeOptions {
        auth_json: Some(serde_json::json!({
            "anthropic": { "type": "api_key", "key": "sk-ant-regular" }
        })),
        ..Default::default()
    })
    .await;
    let (terminal, handle) = VtTerminal::new(120, 40);
    let mut mode = InteractiveMode::new(
        test.runtime.clone(),
        terminal,
        InteractiveModeOptions::default(),
    );
    mode.init();
    for _ in 0..20 {
        mode.pump();
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        handle.screen(|s| !s.serialize().contains("Anthropic subscription auth")),
        "plain API key must not trigger the subscription warning"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn ctrl_z_suspends_terminal_signals_and_resumes_with_full_repaint() {
    use std::rc::Rc;

    let test = make_runtime(TestRuntimeOptions::default()).await;
    let (terminal, handle) = VtTerminal::new(80, 24);
    let signal_log: Rc<std::cell::RefCell<Vec<&'static str>>> =
        Rc::new(std::cell::RefCell::new(Vec::new()));
    let suspend_signal = {
        let signal_log = Rc::clone(&signal_log);
        let handle = handle.clone();
        Rc::new(move || {
            // At signal time the terminal must already be suspended and not
            // yet resumed (oracle: ui.stop() precedes SIGTSTP).
            assert_eq!(handle.lifecycle(), vec!["suspend"]);
            signal_log.borrow_mut().push("sigtstp");
        })
    };
    let mut mode = InteractiveMode::new(
        test.runtime.clone(),
        terminal,
        InteractiveModeOptions {
            suspend_signal: Some(suspend_signal),
            ..Default::default()
        },
    );
    mode.init();
    mode.pump();
    let frames_before = handle.screen(pi_vt::VtScreen::sync_frames_completed);

    // Ctrl+Z (0x1a) through the editor interceptor.
    send(&mut mode, &handle, "\x1a");
    mode.pump();

    assert_eq!(
        *signal_log.borrow(),
        vec!["sigtstp"],
        "SIGTSTP step must run"
    );
    assert_eq!(handle.lifecycle(), vec!["suspend", "resume"]);
    let frames_after = handle.screen(pi_vt::VtScreen::sync_frames_completed);
    assert!(
        frames_after > frames_before,
        "resume must repaint ({frames_before} -> {frames_after})"
    );
    // UI recovered: footer still present, input still works.
    send(&mut mode, &handle, "after resume");
    mode.pump();
    let screen = handle.screen(pi_vt::VtScreen::serialize);
    assert!(screen.contains("after resume"), "{screen}");
    assert_no_flicker(&handle);
}

/// Fg color of the editor's bottom border (last full-width `─` row).
fn editor_border_fg(handle: &VtHandle) -> pi_vt::Color {
    handle.screen(|screen| {
        let rows = screen.rows();
        let row = rows
            .iter()
            .rposition(|r| {
                let dashes = r.chars().filter(|c| *c == '─').count();
                dashes > 40 && r.chars().all(|c| c == '─' || c == ' ')
            })
            .expect("editor border row");
        screen.cell(0, row).expect("border cell").style.fg
    })
}

#[tokio::test(flavor = "current_thread")]
async fn editor_border_tracks_thinking_level_and_bash_mode() {
    let test = make_runtime(TestRuntimeOptions {
        with_auth: true,
        ..Default::default()
    })
    .await;
    let session = test.runtime.session();
    let (terminal, handle) = VtTerminal::new(80, 24);
    let mut mode = InteractiveMode::new(
        test.runtime.clone(),
        terminal,
        InteractiveModeOptions::default(),
    );
    mode.init();
    mode.pump();
    let off_color = editor_border_fg(&handle);

    // `!` prefix → bash mode border override.
    send(&mut mode, &handle, "!echo hi");
    mode.pump();
    let bash_color = editor_border_fg(&handle);
    assert_ne!(off_color, bash_color, "bash mode must recolor the border");

    // Escape leaves bash mode and restores the thinking-level border.
    send(&mut mode, &handle, "\x1b");
    mode.pump();
    assert_eq!(editor_border_fg(&handle), off_color);
    assert!(handle.screen(|s| !s.serialize().contains("!echo")));

    // Shift+Tab cycles the thinking level → new border color.
    send(&mut mode, &handle, "\x1b[Z");
    mode.pump();
    assert_ne!(
        session.thinking_level(),
        pi_agent::AgentThinkingLevel::Off,
        "shift+tab should cycle thinking"
    );
    let minimal_color = editor_border_fg(&handle);
    assert_ne!(minimal_color, off_color, "thinking border must change");
    assert_ne!(minimal_color, bash_color);
    assert_no_flicker(&handle);
}

#[tokio::test(flavor = "current_thread")]
async fn terminal_progress_setting_drives_osc_9_4_around_agent_runs() {
    // Enabled: agent_start → set_progress(true), agent_end → set_progress(false).
    let test = make_runtime(TestRuntimeOptions {
        with_auth: true,
        script: vec![],
        global_settings: Some(serde_json::json!({
            "terminal": { "showTerminalProgress": true }
        })),
        ..Default::default()
    })
    .await;
    let response = assistant_text_message(&test.model, "progress reply");
    drop(test);
    let test = make_runtime(TestRuntimeOptions {
        with_auth: true,
        script: vec![response.clone()],
        global_settings: Some(serde_json::json!({
            "terminal": { "showTerminalProgress": true }
        })),
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
    assert!(handle.progress_calls().is_empty(), "no progress before run");

    send(&mut mode, &handle, "go");
    send(&mut mode, &handle, "\r");
    pump_until(&mut mode, &handle, Duration::from_secs(2), |screen| {
        screen.contains("progress reply")
    })
    .await;
    pump_until(&mut mode, &handle, Duration::from_secs(2), |_| {
        handle.progress_calls() == vec![true, false]
    })
    .await;

    // Disabled (default): the same run issues no OSC 9;4 traffic at all.
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
    send(&mut mode, &handle, "go");
    send(&mut mode, &handle, "\r");
    pump_until(&mut mode, &handle, Duration::from_secs(2), |screen| {
        screen.contains("progress reply")
    })
    .await;
    assert!(
        handle.progress_calls().is_empty(),
        "progress emitted with setting off: {:?}",
        handle.progress_calls()
    );
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

// ============================================================================
// Follow-up (Alt+Enter) / dequeue (Alt+Up) — oracle handleFollowUp (:3672)
// and handleDequeue (:3704)
// ============================================================================

/// Kitty CSI-u encoding of Alt+Enter (matches independent of the global
/// kitty-active flag, unlike legacy `\x1b\r`).
const ALT_ENTER: &str = "\x1b[13;3u";
/// CSI 1;3A — Alt+Up.
const ALT_UP: &str = "\x1b[1;3A";

#[tokio::test(flavor = "current_thread")]
async fn idle_alt_enter_submits_like_enter() {
    let seed = make_runtime(TestRuntimeOptions {
        with_auth: true,
        ..Default::default()
    })
    .await;
    let response = assistant_text_message(&seed.model, "IDLE-FOLLOWUP-REPLY");
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

    send(&mut mode, &handle, "idle follow-up text");
    send(&mut mode, &handle, ALT_ENTER);
    let screen = pump_until(&mut mode, &handle, Duration::from_secs(2), |screen| {
        screen.contains("IDLE-FOLLOWUP-REPLY")
    })
    .await;
    // Submitted as a normal prompt: user message rendered, nothing queued.
    assert!(screen.contains("idle follow-up text"));
    assert!(!screen.contains("Follow-up:"), "{screen}");
    assert!(test.runtime.session().get_follow_up_messages().is_empty());
    assert_no_flicker(&handle);
}

/// Bridge that registers one extension command (`/ext`).
struct CommandBridge;

impl ExtensionBridge for CommandBridge {
    fn needs_sidecar(&self) -> bool {
        false
    }

    fn discovered_paths(&self) -> &[PathBuf] {
        &[]
    }

    fn registered_commands(&self) -> Vec<RegisteredCommand> {
        vec![RegisteredCommand {
            invocation_name: "ext".to_owned(),
            description: None,
            source_info: SourceInfo::synthetic("test-ext", "test", None, None, None),
        }]
    }
}

fn last_user_text(messages: &[Message]) -> String {
    messages
        .iter()
        .rev()
        .find_map(|message| match message {
            Message::User(user) => Some(match &user.content {
                UserContent::Text(text) => text.clone(),
                UserContent::Blocks(blocks) => blocks
                    .iter()
                    .filter_map(|block| match block {
                        Content::Text(text) => Some(text.text.to_string()),
                        _ => None,
                    })
                    .collect(),
            }),
            _ => None,
        })
        .unwrap_or_default()
}

/// Stream fn keyed on the EXACT latest user text: known prompts answer
/// immediately, anything else (the compaction summarization request) stalls,
/// holding `is_compacting()` true for the duration of the test.
fn compaction_routing_stream_fn() -> StreamFn {
    Arc::new(move |model, context, _options| {
        Box::pin(async move {
            let stream = pi_ai::create_assistant_message_event_stream();
            let reply = match last_user_text(&context.messages).as_str() {
                "seed please" => Some("SEED-DONE"),
                "/ext go" => Some("EXT-RAN"),
                _ => None,
            };
            if let Some(text) = reply {
                let message = assistant_text_message(&model, text);
                stream.push(AssistantMessageEvent::Done {
                    reason: StopReason::Stop,
                    message,
                });
            }
            // else: no events — the summarization future stays pending.
            stream
        })
    })
}

#[tokio::test(flavor = "current_thread")]
async fn compaction_executes_extension_commands_and_queues_plain_text() {
    let test = make_runtime(TestRuntimeOptions {
        with_auth: true,
        stream_fn: Some(compaction_routing_stream_fn()),
        bridge: Some(Arc::new(CommandBridge)),
        // Tiny keep-recent budget so the seeded exchanges are compactable.
        global_settings: Some(serde_json::json!({
            "compaction": { "keepRecentTokens": 1 }
        })),
        ..Default::default()
    })
    .await;
    let session = test.runtime.session();
    let (terminal, handle) = VtTerminal::new(100, 30);
    let mut mode = InteractiveMode::new(
        test.runtime.clone(),
        terminal,
        InteractiveModeOptions::default(),
    );
    mode.init();

    // Seed two exchanges so prepare_compaction finds a cut point between
    // turns, then start a compaction that never finishes (stalled
    // summarization stream).
    for _ in 0..2 {
        send(&mut mode, &handle, "seed please");
        send(&mut mode, &handle, "\r");
        pump_until(&mut mode, &handle, Duration::from_secs(2), |screen| {
            screen.contains("SEED-DONE")
        })
        .await;
        pump_until(&mut mode, &handle, Duration::from_secs(2), |_| {
            !test.runtime.session().is_streaming()
        })
        .await;
    }
    send(&mut mode, &handle, "/compact");
    send(&mut mode, &handle, "\r");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while !session.is_compacting() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "compaction never started:\n{}",
            handle.screen(|screen| screen.serialize())
        );
        mode.pump();
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // Registered extension command: executes immediately via session.prompt.
    send(&mut mode, &handle, "/ext go");
    send(&mut mode, &handle, ALT_ENTER);
    let screen = pump_until(&mut mode, &handle, Duration::from_secs(2), |screen| {
        screen.contains("EXT-RAN")
    })
    .await;
    assert!(
        !screen.contains("Queued message for after compaction"),
        "extension command was queued instead of executed:\n{screen}"
    );
    assert!(
        session.is_compacting(),
        "compaction should still be running"
    );

    // Plain text: queues for after compaction.
    send(&mut mode, &handle, "plain queued text");
    send(&mut mode, &handle, ALT_ENTER);
    let screen = pump_until(&mut mode, &handle, Duration::from_secs(2), |screen| {
        screen.contains("Queued message for after compaction")
    })
    .await;
    assert!(screen.contains("Follow-up: plain queued text"), "{screen}");
    assert_no_flicker(&handle);
}

#[tokio::test(flavor = "current_thread")]
async fn dequeue_reports_zero_and_plural_restore_statuses() {
    let test = make_runtime(TestRuntimeOptions {
        with_auth: true,
        stream_fn: Some(delayed_text_stream(
            "SLOW-STREAM-BODY",
            Duration::from_secs(5),
        )),
        ..Default::default()
    })
    .await;
    let (terminal, handle) = VtTerminal::new(100, 30);
    let mut mode = InteractiveMode::new(
        test.runtime.clone(),
        terminal,
        InteractiveModeOptions::default(),
    );
    mode.init();

    // Nothing queued: exact zero-restore status.
    send(&mut mode, &handle, ALT_UP);
    pump_until(&mut mode, &handle, Duration::from_secs(2), |screen| {
        screen.contains("No queued messages to restore")
    })
    .await;

    // Queue two follow-ups while streaming, then dequeue both.
    send(&mut mode, &handle, "go now");
    send(&mut mode, &handle, "\r");
    pump_until(&mut mode, &handle, Duration::from_secs(2), |screen| {
        screen.contains("SLOW-STREAM-BODY")
    })
    .await;
    send(&mut mode, &handle, "first queued");
    send(&mut mode, &handle, ALT_ENTER);
    pump_until(&mut mode, &handle, Duration::from_secs(2), |screen| {
        screen.contains("Follow-up: first queued")
    })
    .await;
    send(&mut mode, &handle, "second queued");
    send(&mut mode, &handle, ALT_ENTER);
    pump_until(&mut mode, &handle, Duration::from_secs(2), |screen| {
        screen.contains("Follow-up: second queued")
    })
    .await;
    send(&mut mode, &handle, ALT_UP);
    let screen = pump_until(&mut mode, &handle, Duration::from_secs(2), |screen| {
        screen.contains("Restored 2 queued messages to editor")
    })
    .await;
    // Both messages are back in the editor; queue display is gone.
    assert!(screen.contains("first queued"));
    assert!(screen.contains("second queued"));
    assert!(!screen.contains("Follow-up: first queued"), "{screen}");
    assert!(test.runtime.session().get_follow_up_messages().is_empty());
    assert_no_flicker(&handle);
}

#[tokio::test(flavor = "current_thread")]
async fn dequeue_reports_singular_restore_status() {
    let test = make_runtime(TestRuntimeOptions {
        with_auth: true,
        stream_fn: Some(delayed_text_stream(
            "SLOW-STREAM-BODY",
            Duration::from_secs(5),
        )),
        ..Default::default()
    })
    .await;
    let (terminal, handle) = VtTerminal::new(100, 30);
    let mut mode = InteractiveMode::new(
        test.runtime.clone(),
        terminal,
        InteractiveModeOptions::default(),
    );
    mode.init();

    send(&mut mode, &handle, "go now");
    send(&mut mode, &handle, "\r");
    pump_until(&mut mode, &handle, Duration::from_secs(2), |screen| {
        screen.contains("SLOW-STREAM-BODY")
    })
    .await;
    send(&mut mode, &handle, "solo message");
    send(&mut mode, &handle, ALT_ENTER);
    pump_until(&mut mode, &handle, Duration::from_secs(2), |screen| {
        screen.contains("Follow-up: solo message")
    })
    .await;
    send(&mut mode, &handle, ALT_UP);
    pump_until(&mut mode, &handle, Duration::from_secs(2), |screen| {
        screen.contains("Restored 1 queued message to editor")
    })
    .await;
    assert_no_flicker(&handle);
}

#[tokio::test(flavor = "current_thread")]
async fn follow_up_expands_large_paste_markers() {
    let test = make_runtime(TestRuntimeOptions {
        with_auth: true,
        stream_fn: Some(delayed_text_stream(
            "SLOW-STREAM-BODY",
            Duration::from_secs(5),
        )),
        ..Default::default()
    })
    .await;
    let session = test.runtime.session();
    let (terminal, handle) = VtTerminal::new(100, 30);
    let mut mode = InteractiveMode::new(
        test.runtime.clone(),
        terminal,
        InteractiveModeOptions::default(),
    );
    mode.init();

    send(&mut mode, &handle, "go now");
    send(&mut mode, &handle, "\r");
    pump_until(&mut mode, &handle, Duration::from_secs(2), |screen| {
        screen.contains("SLOW-STREAM-BODY")
    })
    .await;

    // Large bracketed paste collapses to a marker in the editor.
    let pasted = "paste-line\n".repeat(12);
    let pasted = pasted.trim_end();
    send(&mut mode, &handle, &format!("\x1b[200~{pasted}\x1b[201~"));
    pump_until(&mut mode, &handle, Duration::from_secs(2), |screen| {
        screen.contains("[paste #1")
    })
    .await;

    // Alt+Enter queues the EXPANDED text, not the marker.
    send(&mut mode, &handle, ALT_ENTER);
    pump_until(&mut mode, &handle, Duration::from_secs(2), |screen| {
        screen.contains("Follow-up:")
    })
    .await;
    let follow_up = session.get_follow_up_messages();
    assert_eq!(follow_up.len(), 1, "{follow_up:?}");
    assert!(
        follow_up[0].contains("paste-line\npaste-line"),
        "{:?}",
        follow_up[0]
    );
    assert!(!follow_up[0].contains("[paste"), "{:?}", follow_up[0]);
    assert_no_flicker(&handle);
}

/// Editor history is in-memory only (oracle editor.ts:402-408): a submitted
/// prompt is recalled with Up and resubmits verbatim, and nothing is
/// persisted under the agent dir.
#[tokio::test(flavor = "current_thread")]
async fn editor_history_recalls_in_memory_and_persists_nothing() {
    let seed = make_runtime(TestRuntimeOptions {
        with_auth: true,
        ..Default::default()
    })
    .await;
    let first = assistant_text_message(&seed.model, "first reply");
    let second = assistant_text_message(&seed.model, "second reply");
    drop(seed);
    let test = make_runtime(TestRuntimeOptions {
        with_auth: true,
        script: vec![first, second],
        ..Default::default()
    })
    .await;
    let session = test.runtime.session();
    let (terminal, handle) = VtTerminal::new(90, 28);
    let mut mode = InteractiveMode::new(
        test.runtime.clone(),
        terminal,
        InteractiveModeOptions::default(),
    );
    mode.init();

    send(&mut mode, &handle, "alpha-one prompt");
    send(&mut mode, &handle, "\r");
    pump_until(&mut mode, &handle, Duration::from_secs(2), |screen| {
        screen.contains("first reply")
    })
    .await;

    // Up recalls the submitted prompt into the (now empty) editor; Enter
    // resubmits it verbatim.
    send(&mut mode, &handle, "\x1b[A");
    send(&mut mode, &handle, "\r");
    pump_until(&mut mode, &handle, Duration::from_secs(2), |screen| {
        screen.contains("second reply")
    })
    .await;

    let user_texts: Vec<String> = session
        .messages()
        .iter()
        .filter_map(|message| {
            if let Some(Message::User(user)) = message.as_message() {
                match &user.content {
                    UserContent::Text(text) => Some(text.clone()),
                    UserContent::Blocks(blocks) => blocks.iter().find_map(|block| {
                        if let Content::Text(text) = block {
                            Some(text.text.to_string())
                        } else {
                            None
                        }
                    }),
                }
            } else {
                None
            }
        })
        .collect();
    assert_eq!(
        user_texts,
        vec!["alpha-one prompt".to_string(), "alpha-one prompt".to_string()],
        "Up-arrow recall must resubmit the in-memory history entry"
    );

    // Nothing history-related is written to disk: the agent dir holds no
    // new files beyond what runtime creation itself produced.
    let unexpected: Vec<String> = std::fs::read_dir(test.tmp.path().join("agent"))
        .expect("agent dir")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains("history"))
        .collect();
    assert!(unexpected.is_empty(), "{unexpected:?}");
    assert_no_flicker(&handle);
}

/// Oracle interactive-mode.ts:892-894: the initial startup prompt is sent
/// with `initialImages`; the provider call must see the image content in
/// the first user message.
#[tokio::test(flavor = "current_thread")]
async fn initial_message_carries_initial_images_to_the_provider() {
    use std::collections::VecDeque;
    use parking_lot::Mutex;

    let contexts: Arc<Mutex<Vec<pi_ai::Context>>> = Arc::new(Mutex::new(Vec::new()));
    let seed = make_runtime(TestRuntimeOptions {
        with_auth: true,
        ..Default::default()
    })
    .await;
    let reply = assistant_text_message(&seed.model, "image-received");
    drop(seed);
    let stream_fn: StreamFn = {
        let contexts = contexts.clone();
        let script = Arc::new(Mutex::new(VecDeque::from(vec![reply])));
        Arc::new(move |model: pi_ai::Model, context, _options| {
            let contexts = contexts.clone();
            let script = script.clone();
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
    };
    let test = make_runtime(TestRuntimeOptions {
        with_auth: true,
        stream_fn: Some(stream_fn),
        ..Default::default()
    })
    .await;
    let (terminal, handle) = VtTerminal::new(90, 28);
    let mut mode = InteractiveMode::new(
        test.runtime.clone(),
        terminal,
        InteractiveModeOptions {
            initial_message: Some("describe the attachment".to_string()),
            initial_images: vec![pi_ai::ImageContent {
                data: "QUJDREVG".to_string(),
                mime_type: "image/png".to_string(),
            }],
            ..Default::default()
        },
    );
    mode.init();
    mode.begin_startup_messages();
    pump_until(&mut mode, &handle, Duration::from_secs(2), |screen| {
        screen.contains("image-received")
    })
    .await;

    let contexts = contexts.lock();
    assert_eq!(contexts.len(), 1, "one provider call expected");
    let Message::User(user) = &contexts[0].messages[0] else {
        panic!("first context message must be the user prompt");
    };
    let UserContent::Blocks(blocks) = &user.content else {
        panic!("image prompt must use content blocks");
    };
    assert!(
        blocks.iter().any(|block| matches!(
            block,
            Content::Image(image) if image.data == "QUJDREVG" && image.mime_type == "image/png"
        )),
        "provider context missing the initial image: {blocks:?}"
    );
    assert!(blocks.iter().any(|block| matches!(
        block,
        Content::Text(text) if text.text.to_string().contains("describe the attachment")
    )));
}

/// Oracle interactive-mode.ts:876-878: migrated credentials surface as a
/// TUI startup warning with the exact oracle string.
#[tokio::test(flavor = "current_thread")]
async fn migrated_providers_show_startup_warning() {
    let test = make_runtime(TestRuntimeOptions {
        with_auth: true,
        ..Default::default()
    })
    .await;
    let (terminal, handle) = VtTerminal::new(100, 30);
    let mut mode = InteractiveMode::new(
        test.runtime.clone(),
        terminal,
        InteractiveModeOptions {
            migrated_providers: vec!["anthropic".to_string(), "openai".to_string()],
            ..Default::default()
        },
    );
    mode.init();
    let screen = pump_until(&mut mode, &handle, Duration::from_secs(2), |screen| {
        screen.contains("Migrated credentials to auth.json: anthropic, openai")
    })
    .await;
    assert!(screen.contains("Migrated credentials to auth.json: anthropic, openai"));
}

/// Oracle `maybeSaveImplicitProjectTrustAfterReload` (interactive-mode.ts:
/// 4378-4402): a cwd implicitly trusted at startup that gained a `.pi`
/// directory during the session gets its trust persisted by the first
/// `/reload`; a second reload does not re-save.
#[tokio::test(flavor = "multi_thread")]
async fn reload_persists_implicit_project_trust_once() {
    let test = make_runtime(TestRuntimeOptions {
        with_auth: true,
        ..Default::default()
    })
    .await;
    let session = test.runtime.session();
    let cwd = session.cwd().to_path_buf();
    let agent_dir = test.runtime.services().agent_dir.clone();
    let (terminal, handle) = VtTerminal::new(160, 30);
    let mut mode = InteractiveMode::new(
        test.runtime.clone(),
        terminal,
        InteractiveModeOptions {
            auto_trust_on_reload_cwd: Some(cwd.clone()),
            ..Default::default()
        },
    );
    mode.init();

    // The project gains trust-requiring resources mid-session.
    std::fs::create_dir_all(cwd.join(".pi")).expect(".pi dir");
    std::fs::write(cwd.join(".pi/settings.json"), "{}").expect("project settings");

    send(&mut mode, &handle, "/reload");
    send(&mut mode, &handle, "\r");
    pump_until(&mut mode, &handle, Duration::from_secs(5), |screen| {
        screen.contains(
            "Reloaded keybindings, extensions, skills, prompts, themes, and context files; saved project trust",
        )
    })
    .await;

    let trust_json =
        std::fs::read_to_string(agent_dir.join("trust.json")).expect("trust.json written");
    let parsed: serde_json::Value = serde_json::from_str(&trust_json).expect("valid trust.json");
    let canonical = cwd.canonicalize().unwrap_or(cwd.clone());
    let entry = parsed
        .get(canonical.to_string_lossy().as_ref())
        .or_else(|| parsed.get(cwd.to_string_lossy().as_ref()));
    assert_eq!(
        entry.and_then(serde_json::Value::as_bool),
        Some(true),
        "trust.json must record the cwd as trusted: {trust_json}"
    );

    // Second reload: the saved decision short-circuits — the status is the
    // plain string (it REPLACES the previous status) and trust.json is
    // byte-identical.
    send(&mut mode, &handle, "/reload");
    send(&mut mode, &handle, "\r");
    let screen = pump_until(&mut mode, &handle, Duration::from_secs(5), |screen| {
        screen.contains("Reloaded keybindings, extensions, skills, prompts, themes, and context files")
            && !screen.contains("; saved project trust")
    })
    .await;
    assert!(!screen.contains("; saved project trust"), "{screen}");
    let trust_json_after = std::fs::read_to_string(agent_dir.join("trust.json")).expect("trust.json");
    assert_eq!(trust_json, trust_json_after, "second reload must not rewrite trust.json");
}

/// Without `autoTrustOnReloadCwd` (explicit override or trust-requiring
/// resources present at startup) `/reload` never writes trust.json.
#[tokio::test(flavor = "multi_thread")]
async fn reload_without_auto_trust_saves_nothing() {
    let test = make_runtime(TestRuntimeOptions {
        with_auth: true,
        ..Default::default()
    })
    .await;
    let session = test.runtime.session();
    let cwd = session.cwd().to_path_buf();
    let agent_dir = test.runtime.services().agent_dir.clone();
    let (terminal, handle) = VtTerminal::new(100, 30);
    let mut mode = InteractiveMode::new(
        test.runtime.clone(),
        terminal,
        InteractiveModeOptions::default(),
    );
    mode.init();
    std::fs::create_dir_all(cwd.join(".pi")).expect(".pi dir");
    std::fs::write(cwd.join(".pi/settings.json"), "{}").expect("project settings");

    send(&mut mode, &handle, "/reload");
    send(&mut mode, &handle, "\r");
    let screen = pump_until(&mut mode, &handle, Duration::from_secs(5), |screen| {
        screen.contains("Reloaded keybindings, extensions, skills, prompts, themes, and context files")
    })
    .await;
    assert!(!screen.contains("; saved project trust"), "{screen}");
    assert!(
        !agent_dir.join("trust.json").exists(),
        "no implicit trust may be persisted without autoTrustOnReloadCwd"
    );
}
