//! cwd-bound runtime services — port of `core/agent-session-services.ts`.
//!
//! `create_agent_session_services` performs the oracle's eager, strictly
//! ordered construction: resolve cwd → agent dir → AuthStorage
//! (`agentDir/auth.json`) → SettingsManager → ModelRegistry
//! (`agentDir/models.json`) → DefaultResourceLoader + reload → provider
//! registrations → flag application. Provider registrations and extension
//! flags are Phase 6 (extensions); their steps are no-ops behind the
//! `ExtensionBridge` seam today.

use std::path::PathBuf;
use std::sync::Arc;

use parking_lot::Mutex;
use tokio::sync::RwLock;

use crate::auth_storage::AuthStorage;
use crate::config::{get_agent_dir, resolve_path};
use crate::model_registry::ModelRegistry;
use crate::resource_loader::{DefaultResourceLoader, ResourceLoaderOptions};
use crate::settings_manager::SettingsManager;

/// Non-fatal issue collected during services/session creation (oracle
/// `AgentSessionRuntimeDiagnostic`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuntimeDiagnostic {
    pub level: DiagnosticLevel,
    pub message: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiagnosticLevel {
    Info,
    Warning,
    Error,
}

/// Coherent cwd-bound runtime services (oracle `AgentSessionServices`).
#[derive(Clone)]
pub struct AgentSessionServices {
    pub cwd: PathBuf,
    pub agent_dir: PathBuf,
    pub auth_storage: Arc<AuthStorage>,
    pub settings_manager: Arc<Mutex<SettingsManager>>,
    pub model_registry: Arc<RwLock<ModelRegistry>>,
    pub resource_loader: Arc<Mutex<DefaultResourceLoader>>,
    pub diagnostics: Vec<RuntimeDiagnostic>,
}

/// Inputs for [`create_agent_session_services`] (oracle
/// `CreateAgentSessionServicesOptions`). Pre-existing shared services may be
/// injected; anything absent is created in oracle order.
#[derive(Default)]
pub struct CreateAgentSessionServicesOptions {
    pub cwd: PathBuf,
    pub agent_dir: Option<PathBuf>,
    pub auth_storage: Option<Arc<AuthStorage>>,
    pub settings_manager: Option<Arc<Mutex<SettingsManager>>>,
    pub model_registry: Option<Arc<RwLock<ModelRegistry>>>,
    /// Additional CLI resource paths merged into loader options.
    pub resource_loader_options: Option<ResourceLoaderOptions>,
}

/// Create cwd-bound runtime services in the oracle's construction order.
pub fn create_agent_session_services(
    options: CreateAgentSessionServicesOptions,
) -> AgentSessionServices {
    // 1. Resolve cwd.
    let cwd = resolve_path(&options.cwd.to_string_lossy(), None);
    // 2. Agent dir.
    let agent_dir = options
        .agent_dir
        .map(|dir| resolve_path(&dir.to_string_lossy(), None))
        .unwrap_or_else(get_agent_dir);
    // 3. AuthStorage bound to agentDir/auth.json.
    let auth_storage = options
        .auth_storage
        .unwrap_or_else(|| Arc::new(AuthStorage::new(agent_dir.join("auth.json"))));
    // 4. SettingsManager for (cwd, agentDir).
    let settings_manager = options.settings_manager.unwrap_or_else(|| {
        Arc::new(Mutex::new(SettingsManager::create(
            &cwd,
            Some(agent_dir.clone()),
        )))
    });
    // 5. ModelRegistry sharing the SAME AuthStorage, catalog from models.json.
    let model_registry = options.model_registry.unwrap_or_else(|| {
        Arc::new(RwLock::new(ModelRegistry::create(
            auth_storage.clone(),
            agent_dir.join("models.json"),
        )))
    });
    // 6. Resource loader + reload (discovery happens at construction).
    let loader_options = {
        let mut base = options
            .resource_loader_options
            .unwrap_or_else(|| ResourceLoaderOptions::new(&cwd));
        base.cwd = cwd.clone();
        base.agent_dir = agent_dir.clone();
        base
    };
    let resource_loader = {
        let settings = settings_manager.lock();
        Arc::new(Mutex::new(DefaultResourceLoader::from_settings(
            loader_options,
            &settings,
        )))
    };

    // 7-8. Provider registrations + extension flag application are extension
    // runtime steps (Phase 6). Zero-extension operation registers nothing.
    let diagnostics = Vec::new();

    AgentSessionServices {
        cwd,
        agent_dir,
        auth_storage,
        settings_manager,
        model_registry,
        resource_loader,
        diagnostics,
    }
}
