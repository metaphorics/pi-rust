//! AgentSessionRuntime — port of `core/agent-session-runtime.ts`.
//!
//! Owns the current [`AgentSession`] plus its cwd-bound services. Session
//! replacement (switch/new/fork) tears down the current session first, then
//! creates and applies the next runtime via the stored factory. Extension
//! lifecycle hooks route through [`ExtensionBridge::emit_lifecycle`];
//! [`NoopExtensionBridge`] always continues.

use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use parking_lot::Mutex;

use crate::extension_bridge::{
    ExtensionBridge, ForkPosition, HookOutcome, SessionLifecycleEvent, SessionShutdownReason,
    SessionStartReason,
};
use crate::session_manager::SessionManager;

use super::services::{AgentSessionServices, RuntimeDiagnostic};
use super::{AgentSession, extract_user_content_text};

/// Inputs handed to the runtime factory for one (re)creation.
pub struct CreateRuntimeOptions {
    pub cwd: PathBuf,
    pub agent_dir: PathBuf,
    pub session_manager: SessionManager,
    pub session_start_reason: SessionStartReason,
    pub previous_session_file: Option<PathBuf>,
}

/// Result returned by the runtime factory (oracle
/// `CreateAgentSessionRuntimeResult`).
pub struct CreateRuntimeResult {
    pub session: AgentSession,
    pub services: AgentSessionServices,
    pub diagnostics: Vec<RuntimeDiagnostic>,
    pub model_fallback_message: Option<String>,
}

type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Factory creating a full runtime for a target cwd and session manager
/// (oracle `CreateAgentSessionRuntimeFactory`).
pub type CreateRuntimeFactory = Arc<
    dyn Fn(CreateRuntimeOptions) -> BoxFuture<'static, Result<CreateRuntimeResult, String>>
        + Send
        + Sync,
>;

/// Callback re-binding mode I/O to a replacement session.
pub type RebindSessionFn = Arc<dyn Fn(AgentSession) + Send + Sync>;

struct RuntimeState {
    session: AgentSession,
    services: AgentSessionServices,
    diagnostics: Vec<RuntimeDiagnostic>,
    model_fallback_message: Option<String>,
    rebind_session: Option<RebindSessionFn>,
}

/// Result of a session replacement attempt.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReplaceResult {
    pub cancelled: bool,
    /// Fork with `position: before` returns the selected user text.
    pub selected_text: Option<String>,
}

pub struct AgentSessionRuntime {
    state: Mutex<RuntimeState>,
    create_runtime: CreateRuntimeFactory,
    bridge: Arc<dyn ExtensionBridge>,
}

impl AgentSessionRuntime {
    /// Create the initial runtime (oracle `createAgentSessionRuntime`).
    pub async fn create(
        create_runtime: CreateRuntimeFactory,
        options: CreateRuntimeOptions,
        bridge: Arc<dyn ExtensionBridge>,
    ) -> Result<Self, String> {
        assert_session_cwd_exists(&options.session_manager, &options.cwd)?;
        let result = (create_runtime)(options).await?;
        Ok(Self {
            state: Mutex::new(RuntimeState {
                session: result.session,
                services: result.services,
                diagnostics: result.diagnostics,
                model_fallback_message: result.model_fallback_message,
                rebind_session: None,
            }),
            create_runtime,
            bridge,
        })
    }

    pub fn session(&self) -> AgentSession {
        self.state.lock().session.clone()
    }

    pub fn services(&self) -> AgentSessionServices {
        self.state.lock().services.clone()
    }

    pub fn cwd(&self) -> PathBuf {
        self.state.lock().services.cwd.clone()
    }

    pub fn diagnostics(&self) -> Vec<RuntimeDiagnostic> {
        self.state.lock().diagnostics.clone()
    }

    pub fn model_fallback_message(&self) -> Option<String> {
        self.state.lock().model_fallback_message.clone()
    }

    pub fn set_rebind_session(&self, rebind_session: Option<RebindSessionFn>) {
        self.state.lock().rebind_session = rebind_session;
    }

    fn emit_before_switch(
        &self,
        reason: SessionStartReason,
        target_session_file: Option<PathBuf>,
    ) -> bool {
        matches!(
            self.bridge
                .emit_lifecycle(&SessionLifecycleEvent::SessionBeforeSwitch {
                    reason,
                    target_session_file,
                }),
            HookOutcome::Cancel
        )
    }

    fn emit_before_fork(&self, entry_id: &str, position: ForkPosition) -> bool {
        matches!(
            self.bridge
                .emit_lifecycle(&SessionLifecycleEvent::SessionBeforeFork {
                    entry_id: entry_id.to_string(),
                    position,
                }),
            HookOutcome::Cancel
        )
    }

