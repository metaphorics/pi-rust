//! Extension tool bridging (Phase 6 commit C7, plan §2 `tool/` family).
//!
//! Every sidecar [`ToolRegistration`] becomes a [`SessionToolDefinition`]
//! whose `execute` performs a `tool/execute` request against the live
//! sidecar. Contract (oracle `wrapRegisteredTool` + host.ts `tool/execute`):
//! - name/label/description/parameters cross verbatim (I9 — they are prompt
//!   surface);
//! - streamed partials arrive as `tool/update` notifications and are routed
//!   to the loop's `on_update` callback by [`ToolUpdateRouter`];
//! - a throwing extension tool ⇒ wire `err` ⇒ `Err(message)` ⇒ the agent
//!   loop records an error tool result (pi parity: thrown execute);
//! - success relays pi's full `AgentToolResult` surface — `addedToolNames`
//!   and `terminate` ride through to the loop, which already implements
//!   batch termination and `ToolResultMessage.addedToolNames`;
//! - cancellation maps the loop's token onto a wire cancel frame;
//! - after the response, an [`ExtensionHost::barrier`] fence guarantees the
//!   notifications the sidecar sent BEFORE responding (`setActiveTools`,
//!   `refreshTools`, `appendEntry`, ...) have been applied — pi's
//!   synchronous read-after-write semantics at tool-call granularity.

use std::collections::HashMap;
use std::sync::{Arc, Weak};

use pi_agent::{AgentToolResult, CancellationToken, ToolDefinition, ToolExecuteFn};
use pi_ext_protocol::{Request, ToolExecuteParams, ToolExecuteResult, ToolRegistration};
use serde_json::Value;

use crate::session::SessionToolDefinition;

use super::ExtensionHost;

/// Callback receiving one raw `tool/update` partial.
pub type ToolUpdateCallback = Arc<dyn Fn(Value) + Send + Sync>;

/// Routes `tool/update` notifications to the in-flight tool call's
/// `on_update` callback. Registration is RAII: the guard removes the entry
/// on every exit path (success, remote error, cancellation, panic), so a
/// reused tool-call id can never reach a dead invocation.
#[derive(Default)]
pub struct ToolUpdateRouter {
    map: parking_lot::Mutex<HashMap<String, ToolUpdateCallback>>,
}

impl ToolUpdateRouter {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Register a partial-update callback for `tool_call_id`; dropping the
    /// returned guard unregisters it.
    pub fn register(
        self: &Arc<Self>,
        tool_call_id: &str,
        callback: ToolUpdateCallback,
    ) -> ToolUpdateGuard {
        self.map.lock().insert(tool_call_id.to_string(), callback);
        ToolUpdateGuard {
            router: Arc::downgrade(self),
            tool_call_id: tool_call_id.to_string(),
        }
    }

    /// Deliver one `tool/update` partial. Unknown ids are dropped silently
    /// (the call already settled — pi ignores late updates the same way via
    /// its `accepting` latch).
    pub fn dispatch(&self, tool_call_id: &str, partial: Value) {
        let callback = self.map.lock().get(tool_call_id).cloned();
        if let Some(callback) = callback {
            callback(partial);
        }
    }
}

/// RAII unregistration for [`ToolUpdateRouter::register`].
pub struct ToolUpdateGuard {
    router: Weak<ToolUpdateRouter>,
    tool_call_id: String,
}

impl Drop for ToolUpdateGuard {
    fn drop(&mut self) {
        if let Some(router) = self.router.upgrade() {
            router.map.lock().remove(&self.tool_call_id);
        }
    }
}

/// Produces a fresh partial state patch (`state/update`) mirroring the
/// host-side values sync getters read during tool execution.
pub type FreshStateFn = Arc<dyn Fn() -> pi_ext_protocol::StateUpdate + Send + Sync>;

/// Shared context captured by every bridged tool's `execute`.
pub struct ExtensionToolContext {
    /// Weak: the session's tool registry must not keep the host (and its
    /// Bun process) alive after the binding is dropped.
    pub host: Weak<ExtensionHost>,
    pub router: Arc<ToolUpdateRouter>,
    /// Snapshot of the host state pushed to the sidecar mirror right before
    /// `tool/execute`, so `getActiveTools()`/`getThinkingLevel()`-class sync
    /// getters (and the `wrapRegisteredTool` active-diff) resolve at call
    /// time — pi's same-process freshness (runner.ts:634).
    pub fresh_state: FreshStateFn,
}

