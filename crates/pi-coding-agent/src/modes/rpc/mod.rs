//! RPC mode: headless operation with a JSON stdin/stdout protocol.
//!
//! Port of `modes/rpc/rpc-mode.ts`. Commands are JSON objects with a `type`
//! field and optional `id` for correlation; responses are
//! `{type:"response",command,success,...}`; `AgentSessionEvent`s stream as
//! they occur. Extension UI requests are emitted as `extension_ui_request`
//! lines and resolved by `extension_ui_response` lines.
//!
//! Ordering contract: the `prompt` response is written from prompt preflight
//! BEFORE any session event of that run. Synchronous commands are handled
//! inline in the reader loop (deterministic response order); `prompt`,
//! `bash`, and `compact` detach so control commands (`abort`, `abort_bash`)
//! stay dispatchable while they run.

pub mod jsonl;
pub mod types;

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use parking_lot::Mutex;
use pi_agent::CancellationToken;
use serde_json::Value;
use tokio::io::AsyncReadExt;
use tokio::sync::{Notify, oneshot};
use uuid::Uuid;

use crate::extension_bridge::{
    BoxFuture, ExtensionBridge, ExtensionUiHost, NotifyType, UiDialogOptions, WidgetPlacement,
};
use crate::session::runtime::AgentSessionRuntime;
use crate::session::{
    AgentSession, AgentSessionEvent, PromptOptions, parse_thinking_level, thinking_level_str,
};
use crate::wire_out::WireOut;

use jsonl::{JsonlDecoder, serialize_json_line};
use types::{
    BashPayload, CompactPayload, ForkPayload, GetEntriesPayload, MessagePayload,
    NewSessionPayload, PromptPayload, RpcExtensionUiRequest, RpcExtensionUiResponse, RpcResponse,
    RpcSessionState, RpcSlashCommand, SetEnabledPayload, SetModelPayload, SetQueueModePayload,
    SetSessionNamePayload, SetThinkingLevelPayload, SwitchSessionPayload,
};

/// Stored unsubscribe closure for the active session subscription.
type Unsubscribe = Box<dyn FnOnce() + Send>;

/// Handler for the `export_html` command: `(session, output_path)` →
/// exported file path. The html-export unit provides the real exporter at
/// wiring time; RPC dispatch only owns the envelope.
pub type ExportHtmlFn = Arc<
    dyn Fn(AgentSession, Option<String>) -> BoxFuture<'static, Result<String, String>>
        + Send
        + Sync,
>;

/// Mode wiring options (host-provided handlers).
#[derive(Default)]
pub struct RpcModeOptions {
    /// `export_html` implementation; commands fail with an error envelope
    /// until one is wired.
    pub export_html: Option<ExportHtmlFn>,
}

/// State shared between the reader loop, detached command tasks, the session
/// event listener, and the extension UI host.
struct RpcShared {
    out: Arc<WireOut>,
    bridge: Arc<dyn ExtensionBridge>,
    session: Mutex<AgentSession>,
    unsubscribe: Mutex<Option<Unsubscribe>>,
    /// Pending extension UI requests waiting for `extension_ui_response`.
    pending: Mutex<HashMap<String, oneshot::Sender<RpcExtensionUiResponse>>>,
    /// Set by the extension shutdown handler (Phase 6 ladder); honored after
    /// the current command / on `agent_settled`.
    shutdown_requested: AtomicBool,
    shutdown_notify: Notify,
    /// Host-wired `export_html` implementation.
    export_html: Option<ExportHtmlFn>,
}

impl RpcShared {
    fn output<T: serde::Serialize>(&self, value: &T) {
        self.out.write(&serialize_json_line(value));
    }

    fn session(&self) -> AgentSession {
        self.session.lock().clone()
    }

