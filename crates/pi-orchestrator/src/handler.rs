//! IPC request dispatch over the supervisor (port of handler.ts).
//!
//! The oracle's `handleIpcRequest` lets supervisor failures propagate and the
//! IPC server's catch-all turns them into `{type:"error", ok:false, error}`
//! responses (ipc/server.ts:139-146). The Rust server dispatches through an
//! infallible trait instead, so this handler performs that conversion itself:
//! every supervisor error becomes an error response carrying the error's
//! message — the same wire bytes either way.

use std::path::PathBuf;

use serde_json::Value;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::ipc::{
    HandlerFuture, InstanceSummary, IpcRequestHandler, OrchestratorRequest, OrchestratorResponse,
    RpcEventSink, RpcStreamHandler,
};
use crate::supervisor::{RpcStream, SpawnOptions, Supervisor};
use crate::wire::RpcCommandEnvelope;

/// Bridges the IPC protocol to the supervisor (port of handler.ts
/// `handleIpcRequest` + `openRpcStream`).
pub struct OrchestratorIpcHandler {
    supervisor: Supervisor,
    spawn_command_override: Option<(PathBuf, Vec<String>)>,
}

impl OrchestratorIpcHandler {
    pub fn new(supervisor: Supervisor) -> Self {
        Self {
            supervisor,
            spawn_command_override: None,
        }
    }

    /// Test-only child command injection threaded into every spawn, mirroring
    /// [`SpawnOptions`]'s `command_override`.
    pub fn with_spawn_command_override(
        supervisor: Supervisor,
        command_override: (PathBuf, Vec<String>),
    ) -> Self {
        Self {
            supervisor,
            spawn_command_override: Some(command_override),
        }
    }
}

fn unknown_instance(instance_id: &str) -> OrchestratorResponse {
    OrchestratorResponse::error(format!("Unknown instance: {instance_id}"))
}

