use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use crate::config;
use crate::types::{InstanceRecord, MachineRecord};

#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("storage lock was poisoned")]
    LockPoisoned,
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

pub type Result<T, E = StorageError> = std::result::Result<T, E>;

/// JSON persistence for orchestrator state.
///
/// All operations share one lock so a read-modify-write update cannot race another
/// update made through this store.
pub struct Storage {
    orchestrator_dir: PathBuf,
    lock: Mutex<()>,
}

impl Storage {
    pub fn from_config() -> Self {
        Self::new(config::get_orchestrator_dir())
    }

    pub fn new(orchestrator_dir: impl Into<PathBuf>) -> Self {
        Self {
            orchestrator_dir: orchestrator_dir.into(),
            lock: Mutex::new(()),
        }
    }

    pub fn orchestrator_dir(&self) -> &Path {
        &self.orchestrator_dir
    }

    pub fn load_machine(&self) -> Result<Option<MachineRecord>> {
        let _guard = self.acquire()?;
        self.load_machine_unlocked()
    }

    pub fn save_machine(&self, machine: &MachineRecord) -> Result<()> {
        let _guard = self.acquire()?;
        self.save_json(self.machine_path(), machine)
    }

    pub fn delete_machine(&self) -> Result<()> {
        let _guard = self.acquire()?;
        remove_if_present(&self.machine_path())
    }

    pub fn load_instances(&self) -> Result<Vec<InstanceRecord>> {
        let _guard = self.acquire()?;
        self.load_instances_unlocked()
    }

    pub fn save_instances(&self, instances: &[InstanceRecord]) -> Result<()> {
        let _guard = self.acquire()?;
        self.save_json(self.instances_path(), instances)
    }

    pub fn get_instance(&self, instance_id: &str) -> Result<Option<InstanceRecord>> {
        let _guard = self.acquire()?;
        Ok(self
            .load_instances_unlocked()?
            .into_iter()
            .find(|instance| instance.id == instance_id))
    }

    pub fn upsert_instance(&self, instance: InstanceRecord) -> Result<()> {
        let _guard = self.acquire()?;
        let mut instances = self.load_instances_unlocked()?;
        if let Some(existing) = instances
            .iter_mut()
            .find(|existing| existing.id == instance.id)
        {
            *existing = instance;
        } else {
            instances.push(instance);
        }
        self.save_json(self.instances_path(), &instances)
    }

    pub fn remove_instance(&self, instance_id: &str) -> Result<()> {
        let _guard = self.acquire()?;
        let mut instances = self.load_instances_unlocked()?;
        instances.retain(|instance| instance.id != instance_id);
        self.save_json(self.instances_path(), &instances)
    }

    fn acquire(&self) -> Result<MutexGuard<'_, ()>> {
        self.lock.lock().map_err(|_| StorageError::LockPoisoned)
    }

    fn machine_path(&self) -> PathBuf {
        self.orchestrator_dir.join("machine.json")
    }

    fn instances_path(&self) -> PathBuf {
        self.orchestrator_dir.join("instances.json")
    }

    fn load_machine_unlocked(&self) -> Result<Option<MachineRecord>> {
        match fs::read_to_string(self.machine_path()) {
            Ok(data) => Ok(Some(serde_json::from_str(&data)?)),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    fn load_instances_unlocked(&self) -> Result<Vec<InstanceRecord>> {
        match fs::read_to_string(self.instances_path()) {
            Ok(data) => Ok(serde_json::from_str(&data)?),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(Vec::new()),
            Err(error) => Err(error.into()),
        }
    }

    fn save_json<T>(&self, path: PathBuf, value: &T) -> Result<()>
    where
        T: serde::Serialize + ?Sized,
    {
        fs::create_dir_all(&self.orchestrator_dir)?;
        fs::write(path, serde_json::to_string_pretty(value)?)?;
        Ok(())
    }
}

fn remove_if_present(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::InstanceStatus;

    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "pi-orchestrator-storage-{}-{}",
                std::process::id(),
                jiff::Timestamp::now().as_nanosecond()
            ));
            Self(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn instance(id: &str, status: InstanceStatus) -> InstanceRecord {
        InstanceRecord {
            id: id.into(),
            status,
            cwd: "/work".into(),
            created_at: "2025-12-09T00:53:29.825Z".into(),
            last_seen_at: None,
            label: None,
            session_id: None,
            session_file: None,
            radius_pi_id: None,
        }
    }

    #[test]
    fn missing_files_have_oracle_defaults() {
        let dir = TestDir::new();
        let storage = Storage::new(&dir.0);
        assert_eq!(storage.load_machine().unwrap(), None);
        assert_eq!(storage.load_instances().unwrap(), Vec::new());
        storage.delete_machine().unwrap();
    }

    #[test]
    fn persistence_is_pretty_json_without_a_trailing_newline() {
        let dir = TestDir::new();
        let storage = Storage::new(&dir.0);
        storage
            .save_instances(&[instance("worker-1", InstanceStatus::Online)])
            .unwrap();

        assert_eq!(
            fs::read_to_string(dir.0.join("instances.json")).unwrap(),
            "[\n  {\n    \"id\": \"worker-1\",\n    \"status\": \"online\",\n    \"cwd\": \"/work\",\n    \"createdAt\": \"2025-12-09T00:53:29.825Z\"\n  }\n]"
        );
    }

    #[test]
    fn upsert_preserves_position_and_remove_persists() {
        let dir = TestDir::new();
        let storage = Storage::new(&dir.0);
        storage
            .upsert_instance(instance("one", InstanceStatus::Starting))
            .unwrap();
        storage
            .upsert_instance(instance("two", InstanceStatus::Online))
            .unwrap();
        storage
            .upsert_instance(instance("one", InstanceStatus::Error))
            .unwrap();

        let records = storage.load_instances().unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].id, "one");
        assert_eq!(records[0].status, InstanceStatus::Error);
        assert_eq!(records[1].id, "two");
        assert_eq!(
            storage.get_instance("two").unwrap().unwrap().status,
            InstanceStatus::Online
        );

        storage.remove_instance("one").unwrap();
        assert_eq!(storage.load_instances().unwrap(), vec![records[1].clone()]);
    }

    #[test]
    fn machine_round_trips_and_deletes() {
        let dir = TestDir::new();
        let storage = Storage::new(&dir.0);
        let machine = MachineRecord {
            id: "machine-1".into(),
            created_at: "2025-12-09T00:53:29.825Z".into(),
            last_seen_at: None,
            label: Some("desk".into()),
        };
        storage.save_machine(&machine).unwrap();
        assert_eq!(storage.load_machine().unwrap(), Some(machine));
        storage.delete_machine().unwrap();
        assert_eq!(storage.load_machine().unwrap(), None);
    }
}