    /// Oracle `rebindSession`: swap the session, rebind the UI host, and
    /// re-subscribe the event stream.
    fn rebind(self: &Arc<Self>, session: AgentSession) {
        *self.session.lock() = session.clone();
        self.bridge.bind_ui(Arc::new(RpcUiHost {
            shared: self.clone(),
        }));
        if let Some(unsubscribe) = self.unsubscribe.lock().take() {
            unsubscribe();
        }
        let shared = self.clone();
        let unsubscribe = session.subscribe(Arc::new(move |event: &AgentSessionEvent| {
            shared.output(event);
            if matches!(event, AgentSessionEvent::AgentSettled)
                && shared.shutdown_requested.load(Ordering::SeqCst)
            {
                shared.shutdown_notify.notify_one();
            }
        }));
        *self.unsubscribe.lock() = Some(Box::new(unsubscribe));
    }
}

// ============================================================================
// Extension UI host (rpc-mode.ts:108-299)
// ============================================================================

/// RPC implementation of [`ExtensionUiHost`]: blocking dialogs become
/// `extension_ui_request` lines resolved via the pending map; the rest are
/// fire-and-forget lines.
struct RpcUiHost {
    shared: Arc<RpcShared>,
}

/// Sleep-poll a [`CancellationToken`] (codebase pattern, see
/// `cancellable_sleep`); resolves when cancelled.
async fn wait_cancelled(signal: CancellationToken) {
    loop {
        if signal.is_cancelled() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
}

impl RpcUiHost {
    /// Oracle `createDialogPromise`: emit the request, then resolve with the
    /// response, or with `default` on timeout/abort.
    fn dialog<T: Send + 'static>(
        &self,
        opts: UiDialogOptions,
        default: T,
        payload: types::UiRequestPayload,
        parse: impl FnOnce(RpcExtensionUiResponse) -> T + Send + 'static,
    ) -> BoxFuture<'static, T> {
        if opts.signal.as_ref().is_some_and(CancellationToken::is_cancelled) {
            return Box::pin(std::future::ready(default));
        }

        let id = Uuid::new_v4().to_string();
        let (tx, rx) = oneshot::channel();
        self.shared.pending.lock().insert(id.clone(), tx);
        self.shared
            .output(&RpcExtensionUiRequest::new(id.clone(), payload));

        let shared = self.shared.clone();
        Box::pin(async move {
            let timeout = async {
                match opts.timeout_ms {
                    Some(ms) => tokio::time::sleep(std::time::Duration::from_millis(ms)).await,
                    None => std::future::pending().await,
                }
            };
            let aborted = async {
                match opts.signal {
                    Some(signal) => wait_cancelled(signal).await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                response = rx => match response {
                    Ok(response) => parse(response),
                    Err(_) => default,
                },
                _ = timeout => {
                    shared.pending.lock().remove(&id);
                    default
                }
                _ = aborted => {
                    shared.pending.lock().remove(&id);
                    default
                }
            }
        })
    }

    fn fire_and_forget(&self, payload: types::UiRequestPayload) {
        self.shared.output(&RpcExtensionUiRequest::new(
            Uuid::new_v4().to_string(),
            payload,
        ));
    }
}

