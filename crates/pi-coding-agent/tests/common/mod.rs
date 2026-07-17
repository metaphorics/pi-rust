//! Shared harness for mode tests: captured WireOut sink, scripted stream
//! functions, and an `AgentSessionRuntime` factory over temp dirs.
//!
//! Compiled once per test crate; not every crate uses every helper.
#![allow(dead_code)]

pub mod vt_terminal;

use std::collections::VecDeque;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use parking_lot::Mutex;
use pi_agent::{AgentThinkingLevel, StreamFn};
use pi_ai::{AssistantMessage, AssistantMessageEvent, Content, Model, StopReason, TextContent};
use pi_coding_agent::extension_bridge::{ExtensionBridge, ExtensionUiHost, NoopExtensionBridge};
use pi_coding_agent::session::runtime::{
    AgentSessionRuntime, CreateRuntimeFactory, CreateRuntimeOptions, CreateRuntimeResult,
};
use pi_coding_agent::session::services::{
    CreateAgentSessionServicesOptions, create_agent_session_services,
};
use pi_coding_agent::session::{AgentSession, AgentSessionConfig, PromptTemplate};
use pi_coding_agent::system_prompt::Skill;
use pi_coding_agent::wire_out::WireOut;
use pi_coding_agent::{AuthStorage, ModelRegistry, SessionManager};

// ============================================================================
// Captured stdout sink
// ============================================================================

struct SharedWriter(Arc<Mutex<Vec<u8>>>);

impl Write for SharedWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// In-memory stdout capture behind a [`WireOut`].
#[derive(Clone)]
pub struct CapturedOut {
    buf: Arc<Mutex<Vec<u8>>>,
}

impl CapturedOut {
    pub fn new() -> (Arc<WireOut>, CapturedOut) {
        let buf: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let out = Arc::new(WireOut::new_with_writer(Box::new(SharedWriter(
            buf.clone(),
        ))));
        (out, CapturedOut { buf })
    }

    pub fn raw(&self) -> String {
        String::from_utf8_lossy(&self.buf.lock()).into_owned()
    }

    /// Complete lines written so far (trailing partial line dropped).
    pub fn lines(&self) -> Vec<String> {
        let raw = self.raw();
        let mut lines: Vec<String> = raw.split('\n').map(str::to_string).collect();
        lines.pop();
        lines
    }
}

