#![cfg(unix)]

mod support {
    pub mod fake_pi;
}

use std::path::PathBuf;
use std::sync::Arc;

use pi_orchestrator::handler::OrchestratorIpcHandler;
use pi_orchestrator::ipc::{
    IpcServer, OrchestratorRequest, OrchestratorResponse, connect_rpc_stream_to,
    send_ipc_request_to, start_ipc_server_at,
};
use pi_orchestrator::storage::Storage;
use pi_orchestrator::supervisor::{NullPresence, Supervisor};
use pi_orchestrator::types::InstanceStatus;
use pi_orchestrator::wire::RpcCommandEnvelope;
use serde_json::{Value, json};
use support::fake_pi::FakePi;

struct Fixture {
    dir: tempfile::TempDir,
    fake: FakePi,
    socket_path: PathBuf,
    server: Option<IpcServer>,
}

impl Fixture {
    async fn start() -> Self {
        Self::start_with_child(None).await
    }

    async fn start_with_child(command_override: Option<(PathBuf, Vec<String>)>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let fake = FakePi::new();
        let supervisor = Supervisor::new(Storage::new(dir.path()), Arc::new(NullPresence));
        let child = command_override.unwrap_or_else(|| fake.options().command_override.unwrap());
        let handler = Arc::new(OrchestratorIpcHandler::with_spawn_command_override(
            supervisor, child,
        ));
        let socket_path = dir.path().join("orchestrator.sock");
        let server = start_ipc_server_at(&socket_path, handler).await.unwrap();
        Self {
            dir,
            fake,
            socket_path,
            server: Some(server),
        }
    }

    fn spawn_cwd(&self) -> String {
        self.fake.options().cwd.to_string_lossy().into_owned()
    }

    async fn request(&self, request: OrchestratorRequest) -> OrchestratorResponse {
        send_ipc_request_to(&self.socket_path, &request)
            .await
            .unwrap()
    }

    async fn spawn(&self) -> String {
        match self
            .request(OrchestratorRequest::Spawn {
                cwd: self.spawn_cwd(),
                label: Some("worker".into()),
                provider: None,
                model: None,
            })
            .await
        {
            OrchestratorResponse::SpawnResult {
                ok: true,
                instance: Some(instance),
                ..
            } => instance.id,
            other => panic!("unexpected spawn response: {other:?}"),
        }
    }

    async fn finish(mut self) {
        self.server.take().unwrap().shutdown().await.unwrap();
    }
}

fn rpc_command(value: Value) -> RpcCommandEnvelope {
    RpcCommandEnvelope::try_from(value).unwrap()
}