impl ExtensionUiHost for RpcUiHost {
    fn select(
        &self,
        title: String,
        options: Vec<String>,
        opts: UiDialogOptions,
    ) -> BoxFuture<'static, Option<String>> {
        let timeout = opts.timeout_ms;
        self.dialog(
            opts,
            None,
            types::UiRequestPayload::Select {
                title,
                options,
                timeout,
            },
            |r| if r.is_cancelled() { None } else { r.value },
        )
    }

    fn confirm(
        &self,
        title: String,
        message: String,
        opts: UiDialogOptions,
    ) -> BoxFuture<'static, bool> {
        let timeout = opts.timeout_ms;
        self.dialog(
            opts,
            false,
            types::UiRequestPayload::Confirm {
                title,
                message,
                timeout,
            },
            |r| {
                if r.is_cancelled() {
                    false
                } else {
                    r.confirmed.unwrap_or(false)
                }
            },
        )
    }

    fn input(
        &self,
        title: String,
        placeholder: Option<String>,
        opts: UiDialogOptions,
    ) -> BoxFuture<'static, Option<String>> {
        let timeout = opts.timeout_ms;
        self.dialog(
            opts,
            None,
            types::UiRequestPayload::Input {
                title,
                placeholder,
                timeout,
            },
            |r| if r.is_cancelled() { None } else { r.value },
        )
    }

    fn editor(
        &self,
        title: String,
        prefill: Option<String>,
    ) -> BoxFuture<'static, Option<String>> {
        // Oracle `editor`: no timeout/abort support (rpc-mode.ts:236-253).
        self.dialog(
            UiDialogOptions::default(),
            None,
            types::UiRequestPayload::Editor { title, prefill },
            |r| if r.is_cancelled() { None } else { r.value },
        )
    }

    fn notify(&self, message: String, notify_type: Option<NotifyType>) {
        self.fire_and_forget(types::UiRequestPayload::Notify {
            message,
            notify_type,
        });
    }

    fn set_status(&self, key: String, text: Option<String>) {
        self.fire_and_forget(types::UiRequestPayload::SetStatus {
            status_key: key,
            status_text: text,
        });
    }

    fn set_widget(
        &self,
        key: String,
        lines: Option<Vec<String>>,
        placement: Option<WidgetPlacement>,
    ) {
        self.fire_and_forget(types::UiRequestPayload::SetWidget {
            widget_key: key,
            widget_lines: lines,
            widget_placement: placement,
        });
    }

    fn set_title(&self, title: String) {
        self.fire_and_forget(types::UiRequestPayload::SetTitle { title });
    }

    fn set_editor_text(&self, text: String) {
        self.fire_and_forget(types::UiRequestPayload::SetEditorText { text });
    }
}

// ============================================================================
// Command dispatch (rpc-mode.ts:362-672)
// ============================================================================

fn success(id: Option<Value>, command: &str, data: Option<Value>) -> RpcResponse {
    RpcResponse::success(id, command, data)
}

fn error(id: Option<Value>, command: Option<Value>, message: impl Into<String>) -> RpcResponse {
    RpcResponse::error(id, command, message)
}

fn to_value<T: serde::Serialize>(value: &T) -> Value {
    serde_json::to_value(value).expect("wire value must serialize")
}

/// Decode a typed payload from the raw command object; failures map to the
/// oracle's thrown-error envelope for that command (boxed: the happy path
/// should not carry the envelope's size).
fn payload<T: serde::de::DeserializeOwned>(
    raw: &Value,
    id: &Option<Value>,
    command: &str,
) -> Result<T, Box<RpcResponse>> {
    serde_json::from_value(raw.clone()).map_err(|e| {
        Box::new(error(
            id.clone(),
            Some(Value::String(command.to_string())),
            e.to_string(),
        ))
    })
}

