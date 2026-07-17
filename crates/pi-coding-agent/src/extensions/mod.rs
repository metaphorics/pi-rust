//! Rust side of the Bun extension sidecar: detection, spawn, transport, and
//! lifecycle (Phase 6 commit C5).
//!
//! Event dispatch (C6) and tools/providers (C7) consume [`ExtensionHost`] and
//! the [`Incoming`] stream; they are not implemented here.
//!
//! Lifecycle contract (plan §4):
//! - Zero extensions ⇒ zero Bun activity, ever ([`BridgeState::NotNeeded`]).
//! - Lazy spawn on first [`ExtensionHost::ensure_ready`]; nothing at boot.
//! - Handshake: the sidecar speaks first (`lifecycle/hello`); the host then
//!   sends `lifecycle/init`, during which the sidecar loads extensions and
//!   emits `lifecycle/initialized` *before* answering the init request.
//!   `lifecycle/load` is incremental-only (later installs).
//! - Each death grants exactly one respawn attempt (full replay: spawn,
//!   handshake, init). A failed respawn disables extensions for the process.
//! - Deliberate shutdown is terminal; it is never respawned.
//!
//! The inbound application-frame channel is host-owned and survives respawns:
//! attach the consumer (C6) via [`ExtensionHost::take_incoming`] *before* the
//! first `ensure_ready` so boot-time sidecar traffic (e.g. `error/extension`
//! floods) can always drain. Without a consumer the init timeout still bounds
//! the failure.

pub mod actions;
pub mod binding;
pub mod client;
pub mod detect;
pub mod events;
pub mod provider;
pub mod session_sync;
pub mod spawn;
pub mod state;
pub mod tools;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use pi_ext_protocol::{
    Empty, ExtensionError, InitParams, InitializedParams, LoadParams, Registrations, Request,
};
use serde::Deserialize;
use thiserror::Error;
use tokio::sync::{Mutex, mpsc};

pub use client::{ClientConfig, ClientError, Incoming, PendingReply, SidecarConnection};
pub use detect::{
    BUN_ENV_OVERRIDE, BUN_INSTALL_COMMAND, BunEnvironment, BunResolveError, SIDECAR_ENV_OVERRIDE,
    SidecarResolveError, resolve_bun, resolve_sidecar_entry,
};
pub use spawn::{LaunchResolveError, SidecarLauncher};
pub use state::{BridgeState, DeadReason, DisabledReason};

use spawn::spawn_sidecar;

/// Deadlines for the lifecycle phases.
#[derive(Clone, Debug)]
pub struct SidecarTimeouts {
    /// Spawn → `lifecycle/hello`.
    pub handshake: Duration,
    /// `lifecycle/init` request (covers extension loading in the sidecar).
    pub init: Duration,
    /// Grace for `lifecycle/shutdown` and for process exit before a kill.
    pub shutdown_grace: Duration,
}

impl Default for SidecarTimeouts {
    fn default() -> Self {
        Self {
            handshake: Duration::from_secs(10),
            init: Duration::from_secs(60),
            shutdown_grace: Duration::from_secs(3),
        }
    }
}

/// Produces the `lifecycle/init` payload for each (re)spawn, so a respawn
/// replays against current host state. `configured_paths` is overwritten by
/// the host with its canonical extension paths.
pub type InitSource = Arc<dyn Fn() -> InitParams + Send + Sync>;

/// Deferred launcher resolution, run at most once and only when a spawn is
/// actually needed (invariant I6: zero extensions ⇒ zero Bun activity, not
/// even detection).
pub type LauncherResolver =
    Box<dyn FnOnce() -> Result<SidecarLauncher, LaunchResolveError> + Send + Sync>;

/// Where the sidecar launch spec comes from.
pub enum LauncherSource {
    /// Already resolved (tests; callers that detected eagerly on purpose).
    Resolved(SidecarLauncher),
    /// Resolved lazily on the first spawn attempt.
    Lazy(LauncherResolver),
}

