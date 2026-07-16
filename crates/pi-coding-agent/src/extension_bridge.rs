//! Extension bridge seam for Phase 6.
//!
//! Discovery of extension *paths* lives in [`crate::resource_loader`]. Loading
//! and executing extensions happens in the Bun sidecar (Phase 6). This module
//! documents the host-side trait boundary the sidecar RPC will implement.

use std::path::PathBuf;

/// Why a session is starting (oracle `SessionStartEvent.reason`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionStartReason {
    Startup,
    New,
    Resume,
    Fork,
    Reload,
}

/// Why the current session is being replaced/shut down (oracle
/// `SessionShutdownEvent.reason`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionShutdownReason {
    New,
    Resume,
    Fork,
    Reload,
    Quit,
}

/// Fork anchor position (oracle `session_before_fork` position).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ForkPosition {
    Before,
    At,
}

/// Session lifecycle events routed through the extension bridge (Phase 6
/// forwards these to the sidecar; `NoopExtensionBridge` continues).
#[derive(Clone, Debug)]
pub enum SessionLifecycleEvent {
    SessionStart {
        reason: SessionStartReason,
        previous_session_file: Option<PathBuf>,
    },
    SessionBeforeSwitch {
        /// `"new" | "resume"` in the oracle event.
        reason: SessionStartReason,
        target_session_file: Option<PathBuf>,
    },
    SessionBeforeFork {
        entry_id: String,
        position: ForkPosition,
    },
    SessionShutdown {
        reason: SessionShutdownReason,
        target_session_file: Option<PathBuf>,
    },
}

/// Outcome of a blocking lifecycle hook.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HookOutcome {
    Continue,
    Cancel,
}

/// Discovered extension source paths (not yet loaded).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DiscoveredExtensions {
    /// Absolute or project-relative paths that would be handed to the loader.
    pub paths: Vec<PathBuf>,
}

/// Host-side extension runtime surface.
///
/// Phase 6 plugs the Bun sidecar behind this trait. Until then,
/// [`NoopExtensionBridge`] reports empty registrations and is never invoked
/// on the hot path.
pub trait ExtensionBridge: Send + Sync {
    /// Whether any extensions were discovered and a sidecar is required.
    fn needs_sidecar(&self) -> bool;

    /// Paths discovered by the resource loader (for diagnostics / spawn args).
    fn discovered_paths(&self) -> &[PathBuf];

    /// Route a session lifecycle event through registered hooks. The default
    /// (and `NoopExtensionBridge`) always continues.
    fn emit_lifecycle(&self, _event: &SessionLifecycleEvent) -> HookOutcome {
        HookOutcome::Continue
    }
}

/// Placeholder bridge used until Phase 6.
#[derive(Clone, Debug, Default)]
pub struct NoopExtensionBridge {
    paths: Vec<PathBuf>,
}

impl NoopExtensionBridge {
    pub fn new(paths: Vec<PathBuf>) -> Self {
        Self { paths }
    }
}

impl ExtensionBridge for NoopExtensionBridge {
    fn needs_sidecar(&self) -> bool {
        !self.paths.is_empty()
    }

    fn discovered_paths(&self) -> &[PathBuf] {
        &self.paths
    }
}