/// Handle one command. `None` means the command detached and writes its own
/// response (`prompt`, `bash`, `compact`).
#[allow(clippy::too_many_lines)]
async fn handle_command(
    shared: &Arc<RpcShared>,
    runtime: &Arc<AgentSessionRuntime>,
    tasks: &mut tokio::task::JoinSet<()>,
    id: Option<Value>,
    command_type: &str,
    raw: Value,
) -> Option<RpcResponse> {
    let session = shared.session();

    match command_type {
        // =================================================================
        // Prompting
        // =================================================================
        "prompt" => {
            let p: PromptPayload = match payload(&raw, &id, "prompt") {
                            Ok(p) => p,
                            Err(response) => return Some(*response),
                        };
            // Emit the authoritative response only after prompt preflight
            // succeeds; queued and immediately handled prompts also count as
            // success (rpc-mode.ts:373-395).
            let shared = shared.clone();
            tasks.spawn(async move {
                let preflight_succeeded = Arc::new(AtomicBool::new(false));
                let preflight = {
                    let shared = shared.clone();
                    let preflight_succeeded = preflight_succeeded.clone();
                    let id = id.clone();
                    Box::new(move |did_succeed: bool| {
                        if did_succeed {
                            preflight_succeeded.store(true, Ordering::SeqCst);
                            shared.output(&success(id, "prompt", None));
                        }
                    })
                };
                let result = shared
                    .session()
                    .prompt(
                        &p.message,
                        PromptOptions {
                            images: p.images.unwrap_or_default(),
                            streaming_behavior: p.streaming_behavior,
                            preflight_result: Some(preflight),
                            ..Default::default()
                        },
                    )
                    .await;
                if let Err(e) = result
                    && !preflight_succeeded.load(Ordering::SeqCst)
                {
                    shared.output(&error(id, Some(Value::String("prompt".into())), e));
                }
            });
            None
        }

        "steer" => {
            let p: MessagePayload = match payload(&raw, &id, "steer") {
                            Ok(p) => p,
                            Err(response) => return Some(*response),
                        };
            session.steer(&p.message, p.images.unwrap_or_default());
            Some(success(id, "steer", None))
        }

        "follow_up" => {
            let p: MessagePayload = match payload(&raw, &id, "follow_up") {
                            Ok(p) => p,
                            Err(response) => return Some(*response),
                        };
            session.follow_up(&p.message, p.images.unwrap_or_default());
            Some(success(id, "follow_up", None))
        }

        "abort" => {
            session.abort().await;
            Some(success(id, "abort", None))
        }

        "new_session" => {
            let p: NewSessionPayload = match payload(&raw, &id, "new_session") {
                            Ok(p) => p,
                            Err(response) => return Some(*response),
                        };
            match runtime.new_session(p.parent_session).await {
                Ok(result) => Some(success(
                    id,
                    "new_session",
                    Some(serde_json::json!({ "cancelled": result.cancelled })),
                )),
                Err(e) => Some(error(id, Some(Value::String("new_session".into())), e)),
            }
        }

        // =================================================================
        // State
        // =================================================================
        "get_state" => {
            let state = RpcSessionState {
                model: session.model(),
                thinking_level: session.thinking_level(),
                is_streaming: session.is_streaming(),
                is_compacting: session.is_compacting(),
                steering_mode: session.steering_mode(),
                follow_up_mode: session.follow_up_mode(),
                session_file: session
                    .session_file()
                    .map(|p| p.to_string_lossy().into_owned()),
                session_id: session.session_id(),
                session_name: session.session_name(),
                auto_compaction_enabled: session.auto_compaction_enabled(),
                message_count: session.messages().len(),
                pending_message_count: session.pending_message_count(),
            };
            Some(success(id, "get_state", Some(to_value(&state))))
        }

        // =================================================================
        // Model
        // =================================================================
        "set_model" => {
            let p: SetModelPayload = match payload(&raw, &id, "set_model") {
                            Ok(p) => p,
                            Err(response) => return Some(*response),
                        };
            let registry = runtime.services().model_registry;
            let model = {
                let registry = registry.read().await;
                registry
                    .get_available()
                    .await
                    .into_iter()
                    .find(|m| m.provider == p.provider && m.id == p.model_id)
            };
            let Some(model) = model else {
                return Some(error(
                    id,
                    Some(Value::String("set_model".into())),
                    format!("Model not found: {}/{}", p.provider, p.model_id),
                ));
            };
            match session.set_model(model.clone()).await {
                Ok(()) => Some(success(id, "set_model", Some(to_value(&model)))),
                Err(e) => Some(error(id, Some(Value::String("set_model".into())), e)),
            }
        }

        "cycle_model" => match session.cycle_model(true).await {
            None => Some(success(id, "cycle_model", Some(Value::Null))),
            Some(result) => Some(success(
                id,
                "cycle_model",
                Some(serde_json::json!({
                    "model": to_value(&result.model),
                    "thinkingLevel": thinking_level_str(result.thinking_level),
                    "isScoped": result.is_scoped,
                })),
            )),
        },

        "get_available_models" => {
            let registry = runtime.services().model_registry;
            let models = registry.read().await.get_available().await;
            Some(success(
                id,
                "get_available_models",
                Some(serde_json::json!({ "models": to_value(&models) })),
            ))
        }

        // =================================================================
        // Thinking
        // =================================================================
        "set_thinking_level" => {
            let p: SetThinkingLevelPayload = match payload(&raw, &id, "set_thinking_level") {
                            Ok(p) => p,
                            Err(response) => return Some(*response),
                        };
            session.set_thinking_level(parse_thinking_level(&p.level));
            Some(success(id, "set_thinking_level", None))
        }

        "cycle_thinking_level" => match session.cycle_thinking_level() {
            None => Some(success(id, "cycle_thinking_level", Some(Value::Null))),
            Some(level) => Some(success(
                id,
                "cycle_thinking_level",
                Some(serde_json::json!({ "level": thinking_level_str(level) })),
            )),
        },

        // =================================================================
        // Queue modes
        // =================================================================
        "set_steering_mode" => {
            let p: SetQueueModePayload = match payload(&raw, &id, "set_steering_mode") {
                            Ok(p) => p,
                            Err(response) => return Some(*response),
                        };
            session.set_steering_mode(&p.mode);
            Some(success(id, "set_steering_mode", None))
        }

        "set_follow_up_mode" => {
            let p: SetQueueModePayload = match payload(&raw, &id, "set_follow_up_mode") {
                            Ok(p) => p,
                            Err(response) => return Some(*response),
                        };
            session.set_follow_up_mode(&p.mode);
            Some(success(id, "set_follow_up_mode", None))
        }

        // =================================================================
        // Compaction
        // =================================================================
        "compact" => {
            let p: CompactPayload = match payload(&raw, &id, "compact") {
                            Ok(p) => p,
                            Err(response) => return Some(*response),
                        };
            let shared = shared.clone();
            tasks.spawn(async move {
                let response = match shared.session().compact(p.custom_instructions).await {
                    Ok(result) => success(id, "compact", Some(to_value(&result))),
                    Err(e) => error(id, Some(Value::String("compact".into())), e),
                };
                shared.output(&response);
            });
            None
        }

        "set_auto_compaction" => {
            let p: SetEnabledPayload = match payload(&raw, &id, "set_auto_compaction") {
                            Ok(p) => p,
                            Err(response) => return Some(*response),
                        };
            session.set_auto_compaction_enabled(p.enabled);
            Some(success(id, "set_auto_compaction", None))
        }

        // =================================================================
        // Retry
        // =================================================================
        "set_auto_retry" => {
            let p: SetEnabledPayload = match payload(&raw, &id, "set_auto_retry") {
                            Ok(p) => p,
                            Err(response) => return Some(*response),
                        };
            session.set_auto_retry_enabled(p.enabled);
            Some(success(id, "set_auto_retry", None))
        }

        "abort_retry" => {
            session.abort_retry();
            Some(success(id, "abort_retry", None))
        }

        // =================================================================
        // Bash
        // =================================================================
        "bash" => {
            let p: BashPayload = match payload(&raw, &id, "bash") {
                            Ok(p) => p,
                            Err(response) => return Some(*response),
                        };
            let shared = shared.clone();
            tasks.spawn(async move {
                let response = match shared
                    .session()
                    .execute_bash(&p.command, None, p.exclude_from_context)
                    .await
                {
                    Ok(result) => success(id, "bash", Some(to_value(&result))),
                    Err(e) => error(id, Some(Value::String("bash".into())), e),
                };
                shared.output(&response);
            });
            None
        }

        "abort_bash" => {
            session.abort_bash();
            Some(success(id, "abort_bash", None))
        }

        // =================================================================
        // Session
        // =================================================================
        "get_session_stats" => {
            let stats = session.get_session_stats();
            Some(success(id, "get_session_stats", Some(to_value(&stats))))
        }

        "export_html" => {
            #[derive(serde::Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct ExportHtmlPayload {
                #[serde(default)]
                output_path: Option<String>,
            }
            let p: ExportHtmlPayload = match payload(&raw, &id, "export_html") {
                            Ok(p) => p,
                            Err(response) => return Some(*response),
                        };
            let Some(export_html) = shared.export_html.clone() else {
                return Some(error(
                    id,
                    Some(Value::String("export_html".into())),
                    "export_html is not implemented",
                ));
            };
            match export_html(session, p.output_path).await {
                Ok(path) => Some(success(
                    id,
                    "export_html",
                    Some(serde_json::json!({ "path": path })),
                )),
                Err(e) => Some(error(id, Some(Value::String("export_html".into())), e)),
            }
        }

        "switch_session" => {
            let p: SwitchSessionPayload = match payload(&raw, &id, "switch_session") {
                            Ok(p) => p,
                            Err(response) => return Some(*response),
                        };
            match runtime.switch_session(Path::new(&p.session_path), None).await {
                Ok(result) => Some(success(
                    id,
                    "switch_session",
                    Some(serde_json::json!({ "cancelled": result.cancelled })),
                )),
                Err(e) => Some(error(id, Some(Value::String("switch_session".into())), e)),
            }
        }

        "fork" => {
            let p: ForkPayload = match payload(&raw, &id, "fork") {
                            Ok(p) => p,
                            Err(response) => return Some(*response),
                        };
            match runtime
                .fork(&p.entry_id, crate::extension_bridge::ForkPosition::Before)
                .await
            {
                Ok(result) => {
                    let mut data = serde_json::Map::new();
                    if let Some(text) = result.selected_text {
                        data.insert("text".into(), Value::String(text));
                    }
                    data.insert("cancelled".into(), Value::Bool(result.cancelled));
                    Some(success(id, "fork", Some(Value::Object(data))))
                }
                Err(e) => Some(error(id, Some(Value::String("fork".into())), e)),
            }
        }

        "clone" => {
            let leaf_id =
                session.with_session_manager(|sm| sm.get_leaf_id().map(str::to_string));
            let Some(leaf_id) = leaf_id else {
                return Some(error(
                    id,
                    Some(Value::String("clone".into())),
                    "Cannot clone session: no current entry selected",
                ));
            };
            match runtime
                .fork(&leaf_id, crate::extension_bridge::ForkPosition::At)
                .await
            {
                Ok(result) => Some(success(
                    id,
                    "clone",
                    Some(serde_json::json!({ "cancelled": result.cancelled })),
                )),
                Err(e) => Some(error(id, Some(Value::String("clone".into())), e)),
            }
        }

        "get_fork_messages" => {
            let messages: Vec<Value> = session
                .get_user_messages_for_forking()
                .into_iter()
                .map(|(entry_id, text)| {
                    serde_json::json!({ "entryId": entry_id, "text": text })
                })
                .collect();
            Some(success(
                id,
                "get_fork_messages",
                Some(serde_json::json!({ "messages": messages })),
            ))
        }

        "get_entries" => {
            let p: GetEntriesPayload = match payload(&raw, &id, "get_entries") {
                            Ok(p) => p,
                            Err(response) => return Some(*response),
                        };
            let (mut entries, leaf_id) = session.with_session_manager(|sm| {
                (sm.get_entries(), sm.get_leaf_id().map(str::to_string))
            });
            if let Some(since) = &p.since {
                let Some(since_index) = entries
                    .iter()
                    .position(|e| e.id() == Some(since.as_str()))
                else {
                    return Some(error(
                        id,
                        Some(Value::String("get_entries".into())),
                        format!("Entry not found: {since}"),
                    ));
                };
                entries.drain(..=since_index);
            }
            Some(success(
                id,
                "get_entries",
                Some(serde_json::json!({ "entries": to_value(&entries), "leafId": leaf_id })),
            ))
        }

        "get_tree" => {
            let (tree, leaf_id) = session
                .with_session_manager(|sm| (sm.get_tree(), sm.get_leaf_id().map(str::to_string)));
            Some(success(
                id,
                "get_tree",
                Some(serde_json::json!({ "tree": to_value(&tree), "leafId": leaf_id })),
            ))
        }

        "get_last_assistant_text" => {
            let text = session.get_last_assistant_text();
            Some(success(
                id,
                "get_last_assistant_text",
                Some(serde_json::json!({ "text": text })),
            ))
        }

        "set_session_name" => {
            let p: SetSessionNamePayload = match payload(&raw, &id, "set_session_name") {
                            Ok(p) => p,
                            Err(response) => return Some(*response),
                        };
            let name = p.name.trim();
            if name.is_empty() {
                return Some(error(
                    id,
                    Some(Value::String("set_session_name".into())),
                    "Session name cannot be empty",
                ));
            }
            session.set_session_name(name);
            Some(success(id, "set_session_name", None))
        }

        // =================================================================
        // Messages
        // =================================================================
        "get_messages" => Some(success(
            id,
            "get_messages",
            Some(serde_json::json!({ "messages": to_value(&session.messages()) })),
        )),

        // =================================================================
        // Commands (available for invocation via prompt)
        // =================================================================
        "get_commands" => {
            let mut commands: Vec<RpcSlashCommand> = Vec::new();

            for command in shared.bridge.registered_commands() {
                commands.push(RpcSlashCommand {
                    name: command.invocation_name,
                    description: command.description,
                    source: "extension",
                    source_info: command.source_info,
                });
            }

            for template in session.prompt_templates() {
                commands.push(RpcSlashCommand {
                    name: template.name,
                    description: (!template.description.is_empty())
                        .then_some(template.description),
                    source: "prompt",
                    source_info: template.source_info,
                });
            }

            for skill in session.skills() {
                commands.push(RpcSlashCommand {
                    name: format!("skill:{}", skill.name),
                    description: Some(skill.description),
                    source: "skill",
                    source_info: skill.source_info,
                });
            }

            Some(success(
                id,
                "get_commands",
                Some(serde_json::json!({ "commands": to_value(&commands) })),
            ))
        }

        unknown => Some(error(
            id,
            Some(Value::String(unknown.to_string())),
            format!("Unknown command: {unknown}"),
        )),
    }
}