impl LauncherSource {
    /// Standard production source: `$PI_RUST_BUN`/`$BUN_INSTALL`/`$PATH` for
    /// Bun and `$PI_RUST_SIDECAR`/`<package_dir>/sidecar` for the entry,
    /// evaluated only when the first spawn is needed.
    pub fn detect(cwd: PathBuf) -> Self {
        Self::Lazy(Box::new(move || {
            let sidecar_override = std::env::var_os(SIDECAR_ENV_OVERRIDE).map(PathBuf::from);
            SidecarLauncher::resolve(
                &BunEnvironment::from_env(),
                sidecar_override.as_deref(),
                &crate::config::get_package_dir(),
                &cwd,
            )
        }))
    }
}

pub struct ExtensionHostConfig {
    /// Discovered extension paths. Empty ⇒ [`BridgeState::NotNeeded`].
    pub extension_paths: Vec<PathBuf>,
    /// Launch spec source; never consulted while zero extensions exist.
    /// Resolution failure ⇒ [`DisabledReason::LaunchUnavailable`] on first
    /// use (its message carries the Bun install command when Bun is missing).
    pub launcher: LauncherSource,
    pub init: InitSource,
    pub timeouts: SidecarTimeouts,
    pub client: ClientConfig,
}

/// `lifecycle/load` response (sidecar contract C3 §6).
#[derive(Clone, Debug, Default, Deserialize)]
pub struct LoadOutcome {
    pub registrations: Registrations,
    pub errors: Vec<ExtensionError>,
}

/// An extension path that could not be canonicalized.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
#[error("extension path `{path}` cannot be canonicalized: {error}")]
pub struct ExtensionPathError {
    pub path: PathBuf,
    pub error: String,
}

#[derive(Debug, Error)]
pub enum HostError {
    #[error("no extensions installed; sidecar not needed")]
    NotNeeded,
    #[error("extensions disabled: {0}")]
    Disabled(DisabledReason),
    #[error("sidecar was shut down")]
    ShutDown,
    #[error("sidecar attempt failed: {0}")]
    Failed(DeadReason),
    #[error(transparent)]
    Rpc(#[from] ClientError),
    #[error("decoding sidecar response: {0}")]
    Decode(String),
    #[error(transparent)]
    Path(#[from] ExtensionPathError),
}

struct HostInner {
    state: BridgeState,
    connection: Option<Arc<SidecarConnection>>,
    /// Cached resolved launcher (resolution runs at most once).
    launcher: Option<SidecarLauncher>,
    /// Pending lazy resolver; `None` once consumed or when pre-resolved.
    resolver: Option<LauncherResolver>,
}

impl HostInner {
    fn transition(&mut self, next: BridgeState) {
        debug_assert!(
            self.state.can_transition(&next),
            "illegal bridge transition {:?} -> {next:?}",
            self.state
        );
        self.state = next;
    }
}

impl std::fmt::Debug for ExtensionHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExtensionHost")
            .field("paths", &self.paths)
            .finish_non_exhaustive()
    }
}

/// Owner of the sidecar process lifecycle.
pub struct ExtensionHost {
    paths: Vec<PathBuf>,
    init: InitSource,
    timeouts: SidecarTimeouts,
    client_config: ClientConfig,
    incoming_tx: mpsc::Sender<Incoming>,
    incoming_rx: parking_lot::Mutex<Option<mpsc::Receiver<Incoming>>>,
    inner: Mutex<HostInner>,
}

