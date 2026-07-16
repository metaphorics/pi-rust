mod support {
    pub mod fake_pi;
}

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use parking_lot::Mutex;
use pi_orchestrator::radius::{Presence, PresenceCoordinator, RadiusError};
use pi_orchestrator::storage::{Storage, StorageOp};
use pi_orchestrator::supervisor::{NullPresence, SpawnOptions, Supervisor, SupervisorError};
use pi_orchestrator::types::{InstanceRecord, InstanceStatus, MachineRecord};
use pi_orchestrator::wire::RpcCommandEnvelope;
use serde_json::{Value, json};
use support::fake_pi::FakePi;

const SESSION_METADATA_COMMANDS: [&str; 6] = [
    "new_session",
    "switch_session",
    "fork",
    "clone",
    "set_session_name",
    "prompt",
];

type RegisterHook =
    Box<dyn Fn(InstanceRecord) -> Result<InstanceRecord, RadiusError> + Send + Sync>;
type DisconnectHook = Box<dyn Fn(&InstanceRecord) -> Result<(), RadiusError> + Send + Sync>;

#[derive(Clone, Default)]
struct PresenceLog(Arc<Mutex<Vec<String>>>);

impl PresenceLog {
    fn push(&self, entry: String) {
        self.0.lock().push(entry);
    }

    fn entries(&self) -> Vec<String> {
        self.0.lock().clone()
    }
}

struct TestPresence {
    log: PresenceLog,
    coordinator: Mutex<Option<Weak<dyn PresenceCoordinator>>>,
    register: RegisterHook,
    disconnect: DisconnectHook,
}

impl TestPresence {
    fn with_hooks(register: RegisterHook, disconnect: DisconnectHook) -> (Arc<Self>, PresenceLog) {
        let log = PresenceLog::default();
        let presence = Arc::new(Self {
            log: log.clone(),
            coordinator: Mutex::new(None),
            register,
            disconnect,
        });
        (presence, log)
    }

    fn assigning(radius_pi_id: &str) -> (Arc<Self>, PresenceLog) {
        let radius_pi_id = radius_pi_id.to_owned();
        Self::with_hooks(
            Box::new(move |mut instance| {
                instance.radius_pi_id = Some(radius_pi_id.clone());
                Ok(instance)
            }),
            Box::new(|_| Ok(())),
        )
    }
}

#[async_trait::async_trait]
impl Presence for TestPresence {
    fn set_coordinator(&self, coordinator: Weak<dyn PresenceCoordinator>) {
        *self.coordinator.lock() = Some(coordinator);
    }

    async fn start(&self, _label: Option<String>) -> Result<Option<MachineRecord>, RadiusError> {
        Ok(None)
    }

    async fn stop(&self) -> Result<(), RadiusError> {
        Ok(())
    }

    async fn register_pi(&self, instance: InstanceRecord) -> Result<InstanceRecord, RadiusError> {
        self.log.push(format!(
            "register:{}:{}",
            instance.id,
            instance.session_id.as_deref().unwrap_or("-")
        ));
        (self.register)(instance)
    }

    /// Bracket the disconnect with start/end log entries around yield
    /// points so tests can prove calls never overlap (sequential shutdown
    /// and recovery).
    async fn disconnect_pi(&self, instance: &InstanceRecord) -> Result<(), RadiusError> {
        self.log.push(format!("disconnect-start:{}", instance.id));
        for _ in 0..4 {
            tokio::task::yield_now().await;
        }
        let result = (self.disconnect)(instance);
        self.log.push(format!(
            "disconnect:{}:{}",
            instance.id,
            instance.radius_pi_id.as_deref().unwrap_or("-")
        ));
        result
    }
}

fn command(value: Value) -> RpcCommandEnvelope {
    RpcCommandEnvelope::try_from(value).unwrap()
}

fn spawn_options(fake: &FakePi) -> SpawnOptions {
    let options = fake.options();
    SpawnOptions {
        cwd: options.cwd.to_string_lossy().into_owned(),
        label: None,
        command_override: options.command_override,
    }
}

fn exiting_child_options(cwd: &Path) -> SpawnOptions {
    SpawnOptions {
        cwd: cwd.to_string_lossy().into_owned(),
        label: None,
        command_override: Some((
            PathBuf::from("python3"),
            vec!["-c".into(), "import sys; sys.exit(3)".into()],
        )),
    }
}

