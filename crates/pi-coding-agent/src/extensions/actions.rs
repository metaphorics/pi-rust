//! Server side of `action/*` (Phase 6 commit C6, plan §3/§8-F3).
//!
//! The sidecar proxies pi's `ExtensionActions` surfaces onto the wire:
//! SYNC-VOID members arrive as notifications (the sidecar already applied an
//! optimistic mirror update), ASYNC members as requests the host must
//! answer. Everything lands on the [`HostActions`] trait — THE seam Phase 5
//! modes implement; [`super::binding::SessionHostActions`] binds the
//! session-scoped subset to a live [`crate::session::AgentSession`].

use std::collections::HashMap;
use std::sync::Arc;

use pi_agent::CancellationToken;
use pi_ai::Model;
use pi_ext_protocol::{
    AppendEntryParams, CancelledResult, CompactParams, ForkParams, NavigateTreeParams,
    NewSessionParams, Notification, ProtocolError, RefreshToolsParams, Request, RequestId,
    ResponseResult, SendMessageParams, SendUserMessageParams, SetLabelParams,
    SetThinkingLevelParams, SwitchSessionParams,
};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::extension_bridge::{BoxFuture, ExtensionUiHost, NotifyType, UiDialogOptions};

use super::Incoming;
use super::events::EventForwarder;

fn ready<T: Send + 'static>(value: T) -> BoxFuture<'static, T> {
    Box::pin(std::future::ready(value))
}

/// Host surface behind `action/*`. Defaults are the no-op / unsupported
/// behavior a UI-less host exhibits; modes override what they can serve.
///
/// Notifications (SYNC-VOID in pi) return futures the serve loop awaits *in
/// arrival order* before touching the next inbound frame, so read-after-
/// write across actions holds. Requests run concurrently (a `waitForIdle`
/// must not wedge the action stream) and are cancellable via cancel frames.
pub trait HostActions: Send + Sync {
    fn send_message(&self, _params: SendMessageParams) -> BoxFuture<'static, ()> {
        ready(())
    }
    fn send_user_message(&self, _params: SendUserMessageParams) -> BoxFuture<'static, ()> {
        ready(())
    }
    fn append_entry(&self, _params: AppendEntryParams) -> BoxFuture<'static, ()> {
        ready(())
    }
    fn set_session_name(&self, _name: String) -> BoxFuture<'static, ()> {
        ready(())
    }
    fn set_label(&self, _params: SetLabelParams) -> BoxFuture<'static, ()> {
        ready(())
    }
    fn set_active_tools(&self, _tool_names: Vec<String>) -> BoxFuture<'static, ()> {
        ready(())
    }
    /// `refreshTools()` — the params carry the sidecar's current
    /// registration snapshot (late `pi.registerTool` calls).
    fn refresh_tools(&self, _params: RefreshToolsParams) -> BoxFuture<'static, ()> {
        ready(())
    }
    fn set_thinking_level(&self, _params: SetThinkingLevelParams) -> BoxFuture<'static, ()> {
        ready(())
    }
    fn shutdown(&self) -> BoxFuture<'static, ()> {
        ready(())
    }
    fn abort(&self) -> BoxFuture<'static, ()> {
        ready(())
    }
    /// `ctx.compact()` — a manual host compaction. The serve loop already
    /// counted the sidecar's pending callback before this runs.
    fn compact(&self, _params: CompactParams) -> BoxFuture<'static, ()> {
        ready(())
    }

    /// `true` when the model was applied (sidecar contract: bare boolean ok).
    /// `signal` is the per-request cancel token (a cancel frame stops the
    /// wait; the response is dropped by the sidecar anyway).
    fn set_model(&self, _model: Model, _signal: CancellationToken) -> BoxFuture<'static, bool> {
        ready(false)
    }
    fn wait_for_idle(&self, _signal: CancellationToken) -> BoxFuture<'static, ()> {
        ready(())
    }
    fn new_session(
        &self,
        _params: NewSessionParams,
        _signal: CancellationToken,
    ) -> BoxFuture<'static, Result<CancelledResult, String>> {
        ready(Err("newSession is not supported by this mode".to_string()))
    }
    fn fork(
        &self,
        _params: ForkParams,
        _signal: CancellationToken,
    ) -> BoxFuture<'static, Result<CancelledResult, String>> {
        ready(Err("fork is not supported by this mode".to_string()))
    }
    fn navigate_tree(
        &self,
        _params: NavigateTreeParams,
    ) -> BoxFuture<'static, Result<CancelledResult, String>> {
        ready(Err("navigateTree is not supported by this mode".to_string()))
    }
    fn switch_session(
        &self,
        _params: SwitchSessionParams,
        _signal: CancellationToken,
    ) -> BoxFuture<'static, Result<CancelledResult, String>> {
        ready(Err(
            "switchSession is not supported by this mode".to_string()
        ))
    }
    /// `ctx.reload()` — in-place extension reload + session recreate.
    fn reload(&self, _signal: CancellationToken) -> BoxFuture<'static, Result<(), String>> {
        ready(Err("reload is not supported by this mode".to_string()))
    }
    fn replaced_send_message(&self, _params: SendMessageParams) -> BoxFuture<'static, ()> {
        ready(())
    }
    fn replaced_send_user_message(&self, _params: SendUserMessageParams) -> BoxFuture<'static, ()> {
        ready(())
    }
    /// `pi.registerProvider` at runtime (load-time registrations arrive in
    /// the `initialized`/`refreshTools` snapshots). Ordered: the serve loop
    /// awaits it before the next inbound frame.
    fn provider_register(
        &self,
        _registration: pi_ext_protocol::ProviderRegistration,
    ) -> BoxFuture<'static, ()> {
        ready(())
    }
    /// `pi.unregisterProvider` at runtime.
    fn provider_unregister(&self, _name: String) -> BoxFuture<'static, ()> {
        ready(())
    }
}