#[tokio::test]
async fn lifecycle_round_trips_over_the_socket() {
    let fixture = Fixture::start().await;

    let spawn = fixture
        .request(OrchestratorRequest::Spawn {
            cwd: fixture.spawn_cwd(),
            label: Some("worker".into()),
            provider: None,
            model: None,
        })
        .await;
    let OrchestratorResponse::SpawnResult {
        ok: true,
        error: None,
        instance: Some(instance),
    } = spawn
    else {
        panic!("unexpected spawn response: {spawn:?}");
    };
    assert_eq!(instance.status, InstanceStatus::Online);
    assert_eq!(instance.cwd, fixture.spawn_cwd());
    assert_eq!(instance.label.as_deref(), Some("worker"));
    assert_eq!(instance.session_id.as_deref(), Some("session-0"));
    assert_eq!(
        instance.session_file.as_deref(),
        Some("/tmp/session-0.jsonl")
    );

    let list = fixture.request(OrchestratorRequest::List).await;
    let OrchestratorResponse::ListResult {
        ok: true,
        instances: Some(instances),
        ..
    } = list
    else {
        panic!("unexpected list response: {list:?}");
    };
    assert_eq!(instances.len(), 1);
    assert_eq!(instances[0].id, instance.id);

    let status = fixture
        .request(OrchestratorRequest::Status {
            instance_id: instance.id.clone(),
        })
        .await;
    let OrchestratorResponse::StatusResult {
        ok: true,
        instance: Some(status_instance),
        ..
    } = status
    else {
        panic!("unexpected status response: {status:?}");
    };
    assert_eq!(status_instance.status, InstanceStatus::Online);

    let rpc = fixture
        .request(OrchestratorRequest::Rpc {
            instance_id: instance.id.clone(),
            command: rpc_command(json!({"type":"echo","id":"req-1","value":{"n":7}})),
        })
        .await;
    let OrchestratorResponse::RpcResult {
        ok: true, response, ..
    } = rpc
    else {
        panic!("unexpected rpc response: {rpc:?}");
    };
    assert_eq!(
        response,
        json!({"type":"response","id":"req-1","command":"echo","success":true,"data":{"n":7}})
    );

    // A session-metadata command refreshes the persisted record before the
    // rpc_result is returned (get_state sync).
    let rpc = fixture
        .request(OrchestratorRequest::Rpc {
            instance_id: instance.id.clone(),
            command: rpc_command(json!({"type":"new_session"})),
        })
        .await;
    assert!(matches!(
        rpc,
        OrchestratorResponse::RpcResult { ok: true, .. }
    ));
    let status = fixture
        .request(OrchestratorRequest::Status {
            instance_id: instance.id.clone(),
        })
        .await;
    let OrchestratorResponse::StatusResult {
        instance: Some(refreshed),
        ..
    } = status
    else {
        panic!("unexpected status response: {status:?}");
    };
    assert_eq!(refreshed.session_id.as_deref(), Some("session-1"));

    let stop = fixture
        .request(OrchestratorRequest::Stop {
            instance_id: instance.id.clone(),
        })
        .await;
    assert_eq!(
        stop,
        OrchestratorResponse::StopResult {
            ok: true,
            error: None,
            instance_id: Some(instance.id.clone()),
        }
    );

    let status = fixture
        .request(OrchestratorRequest::Status {
            instance_id: instance.id.clone(),
        })
        .await;
    assert_eq!(
        status,
        OrchestratorResponse::error(format!("Unknown instance: {}", instance.id))
    );
    assert_eq!(
        Storage::new(fixture.dir.path()).load_instances().unwrap(),
        vec![]
    );

    fixture.finish().await;
}

#[tokio::test]
async fn unknown_instances_get_the_exact_oracle_error() {
    let fixture = Fixture::start().await;
    let expected = OrchestratorResponse::error("Unknown instance: nope");

    let requests = [
        OrchestratorRequest::Status {
            instance_id: "nope".into(),
        },
        OrchestratorRequest::Stop {
            instance_id: "nope".into(),
        },
        OrchestratorRequest::Rpc {
            instance_id: "nope".into(),
            command: rpc_command(json!({"type":"echo"})),
        },
        OrchestratorRequest::RpcStream {
            instance_id: "nope".into(),
        },
    ];
    for request in requests {
        assert_eq!(fixture.request(request).await, expected);
    }

    fixture.finish().await;
}

#[tokio::test]
async fn failed_spawn_returns_an_error_response() {
    let fixture = Fixture::start_with_child(Some((
        PathBuf::from("python3"),
        vec!["-c".into(), "import sys; sys.exit(3)".into()],
    )))
    .await;

    let spawn = fixture
        .request(OrchestratorRequest::Spawn {
            cwd: fixture.spawn_cwd(),
            label: None,
            provider: None,
            model: None,
        })
        .await;
    let OrchestratorResponse::Error { ok: false, error } = spawn else {
        panic!("unexpected spawn response: {spawn:?}");
    };
    assert!(error.contains("RPC process"), "{error}");

    fixture.finish().await;
}