async fn wait_until(mut predicate: impl FnMut() -> bool, what: &str) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while !predicate() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
}

#[tokio::test]
async fn spawn_reaches_online_after_session_sync_and_radius_registration() {
    let dir = tempfile::tempdir().unwrap();
    let probe = Storage::new(dir.path());
    let (presence, log) = TestPresence::assigning("radius-1");
    let supervisor = Supervisor::new(Storage::new(dir.path()), presence.clone());
    let fake = FakePi::new();

    let record = supervisor
        .spawn_instance(spawn_options(&fake))
        .await
        .unwrap();

    assert_eq!(record.status, InstanceStatus::Online);
    assert_eq!(record.cwd, spawn_options(&fake).cwd);
    assert_eq!(record.session_id.as_deref(), Some("session-0"));
    assert_eq!(record.session_file.as_deref(), Some("/tmp/session-0.jsonl"));
    assert_eq!(record.radius_pi_id.as_deref(), Some("radius-1"));
    assert!(record.last_seen_at.is_some());
    // Registration saw the already-synced record: get_state ran first.
    assert_eq!(log.entries(), [format!("register:{}:session-0", record.id)]);
    // Live view and persisted record agree.
    assert_eq!(
        supervisor.get_live_instance(&record.id),
        Some(record.clone())
    );
    assert_eq!(
        probe.get_instance(&record.id).unwrap(),
        Some(record.clone())
    );

    // The wired coordinator reaches back into the supervisor.
    let coordinator = presence.coordinator.lock().clone().unwrap();
    let coordinator = coordinator.upgrade().unwrap();
    let mut updated = record.clone();
    updated.label = Some("renamed".into());
    coordinator.update_instance(updated.clone()).await;
    assert_eq!(
        supervisor.get_live_instance(&record.id),
        Some(updated.clone())
    );
    assert_eq!(probe.get_instance(&record.id).unwrap(), Some(updated));

    supervisor.shutdown().await.unwrap();
}

#[tokio::test]
async fn spawn_failure_persists_stopped_record_and_clears_live_entry() {
    let dir = tempfile::tempdir().unwrap();
    let probe = Storage::new(dir.path());
    let (presence, log) = TestPresence::assigning("radius-1");
    let supervisor = Supervisor::new(Storage::new(dir.path()), presence);

    supervisor
        .spawn_instance(exiting_child_options(dir.path()))
        .await
        .unwrap_err();

    // failSpawn keeps the record in instances.json as `stopped`.
    let stored = probe.load_instances().unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].status, InstanceStatus::Stopped);
    assert!(supervisor.list_live_instances().is_empty());
    // The failure happened before registration; Radius was never touched.
    assert_eq!(log.entries(), Vec::<String>::new());
}

#[tokio::test]
async fn registration_failure_fails_spawn_and_disposes_the_child() {
    let dir = tempfile::tempdir().unwrap();
    let probe = Storage::new(dir.path());
    let (presence, log) = TestPresence::with_hooks(
        Box::new(|_| Err(RadiusError::MissingCredentials)),
        Box::new(|_| Ok(())),
    );
    let supervisor = Supervisor::new(Storage::new(dir.path()), presence);
    let fake = FakePi::new();
    let marker = fake.options().cwd.join("sigterm.marker");

    let error = supervisor
        .spawn_instance(spawn_options(&fake))
        .await
        .unwrap_err();

    assert!(
        error
            .to_string()
            .contains("Radius credentials are required"),
        "unexpected error: {error}"
    );
    let stored = probe.load_instances().unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].status, InstanceStatus::Stopped);
    assert!(supervisor.list_live_instances().is_empty());
    // No radiusPiId was acquired, so cleanup never disconnected.
    assert_eq!(
        log.entries(),
        [format!("register:{}:session-0", stored[0].id)]
    );
    // Cleanup disposed the child (SIGTERM observed before failSpawn returned).
    assert!(marker.exists(), "failSpawn did not dispose the child");
}

