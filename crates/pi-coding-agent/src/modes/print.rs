//! Print mode (single-shot) — port of `modes/print-mode.ts`.
//!
//! Used for:
//! - `pi -p "prompt"` — text output (final assistant text only)
//! - `pi --mode json "prompt"` — JSONL event stream (see [`super::json`])
//!
//! This is a printer module of the `pi` binary: stdout goes through
//! [`WireOut`]; the `eprintln!` calls are the oracle's `console.error`
//! stderr contract, not diagnostics.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use parking_lot::Mutex;
use pi_agent::AgentMessage;
use pi_ai::{Content, ImageContent, Message, StopReason};

use crate::session::runtime::AgentSessionRuntime;
use crate::session::{AgentSession, AgentSessionEvent, PromptOptions};
use crate::wire_out::WireOut;

/// Output mode: text (final response only) or json (all events).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PrintOutputMode {
    #[default]
    Text,
    Json,
}

/// Options for print mode (oracle `PrintModeOptions`).
#[derive(Debug, Default)]
pub struct PrintModeOptions {
    /// Output mode: text for the final response only, json for all events.
    pub mode: PrintOutputMode,
    /// Additional prompts sent after `initial_message`.
    pub messages: Vec<String>,
    /// First message to send (may contain @file content).
    pub initial_message: Option<String>,
    /// Images attached to the initial message.
    pub initial_images: Vec<ImageContent>,
}

/// Stored unsubscribe closure for the active session subscription.
type Unsubscribe = Box<dyn FnOnce() + Send>;

/// Shared print-mode binding state (session handle + live subscription).
struct PrintBinding {
    mode: PrintOutputMode,
    out: Arc<WireOut>,
    session: Mutex<AgentSession>,
    unsubscribe: Mutex<Option<Unsubscribe>>,
}

impl PrintBinding {
    /// Oracle `rebindSession`: swap the session handle and re-subscribe.
    /// In json mode every session event is one JSON line on stdout.
    fn rebind(self: &Arc<Self>, session: AgentSession) {
        *self.session.lock() = session.clone();
        if let Some(unsubscribe) = self.unsubscribe.lock().take() {
            unsubscribe();
        }
        let mode = self.mode;
        let out = self.out.clone();
        let unsubscribe = session.subscribe(Arc::new(move |event: &AgentSessionEvent| {
            if mode == PrintOutputMode::Json {
                out.write(&super::rpc::jsonl::serialize_json_line(event));
            }
        }));
        *self.unsubscribe.lock() = Some(Box::new(unsubscribe));
    }

    fn session(&self) -> AgentSession {
        self.session.lock().clone()
    }
}

/// Run in print (single-shot) mode over process stdout with signal handling.
pub async fn run_print_mode(runtime: Arc<AgentSessionRuntime>, options: PrintModeOptions) -> i32 {
    let out = Arc::new(WireOut::new_with_writer(Box::new(std::io::stdout())));
    run_print_mode_with_out(runtime, options, out, true).await
}

/// Print-mode core with injectable stdout sink (tests) and optional signal
/// registration.
pub async fn run_print_mode_with_out(
    runtime: Arc<AgentSessionRuntime>,
    options: PrintModeOptions,
    out: Arc<WireOut>,
    register_signals: bool,
) -> i32 {
    let PrintModeOptions {
        mode,
        messages,
        initial_message,
        initial_images,
    } = options;

    let binding = Arc::new(PrintBinding {
        mode,
        out: out.clone(),
        session: Mutex::new(runtime.session()),
        unsubscribe: Mutex::new(None),
    });

    let disposed = Arc::new(AtomicBool::new(false));
    let dispose_runtime = {
        let runtime = runtime.clone();
        let binding = binding.clone();
        let disposed = disposed.clone();
        move || {
            if disposed.swap(true, Ordering::SeqCst) {
                return;
            }
            if let Some(unsubscribe) = binding.unsubscribe.lock().take() {
                unsubscribe();
            }
            runtime.dispose();
        }
    };

    // Oracle signal handlers: SIGTERM -> 143, SIGHUP -> 129 (non-win32);
    // dispose the runtime, then exit without flushing.
    let signal_task = if register_signals {
        let dispose_runtime = dispose_runtime.clone();
        Some(tokio::spawn(async move {
            let mut sigterm =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .expect("SIGTERM handler");
            let mut sighup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())
                .expect("SIGHUP handler");
            let code = tokio::select! {
                _ = sigterm.recv() => 143,
                _ = sighup.recv() => 129,
            };
            dispose_runtime();
            std::process::exit(code);
        }))
    } else {
        None
    };

    // Rebind on session replacement (extension commands, Phase 6).
    runtime.set_rebind_session(Some(Arc::new({
        let binding = binding.clone();
        move |session: AgentSession| binding.rebind(session)
    })));

    let mut exit_code = 0;

    // try-block analog: first prompt error reports to stderr and exits 1.
    let run = async {
        if mode == PrintOutputMode::Json {
            let header = binding
                .session()
                .with_session_manager(|sm| sm.get_header().cloned());
            if let Some(header) = header {
                out.write(&super::rpc::jsonl::serialize_json_line(&header));
            }
        }

        binding.rebind(runtime.session());

        if let Some(initial_message) = &initial_message {
            binding
                .session()
                .prompt(
                    initial_message,
                    PromptOptions {
                        images: initial_images.clone(),
                        ..Default::default()
                    },
                )
                .await?;
        }

        for message in &messages {
            binding
                .session()
                .prompt(message, PromptOptions::default())
                .await?;
        }

        Ok::<(), String>(())
    };

    match run.await {
        Ok(()) => {
            if mode == PrintOutputMode::Text {
                exit_code = write_final_text(&binding.session(), &out);
            }
        }
        Err(error) => {
            eprintln!("{error}");
            exit_code = 1;
        }
    }

    // finally: remove signal handlers, dispose, flush.
    if let Some(task) = signal_task {
        task.abort();
    }
    dispose_runtime();
    out.flush();
    exit_code
}

/// Text-mode epilogue (print-mode.ts:126-146): inspect the last assistant
/// message; errors/aborts report to stderr with exit 1, otherwise every text
/// block prints to stdout.
fn write_final_text(session: &AgentSession, out: &WireOut) -> i32 {
    let messages = session.messages();
    let Some(AgentMessage::Standard(Message::Assistant(assistant))) = messages.last() else {
        return 0;
    };

    match assistant.stop_reason {
        StopReason::Error | StopReason::Aborted => {
            let fallback = match assistant.stop_reason {
                StopReason::Aborted => "Request aborted",
                _ => "Request error",
            };
            let message = assistant
                .error_message
                .clone()
                .filter(|m| !m.is_empty())
                .unwrap_or_else(|| fallback.to_string());
            eprintln!("{message}");
            1
        }
        _ => {
            for content in &assistant.content {
                if let Content::Text(text) = content {
                    out.write(&format!("{}\n", text.text));
                }
            }
            0
        }
    }
}