/// Frames C6 has no consumer for yet (`ui/frame`, `tool/update`,
/// `provider/event`, ... — C7/C8 seams). Default: dropped.
pub type NotificationSink = Arc<dyn Fn(Notification) + Send + Sync>;

/// Everything the inbound serve loop dispatches onto.
pub struct ActionServerConfig {
    pub actions: Arc<dyn HostActions>,
    /// Dialog / void-setter UI host. `None` mirrors pi's no-op UI: dialogs
    /// resolve to their cancel fallbacks.
    pub ui: parking_lot::Mutex<Option<Arc<dyn ExtensionUiHost>>>,
    /// C7/C8 seam for frames, tool updates, and provider events.
    pub fallback: Option<NotificationSink>,
}

/// Spawn the inbound consumer. Owns the host-scoped [`Incoming`] receiver
/// (valid across respawns); responses go to whatever connection is current.
pub fn spawn_action_server(
    forwarder: Arc<EventForwarder>,
    mut incoming: mpsc::Receiver<Incoming>,
    config: Arc<ActionServerConfig>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut in_flight: HashMap<u64, CancellationToken> = HashMap::new();
        while let Some(item) = incoming.recv().await {
            match item {
                Incoming::Notification(notification) => {
                    handle_notification(&forwarder, &config, notification).await;
                }
                Incoming::Request { id, request } => {
                    in_flight.retain(|_, token| !token.is_cancelled());
                    let cancel = CancellationToken::new();
                    in_flight.insert(id.get(), cancel.clone());
                    let forwarder = forwarder.clone();
                    let config = config.clone();
                    tokio::spawn(async move {
                        let result = handle_request(&config, request, cancel).await;
                        respond(&forwarder, id, result).await;
                    });
                }
                Incoming::Cancel { id } => {
                    if let Some(token) = in_flight.remove(&id.get()) {
                        token.cancel();
                    }
                }
                Incoming::Barrier(done) => {
                    // Everything enqueued before the fence has been handled
                    // (notifications are awaited serially above).
                    let _ = done.send(());
                }
            }
        }
    })
}

async fn respond(forwarder: &EventForwarder, id: RequestId, result: ResponseResult) {
    let Some(connection) = forwarder.host().current_connection().await else {
        return; // Connection died; nobody is waiting anymore.
    };
    if let Err(error) = connection.respond(id, result).await {
        forwarder.error_sink()(pi_ext_protocol::ExtensionError {
            extension_path: "<bridge>".to_string(),
            event: "action_response".to_string(),
            error: error.to_string(),
            stack: None,
        });
    }
}

fn ok(value: Value) -> ResponseResult {
    ResponseResult::Ok { ok: value }
}

fn err(message: impl Into<String>) -> ResponseResult {
    ResponseResult::Err {
        err: ProtocolError {
            code: "host_error".to_string(),
            message: message.into(),
            stack: None,
            extension_path: None,
        },
    }
}

fn cancelled_value(result: Result<CancelledResult, String>) -> ResponseResult {
    match result {
        Ok(cancelled) => ok(json!({ "cancelled": cancelled.cancelled })),
        Err(message) => err(message),
    }
}

