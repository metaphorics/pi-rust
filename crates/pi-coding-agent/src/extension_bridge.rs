//! Extension bridge seam for Phase 6.
//!
//! Discovery of extension *paths* lives in [`crate::resource_loader`]. Loading
//! and executing extensions happens in the Bun sidecar (Phase 6). This module
//! documents the host-side trait boundary the sidecar RPC will implement.

use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use pi_agent::CancellationToken;
use serde::{Deserialize, Serialize};

use crate::source_info::SourceInfo;

/// Boxed future used by the async UI host methods.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

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

/// An extension-registered slash command (oracle
/// `extensionRunner.getRegisteredCommands()` projection used by RPC
/// `get_commands` and interactive slash dispatch).
#[derive(Clone, Debug, PartialEq)]
pub struct RegisteredCommand {
    /// Oracle `invocationName` (without leading slash).
    pub invocation_name: String,
    pub description: Option<String>,
    pub source_info: SourceInfo,
}

/// Notification severity (oracle `ctx.ui.notify` type).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NotifyType {
    Info,
    Warning,
    Error,
}

/// Widget placement (oracle `ExtensionWidgetOptions.placement`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum WidgetPlacement {
    AboveEditor,
    BelowEditor,
}

/// Options for blocking UI dialogs (oracle `ExtensionUIDialogOptions`).
#[derive(Clone, Debug, Default)]
pub struct UiDialogOptions {
    /// Auto-resolve with the cancel fallback after this many milliseconds.
    pub timeout_ms: Option<u64>,
    /// Abort signal; resolves with the cancel fallback when cancelled.
    pub signal: Option<CancellationToken>,
}

/// Host-side UI provider extensions call INTO (oracle `ExtensionUIContext`
/// subset that crosses the mode boundary; rpc-mode.ts:108-299).
///
/// Each mode binds its own implementation: rpc emits `extension_ui_request`
/// wire lines and resolves them from `extension_ui_response` lines;
/// interactive (wave C) backs these with dialogs; print logs errors.
pub trait ExtensionUiHost: Send + Sync {
    /// Blocking select dialog; `None` on cancel/timeout.
    fn select(
        &self,
        title: String,
        options: Vec<String>,
        opts: UiDialogOptions,
    ) -> BoxFuture<'static, Option<String>>;

    /// Blocking confirm dialog; `false` on cancel/timeout.
    fn confirm(
        &self,
        title: String,
        message: String,
        opts: UiDialogOptions,
    ) -> BoxFuture<'static, bool>;

    /// Blocking input dialog; `None` on cancel/timeout.
    fn input(
        &self,
        title: String,
        placeholder: Option<String>,
        opts: UiDialogOptions,
    ) -> BoxFuture<'static, Option<String>>;

    /// Blocking editor dialog; `None` on cancel.
    fn editor(&self, title: String, prefill: Option<String>) -> BoxFuture<'static, Option<String>>;

    /// Fire-and-forget notification.
    fn notify(&self, message: String, notify_type: Option<NotifyType>);

    /// Fire-and-forget status line update (`None` clears the key).
    fn set_status(&self, key: String, text: Option<String>);

    /// Fire-and-forget widget update (`None` clears the key).
    fn set_widget(
        &self,
        key: String,
        lines: Option<Vec<String>>,
        placement: Option<WidgetPlacement>,
    );

    /// Fire-and-forget terminal title update.
    fn set_title(&self, title: String);

    /// Fire-and-forget editor text replacement.
    fn set_editor_text(&self, text: String);
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

    /// Extension-registered slash commands (RPC `get_commands`, interactive
    /// dispatch). Empty until the Phase 6 sidecar reports registrations.
    fn registered_commands(&self) -> Vec<RegisteredCommand> {
        Vec::new()
    }

    /// Bind the host-side UI provider for the active mode. The Phase 6
    /// sidecar routes extension `ctx.ui` calls to the bound host; without a
    /// sidecar there is nothing to route, so the default drops the handle.
    fn bind_ui(&self, _ui: Arc<dyn ExtensionUiHost>) {}
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
