#![cfg(unix)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use pi_orchestrator::ipc::{
    HandlerFuture, InstanceSummary, IpcRequestHandler, IpcServerError, OrchestratorRequest,
    OrchestratorResponse, RpcEventSink, RpcStreamHandler, encode_message, send_ipc_request_to,
    start_ipc_server_at,
};
use pi_orchestrator::types::InstanceStatus;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::mpsc;

fn summary() -> InstanceSummary {
    InstanceSummary {
        id: "pi-1".into(),
        status: InstanceStatus::Online,
        cwd: "/work".into(),
        label: None,
        session_id: Some("session-1".into()),
        session_file: None,
        radius_pi_id: None,
    }
}

struct TestHandler {
    commands: mpsc::UnboundedSender<Value>,
    closed: Arc<AtomicBool>,
    active_commands: Arc<AtomicUsize>,
}

impl IpcRequestHandler for TestHandler {
    fn handle(&self, request: OrchestratorRequest) -> HandlerFuture<'_, OrchestratorResponse> {
        Box::pin(async move {
            match request {
                OrchestratorRequest::List => OrchestratorResponse::ListResult {
                    ok: true,
                    error: None,
                    instances: Some(vec![summary()]),
                },
                OrchestratorRequest::RpcStream { .. } => OrchestratorResponse::RpcReady {
                    ok: true,
                    error: None,
                    instance: Some(summary()),
                },
                _ => OrchestratorResponse::error("unsupported test request"),
            }
        })
    }

    fn open_rpc_stream<'a>(
        &'a self,
        instance_id: &'a str,
        events: RpcEventSink,
    ) -> HandlerFuture<'a, Option<Box<dyn RpcStreamHandler>>> {
        Box::pin(async move {
            if instance_id != "pi-1" {
                return None;
            }
            events
                .send(&json!({"type":"session_event","event":"opened"}))
                .expect("queue opening event");
            Some(Box::new(TestRpcStream {
                commands: self.commands.clone(),
                closed: Arc::clone(&self.closed),
                events,
                active_commands: Arc::clone(&self.active_commands),
            }) as Box<dyn RpcStreamHandler>)
        })
    }
}

struct TestRpcStream {
    commands: mpsc::UnboundedSender<Value>,
    closed: Arc<AtomicBool>,
    events: RpcEventSink,
    active_commands: Arc<AtomicUsize>,
}

impl RpcStreamHandler for TestRpcStream {
    fn handle_request(&mut self, request: Value) -> HandlerFuture<'_, Result<(), String>> {
        let commands = self.commands.clone();
        let events = self.events.clone();
        let active_commands = Arc::clone(&self.active_commands);
        Box::pin(async move {
            assert_eq!(active_commands.fetch_add(1, Ordering::SeqCst), 0);
            if request.get("sequence") == Some(&json!(1)) {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            commands
                .send(request.clone())
                .map_err(|_| "command receiver closed".to_owned())?;
            events
                .send(&json!({"type":"response","sequence":request["sequence"]}))
                .map_err(|error| error.to_string())?;
            assert_eq!(active_commands.fetch_sub(1, Ordering::SeqCst), 1);
            Ok(())
        })
    }

    fn close(&mut self) {
        self.closed.store(true, Ordering::SeqCst);
    }
}

fn test_handler() -> (
    Arc<TestHandler>,
    mpsc::UnboundedReceiver<Value>,
    Arc<AtomicBool>,
) {
    let (commands, command_rx) = mpsc::unbounded_channel();
    let closed = Arc::new(AtomicBool::new(false));
    (
        Arc::new(TestHandler {
            commands,
            closed: Arc::clone(&closed),
            active_commands: Arc::new(AtomicUsize::new(0)),
        }),
        command_rx,
        closed,
    )
}