fn dialog_options(
    dialog: &pi_ext_protocol::DialogOptions,
    cancel: CancellationToken,
) -> UiDialogOptions {
    UiDialogOptions {
        timeout_ms: dialog.timeout,
        signal: Some(cancel),
    }
}

async fn handle_request(
    config: &ActionServerConfig,
    request: Request,
    cancel: CancellationToken,
) -> ResponseResult {
    let ui = config.ui.lock().clone();
    match request {
        Request::ActionSetModel(params) => ok(Value::Bool(
            config.actions.set_model(params.model, cancel).await,
        )),
        Request::ActionWaitForIdle(_) => {
            config.actions.wait_for_idle(cancel).await;
            ok(json!({}))
        }
        Request::ActionNewSession(params) => {
            cancelled_value(config.actions.new_session(params, cancel).await)
        }
        Request::ActionFork(params) => cancelled_value(config.actions.fork(params, cancel).await),
        Request::ActionNavigateTree(params) => {
            cancelled_value(config.actions.navigate_tree(params).await)
        }
        Request::ActionSwitchSession(params) => {
            cancelled_value(config.actions.switch_session(params, cancel).await)
        }
        Request::ActionReload(_) => match config.actions.reload(cancel).await {
            Ok(()) => ok(json!({})),
            Err(message) => err(message),
        },
        Request::ActionReplacedSendMessage(params) => {
            config.actions.replaced_send_message(params).await;
            ok(json!({}))
        }
        Request::ActionReplacedSendUserMessage(params) => {
            config.actions.replaced_send_user_message(params).await;
            ok(json!({}))
        }

        // UI dialogs: without a bound host these resolve like pi's no-op UI
        // context (undefined / false), never an error.
        Request::UiSelect(params) => match ui {
            Some(ui) => {
                let opts = dialog_options(&params.dialog, cancel);
                match ui.select(params.title, params.options, opts).await {
                    Some(choice) => ok(Value::String(choice)),
                    None => ok(Value::Null),
                }
            }
            None => ok(Value::Null),
        },
        Request::UiConfirm(params) => match ui {
            Some(ui) => {
                let opts = dialog_options(&params.dialog, cancel);
                ok(Value::Bool(
                    ui.confirm(params.title, params.message, opts).await,
                ))
            }
            None => ok(Value::Bool(false)),
        },
        Request::UiInput(params) => match ui {
            Some(ui) => {
                let opts = dialog_options(&params.dialog, cancel);
                match ui.input(params.title, params.placeholder, opts).await {
                    Some(text) => ok(Value::String(text)),
                    None => ok(Value::Null),
                }
            }
            None => ok(Value::Null),
        },
        Request::UiEditor(params) => match ui {
            Some(ui) => match ui.editor(params.title, Some(params.text)).await {
                Some(text) => ok(Value::String(text)),
                None => ok(Value::Null),
            },
            None => ok(Value::Null),
        },
        // Custom component dialogs mount bridged frames (C8/F2). The
        // response value is unused by the sidecar (the extension promise
        // resolves through ui/done); it only finalizes the slot lifetime.
        Request::UiCustom(params) => {
            if let Some(ui) = ui {
                ui.custom(params.slot, params.overlay, params.overlay_options, cancel)
                    .await;
            }
            ok(Value::Null)
        }
        Request::UiGetAllThemes(_) => match ui {
            Some(ui) => {
                let catalog: Vec<pi_ext_protocol::ThemeCatalogEntry> = ui
                    .get_all_themes()
                    .into_iter()
                    .map(|item| pi_ext_protocol::ThemeCatalogEntry {
                        name: item.name,
                        path: item.path.map(|p| p.to_string_lossy().into_owned()),
                    })
                    .collect();
                ok(serde_json::to_value(catalog).unwrap_or_else(|_| json!([])))
            }
            None => ok(json!([])),
        },
        Request::UiGetTheme(params) => match ui.and_then(|ui| ui.get_theme_json(&params.name)) {
            Some((name, theme_json)) => ok(json!({ "name": name, "json": theme_json })),
            None => ok(Value::Null),
        },

        Request::SessionSetup(_) => {
            err("session/setup is host-initiated; the sidecar never sends it")
        }
        Request::LifecycleInit(_) | Request::LifecycleLoad(_) | Request::LifecycleShutdown(_) => {
            err("lifecycle requests are host-initiated")
        }
        Request::EventEmit(_) => err("event/emit is host-initiated"),
        Request::ToolExecute(_) | Request::ProviderStream(_) => {
            err("tool/provider requests are host-initiated")
        }
        Request::CommandExecute(_) | Request::ShortcutInvoke(_) => {
            err("command/shortcut requests are host-initiated")
        }
        Request::UiRender(_) | Request::UiAutocomplete(_) | Request::UiTerminalInput(_) => {
            err("ui render surface is host-initiated")
        }
    }
}