    /// Oracle `teardownCurrent`: shutdown hooks, then dispose the session.
    fn teardown_current(
        &self,
        reason: SessionShutdownReason,
        target_session_file: Option<PathBuf>,
    ) {
        let session = self.session();
        let _ = self
            .bridge
            .emit_lifecycle(&SessionLifecycleEvent::SessionShutdown {
                reason,
                target_session_file,
            });
        session.dispose();
    }

    fn apply(&self, result: CreateRuntimeResult) {
        let mut state = self.state.lock();
        state.session = result.session;
        state.services = result.services;
        state.diagnostics = result.diagnostics;
        state.model_fallback_message = result.model_fallback_message;
    }

    fn finish_session_replacement(&self) {
        let (rebind, session) = {
            let state = self.state.lock();
            (state.rebind_session.clone(), state.session.clone())
        };
        if let Some(rebind) = rebind {
            rebind(session);
        }
    }

    /// Switch to an existing session file (oracle `switchSession`).
    pub async fn switch_session(
        &self,
        session_path: &Path,
        cwd_override: Option<&str>,
    ) -> Result<ReplaceResult, String> {
        if self.emit_before_switch(
            SessionStartReason::Resume,
            Some(session_path.to_path_buf()),
        ) {
            return Ok(ReplaceResult {
                cancelled: true,
                selected_text: None,
            });
        }

        let previous_session_file = self.session().session_file();
        let session_manager = SessionManager::open(session_path, None, cwd_override)
            .map_err(|e| e.to_string())?;
        assert_session_cwd_exists(&session_manager, &self.cwd())?;
        let target_file = session_manager.get_session_file().map(PathBuf::from);
        self.teardown_current(SessionShutdownReason::Resume, target_file);

        let agent_dir = self.services().agent_dir.clone();
        let cwd = session_manager.get_cwd().to_path_buf();
        let result = (self.create_runtime)(CreateRuntimeOptions {
            cwd,
            agent_dir,
            session_manager,
            session_start_reason: SessionStartReason::Resume,
            previous_session_file,
        })
        .await?;
        self.apply(result);
        self.finish_session_replacement();
        Ok(ReplaceResult::default())
    }

    /// Start a fresh session in the current cwd (oracle `newSession`).
    pub async fn new_session(
        &self,
        parent_session: Option<String>,
    ) -> Result<ReplaceResult, String> {
        if self.emit_before_switch(SessionStartReason::New, None) {
            return Ok(ReplaceResult {
                cancelled: true,
                selected_text: None,
            });
        }

        let session = self.session();
        let previous_session_file = session.session_file();
        let cwd = self.cwd();
        let (is_persisted, session_dir) = session.with_session_manager(|sm| {
            (sm.is_persisted(), sm.get_session_dir().to_path_buf())
        });

        let mut session_manager = if is_persisted {
            SessionManager::create(&cwd, Some(session_dir), None).map_err(|e| e.to_string())?
        } else {
            SessionManager::in_memory(Some(&cwd.to_string_lossy()), None)
                .map_err(|e| e.to_string())?
        };
        if let Some(parent) = parent_session {
            session_manager
                .new_session(crate::session_manager::NewSessionOptions {
                    parent_session: Some(parent),
                    ..Default::default()
                })
                .map_err(|e| e.to_string())?;
        }

        let target_file = session_manager.get_session_file().map(PathBuf::from);
        self.teardown_current(SessionShutdownReason::New, target_file);

        let agent_dir = self.services().agent_dir.clone();
        let result = (self.create_runtime)(CreateRuntimeOptions {
            cwd,
            agent_dir,
            session_manager,
            session_start_reason: SessionStartReason::New,
            previous_session_file,
        })
        .await?;
        self.apply(result);
        self.finish_session_replacement();
        Ok(ReplaceResult::default())
    }