/// Resolves when `token` is cancelled (house token is a bare flag; polls at
/// the same cadence as the forwarder's cancel arms).
pub(super) async fn wait_cancelled(token: CancellationToken) {
    while !token.is_cancelled() {
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
}

/// Build the session tool definitions for a registration snapshot.
pub fn extension_tool_definitions(
    registrations: &[ToolRegistration],
    context: &Arc<ExtensionToolContext>,
) -> Vec<SessionToolDefinition> {
    registrations
        .iter()
        .map(|registration| extension_tool_definition(registration, context))
        .collect()
}

/// Wrap one [`ToolRegistration`] as a pi-agent tool. Strings and parameter
/// schemas are relayed verbatim (I9).
pub fn extension_tool_definition(
    registration: &ToolRegistration,
    context: &Arc<ExtensionToolContext>,
) -> SessionToolDefinition {
    let name = registration.name.clone();
    let execute: ToolExecuteFn = {
        let context = context.clone();
        let name = name.clone();
        Arc::new(move |tool_call_id, args, cancel, on_update| {
            let context = context.clone();
            let name = name.clone();
            Box::pin(async move {
                execute_extension_tool(&context, name, tool_call_id, args, cancel, on_update).await
            })
        })
    };

    SessionToolDefinition {
        definition: Arc::new(ToolDefinition {
            name: registration.name.clone(),
            label: registration.label.clone(),
            description: registration.description.clone(),
            parameters: registration.parameters.clone(),
            execution_mode: None,
            prepare_arguments: None,
            execute,
            renderer: None,
        }),
        prompt_snippet: registration.prompt_snippet.clone(),
        // The wire flattens pi's promptGuidelines string[] with '\n' (the
        // sidecar mirror splits identically).
        prompt_guidelines: registration
            .prompt_guidelines
            .as_deref()
            .map(|joined| joined.split('\n').map(str::to_string).collect())
            .unwrap_or_default(),
        source: "extension",
        source_info: Some(super::binding::wire_source_info(
            registration.source_info.clone(),
        )),
    }
}

async fn execute_extension_tool(
    context: &ExtensionToolContext,
    name: String,
    tool_call_id: String,
    args: Value,
    cancel: Option<CancellationToken>,
    on_update: Option<pi_agent::AgentToolUpdateCallback>,
) -> Result<AgentToolResult, String> {
    let Some(host) = context.host.upgrade() else {
        return Err("Extension sidecar is not available".to_string());
    };
    // Tools run mid-turn: never trigger the (turn-boundary-gated, I8)
    // respawn from here — a dead sidecar fails the call as a tool error.
    let Some(connection) = host.current_connection().await else {
        return Err("Extension sidecar is not running".to_string());
    };

    // Freshness point (pi resolves runner state at call time): patch the
    // sidecar mirror so sync getters and the wrapper's active-tool diff see
    // the host's CURRENT state, not the last event's snapshot.
    let patch = (context.fresh_state)();
    if connection
        .notify(pi_ext_protocol::Notification::StateUpdate(Box::new(patch)))
        .await
        .is_err()
    {
        return Err("Extension sidecar is not running".to_string());
    }

    // Route streamed partials to the loop callback for the lifetime of this
    // call (RAII guard: removed on every exit path).
    let _update_guard = on_update.map(|callback| {
        context.router.register(
            &tool_call_id,
            Arc::new(move |partial: Value| {
                // Partials carry pi's AgentToolResult shape; a malformed one
                // is dropped (never fails the call).
                if let Ok(partial) = serde_json::from_value::<AgentToolResult>(partial) {
                    callback(partial);
                }
            }),
        )
    });

    let mut pending = connection
        .begin_request(Request::ToolExecute(ToolExecuteParams {
            tool_call_id: tool_call_id.clone(),
            name,
            args,
        }))
        .await
        .map_err(|error| error.to_string())?;

    let outcome = match cancel {
        Some(token) => {
            tokio::select! {
                outcome = &mut pending => outcome,
                _ = wait_cancelled(token) => {
                    // Abandon + wire cancel frame; the sidecar aborts the
                    // extension tool's AbortSignal (wrapper.ts signature).
                    pending.cancel().await;
                    return Err("Tool execution was aborted".to_string());
                }
            }
        }
        None => (&mut pending).await,
    };

    let value = match outcome {
        Ok(value) => value,
        Err(error) => {
            // Remote err = the extension tool threw (host.ts rethrows). The
            // loop records an error tool result — pi parity.
            return Err(match error {
                super::ClientError::Remote(remote) => remote.message,
                other => other.to_string(),
            });
        }
    };

    // Ordering fence: the notifications the sidecar sent BEFORE this
    // response (setActiveTools / refreshTools / appendEntry mirrors) have
    // been fully applied once the barrier resolves, so the loop's next-turn
    // context observes them — pi's synchronous semantics.
    host.barrier().await;

    let decoded: ToolExecuteResult = serde_json::from_value(value)
        .map_err(|error| format!("malformed tool/execute result: {error}"))?;
    Ok(AgentToolResult {
        content: decoded.content,
        // `Null` details ⇒ ToolResultMessage.details omitted (loop contract),
        // matching pi's absent `details`.
        details: decoded.details.unwrap_or(Value::Null),
        added_tool_names: decoded.added_tool_names,
        terminate: decoded.terminate,
    })
}
