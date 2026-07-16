mod support {
    pub mod fake_pi;
}

use pi_orchestrator::rpc_process::RpcProcessInstance;
use pi_orchestrator::wire::RpcCommandEnvelope;
use serde_json::{Value, json};
use support::fake_pi::FakePi;
use tokio::sync::mpsc;

fn command(value: Value) -> RpcCommandEnvelope {
    RpcCommandEnvelope::try_from(value).unwrap()
}

async fn wait_for_stderr(process: &RpcProcessInstance, expected: &str) {
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            if process.stderr().await.contains(expected) {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn correlates_out_of_order_responses_and_generates_pi_ids() {
    let fake = FakePi::new();
    let process = RpcProcessInstance::spawn(fake.options()).unwrap();
    let response = process
        .send(command(json!({
            "type": "echo",
            "id": "client-id",
            "value": "kept"
        })))
        .await
        .unwrap();
    assert_eq!(response.id.as_deref(), Some("client-id"));
    assert_eq!(response.raw["data"], "kept");

    let first = process.clone();
    let first = tokio::spawn(async move {
        first
            .send(command(json!({ "type": "pair", "value": 1 })))
            .await
            .unwrap()
    });
    let second = process.clone();
    let second = tokio::spawn(async move {
        second
            .send(command(json!({ "type": "pair", "value": 2 })))
            .await
            .unwrap()
    });

    let first = first.await.unwrap();
    let second = second.await.unwrap();
    assert_eq!(first.raw["data"], 1);
    assert_eq!(second.raw["data"], 2);
    for id in [first.id.unwrap(), second.id.unwrap()] {
        let suffix = id.strip_prefix("orchestrator_").unwrap();
        let (sequence, uuid) = suffix.split_once('_').unwrap();
        assert!(matches!(sequence, "1" | "2"));
        assert!(uuid::Uuid::parse_str(uuid).is_ok());
    }

    process.dispose().await;
}

#[tokio::test]
async fn fans_events_to_every_subscriber_and_routes_ui_to_one_handler() {
    let fake = FakePi::new();
    let process = RpcProcessInstance::spawn(fake.options()).unwrap();
    let mut events_one = process.subscribe_events().await;
    let mut events_two = process.subscribe_events().await;
    let (ui_sender, mut ui_requests) = mpsc::unbounded_channel();
    process.set_ui_request_handler(Some(ui_sender)).await;

    process
        .send(command(json!({ "type": "emit", "value": 42 })))
        .await
        .unwrap();
    assert_eq!(events_one.recv().await.unwrap()["value"], 42);
    assert_eq!(events_two.recv().await.unwrap()["value"], 42);

    process
        .send(command(json!({ "type": "ui" })))
        .await
        .unwrap();
    let request = ui_requests.recv().await.unwrap();
    assert_eq!(request["type"], "extension_ui_request");
    assert_eq!(request["id"], "ui-1");

    process
        .handle_ui_response(&json!({
            "type": "extension_ui_response",
            "id": "ui-1",
            "value": "picked"
        }))
        .unwrap();
    assert_eq!(events_one.recv().await.unwrap()["value"], "picked");
    assert_eq!(events_two.recv().await.unwrap()["value"], "picked");

    process.dispose().await;
}

#[tokio::test]
async fn exit_rejects_every_pending_once_and_preserves_stderr() {
    let fake = FakePi::new();
    let process = RpcProcessInstance::spawn(fake.options()).unwrap();
    let mut exits = process.subscribe_exit().await;
    let mut events = process.subscribe_events().await;
    process
        .send(command(
            json!({ "type": "stderr", "value": "child failed" }),
        ))
        .await
        .unwrap();
    wait_for_stderr(&process, "child failed").await;

    let pending = process.clone();
    let pending = tokio::spawn(async move {
        pending
            .send(command(json!({ "type": "pending" })))
            .await
            .unwrap_err()
    });
    assert_eq!(events.recv().await.unwrap()["type"], "pending_received");
    let exiting = process.clone();
    let exiting = tokio::spawn(async move {
        exiting
            .send(command(json!({
                "type": "exit",
                "code": 7
            })))
            .await
            .unwrap_err()
    });

    let pending_error = pending.await.unwrap();
    let exiting_error = exiting.await.unwrap();
    let expected = "RPC process exited (code=7 signal=null). Stderr: child failed";
    assert_eq!(pending_error.to_string(), expected);
    assert_eq!(exiting_error.to_string(), expected);
    assert_eq!(exits.recv().await.unwrap().to_string(), expected);
    assert!(exits.try_recv().is_err());

    let after_exit = process
        .send(command(json!({ "type": "echo" })))
        .await
        .unwrap_err();
    assert_eq!(
        after_exit.to_string(),
        "RPC process is not running. Stderr: child failed"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn dispose_rejects_pending_then_sends_sigterm_and_awaits_exit() {
    let fake = FakePi::new();
    let process = RpcProcessInstance::spawn(fake.options()).unwrap();
    let mut exits = process.subscribe_exit().await;
    let mut events = process.subscribe_events().await;
    process
        .send(command(
            json!({ "type": "stderr", "value": "child stopping" }),
        ))
        .await
        .unwrap();
    wait_for_stderr(&process, "child stopping").await;

    let pending = process.clone();
    let pending = tokio::spawn(async move {
        pending
            .send(command(json!({ "type": "pending" })))
            .await
            .unwrap_err()
    });
    assert_eq!(events.recv().await.unwrap()["type"], "pending_received");

    process.dispose().await;
    assert_eq!(pending.await.unwrap().to_string(), "RPC process disposed");
    assert!(process.has_exited());
    wait_for_stderr(&process, "received SIGTERM").await;
    assert_eq!(process.stderr().await, "child stoppingreceived SIGTERM");
    assert!(
        exits
            .recv()
            .await
            .unwrap()
            .to_string()
            .starts_with("RPC process exited (code=null signal=SIGTERM). Stderr: child stopping")
    );
}