#[tokio::test]
async fn stop_disconnects_radius_before_disposing_child_and_removes_record() {
    let dir = tempfile::tempdir().unwrap();
    let probe = Storage::new(dir.path());
    let fake = FakePi::new();
    let marker = fake.options().cwd.join("sigterm.marker");
    let disconnect_marker = marker.clone();
    let (presence, log) = TestPresence::with_hooks(
        Box::new(|mut instance| {
            instance.radius_pi_id = Some("radius-1".into());
            Ok(instance)
        }),
        Box::new(move |_| {
            assert!(
                !disconnect_marker.exists(),
                "child was disposed before the Radius disconnect"
            );
            Ok(())
        }),
    );
    let supervisor = Supervisor::new(Storage::new(dir.path()), presence);

    let record = supervisor
        .spawn_instance(spawn_options(&fake))
        .await
        .unwrap();
    let stopped = supervisor.stop_instance(&record.id).await.unwrap().unwrap();

    assert_eq!(stopped.status, InstanceStatus::Stopped);
    assert_eq!(stopped.id, record.id);
    // Unlike failSpawn, stop removes the persisted record entirely.
    assert_eq!(probe.load_instances().unwrap(), []);
    assert!(supervisor.list_live_instances().is_empty());
    assert_eq!(supervisor.get_instance(&record.id).unwrap(), None);
    assert!(marker.exists(), "stop did not dispose the child");
    assert_eq!(
        log.entries(),
        [
            format!("register:{}:session-0", record.id),
            format!("disconnect-start:{}", record.id),
            format!("disconnect:{}:radius-1", record.id),
        ]
    );

    // Stopping again is a no-op on an unknown instance.
    assert_eq!(supervisor.stop_instance(&record.id).await.unwrap(), None);
}

#[tokio::test]
async fn unexpected_child_exit_marks_error_disconnects_and_keeps_stored_record() {
    let dir = tempfile::tempdir().unwrap();
    let probe = Storage::new(dir.path());
    let (presence, log) = TestPresence::assigning("radius-1");
    let supervisor = Supervisor::new(Storage::new(dir.path()), presence);
    let fake = FakePi::new();

    let record = supervisor
        .spawn_instance(spawn_options(&fake))
        .await
        .unwrap();
    supervisor
        .handle_rpc(&record.id, command(json!({ "type": "crash" })))
        .await
        .unwrap_err();

    let supervisor_probe = supervisor.clone();
    wait_until(
        || supervisor_probe.list_live_instances().is_empty(),
        "unexpected exit to clear the live entry",
    )
    .await;

    // The record survives on disk as `error` with its Radius binding cleared.
    let stored = probe.get_instance(&record.id).unwrap().unwrap();
    assert_eq!(stored.status, InstanceStatus::Error);
    assert_eq!(stored.radius_pi_id, None);
    assert_eq!(stored.session_id.as_deref(), Some("session-0"));
    assert_eq!(
        log.entries(),
        [
            format!("register:{}:session-0", record.id),
            format!("disconnect-start:{}", record.id),
            format!("disconnect:{}:radius-1", record.id),
        ]
    );
    // Dead instances resolve from storage, and nothing restarted the child.
    assert_eq!(supervisor.get_instance(&record.id).unwrap(), Some(stored));
    assert!(supervisor.list_live_instances().is_empty());
}

#[tokio::test]
async fn metadata_commands_refresh_the_session_before_returning() {
    let dir = tempfile::tempdir().unwrap();
    let probe = Storage::new(dir.path());
    let supervisor = Supervisor::new(Storage::new(dir.path()), Arc::new(NullPresence));
    let fake = FakePi::new();
    let record = supervisor
        .spawn_instance(spawn_options(&fake))
        .await
        .unwrap();

    for (index, kind) in SESSION_METADATA_COMMANDS.into_iter().enumerate() {
        let response = supervisor
            .handle_rpc(&record.id, command(json!({ "type": kind })))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(response.command.as_deref(), Some(kind));
        // The refresh completed before handle_rpc returned.
        let session = format!("session-{}", index + 1);
        let stored = probe.get_instance(&record.id).unwrap().unwrap();
        assert_eq!(
            stored.session_id.as_deref(),
            Some(session.as_str()),
            "{kind}"
        );
        assert_eq!(
            stored.session_file.as_deref(),
            Some(format!("/tmp/{session}.jsonl").as_str())
        );
    }

    // Non-member commands advance child state without a refresh...
    supervisor
        .handle_rpc(&record.id, command(json!({ "type": "advance" })))
        .await
        .unwrap()
        .unwrap();
    // ...including get_state itself, whose response proves the drift.
    let response = supervisor
        .handle_rpc(&record.id, command(json!({ "type": "get_state" })))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(response.raw["data"]["sessionId"], "session-7");
    let stored = probe.get_instance(&record.id).unwrap().unwrap();
    assert_eq!(stored.session_id.as_deref(), Some("session-6"));

    // Unknown instances yield no response envelope at all.
    assert!(
        supervisor
            .handle_rpc("missing", command(json!({ "type": "get_state" })))
            .await
            .unwrap()
            .is_none()
    );

    supervisor.shutdown().await.unwrap();
}