impl ExtensionHost {
    /// Construct the host. Never spawns, never detects Bun. Fails only when
    /// a discovered extension path cannot be canonicalized.
    pub fn new(config: ExtensionHostConfig) -> Result<Self, ExtensionPathError> {
        let paths = canonicalize_paths(&config.extension_paths)?;
        let state = if paths.is_empty() {
            BridgeState::NotNeeded
        } else {
            BridgeState::Detected
        };
        let (launcher, resolver) = match config.launcher {
            LauncherSource::Resolved(launcher) => (Some(launcher), None),
            LauncherSource::Lazy(resolver) => (None, Some(resolver)),
        };
        let (incoming_tx, incoming_rx) = mpsc::channel(config.client.incoming_queue);
        Ok(Self {
            paths,
            init: config.init,
            timeouts: config.timeouts,
            client_config: config.client,
            incoming_tx,
            incoming_rx: parking_lot::Mutex::new(Some(incoming_rx)),
            inner: Mutex::new(HostInner {
                state,
                connection: None,
                launcher,
                resolver,
            }),
        })
    }

    /// Canonical, deduplicated extension paths.
    pub fn extension_paths(&self) -> &[PathBuf] {
        &self.paths
    }

    /// The inbound application-frame stream. One receiver per host, valid
    /// across respawns; take it before the first [`ensure_ready`](Self::ensure_ready).
    pub fn take_incoming(&self) -> Option<mpsc::Receiver<Incoming>> {
        self.incoming_rx.lock().take()
    }

    /// Ordering fence over the inbound frame stream: resolves once every
    /// inbound frame enqueued before this call has been fully handled by
    /// the action server (which processes notifications serially, in
    /// arrival order). Call AFTER receiving a response to guarantee the
    /// notifications the sidecar sent before that response took effect.
    /// Resolves immediately when no consumer is attached (receiver dropped
    /// or channel closed) — ordering is then moot.
    pub async fn barrier(&self) {
        let (tx, rx) = tokio::sync::oneshot::channel();
        if self.incoming_tx.send(Incoming::Barrier(tx)).await.is_ok() {
            let _ = rx.await;
        }
    }

    /// The live connection, if any (no spawn, no respawn).
    pub async fn current_connection(&self) -> Option<Arc<SidecarConnection>> {
        let mut inner = self.inner.lock().await;
        self.refresh(&mut inner);
        inner.connection.clone()
    }

    /// In-place extension reload (`ctx.reload()`): re-send `lifecycle/init`
    /// on the LIVE connection. The sidecar re-discovers and re-loads
    /// extensions (pi's runner replacement — no process restart) and emits a
    /// fresh `lifecycle/initialized` before answering. Requires `Ready`.
    pub async fn reinit(&self) -> Result<InitializedParams, HostError> {
        let connection = {
            let mut inner = self.inner.lock().await;
            self.refresh(&mut inner);
            match (&inner.state, &inner.connection) {
                (BridgeState::Ready, Some(connection)) => Arc::clone(connection),
                (BridgeState::NotNeeded, _) => return Err(HostError::NotNeeded),
                (BridgeState::Disabled(reason), _) => {
                    return Err(HostError::Disabled(reason.clone()));
                }
                (BridgeState::Dead(DeadReason::Shutdown) | BridgeState::Draining, _) => {
                    return Err(HostError::ShutDown);
                }
                (BridgeState::Dead(reason), _) => {
                    return Err(HostError::Failed(reason.clone()));
                }
                _ => {
                    return Err(HostError::Failed(DeadReason::LoadFailed(
                        "sidecar is not ready for reload".to_string(),
                    )));
                }
            }
        };
        let mut params = (self.init)();
        params.configured_paths = self.paths.iter().map(|p| path_string(p)).collect();
        connection
            .request_timeout(Request::LifecycleInit(Box::new(params)), self.timeouts.init)
            .await?;
        connection
            .initialized()
            .ok_or_else(|| HostError::Decode("reload without lifecycle/initialized".to_string()))
    }

    /// Current lifecycle state (a dead connection is folded in).
    pub async fn state(&self) -> BridgeState {
        let mut inner = self.inner.lock().await;
        self.refresh(&mut inner);
        inner.state.clone()
    }