impl IpcRequestHandler for OrchestratorIpcHandler {
    fn handle(&self, request: OrchestratorRequest) -> HandlerFuture<'_, OrchestratorResponse> {
        Box::pin(async move {
            match request {
                // The oracle handler forwards only cwd/label; the protocol's
                // provider/model fields have no consumer in 0.80.7.
                OrchestratorRequest::Spawn {
                    cwd,
                    label,
                    provider: _,
                    model: _,
                } => {
                    let options = SpawnOptions {
                        cwd,
                        label,
                        command_override: self.spawn_command_override.clone(),
                    };
                    match self.supervisor.spawn_instance(options).await {
                        Ok(record) => OrchestratorResponse::SpawnResult {
                            ok: true,
                            error: None,
                            instance: Some(InstanceSummary::from(&record)),
                        },
                        Err(error) => OrchestratorResponse::error(error.to_string()),
                    }
                }
                OrchestratorRequest::List => match self.supervisor.list_instances() {
                    Ok(instances) => OrchestratorResponse::ListResult {
                        ok: true,
                        error: None,
                        instances: Some(instances.iter().map(InstanceSummary::from).collect()),
                    },
                    Err(error) => OrchestratorResponse::error(error.to_string()),
                },
                OrchestratorRequest::Status { instance_id } => {
                    match self.supervisor.get_instance(&instance_id) {
                        Ok(Some(record)) => OrchestratorResponse::StatusResult {
                            ok: true,
                            error: None,
                            instance: Some(InstanceSummary::from(&record)),
                        },
                        Ok(None) => unknown_instance(&instance_id),
                        Err(error) => OrchestratorResponse::error(error.to_string()),
                    }
                }
                OrchestratorRequest::Stop { instance_id } => {
                    match self.supervisor.stop_instance(&instance_id).await {
                        Ok(Some(_)) => OrchestratorResponse::StopResult {
                            ok: true,
                            error: None,
                            instance_id: Some(instance_id),
                        },
                        Ok(None) => unknown_instance(&instance_id),
                        Err(error) => OrchestratorResponse::error(error.to_string()),
                    }
                }
                OrchestratorRequest::Rpc {
                    instance_id,
                    command,
                } => match self.supervisor.handle_rpc(&instance_id, command).await {
                    Ok(Some(response)) => OrchestratorResponse::RpcResult {
                        ok: true,
                        error: None,
                        response: response.raw,
                    },
                    Ok(None) => unknown_instance(&instance_id),
                    Err(error) => OrchestratorResponse::error(error.to_string()),
                },
                OrchestratorRequest::RpcStream { instance_id } => {
                    match self.supervisor.get_instance(&instance_id) {
                        Ok(Some(record)) => OrchestratorResponse::RpcReady {
                            ok: true,
                            error: None,
                            instance: Some(InstanceSummary::from(&record)),
                        },
                        Ok(None) => unknown_instance(&instance_id),
                        Err(error) => OrchestratorResponse::error(error.to_string()),
                    }
                }
            }
        })
    }

    fn open_rpc_stream<'a>(
        &'a self,
        instance_id: &'a str,
        events: RpcEventSink,
    ) -> HandlerFuture<'a, Option<Box<dyn RpcStreamHandler>>> {
        Box::pin(async move {
            let mut stream = self.supervisor.open_rpc_stream(instance_id)?;
            // Move the subscription receivers into a forwarder task; the
            // stream keeps handling commands and ui responses.
            let (_detached_events, placeholder_events) = mpsc::unbounded_channel();
            let mut event_rx = std::mem::replace(&mut stream.events, placeholder_events);
            let (_detached_ui, placeholder_ui) = mpsc::unbounded_channel();
            let mut ui_rx = std::mem::replace(&mut stream.ui_requests, placeholder_ui);

            let sink = events.clone();
            let forwarder = tokio::spawn(async move {
                // Events and ui requests are forwarded verbatim as they
                // arrive. Deviation note: the oracle writes both from one
                // stdout dispatch loop, so an event and a ui request keep the
                // child's emission order; here they travel on two channels
                // and only per-channel order is guaranteed.
                let mut events_open = true;
                let mut ui_open = true;
                while events_open || ui_open {
                    tokio::select! {
                        event = event_rx.recv(), if events_open => match event {
                            Some(value) => {
                                if events.send(&value).is_err() {
                                    break;
                                }
                            }
                            None => events_open = false,
                        },
                        request = ui_rx.recv(), if ui_open => match request {
                            Some(value) => {
                                if events.send(&value).is_err() {
                                    break;
                                }
                            }
                            None => ui_open = false,
                        },
                    }
                }
            });

            Some(Box::new(SupervisorRpcStream {
                stream,
                sink,
                forwarder,
            }) as Box<dyn RpcStreamHandler>)
        })
    }
}

/// One upgraded `rpc_stream` connection (port of handler.ts `openRpcStream`'s
/// returned handle).
struct SupervisorRpcStream {
    stream: RpcStream,
    sink: RpcEventSink,
    forwarder: JoinHandle<()>,
}

impl RpcStreamHandler for SupervisorRpcStream {
    fn handle_request(&mut self, request: Value) -> HandlerFuture<'_, Result<(), String>> {
        Box::pin(async move {
            if request.get("type").and_then(Value::as_str) == Some("extension_ui_response") {
                return self
                    .stream
                    .handle_ui_response(&request)
                    .map_err(|error| error.to_string());
            }
            let command =
                RpcCommandEnvelope::try_from(request).map_err(|error| error.to_string())?;
            let response = self
                .stream
                .handle_rpc(command)
                .await
                .map_err(|error| error.to_string())?;
            self.sink
                .send(&response.raw)
                .map_err(|error| error.to_string())
        })
    }

    fn close(&mut self) {
        self.stream.close();
        self.forwarder.abort();
    }
}