#[test]
fn protocol_serde_matches_typescript_key_order_and_omission() {
    let requests = [
        (
            OrchestratorRequest::Spawn {
                cwd: "/work".into(),
                label: None,
                provider: Some("anthropic".into()),
                model: None,
            },
            "{\"type\":\"spawn\",\"cwd\":\"/work\",\"provider\":\"anthropic\"}\n",
        ),
        (OrchestratorRequest::List, "{\"type\":\"list\"}\n"),
        (
            OrchestratorRequest::Stop {
                instance_id: "pi-1".into(),
            },
            "{\"type\":\"stop\",\"instanceId\":\"pi-1\"}\n",
        ),
        (
            OrchestratorRequest::Status {
                instance_id: "pi-1".into(),
            },
            "{\"type\":\"status\",\"instanceId\":\"pi-1\"}\n",
        ),
        (
            OrchestratorRequest::Rpc {
                instance_id: "pi-1".into(),
                command: pi_orchestrator::wire::RpcCommandEnvelope::try_from(json!({
                    "type": "prompt",
                    "message": "hello"
                }))
                .unwrap(),
            },
            "{\"type\":\"rpc\",\"instanceId\":\"pi-1\",\"command\":{\"type\":\"prompt\",\"message\":\"hello\"}}\n",
        ),
        (
            OrchestratorRequest::RpcStream {
                instance_id: "pi-1".into(),
            },
            "{\"type\":\"rpc_stream\",\"instanceId\":\"pi-1\"}\n",
        ),
    ];
    for (request, golden) in requests {
        assert_eq!(encode_message(&request).unwrap(), golden);
        assert_eq!(
            serde_json::from_str::<OrchestratorRequest>(golden.trim()).unwrap(),
            request
        );
    }

    let responses = [
        (
            OrchestratorResponse::SpawnResult {
                ok: true,
                error: None,
                instance: None,
            },
            "{\"type\":\"spawn_result\",\"ok\":true}\n",
        ),
        (
            OrchestratorResponse::ListResult {
                ok: false,
                error: Some("unavailable".into()),
                instances: None,
            },
            "{\"type\":\"list_result\",\"ok\":false,\"error\":\"unavailable\"}\n",
        ),
        (
            OrchestratorResponse::StopResult {
                ok: true,
                error: None,
                instance_id: Some("pi-1".into()),
            },
            "{\"type\":\"stop_result\",\"ok\":true,\"instanceId\":\"pi-1\"}\n",
        ),
        (
            OrchestratorResponse::StatusResult {
                ok: true,
                error: None,
                instance: Some(summary()),
            },
            "{\"type\":\"status_result\",\"ok\":true,\"instance\":{\"id\":\"pi-1\",\"status\":\"online\",\"cwd\":\"/work\",\"sessionId\":\"session-1\"}}\n",
        ),
        (
            OrchestratorResponse::RpcResult {
                ok: true,
                error: None,
                response: json!({"type":"response","success":true}),
            },
            "{\"type\":\"rpc_result\",\"ok\":true,\"response\":{\"type\":\"response\",\"success\":true}}\n",
        ),
        (
            OrchestratorResponse::RpcReady {
                ok: true,
                error: None,
                instance: Some(summary()),
            },
            "{\"type\":\"rpc_ready\",\"ok\":true,\"instance\":{\"id\":\"pi-1\",\"status\":\"online\",\"cwd\":\"/work\",\"sessionId\":\"session-1\"}}\n",
        ),
        (
            OrchestratorResponse::error("broken"),
            "{\"type\":\"error\",\"ok\":false,\"error\":\"broken\"}\n",
        ),
    ];
    for (response, golden) in responses {
        assert_eq!(encode_message(&response).unwrap(), golden);
        assert_eq!(
            serde_json::from_str::<OrchestratorResponse>(golden.trim()).unwrap(),
            response
        );
    }
}