    /// Lazily spawn (or respawn) the sidecar and return the ready connection.
    ///
    /// `Err(NotNeeded)` when zero extensions are installed — no process ever
    /// exists in that case.
    pub async fn ensure_ready(&self) -> Result<Arc<SidecarConnection>, HostError> {
        let mut inner = self.inner.lock().await;
        self.refresh(&mut inner);
        match inner.state.clone() {
            BridgeState::NotNeeded => Err(HostError::NotNeeded),
            BridgeState::Disabled(reason) => Err(HostError::Disabled(reason)),
            BridgeState::Ready => Ok(Arc::clone(
                inner.connection.as_ref().expect("ready implies connection"),
            )),
            BridgeState::Dead(DeadReason::Shutdown) | BridgeState::Draining => {
                Err(HostError::ShutDown)
            }
            BridgeState::Detected => {
                let launcher = match self.resolve_launcher(&mut inner) {
                    Ok(launcher) => launcher,
                    Err(error) => return Err(error),
                };
                match self.attempt(&mut inner, &launcher, true).await {
                    Ok(connection) => Ok(connection),
                    Err(reason) => {
                        inner.transition(BridgeState::Dead(reason.clone()));
                        Err(HostError::Failed(reason))
                    }
                }
            }
            // One respawn attempt per death; a failed respawn disables.
            BridgeState::Dead(_) => {
                let launcher = match self.resolve_launcher(&mut inner) {
                    Ok(launcher) => launcher,
                    Err(error) => return Err(error),
                };
                inner.transition(BridgeState::Respawning);
                match self.attempt(&mut inner, &launcher, false).await {
                    Ok(connection) => Ok(connection),
                    Err(reason) => {
                        let disabled = DisabledReason::RespawnFailed(reason.to_string());
                        inner.transition(BridgeState::Disabled(disabled.clone()));
                        Err(HostError::Disabled(disabled))
                    }
                }
            }
            state @ (BridgeState::Spawning
            | BridgeState::Handshaking
            | BridgeState::Loading
            | BridgeState::Respawning) => {
                // Attempts run to completion under the inner lock; a transient
                // phase can never be observed here.
                unreachable!("bridge left in transient state {state:?}")
            }
        }
    }

    /// Resolve (once) and cache the launch spec. Failure disables extensions.
    fn resolve_launcher(&self, inner: &mut HostInner) -> Result<SidecarLauncher, HostError> {
        if let Some(launcher) = &inner.launcher {
            return Ok(launcher.clone());
        }
        let resolver = inner
            .resolver
            .take()
            .expect("unresolved launcher has a resolver");
        match resolver() {
            Ok(launcher) => {
                inner.launcher = Some(launcher.clone());
                Ok(launcher)
            }
            Err(error) => {
                let disabled = DisabledReason::LaunchUnavailable(error.to_string());
                inner.state = BridgeState::Disabled(disabled.clone());
                Err(HostError::Disabled(disabled))
            }
        }
    }

    /// Incremental extension loading (`lifecycle/load`; later installs only —
    /// initial loading happens inside `lifecycle/init`).
    pub async fn load_more(&self, paths: &[PathBuf]) -> Result<LoadOutcome, HostError> {
        let canonical = canonicalize_paths(paths)?;
        let connection = self.ensure_ready().await?;
        let params = LoadParams {
            paths: canonical.iter().map(|p| path_string(p)).collect(),
        };
        let value = connection.request(Request::LifecycleLoad(params)).await?;
        serde_json::from_value(value).map_err(|error| HostError::Decode(error.to_string()))
    }