#[tokio::test]
async fn rpc_streams_fan_out_events_and_the_last_ui_handler_wins() {
    let dir = tempfile::tempdir().unwrap();
    let probe = Storage::new(dir.path());
    let supervisor = Supervisor::new(Storage::new(dir.path()), Arc::new(NullPresence));
    let fake = FakePi::new();
    let record = supervisor
        .spawn_instance(spawn_options(&fake))
        .await
        .unwrap();

    assert!(supervisor.open_rpc_stream("missing").is_none());
    let mut stream_a = supervisor.open_rpc_stream(&record.id).unwrap();
    let mut stream_b = supervisor.open_rpc_stream(&record.id).unwrap();

    // Events fan out to every open stream.
    stream_b
        .handle_rpc(command(json!({ "type": "emit", "value": 42 })))
        .await
        .unwrap();
    assert_eq!(stream_a.events.recv().await.unwrap()["value"], 42);
    assert_eq!(stream_b.events.recv().await.unwrap()["value"], 42);

    // UI requests reach only the most recently opened stream.
    stream_a
        .handle_rpc(command(json!({ "type": "ui" })))
        .await
        .unwrap();
    assert_eq!(stream_b.ui_requests.recv().await.unwrap()["id"], "ui-1");
    assert!(stream_a.ui_requests.try_recv().is_err());

    // UI responses flow back through the child and out as events.
    stream_b
        .handle_ui_response(&json!({
            "type": "extension_ui_response",
            "id": "ui-1",
            "value": "picked"
        }))
        .unwrap();
    assert_eq!(stream_a.events.recv().await.unwrap()["value"], "picked");
    assert_eq!(stream_b.events.recv().await.unwrap()["value"], "picked");

    // Closing a non-owner leaves the UI slot with its owner.
    stream_a.close();
    stream_b
        .handle_rpc(command(json!({ "type": "ui" })))
        .await
        .unwrap();
    assert_eq!(stream_b.ui_requests.recv().await.unwrap()["id"], "ui-1");

    // Closing the owner releases the slot; the next stream claims it.
    stream_b.close();
    let mut stream_c = supervisor.open_rpc_stream(&record.id).unwrap();
    stream_c
        .handle_rpc(command(json!({ "type": "ui" })))
        .await
        .unwrap();
    assert_eq!(stream_c.ui_requests.recv().await.unwrap()["id"], "ui-1");

    // Stream commands hit the same metadata-refresh path.
    stream_c
        .handle_rpc(command(json!({ "type": "new_session" })))
        .await
        .unwrap();
    assert_eq!(
        probe
            .get_instance(&record.id)
            .unwrap()
            .unwrap()
            .session_id
            .as_deref(),
        Some("session-1")
    );

    stream_c.close();
    supervisor.shutdown().await.unwrap();
    assert!(supervisor.open_rpc_stream(&record.id).is_none());
}