#[tokio::test]
async fn rpc_stream_forwards_events_ui_requests_and_responses() {
    let fixture = Fixture::start().await;
    let instance_id = fixture.spawn().await;

    let (ready, mut stream) = connect_rpc_stream_to(&fixture.socket_path, &instance_id)
        .await
        .unwrap();
    assert_eq!(ready.id, instance_id);
    assert_eq!(ready.status, InstanceStatus::Online);

    // emit: the child writes an agent_event and then the command response.
    // Total child stdout order is preserved on the socket, so the event
    // always precedes the response.
    stream
        .send(&json!({"type":"emit","id":"emit-1","value":"ping"}))
        .await
        .unwrap();
    let first = stream.next_message().await.unwrap().unwrap();
    assert_eq!(first, json!({"type":"agent_event","value":"ping"}));
    let second = stream.next_message().await.unwrap().unwrap();
    assert_eq!(
        second,
        json!({"type":"response","id":"emit-1","command":"emit","success":true})
    );

    // ui: the child raises an extension_ui_request before the command
    // response; the stream preserves that order. The client answers with
    // extension_ui_response which is written to the child's stdin verbatim
    // (observed as a ui_observed event).
    stream
        .send(&json!({"type":"ui","id":"ui-cmd"}))
        .await
        .unwrap();
    let request = stream.next_message().await.unwrap().unwrap();
    assert_eq!(
        request,
        json!({"type":"extension_ui_request","id":"ui-1","method":"select"})
    );
    let response = stream.next_message().await.unwrap().unwrap();
    assert_eq!(response["type"], json!("response"));
    assert_eq!(response["id"], json!("ui-cmd"));

    stream
        .send(&json!({"type":"extension_ui_response","id":"ui-1","value":"picked"}))
        .await
        .unwrap();
    let observed = stream.next_message().await.unwrap().unwrap();
    assert_eq!(observed, json!({"type":"ui_observed","value":"picked"}));

    // Session-metadata commands on the stream refresh the record before the
    // response line is written.
    stream
        .send(&json!({"type":"new_session","id":"ns-1"}))
        .await
        .unwrap();
    let response = stream.next_message().await.unwrap().unwrap();
    assert_eq!(response["id"], json!("ns-1"));
    let status = fixture
        .request(OrchestratorRequest::Status {
            instance_id: instance_id.clone(),
        })
        .await;
    let OrchestratorResponse::StatusResult {
        instance: Some(refreshed),
        ..
    } = status
    else {
        panic!("unexpected status response: {status:?}");
    };
    assert_eq!(refreshed.session_id.as_deref(), Some("session-1"));

    fixture.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rpc_stream_preserves_child_emission_order_across_bursts() {
    let fixture = Fixture::start().await;
    let instance_id = fixture.spawn().await;

    let (_ready, mut stream) = connect_rpc_stream_to(&fixture.socket_path, &instance_id)
        .await
        .unwrap();

    // The child writes event -> ui request -> response in one stdout chunk.
    // Every iteration must observe exactly that order on the socket; repeats
    // turn scheduling races into deterministic failures.
    for iteration in 0..100 {
        let id = format!("burst-{iteration}");
        stream
            .send(&json!({"type":"burst","id":id,"value":iteration}))
            .await
            .unwrap();
        let event = stream.next_message().await.unwrap().unwrap();
        assert_eq!(
            event,
            json!({"type":"agent_event","value":iteration}),
            "iteration {iteration}: expected the agent_event first"
        );
        let request = stream.next_message().await.unwrap().unwrap();
        assert_eq!(
            request["type"],
            json!("extension_ui_request"),
            "iteration {iteration}: expected the ui request second, got {request}"
        );
        assert_eq!(request["value"], json!(iteration));
        let response = stream.next_message().await.unwrap().unwrap();
        assert_eq!(
            response,
            json!({"type":"response","id":id,"command":"burst","success":true,"data":iteration}),
            "iteration {iteration}: expected the response last"
        );
    }

    fixture.finish().await;
}
