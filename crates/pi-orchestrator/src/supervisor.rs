//! Instance lifecycle supervision (port of supervisor.ts).
//!
//! The oracle relies on JavaScript's run-to-completion for atomicity: every
//! synchronous mutate-then-persist block executes without interleaving. This
//! port reproduces that by persisting to storage while holding the instance's
//! record mutex, giving each record a total order of status transitions.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};

use async_trait::async_trait;
use parking_lot::Mutex;
use serde::Serialize;
use serde_json::{Map, Value};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::radius::{Presence, PresenceCoordinator, RadiusError};
use crate::rpc_process::{RpcProcessError, RpcProcessInstance, RpcProcessOptions};
use crate::storage::{Storage, StorageError};
use crate::types::{InstanceRecord, InstanceStatus, MachineRecord, now_iso_timestamp};
use crate::wire::{RpcCommandEnvelope, RpcResponseEnvelope};

#[derive(Debug, thiserror::Error)]
pub enum SupervisorError {
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error(transparent)]
    Rpc(#[from] RpcProcessError),
    #[error(transparent)]
    Radius(#[from] RadiusError),
}

pub type Result<T, E = SupervisorError> = std::result::Result<T, E>;

#[derive(Clone, Debug)]
pub struct SpawnOptions {
    pub cwd: String,
    pub label: Option<String>,
    /// Test-only child command injection, mirroring [`RpcProcessOptions`].
    pub command_override: Option<(PathBuf, Vec<String>)>,
}

struct UiSlot {
    token: u64,
    sender: mpsc::UnboundedSender<Value>,
}

#[derive(Default)]
struct Resources {
    rpc_process: Option<RpcProcessInstance>,
    radius_pi_id: Option<String>,
    event_task: Option<JoinHandle<()>>,
    exit_task: Option<JoinHandle<()>>,
    ui_task: Option<JoinHandle<()>>,
}

struct LiveInstance {
    record: Mutex<InstanceRecord>,
    resources: Mutex<Resources>,
    subscribers: Mutex<HashMap<u64, mpsc::UnboundedSender<Value>>>,
    ui_handler: Mutex<Option<UiSlot>>,
    next_stream_id: AtomicU64,
}

impl LiveInstance {
    fn new(record: InstanceRecord) -> Self {
        Self {
            record: Mutex::new(record),
            resources: Mutex::new(Resources::default()),
            subscribers: Mutex::new(HashMap::new()),
            ui_handler: Mutex::new(None),
            next_stream_id: AtomicU64::new(0),
        }
    }

    /// Abort forwarding tasks and detach the UI handler (clearBindings).
    ///
    /// The unexpected-exit path passes `abort_exit: false`: it runs inside
    /// the exit task itself, and aborting that handle would cancel the
    /// cleanup mid-flight. The handle is still taken so it is never aborted
    /// later by another path.
    async fn clear_bindings(&self, abort_exit: bool) {
        let (event_task, exit_task, ui_task, process) = {
            let mut resources = self.resources.lock();
            (
                resources.event_task.take(),
                resources.exit_task.take(),
                resources.ui_task.take(),
                resources.rpc_process.clone(),
            )
        };
        let exit_task = if abort_exit { exit_task } else { None };
        for task in [event_task, exit_task, ui_task].into_iter().flatten() {
            task.abort();
        }
        self.ui_handler.lock().take();
        if let Some(process) = process {
            process.set_ui_request_handler(None).await;
        }
    }
}

struct Inner {
    storage: Storage,
    presence: Arc<dyn Presence>,
    live: Mutex<HashMap<String, Arc<LiveInstance>>>,
}

#[derive(Clone)]
pub struct Supervisor {
    inner: Arc<Inner>,
}

impl Supervisor {
    /// Build a supervisor and wire it as the presence coordinator, mirroring
    /// the oracle's module-scope `radiusPresence.setCoordinator` call.
    pub fn new(storage: Storage, presence: Arc<dyn Presence>) -> Self {
        let inner = Arc::new(Inner {
            storage,
            presence,
            live: Mutex::new(HashMap::new()),
        });
        let coordinator: Arc<dyn PresenceCoordinator> = Arc::clone(&inner) as _;
        inner.presence.set_coordinator(Arc::downgrade(&coordinator));
        Self { inner }
    }

    pub async fn spawn_instance(&self, options: SpawnOptions) -> Result<InstanceRecord> {
        let now = now_iso_timestamp();
        let record = InstanceRecord {
            id: Uuid::new_v4().to_string(),
            status: InstanceStatus::Starting,
            cwd: options.cwd.clone(),
            created_at: now.clone(),
            last_seen_at: Some(now),
            label: options.label.clone(),
            session_id: None,
            session_file: None,
            radius_pi_id: None,
        };
        let live = Arc::new(LiveInstance::new(record.clone()));
        self.inner
            .live
            .lock()
            .insert(record.id.clone(), Arc::clone(&live));
        self.inner.storage.upsert_instance(record)?;

        match self.inner.try_spawn(&live, options).await {
            Ok(record) => Ok(record),
            Err(error) => Err(self.inner.fail_spawn(&live, error).await),
        }
    }

    pub async fn stop_instance(&self, instance_id: &str) -> Result<Option<InstanceRecord>> {
        let live = self.inner.live.lock().get(instance_id).cloned();
        let Some(live) = live else {
            return Ok(None);
        };

        self.inner.set_status(&live, InstanceStatus::Stopping)?;
        let cleanup = self.inner.cleanup_acquired_resources(&live).await;
        // Oracle finally block: mark stopped in memory (no upsert), drop the
        // live entry, and remove the persisted record.
        {
            let mut record = live.record.lock();
            record.status = InstanceStatus::Stopped;
            record.last_seen_at = Some(now_iso_timestamp());
        }
        self.inner.live.lock().remove(instance_id);
        let removed = self.inner.storage.remove_instance(instance_id);
        removed?;
        cleanup?;
        Ok(Some(live.record.lock().clone()))
    }

    pub async fn handle_rpc(
        &self,
        instance_id: &str,
        command: RpcCommandEnvelope,
    ) -> Result<Option<RpcResponseEnvelope>> {
        let live = self.inner.live.lock().get(instance_id).cloned();
        let Some(live) = live else {
            return Ok(None);
        };
        let process = live.resources.lock().rpc_process.clone();
        let Some(process) = process else {
            return Ok(None);
        };

        let refresh = command.refreshes_session_metadata();
        let response = process.send(command).await?;
        if refresh {
            self.inner.sync_instance_record(&live).await?;
        }
        Ok(Some(response))
    }

    /// Subscribe to an instance's event stream and claim its UI-request slot.
    ///
    /// Events fan out to every open stream; UI requests go to the most
    /// recently opened stream only (last handler wins).
    pub fn open_rpc_stream(&self, instance_id: &str) -> Option<RpcStream> {
        let live = self.inner.live.lock().get(instance_id).cloned()?;
        let process = live.resources.lock().rpc_process.clone()?;

        let (event_sender, events) = mpsc::unbounded_channel();
        let (ui_sender, ui_requests) = mpsc::unbounded_channel();
        let subscriber_id = live.next_stream_id.fetch_add(1, Ordering::Relaxed);
        let ui_token = live.next_stream_id.fetch_add(1, Ordering::Relaxed);
        live.subscribers.lock().insert(subscriber_id, event_sender);
        *live.ui_handler.lock() = Some(UiSlot {
            token: ui_token,
            sender: ui_sender,
        });

        Some(RpcStream {
            inner: Arc::clone(&self.inner),
            live,
            process,
            subscriber_id,
            ui_token,
            events,
            ui_requests,
        })
    }

    pub fn get_live_instance(&self, instance_id: &str) -> Option<InstanceRecord> {
        let live = self.inner.live.lock().get(instance_id).cloned()?;
        Some(live.record.lock().clone())
    }

    pub fn list_live_instances(&self) -> Vec<InstanceRecord> {
        self.inner
            .live
            .lock()
            .values()
            .map(|live| live.record.lock().clone())
            .collect()
    }

    /// Replace a live record wholesale and persist it (coordinator surface).
    pub fn update_instance(&self, instance: InstanceRecord) -> Result<(), StorageError> {
        self.inner.update_instance(instance)
    }

    pub fn list_instances(&self) -> Result<Vec<InstanceRecord>, StorageError> {
        self.inner.storage.load_instances()
    }

    pub fn get_instance(&self, instance_id: &str) -> Result<Option<InstanceRecord>, StorageError> {
        if let Some(record) = self.get_live_instance(instance_id) {
            return Ok(Some(record));
        }
        self.inner.storage.get_instance(instance_id)
    }

    /// Serve-boot recovery: stale `online`/`starting` records become
    /// `stopped`, every record is disconnected from Radius (stored
    /// `radiusPiId` values are intentionally kept), and the batch is saved.
    pub async fn recover_after_restart(&self) -> Result<()> {
        let recovered_at = now_iso_timestamp();
        let mut instances = self.inner.storage.load_instances()?;
        for instance in &mut instances {
            if matches!(
                instance.status,
                InstanceStatus::Online | InstanceStatus::Starting
            ) {
                instance.status = InstanceStatus::Stopped;
            }
            instance.last_seen_at = Some(recovered_at.clone());
        }
        for instance in &instances {
            self.inner.presence.disconnect_pi(instance).await?;
        }
        self.inner.storage.save_instances(&instances)?;
        Ok(())
    }

    pub async fn shutdown(&self) -> Result<()> {
        let ids: Vec<String> = self.inner.live.lock().keys().cloned().collect();
        for id in ids {
            self.stop_instance(&id).await?;
        }
        Ok(())
    }
}

impl Inner {
    async fn try_spawn(
        self: &Arc<Self>,
        live: &Arc<LiveInstance>,
        options: SpawnOptions,
    ) -> Result<InstanceRecord> {
        let process = RpcProcessInstance::spawn(RpcProcessOptions {
            cwd: options.cwd.into(),
            command_override: options.command_override,
        })?;
        self.bind_rpc_process(live, process).await;
        self.sync_instance_record(live).await?;
        let snapshot = live.record.lock().clone();
        let registered = self.presence.register_pi(snapshot).await?;
        self.update_radius_pi_id(live, registered.radius_pi_id)?;
        self.set_status(live, InstanceStatus::Online)?;
        Ok(live.record.lock().clone())
    }

    /// Oracle failSpawn: persist `error`, release resources, persist
    /// `stopped`, drop the live entry, and propagate. A cleanup or persist
    /// failure replaces the original error, exactly as the TS try/finally
    /// does. The stored record intentionally survives as `stopped`.
    async fn fail_spawn(
        self: &Arc<Self>,
        live: &Arc<LiveInstance>,
        error: SupervisorError,
    ) -> SupervisorError {
        if let Err(persist) = self.set_status(live, InstanceStatus::Error) {
            return persist.into();
        }
        let cleanup = self.cleanup_acquired_resources(live).await;
        if let Err(persist) = self.set_status(live, InstanceStatus::Stopped) {
            return persist.into();
        }
        let id = live.record.lock().id.clone();
        self.live.lock().remove(&id);
        match cleanup {
            Err(cleanup_error) => cleanup_error,
            Ok(()) => error,
        }
    }

    async fn bind_rpc_process(
        self: &Arc<Self>,
        live: &Arc<LiveInstance>,
        process: RpcProcessInstance,
    ) {
        live.clear_bindings(true).await;

        let mut event_receiver = process.subscribe_events().await;
        let mut exit_receiver = process.subscribe_exit().await;
        let (ui_sender, mut ui_receiver) = mpsc::unbounded_channel();
        process.set_ui_request_handler(Some(ui_sender)).await;

        let event_live = Arc::clone(live);
        let event_task = tokio::spawn(async move {
            while let Some(event) = event_receiver.recv().await {
                event_live
                    .subscribers
                    .lock()
                    .retain(|_, subscriber| subscriber.send(event.clone()).is_ok());
            }
        });

        let ui_live = Arc::clone(live);
        let ui_task = tokio::spawn(async move {
            while let Some(request) = ui_receiver.recv().await {
                let handler = ui_live
                    .ui_handler
                    .lock()
                    .as_ref()
                    .map(|slot| slot.sender.clone());
                if let Some(handler) = handler {
                    let _ = handler.send(request);
                }
            }
        });

        let exit_inner = Arc::downgrade(self);
        let exit_live = Arc::clone(live);
        let exit_task = tokio::spawn(async move {
            if exit_receiver.recv().await.is_some()
                && let Some(inner) = exit_inner.upgrade()
            {
                inner.handle_unexpected_exit(&exit_live).await;
            }
        });

        let mut resources = live.resources.lock();
        resources.rpc_process = Some(process);
        resources.event_task = Some(event_task);
        resources.exit_task = Some(exit_task);
        resources.ui_task = Some(ui_task);
    }

    async fn handle_unexpected_exit(self: &Arc<Self>, live: &Arc<LiveInstance>) {
        let id = live.record.lock().id.clone();
        if !self
            .live
            .lock()
            .get(&id)
            .is_some_and(|entry| Arc::ptr_eq(entry, live))
        {
            return;
        }
        // Guard and error-persist are atomic under the record lock so a
        // concurrent stop/failSpawn transition cannot be overwritten. A
        // persist failure aborts the handler before any cleanup, matching
        // the oracle where the setStatus throw rejects the whole listener
        // (only the in-memory status change survives).
        {
            let mut record = live.record.lock();
            if matches!(
                record.status,
                InstanceStatus::Stopping | InstanceStatus::Stopped
            ) {
                return;
            }
            record.status = InstanceStatus::Error;
            record.last_seen_at = Some(now_iso_timestamp());
            if let Err(error) = self.storage.upsert_instance(record.clone()) {
                log::error!("Failed to persist error status for {id}: {error}");
                return;
            }
        }

        live.clear_bindings(false).await;
        live.resources.lock().rpc_process = None;
        let has_radius = live.resources.lock().radius_pi_id.is_some();
        if has_radius {
            let record = live.record.lock().clone();
            let result: Result<()> = match self.presence.disconnect_pi(&record).await {
                Ok(()) => self.update_radius_pi_id(live, None).map_err(Into::into),
                Err(error) => Err(error.into()),
            };
            if let Err(error) = result {
                log::error!("Failed to disconnect Radius Pi {id}: {error}");
            }
        }

        let mut map = self.live.lock();
        if map.get(&id).is_some_and(|entry| Arc::ptr_eq(entry, live)) {
            map.remove(&id);
        }
    }

    /// Refresh persisted session metadata from a `get_state` round-trip.
    /// Without a process, or on a non-success response, only `lastSeenAt`
    /// is bumped.
    async fn sync_instance_record(self: &Arc<Self>, live: &Arc<LiveInstance>) -> Result<()> {
        let process = live.resources.lock().rpc_process.clone();
        let Some(process) = process else {
            return Ok(self.touch(live)?);
        };
        let response = process.send(get_state_command()).await?;
        match response.get_state() {
            Ok(Some(data)) => self.update_session(live, data.session_id, data.session_file)?,
            _ => self.touch(live)?,
        }
        Ok(())
    }

    /// Release everything spawn acquired: bindings, Radius registration
    /// (disconnected before the child is disposed), then the child itself.
    /// The in-memory `radiusPiId` clear is deliberately not persisted,
    /// matching the oracle.
    async fn cleanup_acquired_resources(self: &Arc<Self>, live: &Arc<LiveInstance>) -> Result<()> {
        let process = live.resources.lock().rpc_process.clone();
        live.clear_bindings(true).await;
        let has_radius = live.resources.lock().radius_pi_id.is_some();
        if has_radius {
            let record = live.record.lock().clone();
            self.presence.disconnect_pi(&record).await?;
            live.resources.lock().radius_pi_id = None;
            let mut record = live.record.lock();
            record.radius_pi_id = None;
            record.last_seen_at = Some(now_iso_timestamp());
        }
        if let Some(process) = process {
            live.resources.lock().rpc_process = None;
            process.dispose().await;
        }
        Ok(())
    }

    fn set_status(&self, live: &LiveInstance, status: InstanceStatus) -> Result<(), StorageError> {
        let mut record = live.record.lock();
        record.status = status;
        record.last_seen_at = Some(now_iso_timestamp());
        self.storage.upsert_instance(record.clone())
    }

    fn touch(&self, live: &LiveInstance) -> Result<(), StorageError> {
        let mut record = live.record.lock();
        record.last_seen_at = Some(now_iso_timestamp());
        self.storage.upsert_instance(record.clone())
    }

    fn update_session(
        &self,
        live: &LiveInstance,
        session_id: String,
        session_file: Option<String>,
    ) -> Result<(), StorageError> {
        let mut record = live.record.lock();
        record.session_id = Some(session_id);
        record.session_file = session_file;
        record.last_seen_at = Some(now_iso_timestamp());
        self.storage.upsert_instance(record.clone())
    }

    /// Set the record's `radiusPiId` (clearing on `None`); the resource-side
    /// copy is only updated for `Some`, mirroring the oracle's
    /// `!== undefined` filter.
    fn update_radius_pi_id(
        &self,
        live: &LiveInstance,
        radius_pi_id: Option<String>,
    ) -> Result<(), StorageError> {
        let persisted = {
            let mut record = live.record.lock();
            record.radius_pi_id = radius_pi_id.clone();
            record.last_seen_at = Some(now_iso_timestamp());
            self.storage.upsert_instance(record.clone())
        };
        if let Some(radius_pi_id) = radius_pi_id {
            live.resources.lock().radius_pi_id = Some(radius_pi_id);
        }
        persisted
    }

    fn update_instance(&self, instance: InstanceRecord) -> Result<(), StorageError> {
        if let Some(live) = self.live.lock().get(&instance.id).cloned() {
            *live.record.lock() = instance.clone();
            live.resources.lock().radius_pi_id = instance.radius_pi_id.clone();
        }
        self.storage.upsert_instance(instance)
    }
}

#[async_trait]
impl PresenceCoordinator for Inner {
    async fn get_live_instance(&self, instance_id: &str) -> Option<InstanceRecord> {
        let live = self.live.lock().get(instance_id).cloned()?;
        Some(live.record.lock().clone())
    }

    async fn list_live_instances(&self) -> Vec<InstanceRecord> {
        self.live
            .lock()
            .values()
            .map(|live| live.record.lock().clone())
            .collect()
    }

    async fn update_instance(&self, instance: InstanceRecord) {
        let id = instance.id.clone();
        if let Err(error) = Inner::update_instance(self, instance) {
            log::error!("Failed to persist instance update for {id}: {error}");
        }
    }
}

/// One open `rpc_stream` connection: an event subscription plus (until a
/// newer stream claims it) ownership of the instance's UI-request slot.
pub struct RpcStream {
    inner: Arc<Inner>,
    live: Arc<LiveInstance>,
    process: RpcProcessInstance,
    subscriber_id: u64,
    ui_token: u64,
    pub events: mpsc::UnboundedReceiver<Value>,
    pub ui_requests: mpsc::UnboundedReceiver<Value>,
}

impl RpcStream {
    /// Forward a command; after any session-metadata command the persisted
    /// record is refreshed before the response is returned.
    pub async fn handle_rpc(&self, command: RpcCommandEnvelope) -> Result<RpcResponseEnvelope> {
        let refresh = command.refreshes_session_metadata();
        let response = self.process.send(command).await?;
        if refresh {
            self.inner.sync_instance_record(&self.live).await?;
        }
        Ok(response)
    }

    pub fn handle_ui_response<T: Serialize>(&self, response: &T) -> Result<(), RpcProcessError> {
        self.process.handle_ui_response(response)
    }

    /// Drop the event subscription; release the UI slot only if this stream
    /// still owns it.
    pub fn close(&self) {
        {
            let mut slot = self.live.ui_handler.lock();
            if slot
                .as_ref()
                .is_some_and(|slot| slot.token == self.ui_token)
            {
                *slot = None;
            }
        }
        self.live.subscribers.lock().remove(&self.subscriber_id);
    }
}

/// Presence no-op for radius-less operation and tests.
pub struct NullPresence;

#[async_trait]
impl Presence for NullPresence {
    fn set_coordinator(&self, _coordinator: Weak<dyn PresenceCoordinator>) {}

    async fn start(&self, _label: Option<String>) -> Result<Option<MachineRecord>, RadiusError> {
        Ok(None)
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

fn get_state_command() -> RpcCommandEnvelope {
    let mut raw = Map::new();
    raw.insert("type".into(), Value::String("get_state".into()));
    RpcCommandEnvelope {
        id: None,
        kind: "get_state".into(),
        raw,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(id: &str, status: InstanceStatus) -> InstanceRecord {
        InstanceRecord {
            id: id.into(),
            status,
            cwd: "/work".into(),
            created_at: "2025-12-09T00:53:29.825Z".into(),
            last_seen_at: Some("2025-12-09T00:53:29.825Z".into()),
            label: None,
            session_id: None,
            session_file: None,
            radius_pi_id: None,
        }
    }

    fn supervisor() -> (Supervisor, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let supervisor = Supervisor::new(Storage::new(dir.path()), Arc::new(NullPresence));
        (supervisor, dir)
    }

    #[tokio::test]
    async fn unexpected_exit_ignores_a_stale_live_entry() {
        let (supervisor, _dir) = supervisor();
        let stale = Arc::new(LiveInstance::new(record("one", InstanceStatus::Online)));
        let current = Arc::new(LiveInstance::new(record("one", InstanceStatus::Online)));
        supervisor
            .inner
            .live
            .lock()
            .insert("one".into(), Arc::clone(&current));

        supervisor.inner.handle_unexpected_exit(&stale).await;

        assert!(
            supervisor
                .inner
                .live
                .lock()
                .get("one")
                .is_some_and(|entry| Arc::ptr_eq(entry, &current)),
            "stale exit displaced the current live entry"
        );
        assert_eq!(
            current.record.lock().status,
            InstanceStatus::Online,
            "stale exit mutated the current record"
        );
        assert_eq!(supervisor.inner.storage.load_instances().unwrap(), []);
    }

    #[tokio::test]
    async fn unexpected_exit_skips_stopping_and_stopped_instances() {
        for status in [InstanceStatus::Stopping, InstanceStatus::Stopped] {
            let (supervisor, _dir) = supervisor();
            let live = Arc::new(LiveInstance::new(record("one", status)));
            supervisor
                .inner
                .live
                .lock()
                .insert("one".into(), Arc::clone(&live));

            supervisor.inner.handle_unexpected_exit(&live).await;

            assert_eq!(live.record.lock().status, status);
            assert!(
                supervisor.inner.live.lock().contains_key("one"),
                "guarded exit removed the live entry for {status:?}"
            );
            assert_eq!(supervisor.inner.storage.load_instances().unwrap(), []);
        }
    }
}