#[tokio::test]
async fn recover_after_restart_maps_statuses_and_disconnects_every_record() {
    let dir = tempfile::tempdir().unwrap();
    let probe = Storage::new(dir.path());
    let statuses = [
        ("a", InstanceStatus::Online, Some("radius-a")),
        ("b", InstanceStatus::Starting, None),
        ("c", InstanceStatus::Stopping, None),
        ("d", InstanceStatus::Stopped, Some("radius-d")),
        ("e", InstanceStatus::Error, None),
    ];
    let seeded: Vec<InstanceRecord> = statuses
        .iter()
        .map(|(id, status, radius_pi_id)| InstanceRecord {
            id: (*id).into(),
            status: *status,
            cwd: "/work".into(),
            created_at: "2025-12-09T00:53:29.825Z".into(),
            last_seen_at: Some("2025-12-09T00:53:29.825Z".into()),
            label: None,
            session_id: None,
            session_file: None,
            radius_pi_id: radius_pi_id.map(Into::into),
        })
        .collect();
    probe.save_instances(&seeded).unwrap();

    let (presence, log) = TestPresence::with_hooks(Box::new(Ok), Box::new(|_| Ok(())));
    let supervisor = Supervisor::new(Storage::new(dir.path()), presence);
    supervisor.recover_after_restart().await.unwrap();

    let recovered = probe.load_instances().unwrap();
    assert_eq!(recovered.len(), 5);
    let expected = [
        InstanceStatus::Stopped,  // online -> stopped
        InstanceStatus::Stopped,  // starting -> stopped
        InstanceStatus::Stopping, // preserved
        InstanceStatus::Stopped,  // preserved
        InstanceStatus::Error,    // preserved
    ];
    let recovered_at = recovered[0].last_seen_at.clone().unwrap();
    assert_ne!(recovered_at, "2025-12-09T00:53:29.825Z");
    for (record, expected_status) in recovered.iter().zip(expected) {
        assert_eq!(record.status, expected_status, "{}", record.id);
        assert_eq!(record.last_seen_at.as_deref(), Some(recovered_at.as_str()));
    }
    // Stored radiusPiId values are intentionally preserved.
    assert_eq!(recovered[0].radius_pi_id.as_deref(), Some("radius-a"));
    assert_eq!(recovered[3].radius_pi_id.as_deref(), Some("radius-d"));
    // Every record was disconnected, radius-bound or not, one at a time.
    assert_eq!(
        log.entries(),
        [
            "disconnect-start:a",
            "disconnect:a:radius-a",
            "disconnect-start:b",
            "disconnect:b:-",
            "disconnect-start:c",
            "disconnect:c:-",
            "disconnect-start:d",
            "disconnect:d:radius-d",
            "disconnect-start:e",
            "disconnect:e:-",
        ]
    );
}

#[tokio::test]
async fn shutdown_stops_every_live_instance() {
    let dir = tempfile::tempdir().unwrap();
    let probe = Storage::new(dir.path());
    let (presence, log) = TestPresence::assigning("radius-1");
    let supervisor = Supervisor::new(Storage::new(dir.path()), presence);
    let first = FakePi::new();
    let second = FakePi::new();

    let first_record = supervisor
        .spawn_instance(spawn_options(&first))
        .await
        .unwrap();
    let second_record = supervisor
        .spawn_instance(spawn_options(&second))
        .await
        .unwrap();
    assert_eq!(supervisor.list_live_instances().len(), 2);

    supervisor.shutdown().await.unwrap();

    assert!(supervisor.list_live_instances().is_empty());
    assert_eq!(probe.load_instances().unwrap(), []);
    for (fake, record) in [(&first, &first_record), (&second, &second_record)] {
        assert!(
            fake.options().cwd.join("sigterm.marker").exists(),
            "shutdown did not dispose {}",
            record.id
        );
        assert!(
            log.entries()
                .contains(&format!("disconnect:{}:radius-1", record.id)),
            "shutdown did not disconnect {}",
            record.id
        );
    }
    // Stops ran one at a time: each disconnect finished (across its yield
    // points) before the next began, in either map order.
    let disconnects: Vec<String> = log
        .entries()
        .into_iter()
        .filter(|entry| entry.starts_with("disconnect"))
        .collect();
    assert_eq!(disconnects.len(), 4);
    for pair in disconnects.chunks(2) {
        let started = pair[0]
            .strip_prefix("disconnect-start:")
            .unwrap_or_else(|| panic!("interleaved disconnects: {disconnects:?}"));
        assert!(
            pair[1].starts_with(&format!("disconnect:{started}:")),
            "interleaved disconnects: {disconnects:?}"
        );
    }
}