    /// Graceful, bounded shutdown: `lifecycle/shutdown`, then a grace period
    /// for voluntary exit, then a process-group kill. Terminal.
    pub async fn shutdown(&self) {
        let mut inner = self.inner.lock().await;
        self.refresh(&mut inner);
        let Some(connection) = inner.connection.take() else {
            if !matches!(
                inner.state,
                BridgeState::NotNeeded | BridgeState::Disabled(_)
            ) {
                inner.state = BridgeState::Dead(DeadReason::Shutdown);
            }
            return;
        };
        inner.transition(BridgeState::Draining);
        let grace = self.timeouts.shutdown_grace;
        let _ = connection
            .request_timeout(Request::LifecycleShutdown(Empty {}), grace)
            .await;
        if tokio::time::timeout(grace, connection.wait_closed())
            .await
            .is_err()
        {
            connection.kill();
        }
        // Wait for the reap so exit status is recorded and no zombie remains;
        // the monitor kills on its own if the voluntary exit stalled.
        if tokio::time::timeout(grace, connection.wait_exit())
            .await
            .is_err()
        {
            connection.kill();
            let _ = tokio::time::timeout(grace, connection.wait_exit()).await;
        }
        inner.transition(BridgeState::Dead(DeadReason::Shutdown));
    }

    /// Registrations reported by the sidecar's load phase, if ready.
    pub async fn initialized(&self) -> Option<InitializedParams> {
        let inner = self.inner.lock().await;
        inner
            .connection
            .as_ref()
            .and_then(|connection| connection.initialized())
    }

    /// Fold a dead connection into the state machine.
    fn refresh(&self, inner: &mut HostInner) {
        if let Some(connection) = &inner.connection
            && let Some(reason) = connection.closed()
        {
            inner.connection = None;
            if matches!(inner.state, BridgeState::Ready) {
                inner.transition(BridgeState::Dead(reason));
            }
        }
    }

    /// One full spawn→handshake→init cycle. On error the process is dead
    /// (killed where necessary) and the caller decides the resulting state.
    async fn attempt(
        &self,
        inner: &mut HostInner,
        launcher: &SidecarLauncher,
        phase_states: bool,
    ) -> Result<Arc<SidecarConnection>, DeadReason> {
        if phase_states {
            inner.transition(BridgeState::Spawning);
        }
        let process =
            spawn_sidecar(launcher).map_err(|error| DeadReason::SpawnFailed(error.to_string()))?;
        if phase_states {
            inner.transition(BridgeState::Handshaking);
        }
        let connection = SidecarConnection::establish(
            process,
            &self.client_config,
            self.timeouts.handshake,
            self.incoming_tx.clone(),
        )
        .await
        .map_err(|error| DeadReason::HandshakeFailed(error.to_string()))?;
        if phase_states {
            inner.transition(BridgeState::Loading);
        }
        let mut params = (self.init)();
        params.configured_paths = self.paths.iter().map(|p| path_string(p)).collect();
        let init = connection
            .request_timeout(Request::LifecycleInit(Box::new(params)), self.timeouts.init)
            .await;
        if let Err(error) = init {
            connection.kill();
            return Err(DeadReason::LoadFailed(error.to_string()));
        }
        // The sidecar emits `lifecycle/initialized` before answering init
        // (C3 host contract), and the reader processes frames in order, so
        // by the time the init response resolved it must be recorded. A
        // sidecar that answers init without initializing is broken.
        if connection.initialized().is_none() {
            connection.kill();
            return Err(DeadReason::LoadFailed(
                "init succeeded without lifecycle/initialized".to_string(),
            ));
        }
        inner.transition(BridgeState::Ready);
        inner.connection = Some(Arc::clone(&connection));
        Ok(connection)
    }
}

/// Canonicalize and deduplicate extension paths; a path that cannot be
/// canonicalized (vanished, permissions) is a loud error, never a silent
/// fallthrough to an ambiguous raw path.
fn canonicalize_paths(paths: &[PathBuf]) -> Result<Vec<PathBuf>, ExtensionPathError> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::with_capacity(paths.len());
    for path in paths {
        let canonical = std::fs::canonicalize(path).map_err(|error| ExtensionPathError {
            path: path.clone(),
            error: error.to_string(),
        })?;
        if seen.insert(canonical.clone()) {
            out.push(canonical);
        }
    }
    Ok(out)
}

fn path_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}
