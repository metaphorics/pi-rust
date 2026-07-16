use std::collections::HashMap;
use std::fs;
use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, Weak};

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
/// All instances targeting the same directory share one lock so a read-modify-write
/// update cannot race an update made through another store.
pub struct Storage {
    orchestrator_dir: PathBuf,
    lock: Arc<Mutex<()>>,
}

impl Storage {
    pub fn from_config() -> Self {
        Self::new(config::get_orchestrator_dir())
    }

    pub fn new(orchestrator_dir: impl Into<PathBuf>) -> Self {
        let orchestrator_dir = orchestrator_dir.into();
        let lock = shared_lock(&orchestrator_dir);
        Self {
            orchestrator_dir,
            lock,
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

const LOCK_REGISTRY_CLEANUP_THRESHOLD: usize = 64;
type LockRegistry = HashMap<PathBuf, Weak<Mutex<()>>>;

fn shared_lock(orchestrator_dir: &Path) -> Arc<Mutex<()>> {
    static REGISTRY: OnceLock<Mutex<LockRegistry>> = OnceLock::new();

    let key = stable_path_key(orchestrator_dir);
    let mut registry = REGISTRY
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if registry.len() >= LOCK_REGISTRY_CLEANUP_THRESHOLD {
        registry.retain(|_, lock| lock.strong_count() > 0);
    }
    if let Some(lock) = registry.get(&key).and_then(Weak::upgrade) {
        return lock;
    }

    let lock = Arc::new(Mutex::new(()));
    registry.insert(key, Arc::downgrade(&lock));
    lock
}

fn stable_path_key(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_owned()
    } else if let Ok(current_dir) = std::env::current_dir() {
        current_dir.join(path)
    } else {
        path.to_owned()
    };
    let mut resolved = PathBuf::new();
    let mut missing_depth = 0_usize;

    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                resolved.pop();
                missing_depth = missing_depth.saturating_sub(1);
            }
            Component::Prefix(prefix) => resolved.push(prefix.as_os_str()),
            Component::RootDir => resolved.push(component.as_os_str()),
            Component::Normal(name) if missing_depth == 0 => {
                let candidate = resolved.join(name);
                match fs::canonicalize(&candidate) {
                    Ok(canonical) => resolved = canonical,
                    Err(_) => {
                        resolved.push(name);
                        missing_depth = 1;
                    }
                }
            }
            Component::Normal(name) => {
                resolved.push(name);
                missing_depth += 1;
            }
        }
    }
    resolved
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
    fn concurrent_upserts_across_storage_instances_do_not_lose_records() {
        const INITIAL_RECORDS: usize = 2_000;

        let dir = TestDir::new();
        let seed = Storage::new(&dir.0);
        let initial = (0..INITIAL_RECORDS)
            .map(|index| instance(&format!("seed-{index}"), InstanceStatus::Online))
            .collect::<Vec<_>>();
        seed.save_instances(&initial).unwrap();

        let first = Storage::new(&dir.0);
        let second = Storage::new(&dir.0);
        assert!(Arc::ptr_eq(&first.lock, &second.lock));
        let barrier = Arc::new(std::sync::Barrier::new(3));
        let mut workers = Vec::new();
        for (id, storage) in [("concurrent-one", first), ("concurrent-two", second)] {
            let barrier = Arc::clone(&barrier);
            workers.push(std::thread::spawn(move || {
                barrier.wait();
                storage.upsert_instance(instance(id, InstanceStatus::Starting))
            }));
        }
        barrier.wait();
        for worker in workers {
            worker.join().unwrap().unwrap();
        }

        let records = seed.load_instances().unwrap();
        assert_eq!(records.len(), INITIAL_RECORDS + 2);
        assert!(records.iter().any(|record| record.id == "concurrent-one"));
        assert!(records.iter().any(|record| record.id == "concurrent-two"));
    }

    #[test]
    fn lock_key_is_stable_when_target_directory_is_created() {
        let dir = TestDir::new();
        let before_creation = Storage::new(&dir.0);
        fs::create_dir_all(&dir.0).unwrap();
        let after_creation = Storage::new(&dir.0);

        assert!(Arc::ptr_eq(&before_creation.lock, &after_creation.lock));
    }

    #[cfg(unix)]
    #[test]
    fn symlink_parent_aliases_share_one_lock() {
        use std::os::unix::fs::symlink;

        let dir = TestDir::new();
        let base = dir.0.join("base");
        let other = dir.0.join("other");
        let child = other.join("child");
        fs::create_dir_all(&base).unwrap();
        fs::create_dir_all(&child).unwrap();
        symlink(&child, base.join("link")).unwrap();

        let through_symlink_parent = Storage::new(base.join("link").join(".."));
        let direct = Storage::new(&other);

        assert!(Arc::ptr_eq(&through_symlink_parent.lock, &direct.lock));
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