    /// Fork from an entry into a new session (oracle `fork`).
    ///
    /// `position: Before` (default) targets a user message and returns its
    /// text for the editor; `position: At` keeps the selected entry.
    pub async fn fork(
        &self,
        entry_id: &str,
        position: ForkPosition,
    ) -> Result<ReplaceResult, String> {
        if self.emit_before_fork(entry_id, position) {
            return Ok(ReplaceResult {
                cancelled: true,
                selected_text: None,
            });
        }

        let session = self.session();
        let selected_entry = session
            .with_session_manager(|sm| sm.get_entry(entry_id).cloned())
            .ok_or_else(|| "Invalid entry ID for forking".to_string())?;

        let mut selected_text: Option<String> = None;
        let target_leaf_id: Option<String> = match position {
            ForkPosition::At => selected_entry.id().map(str::to_string),
            ForkPosition::Before => {
                let crate::session_types::SessionEntry::Message {
                    message, parent_id, ..
                } = &selected_entry
                else {
                    return Err("Invalid entry ID for forking".to_string());
                };
                if message.get("role").and_then(serde_json::Value::as_str) != Some("user") {
                    return Err("Invalid entry ID for forking".to_string());
                }
                selected_text = Some(extract_user_content_text(message.get("content")));
                parent_id.as_option().cloned()
            }
        };

        let previous_session_file = session.session_file();
        let cwd = self.cwd();
        let agent_dir = self.services().agent_dir.clone();
        let (is_persisted, session_dir) = session.with_session_manager(|sm| {
            (sm.is_persisted(), sm.get_session_dir().to_path_buf())
        });

        let session_manager = if is_persisted {
            let current_session_file = session
                .session_file()
                .ok_or_else(|| "Persisted session is missing a session file".to_string())?;

            match &target_leaf_id {
                None => {
                    let mut session_manager =
                        SessionManager::create(&cwd, Some(session_dir), None)
                            .map_err(|e| e.to_string())?;
                    session_manager
                        .new_session(crate::session_manager::NewSessionOptions {
                            parent_session: Some(
                                current_session_file.to_string_lossy().into_owned(),
                            ),
                            ..Default::default()
                        })
                        .map_err(|e| e.to_string())?;
                    session_manager
                }
                Some(target_leaf_id) => {
                    let mut session_manager = SessionManager::open(
                        &current_session_file,
                        Some(session_dir),
                        None,
                    )
                    .map_err(|e| e.to_string())?;
                    session_manager
                        .create_branched_session(target_leaf_id)
                        .map_err(|e| e.to_string())?
                        .ok_or_else(|| "Failed to create forked session".to_string())?;
                    session_manager
                }
            }
        } else {
            // In-memory: oracle mutates the live session's manager, then the
            // same manager moves into the new runtime. Mutate in place, tear
            // down, then take the manager out of the disposed session.
            match &target_leaf_id {
                None => {
                    let parent = previous_session_file
                        .as_ref()
                        .map(|p| p.to_string_lossy().into_owned());
                    session.with_session_manager_mut(|sm| {
                        sm.new_session(crate::session_manager::NewSessionOptions {
                            parent_session: parent,
                            ..Default::default()
                        })
                        .map(|_| ())
                        .map_err(|e| e.to_string())
                    })?;
                }
                Some(target_leaf_id) => {
                    session.with_session_manager_mut(|sm| {
                        sm.create_branched_session(target_leaf_id)
                            .map(|_| ())
                            .map_err(|e| e.to_string())
                    })?;
                }
            }
            let target_file = session.session_file();
            self.teardown_current(SessionShutdownReason::Fork, target_file);
            let session_manager = session.take_session_manager();

            let result = (self.create_runtime)(CreateRuntimeOptions {
                cwd,
                agent_dir,
                session_manager,
                session_start_reason: SessionStartReason::Fork,
                previous_session_file,
            })
            .await?;
            self.apply(result);
            self.finish_session_replacement();
            return Ok(ReplaceResult {
                cancelled: false,
                selected_text,
            });
        };

        let target_file = session_manager.get_session_file().map(PathBuf::from);
        self.teardown_current(SessionShutdownReason::Fork, target_file);
        let fork_cwd = if is_persisted && target_leaf_id.is_some() {
            session_manager.get_cwd().to_path_buf()
        } else {
            cwd
        };
        let result = (self.create_runtime)(CreateRuntimeOptions {
            cwd: fork_cwd,
            agent_dir,
            session_manager,
            session_start_reason: SessionStartReason::Fork,
            previous_session_file,
        })
        .await?;
        self.apply(result);
        self.finish_session_replacement();
        Ok(ReplaceResult {
            cancelled: false,
            selected_text,
        })
    }

    /// Shut down the runtime (oracle `dispose`).
    pub fn dispose(&self) {
        self.teardown_current(SessionShutdownReason::Quit, None);
    }
}

/// Oracle `assertSessionCwdExists` / `formatMissingSessionCwdError`.
fn assert_session_cwd_exists(
    session_manager: &SessionManager,
    fallback_cwd: &Path,
) -> Result<(), String> {
    let Some(session_file) = session_manager.get_session_file() else {
        return Ok(());
    };
    let session_cwd = session_manager.get_cwd();
    if session_cwd.as_os_str().is_empty() || session_cwd.exists() {
        return Ok(());
    }
    Err(format!(
        "Stored session working directory does not exist: {}\nSession file: {}\nCurrent working directory: {}",
        session_cwd.display(),
        session_file.display(),
        fallback_cwd.display()
    ))
}