/// Poll until `pred(lines)` holds or the timeout elapses; returns the lines.
pub async fn wait_for_lines(
    out: &CapturedOut,
    timeout_ms: u64,
    pred: impl Fn(&[String]) -> bool,
) -> Vec<String> {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
    loop {
        let lines = out.lines();
        if pred(&lines) {
            return lines;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for output; lines so far:\n{}",
            lines.join("\n")
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

/// Assert `keys` appear in `raw` in the given order (wire key-order check).
pub fn assert_key_order(raw: &str, keys: &[&str]) {
    let mut last = 0;
    for key in keys {
        let needle = format!("\"{key}\"");
        let Some(index) = raw[last..].find(&needle) else {
            panic!("key {key} not found after byte {last} in: {raw}");
        };
        last += index + needle.len();
    }
}

// ============================================================================
// Scripted providers
// ============================================================================

pub fn assistant_text_message(model: &Model, text: &str) -> AssistantMessage {
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

/// Stream fn returning scripted responses in order (errors when exhausted).
pub fn scripted_stream_fn(script: Arc<Mutex<VecDeque<AssistantMessage>>>) -> StreamFn {
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
/// behaves like [`scripted_stream_fn`].
pub fn gated_stream_fn(
    script: Arc<Mutex<VecDeque<AssistantMessage>>>,
    gate: Arc<tokio::sync::Notify>,
) -> StreamFn {
    let calls = Arc::new(AtomicUsize::new(0));
    Arc::new(move |model: Model, _context, _options| {
        let script = script.clone();
        let gate = gate.clone();
        let calls = calls.clone();
        Box::pin(async move {
            if calls.fetch_add(1, Ordering::SeqCst) == 0 {
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

// ============================================================================
// Runtime factory over temp dirs
// ============================================================================

#[derive(Default)]
pub struct TestRuntimeOptions {
    /// Configure a runtime API key for anthropic (prompt preflight passes).
    pub with_auth: bool,
    /// Persist the session to a temp session dir (else in-memory).
    pub persisted: bool,
    /// Scripted assistant responses (ignored when `stream_fn` is set).
    pub script: Vec<AssistantMessage>,
    pub stream_fn: Option<StreamFn>,
    pub skills: Vec<Skill>,
    pub prompt_templates: Vec<PromptTemplate>,
    /// Extension bridge override (defaults to `NoopExtensionBridge`).
    pub bridge: Option<Arc<dyn ExtensionBridge>>,
    /// Pre-seeded `<agent-dir>/settings.json` content.
    pub global_settings: Option<serde_json::Value>,
    /// Pre-seeded `<agent-dir>/auth.json` content.
    pub auth_json: Option<serde_json::Value>,
}

pub struct TestRuntime {
    pub runtime: Arc<AgentSessionRuntime>,
    pub model: Model,
    pub tmp: tempfile::TempDir,
}

/// Test model used by scripted runs (from the builtin catalog).
pub fn test_model(auth: Arc<AuthStorage>) -> Model {
    let registry = ModelRegistry::in_memory(auth);
    registry
        .find("anthropic", "claude-opus-4-8")
        .expect("builtin model")
        .clone()
}

pub async fn make_runtime(options: TestRuntimeOptions) -> TestRuntime {
    let tmp = tempfile::tempdir().expect("tempdir");
    let cwd = tmp.path().join("project");
    std::fs::create_dir_all(&cwd).expect("cwd");
    let agent_dir = tmp.path().join("agent");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");
    if let Some(settings) = &options.global_settings {
        std::fs::write(
            agent_dir.join("settings.json"),
            serde_json::to_string_pretty(settings).expect("settings json"),
        )
        .expect("seed settings.json");
    }

    if let Some(auth_json) = &options.auth_json {
        std::fs::write(
            agent_dir.join("auth.json"),
            serde_json::to_string_pretty(auth_json).expect("auth json"),
        )
        .expect("seed auth.json");
    }
    let auth = Arc::new(AuthStorage::new(agent_dir.join("auth.json")));
    if options.with_auth {
        auth.set_runtime_api_key("anthropic".to_string(), "test-key".to_string());
    }
    let registry = Arc::new(tokio::sync::RwLock::new(ModelRegistry::in_memory(
        auth.clone(),
    )));
    let model = {
        let registry = registry.read().await;
        registry
            .find("anthropic", "claude-opus-4-8")
            .expect("builtin model")
            .clone()
    };

    let session_manager = if options.persisted {
        let session_dir = tmp.path().join("sessions");
        std::fs::create_dir_all(&session_dir).expect("session dir");
        SessionManager::create(&cwd, Some(session_dir), None).expect("session manager")
    } else {
        SessionManager::in_memory(Some(&cwd.to_string_lossy()), None).expect("session manager")
    };

    let script = Arc::new(Mutex::new(VecDeque::from(options.script)));
    let stream_fn = options
        .stream_fn
        .unwrap_or_else(|| scripted_stream_fn(script));
    let skills = options.skills;
    let prompt_templates = options.prompt_templates;
    let bridge = options
        .bridge
        .unwrap_or_else(|| Arc::new(NoopExtensionBridge::default()));

    let factory: CreateRuntimeFactory = Arc::new({
        let auth = auth.clone();
        let registry = registry.clone();
        let model = model.clone();
        move |create_options: CreateRuntimeOptions| {
            let auth = auth.clone();
            let registry = registry.clone();
            let model = model.clone();
            let stream_fn = stream_fn.clone();
            let skills = skills.clone();
            let prompt_templates = prompt_templates.clone();
            Box::pin(async move {
                let services = tokio::task::spawn_blocking({
                    let cwd = create_options.cwd.clone();
                    let agent_dir = create_options.agent_dir.clone();
                    move || {
                        create_agent_session_services(CreateAgentSessionServicesOptions {
                            cwd,
                            agent_dir: Some(agent_dir),
                            auth_storage: Some(auth),
                            model_registry: Some(registry),
                            ..Default::default()
                        })
                    }
                })
                .await
                .map_err(|e| e.to_string())?;

                let session_manager = create_options.session_manager;
                let settings_manager = services.settings_manager.clone();
                let model_registry = services.model_registry.clone();
                let cwd = services.cwd.clone();
                let session = tokio::task::spawn_blocking(move || {
                    AgentSession::new(AgentSessionConfig {
                        session_manager,
                        settings_manager,
                        model_registry,
                        cwd,
                        stream_fn: Some(stream_fn),
                        model: Some(model),
                        thinking_level: AgentThinkingLevel::Off,
                        scoped_models: Vec::new(),
                        custom_tools: Vec::new(),
                        initial_active_tool_names: None,
                        allowed_tool_names: None,
                        excluded_tool_names: None,
                        skills,
                        prompt_templates,
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
        }
    });

    let runtime = AgentSessionRuntime::create(
        factory,
        CreateRuntimeOptions {
            cwd: cwd.clone(),
            agent_dir,
            session_manager,
            session_start_reason: pi_coding_agent::SessionStartReason::Startup,
            previous_session_file: None,
        },
        bridge,
    )
    .await
    .expect("runtime");

    TestRuntime {
        runtime: Arc::new(runtime),
        model,
        tmp,
    }
}

// ============================================================================
// UI-capturing extension bridge (RPC extension UI plumbing tests)
// ============================================================================

/// Bridge that records the UI host bound by the active mode.
#[derive(Default)]
pub struct UiCapturingBridge {
    paths: Vec<PathBuf>,
    ui: Mutex<Option<Arc<dyn ExtensionUiHost>>>,
}

impl UiCapturingBridge {
    pub fn ui(&self) -> Option<Arc<dyn ExtensionUiHost>> {
        self.ui.lock().clone()
    }
}

impl ExtensionBridge for UiCapturingBridge {
    fn needs_sidecar(&self) -> bool {
        false
    }

    fn discovered_paths(&self) -> &[PathBuf] {
        &self.paths
    }

    fn bind_ui(&self, ui: Arc<dyn ExtensionUiHost>) {
        *self.ui.lock() = Some(ui);
    }
}
