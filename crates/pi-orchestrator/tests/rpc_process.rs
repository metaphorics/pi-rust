mod support {
    pub mod fake_pi;
}

use pi_orchestrator::rpc_process::RpcProcessInstance;
use pi_orchestrator::wire::RpcCommandEnvelope;
use serde_json::{Value, json};
use support::fake_pi::FakePi;

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
async fn fans_events_to_every_subscriber_and_routes_ui_to_one_owner() {
    let fake = FakePi::new();
    let process = RpcProcessInstance::spawn(fake.options()).unwrap();
    let (_sub_one, mut lane_one) = process.subscribe_output();
    let (sub_two, mut lane_two) = process.subscribe_output();
    process.claim_ui(sub_two);

    process
        .send(command(json!({ "type": "emit", "value": 42 })))
        .await
        .unwrap();
    assert_eq!(lane_one.recv().await.unwrap().1["value"], 42);
    assert_eq!(lane_two.recv().await.unwrap().1["value"], 42);

    process
        .send(command(json!({ "type": "ui" })))
        .await
        .unwrap();
    let (_, request) = lane_two.recv().await.unwrap();
    assert_eq!(request["type"], "extension_ui_request");
    assert_eq!(request["id"], "ui-1");

    process
        .handle_ui_response(&json!({
            "type": "extension_ui_response",
            "id": "ui-1",
            "value": "picked"
        }))
        .unwrap();
    assert_eq!(lane_one.recv().await.unwrap().1["value"], "picked");
    assert_eq!(lane_two.recv().await.unwrap().1["value"], "picked");
    // The ui request never reached the non-owning lane.
    assert!(lane_one.try_recv().is_err());

    process.dispose().await;
}

#[tokio::test]
async fn output_lane_preserves_child_order_and_barrier_fences_the_response() {
    let fake = FakePi::new();
    let process = RpcProcessInstance::spawn(fake.options()).unwrap();
    let (subscriber, mut lane) = process.subscribe_output();
    process.claim_ui(subscriber);

    // The child writes event -> ui request -> response in one stdout chunk.
    let (response, barrier) = process
        .send_claimed(
            command(json!({ "type": "burst", "id": "b-1", "value": 7 })),
            subscriber,
        )
        .await
        .unwrap();
    assert_eq!(response.id.as_deref(), Some("b-1"));
    assert_eq!(response.raw["data"], 7);

    let (event_seq, event) = lane.recv().await.unwrap();
    assert_eq!(event["type"], "agent_event");
    assert_eq!(event["value"], 7);
    let (request_seq, request) = lane.recv().await.unwrap();
    assert_eq!(request["type"], "extension_ui_request");
    assert_eq!(
        request_seq,
        event_seq + 1,
        "lane sequence must be contiguous"
    );
    assert_eq!(
        barrier, request_seq,
        "the barrier must cover everything the child emitted before the response"
    );

    process.dispose().await;
}

#[tokio::test]
async fn exit_rejects_every_pending_once_and_preserves_stderr() {
    let fake = FakePi::new();
    let process = RpcProcessInstance::spawn(fake.options()).unwrap();
    let mut exits = process.subscribe_exit().await;
    let (_subscriber, mut events) = process.subscribe_output();
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
    assert_eq!(events.recv().await.unwrap().1["type"], "pending_received");
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
    let (_subscriber, mut events) = process.subscribe_output();
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
    assert_eq!(events.recv().await.unwrap().1["type"], "pending_received");

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

#[cfg(unix)]
#[tokio::test]
async fn stdin_failure_tears_down_writer_while_child_stays_alive() {
    use rustix::process::{Pid, Signal, kill_process};

    struct KillOnDrop(Pid);

    impl Drop for KillOnDrop {
        fn drop(&mut self) {
            let _ = kill_process(self.0, Signal::KILL);
        }
    }

    let fake = FakePi::new();
    let process = RpcProcessInstance::spawn(fake.options()).unwrap();
    let mut exits = process.subscribe_exit().await;
    let (_subscriber, mut events) = process.subscribe_output();

    let pending_process = process.clone();
    let pending = tokio::spawn(async move {
        pending_process
            .send(command(json!({ "type": "pending" })))
            .await
            .unwrap_err()
    });
    assert_eq!(events.recv().await.unwrap().1["type"], "pending_received");

    process
        .send(command(json!({ "type": "close_stdin" })))
        .await
        .unwrap();
    let pid = events.recv().await.unwrap().1["value"].as_i64().unwrap() as i32;
    let _kill_child = KillOnDrop(Pid::from_raw(pid).unwrap());

    let failed = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        process.send(command(json!({ "type": "echo" }))),
    )
    .await
    .expect("stdin write failure did not reject the failed request")
    .unwrap_err();
    assert_eq!(failed.to_string(), "Broken pipe (os error 32)");

    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while !process.has_exited() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("writer failure did not publish teardown while child remained alive");

    let pending = tokio::time::timeout(std::time::Duration::from_secs(1), pending)
        .await
        .expect("stdin write failure did not reject an already-pending request")
        .unwrap();
    assert_eq!(pending, failed);
    assert_eq!(exits.recv().await.unwrap(), failed);
    assert!(exits.try_recv().is_err());

    tokio::time::timeout(std::time::Duration::from_millis(100), process.dispose())
        .await
        .expect("dispose waited for the still-alive child after writer teardown");
}