// ============================================================================
// Mode loop
// ============================================================================

/// Run in RPC mode over process stdin/stdout with signal handling.
/// Returns the exit code (EOF and extension-requested shutdown exit 0).
pub async fn run_rpc_mode(runtime: Arc<AgentSessionRuntime>, options: RpcModeOptions) -> i32 {
    let out = Arc::new(WireOut::new_with_writer(Box::new(std::io::stdout())));
    run_rpc_mode_with_io(runtime, tokio::io::stdin(), out, options, true).await
}

/// RPC-mode core with injectable stdin/stdout (tests) and optional signal
/// registration.
pub async fn run_rpc_mode_with_io<R>(
    runtime: Arc<AgentSessionRuntime>,
    mut input: R,
    out: Arc<WireOut>,
    options: RpcModeOptions,
    register_signals: bool,
) -> i32
where
    R: tokio::io::AsyncRead + Unpin,
{
    let shared = Arc::new(RpcShared {
        out,
        bridge: runtime.bridge(),
        session: Mutex::new(runtime.session()),
        unsubscribe: Mutex::new(None),
        pending: Mutex::new(HashMap::new()),
        shutdown_requested: AtomicBool::new(false),
        shutdown_notify: Notify::new(),
        export_html: options.export_html,
    });

    runtime.set_rebind_session(Some(Arc::new({
        let shared = shared.clone();
        move |session: AgentSession| shared.rebind(session)
    })));
    shared.rebind(runtime.session());

    let mut sigterm = if register_signals {
        Some(
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("SIGTERM handler"),
        )
    } else {
        None
    };
    let mut sighup = if register_signals {
        Some(
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())
                .expect("SIGHUP handler"),
        )
    } else {
        None
    };

    let mut decoder = JsonlDecoder::new();
    let mut buf = [0u8; 8192];
    let mut tasks = tokio::task::JoinSet::new();

    // Shutdown ladder (oracle `shutdown`): unsubscribe, dispose the runtime,
    // detach input, flush (unless SIGTERM), exit.
    let shutdown = |shared: &Arc<RpcShared>,
                    runtime: &Arc<AgentSessionRuntime>,
                    tasks: &mut tokio::task::JoinSet<()>,
                    flush: bool| {
        if let Some(unsubscribe) = shared.unsubscribe.lock().take() {
            unsubscribe();
        }
        runtime.set_rebind_session(None);
        runtime.dispose();
        tasks.abort_all();
        if flush {
            shared.out.flush();
        }
    };

    loop {
        tokio::select! {
            read = input.read(&mut buf) => match read {
                Ok(0) | Err(_) => {
                    // stdin EOF: flush the final unterminated line, then
                    // shutdown(0).
                    if let Some(line) = decoder.finish()
                        && handle_input_line(&shared, &runtime, &mut tasks, &line).await
                            == LineOutcome::Shutdown
                    {
                        break;
                    }
                    break;
                }
                Ok(n) => {
                    let mut stop = false;
                    for line in decoder.feed(&buf[..n]) {
                        if handle_input_line(&shared, &runtime, &mut tasks, &line).await
                            == LineOutcome::Shutdown
                        {
                            stop = true;
                            break;
                        }
                    }
                    if stop {
                        break;
                    }
                }
            },
            _ = shared.shutdown_notify.notified() => break,
            _ = async { sigterm.as_mut().expect("sigterm stream").recv().await }, if sigterm.is_some() => {
                shutdown(&shared, &runtime, &mut tasks, false);
                std::process::exit(143);
            }
            _ = async { sighup.as_mut().expect("sighup stream").recv().await }, if sighup.is_some() => {
                shutdown(&shared, &runtime, &mut tasks, true);
                std::process::exit(129);
            }
        }
    }

    shutdown(&shared, &runtime, &mut tasks, true);
    0
}

