//! Sidecar process spawning: piped stdio, own process group, kill-on-drop.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use thiserror::Error;
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};

use crate::extensions::detect::{
    BunEnvironment, BunResolveError, SidecarResolveError, resolve_bun, resolve_sidecar_entry,
};

/// Fully resolved launch spec for the Bun sidecar.
#[derive(Clone, Debug)]
pub struct SidecarLauncher {
    /// Canonical Bun executable (or, in tests, a fake-sidecar script).
    pub bun: PathBuf,
    /// Canonical sidecar entry script.
    pub entry: PathBuf,
    /// Working directory for the sidecar process.
    pub cwd: PathBuf,
    /// Extra environment variables.
    pub envs: Vec<(OsString, OsString)>,
}

#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum LaunchResolveError {
    #[error(transparent)]
    Bun(#[from] BunResolveError),
    #[error(transparent)]
    Sidecar(#[from] SidecarResolveError),
}

impl SidecarLauncher {
    /// Resolve Bun and the sidecar entry from the given environment inputs.
    pub fn resolve(
        env: &BunEnvironment,
        sidecar_override: Option<&Path>,
        package_dir: &Path,
        cwd: &Path,
    ) -> Result<Self, LaunchResolveError> {
        let bun = resolve_bun(env)?;
        let entry = resolve_sidecar_entry(sidecar_override, package_dir)?;
        Ok(Self {
            bun,
            entry,
            cwd: cwd.to_path_buf(),
            envs: Vec::new(),
        })
    }
}

/// A spawned sidecar with its stdio pipes already split out.
pub(crate) struct SidecarProcess {
    pub child: Child,
    pub pid: Option<u32>,
    pub stdin: ChildStdin,
    pub stdout: ChildStdout,
    pub stderr: ChildStderr,
}

pub(crate) fn spawn_sidecar(launcher: &SidecarLauncher) -> std::io::Result<SidecarProcess> {
    let mut command = Command::new(&launcher.bun);
    command
        .arg(&launcher.entry)
        .current_dir(&launcher.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    for (key, value) in &launcher.envs {
        command.env(key, value);
    }
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command.spawn()?;
    let pid = child.id();
    let stdin = child.stdin.take().expect("stdin piped");
    let stdout = child.stdout.take().expect("stdout piped");
    let stderr = child.stderr.take().expect("stderr piped");
    Ok(SidecarProcess {
        child,
        pid,
        stdin,
        stdout,
        stderr,
    })
}

/// Kill the sidecar's whole process group (it is its own group leader).
#[cfg(unix)]
pub(crate) fn kill_process_tree(child: &mut Child, pid: Option<u32>) {
    use rustix::process::{Pid, Signal, kill_process_group};
    if let Some(pid) = pid.and_then(|pid| Pid::from_raw(pid as i32)) {
        let _ = kill_process_group(pid, Signal::KILL);
    } else {
        let _ = child.start_kill();
    }
}

#[cfg(not(unix))]
pub(crate) fn kill_process_tree(child: &mut Child, _pid: Option<u32>) {
    let _ = child.start_kill();
}