async fn handle_notification(
    forwarder: &Arc<EventForwarder>,
    config: &ActionServerConfig,
    notification: Notification,
) {
    let ui = config.ui.lock().clone();
    match notification {
        Notification::ActionSendMessage(params) => config.actions.send_message(params).await,
        Notification::ActionSendUserMessage(params) => {
            config.actions.send_user_message(params).await;
        }
        Notification::ActionAppendEntry(params) => config.actions.append_entry(params).await,
        Notification::ActionSetSessionName(params) => {
            config.actions.set_session_name(params.name).await;
        }
        Notification::ActionSetLabel(params) => config.actions.set_label(params).await,
        Notification::ActionSetActiveTools(params) => {
            config.actions.set_active_tools(params.tool_names).await;
        }
        Notification::ActionRefreshTools(params) => config.actions.refresh_tools(params).await,
        Notification::ActionSetThinkingLevel(params) => {
            config.actions.set_thinking_level(params).await;
        }
        Notification::ActionShutdown(_) => config.actions.shutdown().await,
        Notification::ActionAbort(_) => config.actions.abort().await,
        Notification::ActionCompact(params) => {
            // Count the sidecar's FIFO pending BEFORE the compaction runs so
            // its manual session_before_compact/session_compact events are
            // forwarded even without a subscription (sidecar fix `81f59ef`).
            forwarder.note_compact_requested();
            config.actions.compact(params).await;
        }

        Notification::ExtensionError(error) => forwarder.error_sink()(error),

        Notification::UiNotify(params) => {
            if let Some(ui) = &ui {
                let level = match params.level {
                    pi_ext_protocol::NotificationLevel::Info => NotifyType::Info,
                    pi_ext_protocol::NotificationLevel::Warning => NotifyType::Warning,
                    pi_ext_protocol::NotificationLevel::Error => NotifyType::Error,
                };
                ui.notify(params.message, Some(level));
            }
        }
        Notification::UiSetStatus(params) => {
            if let Some(ui) = &ui {
                ui.set_status(params.key, params.value);
            }
        }
        Notification::UiSetTitle(params) => {
            if let Some(ui) = &ui {
                ui.set_title(params.text);
            }
        }
        Notification::UiSetEditorText(params) => {
            if let Some(ui) = &ui {
                ui.set_editor_text(params.text);
            }
        }

        // Runtime provider registration mutates the host model catalog
        // (C7/F9); ordered through the serve loop like other actions.
        Notification::ProviderRegister(registration) => {
            config.actions.provider_register(registration).await;
        }
        Notification::ProviderUnregister(params) => {
            config.actions.provider_unregister(params.name).await;
        }

        // `provider/event` streams + frames route through the composed
        // fallback sink (C7 providers, C8 frames).
        Notification::ProviderEvent(_)
        | Notification::ToolUpdate(_)
        | Notification::UiFrame(_)
        | Notification::UiDispose(_)
        | Notification::UiDone(_)
        | Notification::UiOverlay(_)
        | Notification::UiEditorSubmit(_)
        | Notification::UiEditorChange(_)
        | Notification::UiTerminalInputActive(_)
        | Notification::UiSetWorkingMessage(_)
        | Notification::UiSetWorkingVisible(_)
        | Notification::UiSetWorkingIndicator(_)
        | Notification::UiSetHiddenThinkingLabel(_)
        | Notification::UiPasteToEditor(_)
        | Notification::UiSetTheme(_)
        | Notification::UiSetToolsExpanded(_) => {
            if let Some(fallback) = &config.fallback {
                fallback(notification);
            }
        }

        // Control-plane notifications never reach the incoming queue
        // (client.rs routes them inline); session/sync, ui/input, and
        // ui/focus are host→sidecar only.
        Notification::LifecycleHello(_)
        | Notification::LifecycleInitialized(_)
        | Notification::LifecyclePing(_)
        | Notification::LifecyclePong(_)
        | Notification::EventNotify(_)
        | Notification::SessionSync(_)
        | Notification::StateUpdate(_)
        | Notification::UiComponentInput(_)
        | Notification::UiFocus(_) => {}
    }
}