#[tokio::test]
async fn single_request_gets_one_response_and_server_closes_connection() {
    let temp = tempfile::tempdir().unwrap();
    let socket_path = temp.path().join("orchestrator.sock");
    let (handler, _, _) = test_handler();
    let server = start_ipc_server_at(&socket_path, handler).await.unwrap();

    let stream = UnixStream::connect(&socket_path).await.unwrap();
    let (read_half, mut write_half) = stream.into_split();
    write_half
        .write_all(
            encode_message(&OrchestratorRequest::List)
                .unwrap()
                .as_bytes(),
        )
        .await
        .unwrap();
    let mut reader = BufReader::new(read_half);
    let mut response = String::new();
    reader.read_line(&mut response).await.unwrap();
    assert!(response.starts_with(r#"{"type":"list_result","ok":true"#));
    let mut remainder = Vec::new();
    reader.read_to_end(&mut remainder).await.unwrap();
    assert!(remainder.is_empty());

    let response = send_ipc_request_to(&socket_path, &OrchestratorRequest::List)
        .await
        .unwrap();
    assert!(matches!(
        response,
        OrchestratorResponse::ListResult { ok: true, .. }
    ));
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn rpc_stream_sends_ready_first_and_drains_buffered_commands_sequentially() {
    let temp = tempfile::tempdir().unwrap();
    let socket_path = temp.path().join("orchestrator.sock");
    let (handler, mut commands, closed) = test_handler();
    let server = start_ipc_server_at(&socket_path, handler).await.unwrap();

    let mut stream = UnixStream::connect(&socket_path).await.unwrap();
    let payload = concat!(
        "{\"type\":\"rpc_stream\",\"instanceId\":\"pi-1\"}\n",
        "{\"type\":\"prompt\",\"sequence\":1}\n",
        "{\"type\":\"prompt\",\"sequence\":2}\n"
    );
    stream.write_all(payload.as_bytes()).await.unwrap();
    let (read_half, write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    let mut first = String::new();
    reader.read_line(&mut first).await.unwrap();
    assert!(first.starts_with(r#"{"type":"rpc_ready","ok":true"#));

    let first_command = tokio::time::timeout(Duration::from_secs(1), commands.recv())
        .await
        .unwrap()
        .unwrap();
    let second_command = tokio::time::timeout(Duration::from_secs(1), commands.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first_command["sequence"], 1);
    assert_eq!(second_command["sequence"], 2);

    let mut opening_event = String::new();
    let mut first_response = String::new();
    let mut second_response = String::new();
    reader.read_line(&mut opening_event).await.unwrap();
    reader.read_line(&mut first_response).await.unwrap();
    reader.read_line(&mut second_response).await.unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&opening_event).unwrap(),
        json!({"type":"session_event","event":"opened"})
    );
    assert_eq!(
        serde_json::from_str::<Value>(&first_response).unwrap(),
        json!({"type":"response","sequence":1})
    );
    assert_eq!(
        serde_json::from_str::<Value>(&second_response).unwrap(),
        json!({"type":"response","sequence":2})
    );

    drop(write_half);
    tokio::time::timeout(Duration::from_secs(1), async {
        while !closed.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn stale_socket_is_replaced_and_live_socket_is_refused_with_exact_text() {
    let temp = tempfile::tempdir().unwrap();
    let socket_path = temp.path().join("orchestrator.sock");

    let stale = tokio::net::UnixListener::bind(&socket_path).unwrap();
    drop(stale);
    let (handler, _, _) = test_handler();
    let server = start_ipc_server_at(&socket_path, handler.clone())
        .await
        .unwrap();

    let error = match start_ipc_server_at(&socket_path, handler).await {
        Ok(_) => panic!("second server unexpectedly started"),
        Err(error) => error,
    };
    assert!(matches!(error, IpcServerError::AlreadyRunning(_)));
    assert_eq!(
        error.to_string(),
        format!("orchestrator is already running: {}", socket_path.display())
    );

    server.shutdown().await.unwrap();
    assert!(!socket_path.exists());
}