#[derive(PartialEq, Eq)]
enum LineOutcome {
    Continue,
    Shutdown,
}

/// Oracle `handleInputLine`: parse, route extension UI responses, dispatch
/// commands, and honor a requested shutdown once idle.
async fn handle_input_line(
    shared: &Arc<RpcShared>,
    runtime: &Arc<AgentSessionRuntime>,
    tasks: &mut tokio::task::JoinSet<()>,
    line: &str,
) -> LineOutcome {
    let parsed: Value = match serde_json::from_str(line) {
        Ok(value) => value,
        Err(e) => {
            shared.output(&RpcResponse::error(
                None,
                Some(Value::String("parse".into())),
                format!("Failed to parse command: {e}"),
            ));
            return LineOutcome::Continue;
        }
    };

    // Extension UI responses resolve the pending map and produce no response.
    if parsed.get("type").and_then(Value::as_str) == Some("extension_ui_response") {
        if let Ok(response) = serde_json::from_value::<RpcExtensionUiResponse>(parsed) {
            let pending = shared.pending.lock().remove(&response.id);
            if let Some(sender) = pending {
                let _ = sender.send(response);
            }
        }
        return LineOutcome::Continue;
    }

    let id = parsed.get("id").cloned();
    let command_type = parsed
        .get("type")
        .and_then(Value::as_str)
        .map(str::to_string);

    let response = match &command_type {
        Some(command_type) => {
            handle_command(shared, runtime, tasks, id, command_type, parsed).await
        }
        None => {
            // Oracle default case with `command.type` undefined: the command
            // key is omitted and the error text stringifies the value.
            let echoed = parsed.get("type").cloned();
            let shown = match &echoed {
                None => "undefined".to_string(),
                Some(value) => value.to_string(),
            };
            Some(RpcResponse::error(
                id,
                echoed,
                format!("Unknown command: {shown}"),
            ))
        }
    };

    if let Some(response) = response {
        shared.output(&response);
    }

    if shared.shutdown_requested.load(Ordering::SeqCst) && shared.session().is_idle() {
        return LineOutcome::Shutdown;
    }
    LineOutcome::Continue
}