#[tokio::test]
async fn stop_disposes_child_even_when_radius_disconnect_fails() {
    let dir = tempfile::tempdir().unwrap();
    let probe = Storage::new(dir.path());
    let fake = FakePi::new();
    let marker = fake.options().cwd.join("sigterm.marker");
    let (presence, log) = TestPresence::with_hooks(
        Box::new(|mut instance| {
            instance.radius_pi_id = Some("radius-1".into());
            Ok(instance)
        }),
        Box::new(|_| Err(RadiusError::MissingCredentials)),
    );
    let supervisor = Supervisor::new(Storage::new(dir.path()), presence);
    let record = supervisor
        .spawn_instance(spawn_options(&fake))
        .await
        .unwrap();

    let error = supervisor.stop_instance(&record.id).await.unwrap_err();

    // The disconnect failure is the primary error (removal succeeded), yet
    // the child was still disposed and the finally semantics completed.
    assert!(
        matches!(error, SupervisorError::Radius(_)),
        "expected the disconnect error, got: {error}"
    );
    assert!(
        marker.exists(),
        "disconnect failure skipped disposing the child"
    );
    assert_eq!(probe.load_instances().unwrap(), []);
    assert!(supervisor.list_live_instances().is_empty());
    assert!(
        log.entries()
            .contains(&format!("disconnect-start:{}", record.id)),
        "disconnect was never attempted"
    );
}

#[tokio::test]
async fn spawn_leaves_no_phantom_when_the_initial_persist_fails() {
    let dir = tempfile::tempdir().unwrap();
    let storage = Storage::new(dir.path()).with_fault_injection(|op| match op {
        StorageOp::UpsertInstance(_) => {
            Err(std::io::Error::other("injected initial persist").into())
        }
        _ => Ok(()),
    });
    let supervisor = Supervisor::new(storage, Arc::new(NullPresence));

    let error = supervisor
        .spawn_instance(exiting_child_options(dir.path()))
        .await
        .unwrap_err();

    // The persist failure surfaces unmasked: nothing was acquired yet.
    assert_eq!(error.to_string(), "injected initial persist");
    assert!(
        supervisor.list_live_instances().is_empty(),
        "failed initial persist left a phantom live entry"
    );
    assert_eq!(Storage::new(dir.path()).load_instances().unwrap(), []);
}

#[tokio::test]
async fn failed_record_removal_keeps_stop_retryable() {
    let dir = tempfile::tempdir().unwrap();
    let probe = Storage::new(dir.path());
    let fake = FakePi::new();
    let marker = fake.options().cwd.join("sigterm.marker");
    let failing = Arc::new(AtomicBool::new(true));
    let storage_flag = Arc::clone(&failing);
    let storage = Storage::new(dir.path()).with_fault_injection(move |op| match op {
        StorageOp::RemoveInstance(_) if storage_flag.load(Ordering::SeqCst) => {
            Err(std::io::Error::other("injected removal failure").into())
        }
        _ => Ok(()),
    });
    let presence_flag = Arc::clone(&failing);
    let (presence, log) = TestPresence::with_hooks(
        Box::new(|mut instance| {
            instance.radius_pi_id = Some("radius-1".into());
            Ok(instance)
        }),
        Box::new(move |_| {
            if presence_flag.load(Ordering::SeqCst) {
                Err(RadiusError::MissingCredentials)
            } else {
                Ok(())
            }
        }),
    );
    let supervisor = Supervisor::new(storage, presence);
    let record = supervisor
        .spawn_instance(spawn_options(&fake))
        .await
        .unwrap();

    let error = supervisor.stop_instance(&record.id).await.unwrap_err();

    // Removal failure is primary (oracle finally-throw supersedes); the
    // disconnect failure is the masked, logged secondary.
    assert_eq!(error.to_string(), "injected removal failure");
    // Teardown still ran to the end: the child is gone.
    assert!(marker.exists(), "failed stop leaked the child");
    // Retryable, coherent state: live entry kept and both the live and the
    // stored view still say `stopping` (the stop is incomplete), with the
    // radius registration still held.
    let lingering = supervisor.get_live_instance(&record.id).unwrap();
    assert_eq!(lingering.status, InstanceStatus::Stopping);
    let stored = probe.load_instances().unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].status, InstanceStatus::Stopping);

    failing.store(false, Ordering::SeqCst);
    let stopped = supervisor.stop_instance(&record.id).await.unwrap().unwrap();

    assert_eq!(stopped.status, InstanceStatus::Stopped);
    assert!(supervisor.list_live_instances().is_empty());
    assert_eq!(probe.load_instances().unwrap(), []);
    // The retry released the still-held radius registration: one failed and
    // one successful disconnect.
    let starts = log
        .entries()
        .iter()
        .filter(|entry| entry.starts_with("disconnect-start:"))
        .count();
    assert_eq!(starts, 2, "retry did not re-attempt the radius disconnect");
}

