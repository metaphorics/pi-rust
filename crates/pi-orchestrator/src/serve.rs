//! Serve-mode bootstrap and shutdown (port of serve.ts).
//!
//! The oracle's `serve()` also owns console banners and `process.exit`; those
//! are CLI concerns and live in the binary (main.rs). This module keeps the
//! lifecycle core: start the socket server, recover persisted instances,
//! start Radius presence, and run the once-guarded shutdown sequence
//! (close server → supervisor shutdown → presence stop → unlink socket).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::handler::OrchestratorIpcHandler;
use crate::ipc::{IpcServer, IpcServerError, start_ipc_server_at};
use crate::radius::{Presence, RadiusError};
use crate::storage::Storage;
use crate::supervisor::{Supervisor, SupervisorError};
use crate::types::MachineRecord;

#[derive(Debug, thiserror::Error)]
pub enum ServeError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Ipc(#[from] IpcServerError),
    #[error(transparent)]
    Supervisor(#[from] SupervisorError),
    #[error(transparent)]
    Radius(#[from] RadiusError),
}

pub struct ServeOptions {
    pub socket_path: PathBuf,
    pub storage: Storage,
    pub presence: Arc<dyn Presence>,
    /// Machine label forwarded to presence registration (the oracle CLI
    /// exposes none; `serve` passes `None`).
    pub label: Option<String>,
    /// Test-only child command injection threaded into every spawn.
    pub spawn_command_override: Option<(PathBuf, Vec<String>)>,
}

/// A started orchestrator: socket server listening, recovery done, presence
/// started. Dropping without [`RunningServe::shutdown`] still closes the
/// server and unlinks the socket (via [`IpcServer`]'s drop), but skips
/// supervisor/presence teardown — call `shutdown` for the full sequence.
pub struct RunningServe {
    socket_path: PathBuf,
    server: Option<IpcServer>,
    supervisor: Supervisor,
    presence: Arc<dyn Presence>,
    /// Radius machine record when presence registration ran (banner data).
    pub machine: Option<MachineRecord>,
}

/// Bootstrap in oracle order: socket dir, IPC server (stale-socket probe),
/// `recoverAfterRestart`, presence start. A failure after the server is
/// listening closes it and unlinks the socket before propagating
/// (serve.ts:26-35).
pub async fn start(options: ServeOptions) -> Result<RunningServe, ServeError> {
    if let Some(parent) = options.socket_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let supervisor = Supervisor::new(options.storage, Arc::clone(&options.presence));
    let handler = Arc::new(match options.spawn_command_override {
        Some(command) => {
            OrchestratorIpcHandler::with_spawn_command_override(supervisor.clone(), command)
        }
        None => OrchestratorIpcHandler::new(supervisor.clone()),
    });
    let server = start_ipc_server_at(&options.socket_path, handler).await?;

    let started: Result<Option<MachineRecord>, ServeError> = async {
        supervisor.recover_after_restart().await?;
        Ok(options.presence.start(options.label).await?)
    }
    .await;

    match started {
        Ok(machine) => Ok(RunningServe {
            socket_path: options.socket_path,
            server: Some(server),
            supervisor,
            presence: options.presence,
            machine,
        }),
        Err(error) => {
            // Best-effort teardown mirrors the oracle's close + unlink; the
            // startup error stays the reported failure.
            if let Err(shutdown_error) = server.shutdown().await {
                log::error!("Failed to close IPC server after startup error: {shutdown_error}");
            }
            Err(error)
        }
    }
}

impl RunningServe {
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    pub fn supervisor(&self) -> &Supervisor {
        &self.supervisor
    }

    /// Once-guarded shutdown (serve.ts:41-58): close the server, stop every
    /// live instance, stop presence, unlink the socket. Every step runs even
    /// when an earlier one fails; the first error is returned and later
    /// masked errors are logged.
    pub async fn shutdown(&mut self) -> Result<(), ServeError> {
        let Some(server) = self.server.take() else {
            return Ok(());
        };
        let mut first_error: Option<ServeError> = None;
        let mut record = |error: ServeError| match &first_error {
            Some(_) => log::error!("Masked error during orchestrator shutdown: {error}"),
            None => first_error = Some(error),
        };
        if let Err(error) = server.shutdown().await {
            record(error.into());
        }
        if let Err(error) = self.supervisor.shutdown().await {
            record(error.into());
        }
        if let Err(error) = self.presence.stop().await {
            record(error.into());
        }
        if let Err(error) = std::fs::remove_file(&self.socket_path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            record(error.into());
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}
