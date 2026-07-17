#![cfg(unix)]

mod support {
    pub mod fake_pi;
}

use std::sync::Arc;
use std::sync::Weak;
use std::time::Duration;

use async_trait::async_trait;
use pi_orchestrator::ipc::{OrchestratorRequest, OrchestratorResponse, send_ipc_request_to};
use pi_orchestrator::radius::{Presence, PresenceCoordinator, RadiusError};
use pi_orchestrator::serve::{ServeError, ServeOptions, start};
use pi_orchestrator::storage::Storage;
use pi_orchestrator::supervisor::NullPresence;
use pi_orchestrator::types::{InstanceRecord, InstanceStatus, MachineRecord, now_iso_timestamp};
use support::fake_pi::FakePi;

fn serve_options(dir: &tempfile::TempDir, fake: &FakePi) -> ServeOptions {
    ServeOptions {
        socket_path: dir.path().join("orchestrator.sock"),
        storage: Storage::new(dir.path()),
        presence: Arc::new(NullPresence),
        label: None,
        spawn_command_override: fake.options().command_override,
    }
}

fn stored_record(id: &str, status: InstanceStatus) -> InstanceRecord {
    InstanceRecord {
        id: id.into(),
        status,
        cwd: "/work".into(),
        created_at: now_iso_timestamp(),
        last_seen_at: None,
        label: None,
        session_id: None,
        session_file: None,
        radius_pi_id: None,
    }
}

struct FailingStartPresence;

#[async_trait]
impl Presence for FailingStartPresence {
    fn set_coordinator(&self, _coordinator: Weak<dyn PresenceCoordinator>) {}

    async fn start(&self, _label: Option<String>) -> Result<Option<MachineRecord>, RadiusError> {
        Err(RadiusError::MissingCredentials)
    }

    async fn stop(&self) -> Result<(), RadiusError> {
        Ok(())
    }

    async fn register_pi(&self, instance: InstanceRecord) -> Result<InstanceRecord, RadiusError> {
        Ok(instance)
    }

    async fn disconnect_pi(&self, _instance: &InstanceRecord) -> Result<(), RadiusError> {
        Ok(())
    }
}

#[tokio::test]
async fn start_recovers_stale_records_and_serves_requests() {
    let dir = tempfile::tempdir().unwrap();
    let fake = FakePi::new();
    Storage::new(dir.path())
        .save_instances(&[
            stored_record("stale-online", InstanceStatus::Online),
            stored_record("old-error", InstanceStatus::Error),
        ])
        .unwrap();

    let mut running = start(serve_options(&dir, &fake)).await.unwrap();
    assert!(running.socket_path().exists());

    // recoverAfterRestart ran before the first request: online -> stopped,
    // error kept.
    let status = send_ipc_request_to(
        running.socket_path(),
        &OrchestratorRequest::Status {
            instance_id: "stale-online".into(),
        },
    )
    .await
    .unwrap();
    let OrchestratorResponse::StatusResult {
        ok: true,
        instance: Some(instance),
        ..
    } = status
    else {
        panic!("unexpected status response: {status:?}");
    };
    assert_eq!(instance.status, InstanceStatus::Stopped);
    let stored = Storage::new(dir.path()).load_instances().unwrap();
    assert_eq!(stored[1].status, InstanceStatus::Error);

    running.shutdown().await.unwrap();
}

#[tokio::test]
async fn shutdown_stops_children_unlinks_socket_and_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let fake = FakePi::new();
    let sigterm_marker = fake.options().cwd.join("sigterm.marker");

    let mut running = start(serve_options(&dir, &fake)).await.unwrap();
    let spawn = send_ipc_request_to(
        running.socket_path(),
        &OrchestratorRequest::Spawn {
            cwd: fake.options().cwd.to_string_lossy().into_owned(),
            label: None,
            provider: None,
            model: None,
        },
    )
    .await
    .unwrap();
    let OrchestratorResponse::SpawnResult {
        ok: true,
        instance: Some(instance),
        ..
    } = spawn
    else {
        panic!("unexpected spawn response: {spawn:?}");
    };
    assert_eq!(running.supervisor().list_live_instances().len(), 1);

    running.shutdown().await.unwrap();

    assert!(!running.socket_path().exists());
    assert!(sigterm_marker.exists(), "child was not SIGTERMed");
    assert!(running.supervisor().list_live_instances().is_empty());
    assert!(
        Storage::new(dir.path())
            .get_instance(&instance.id)
            .unwrap()
            .is_none()
    );
    // Second shutdown is a no-op.
    running.shutdown().await.unwrap();

    // The socket is gone: clients get a connection error.
    let error = send_ipc_request_to(running.socket_path(), &OrchestratorRequest::List)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("No such file"), "{error}");
}

#[tokio::test]
async fn startup_failure_closes_the_server_and_unlinks_the_socket() {
    let dir = tempfile::tempdir().unwrap();
    let fake = FakePi::new();
    let mut options = serve_options(&dir, &fake);
    options.presence = Arc::new(FailingStartPresence);

    let error = match start(options).await {
        Ok(_) => panic!("start unexpectedly succeeded"),
        Err(error) => error,
    };
    assert!(matches!(error, ServeError::Radius(_)));
    assert!(!dir.path().join("orchestrator.sock").exists());
}

#[tokio::test]
async fn one_server_per_socket() {
    let dir = tempfile::tempdir().unwrap();
    let fake = FakePi::new();
    let mut running = start(serve_options(&dir, &fake)).await.unwrap();

    let error = match start(serve_options(&dir, &fake)).await {
        Ok(_) => panic!("second serve unexpectedly started"),
        Err(error) => error,
    };
    assert_eq!(
        error.to_string(),
        format!(
            "orchestrator is already running: {}",
            running.socket_path().display()
        )
    );
    // The refused second server did not clobber the live socket.
    let response = send_ipc_request_to(running.socket_path(), &OrchestratorRequest::List)
        .await
        .unwrap();
    assert!(matches!(
        response,
        OrchestratorResponse::ListResult { ok: true, .. }
    ));

    running.shutdown().await.unwrap();

    // With the stale socket file left behind by a crash, a new serve boots.
    std::fs::write(dir.path().join("orchestrator.sock"), b"").ok();
    let listener =
        std::os::unix::net::UnixListener::bind(dir.path().join("orchestrator-stale.sock"));
    drop(listener);
    let mut running = start(serve_options(&dir, &fake)).await.unwrap();
    running.shutdown().await.unwrap();
}

#[tokio::test]
async fn shutdown_waits_for_child_exit() {
    let dir = tempfile::tempdir().unwrap();
    let fake = FakePi::new();
    let mut running = start(serve_options(&dir, &fake)).await.unwrap();
    send_ipc_request_to(
        running.socket_path(),
        &OrchestratorRequest::Spawn {
            cwd: fake.options().cwd.to_string_lossy().into_owned(),
            label: None,
            provider: None,
            model: None,
        },
    )
    .await
    .unwrap();

    tokio::time::timeout(Duration::from_secs(5), running.shutdown())
        .await
        .expect("shutdown timed out")
        .unwrap();
}