#[tokio::test]
async fn stop_tears_down_even_when_the_stopping_persist_fails() {
    let dir = tempfile::tempdir().unwrap();
    let probe = Storage::new(dir.path());
    let fake = FakePi::new();
    let marker = fake.options().cwd.join("sigterm.marker");
    let failing = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&failing);
    let storage = Storage::new(dir.path()).with_fault_injection(move |op| match op {
        StorageOp::UpsertInstance(_) if flag.load(Ordering::SeqCst) => {
            Err(std::io::Error::other("injected stopping persist").into())
        }
        _ => Ok(()),
    });
    let (presence, log) = TestPresence::assigning("radius-1");
    let supervisor = Supervisor::new(storage, presence);
    let record = supervisor
        .spawn_instance(spawn_options(&fake))
        .await
        .unwrap();
    failing.store(true, Ordering::SeqCst);

    let error = supervisor.stop_instance(&record.id).await.unwrap_err();

    // The `stopping` persist failure is primary: teardown and removal both
    // succeeded, so nothing supersedes it.
    assert_eq!(error.to_string(), "injected stopping persist");
    // Teardown was not bypassed: the radius registration was released and
    // the child disposed despite the failed persist.
    assert!(
        log.entries()
            .contains(&format!("disconnect:{}:radius-1", record.id)),
        "stop with a failing stopping persist skipped the radius disconnect"
    );
    assert!(
        marker.exists(),
        "stop with a failing stopping persist leaked the child"
    );
    // The stop still completed: live entry dropped, stored record removed.
    assert!(
        supervisor.list_live_instances().is_empty(),
        "live entry survived the stop"
    );
    assert_eq!(probe.load_instances().unwrap(), []);
}

#[tokio::test]
async fn fail_spawn_tears_down_even_when_every_persist_fails() {
    let dir = tempfile::tempdir().unwrap();
    let probe = Storage::new(dir.path());
    let fake = FakePi::new();
    let marker = fake.options().cwd.join("sigterm.marker");
    let upserts = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&upserts);
    let storage = Storage::new(dir.path()).with_fault_injection(move |op| match op {
        StorageOp::UpsertInstance(_) => {
            let call = counter.fetch_add(1, Ordering::SeqCst) + 1;
            // Calls: 1 initial persist, 2 session sync; storage then dies for
            // 3 the radiusPiId persist (the spawn error) and, inside
            // failSpawn, 4 the `error` persist and 5 the `stopped` persist.
            if call >= 3 {
                Err(std::io::Error::other(format!("injected #{call}")).into())
            } else {
                Ok(())
            }
        }
        _ => Ok(()),
    });
    let (presence, log) = TestPresence::assigning("radius-1");
    let supervisor = Supervisor::new(storage, presence);

    let error = supervisor
        .spawn_instance(spawn_options(&fake))
        .await
        .unwrap_err();

    // Stacked failures: the last one wins (JS masking), so the `stopped`
    // persist supersedes the `error` persist and the original spawn error.
    assert_eq!(error.to_string(), "injected #5");
    assert_eq!(
        upserts.load(Ordering::SeqCst),
        5,
        "failSpawn skipped a persist"
    );
    // Teardown was not bypassed: the held radius registration was released
    // and the child disposed despite every persist failing.
    let stored = probe.load_instances().unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(
        stored[0].status,
        InstanceStatus::Starting,
        "stored record should keep the last successfully persisted state"
    );
    assert!(
        log.entries()
            .contains(&format!("disconnect:{}:radius-1", stored[0].id)),
        "failSpawn with failing persists skipped the radius disconnect"
    );
    assert!(
        marker.exists(),
        "failSpawn with failing persists leaked the child"
    );
    assert!(
        supervisor.list_live_instances().is_empty(),
        "live entry survived failSpawn"
    );
}
