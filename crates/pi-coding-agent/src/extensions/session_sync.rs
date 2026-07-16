//! `session/sync` producer (Phase 6 commit C6, plan §7).
//!
//! Single-writer invariant (I1): only Rust writes real session files. The
//! sidecar mirror rehydrates from an initial [`SessionSnapshot`] plus
//! incremental syncs whose epoch increases by exactly 1 per message; any gap
//! makes the mirror hold its state until the next full `entries` resync
//! (sidecar contract, session-mirror.ts). Optimistic entries the sidecar
//! appended locally reconcile against the authoritative `appended` copies as
//! a multiset (ids differ; type + payload agree).

use pi_ext_protocol::{SessionSnapshot, SessionSyncParams};
use serde_json::Value;

use crate::session_manager::SessionManager;

/// Tracks what the sidecar mirror has seen and mints epoch-consecutive sync
/// messages. One instance per [`super::ExtensionHost`]; a respawn or reload
/// re-baselines through [`snapshot`](SessionSync::snapshot) (the
/// `lifecycle/init` payload carries the fresh snapshot and epoch).
#[derive(Debug, Default)]
pub struct SessionSync {
    epoch: u64,
    /// Identity of the session the mirror currently holds.
    key: Option<SessionKey>,
    /// Entries already shipped for the current session.
    synced_entries: usize,
    last_leaf: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SessionKey {
    session_id: String,
    session_file: String,
}

/// The wire `sessionFile` for a manager: the real path, or `""` for
/// in-memory sessions (the DTO field is required; the mirror stores it
/// opaquely).
pub fn session_file_string(session_manager: &SessionManager) -> String {
    session_manager
        .get_session_file()
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn key_of(session_manager: &SessionManager) -> SessionKey {
    SessionKey {
        session_id: session_manager.get_session_id().to_string(),
        session_file: session_file_string(session_manager),
    }
}

fn entry_values(session_manager: &SessionManager) -> Vec<Value> {
    session_manager
        .get_entries()
        .iter()
        .filter_map(|entry| serde_json::to_value(entry).ok())
        .collect()
}

impl SessionSync {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// Full snapshot for `lifecycle/init` (initial spawn, respawn replay, or
    /// reload re-init). Adopts the session as current and consumes an epoch.
    pub fn snapshot(&mut self, session_manager: &SessionManager) -> SessionSnapshot {
        let entries = entry_values(session_manager);
        self.adopt(session_manager, entries.len());
        SessionSnapshot {
            epoch: self.epoch,
            session_file: session_file_string(session_manager),
            header: session_manager
                .get_header()
                .and_then(|header| serde_json::to_value(header).ok()),
            entries,
            leaf_id: session_manager.get_leaf_id().map(str::to_string),
            name: session_manager.get_session_name(),
        }
    }

    /// Sync message covering everything that changed since the last sync;
    /// `None` when the mirror is already current.
    ///
    /// A session switch or a shrunk entry list produces a full resync
    /// (`entries`); appended entries and leaf moves ship incrementally.
    pub fn delta(&mut self, session_manager: &SessionManager) -> Option<SessionSyncParams> {
        let key = key_of(session_manager);
        let entries = session_manager.get_entries();
        let leaf = session_manager.get_leaf_id().map(str::to_string);

        let full = self.key.as_ref() != Some(&key) || entries.len() < self.synced_entries;
        if full {
            let values = entry_values(session_manager);
            self.adopt(session_manager, values.len());
            return Some(SessionSyncParams {
                epoch: self.epoch,
                session_file: key.session_file,
                header: session_manager
                    .get_header()
                    .and_then(|header| serde_json::to_value(header).ok()),
                entries: Some(values),
                appended: None,
                leaf_id: leaf,
                name: session_manager.get_session_name(),
            });
        }

        let grew = entries.len() > self.synced_entries;
        let leaf_moved = leaf != self.last_leaf;
        if !grew && !leaf_moved {
            return None;
        }

        let appended: Vec<Value> = entries[self.synced_entries..]
            .iter()
            .filter_map(|entry| serde_json::to_value(entry).ok())
            .collect();
        self.epoch += 1;
        self.synced_entries = entries.len();
        self.last_leaf = leaf.clone();
        Some(SessionSyncParams {
            epoch: self.epoch,
            session_file: key.session_file,
            header: None,
            entries: None,
            appended: Some(appended),
            leaf_id: leaf,
            name: session_manager.get_session_name(),
        })
    }

    fn adopt(&mut self, session_manager: &SessionManager, entry_count: usize) {
        self.epoch += 1;
        self.key = Some(key_of(session_manager));
        self.synced_entries = entry_count;
        self.last_leaf = session_manager.get_leaf_id().map(str::to_string);
    }
}
