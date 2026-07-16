use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::Duration;

use async_trait::async_trait;
use pi_ai::auth::{Credential, CredentialStore, FileCredentialStore};
use rand::Rng;
use serde_json::{Map, Value, json};
use tokio::task::JoinHandle;

use crate::config;
use crate::storage::{Storage, StorageError};
use crate::types::{InstanceRecord, MachineRecord, RadiusRegistration, now_iso_timestamp};

const DEFAULT_RADIUS_URL: &str = "https://radius.pi.dev/";
const DEFAULT_ORCHESTRATOR_BASE_PATH: &str = "/v1/";
const NOT_FOUND_RETRY_THRESHOLD: u32 = 3;
const HEARTBEAT_BACKOFF_BASE_MS: u64 = 1_000;
const HEARTBEAT_BACKOFF_MAX_MS: u64 = 30_000;
const RADIUS_PROVIDER: &str = "radius";

#[derive(Debug, thiserror::Error)]
pub enum RadiusError {
    #[error("Radius credentials are required in ~/.pi/agent/auth.json or PI_RADIUS_API_KEY")]
    MissingCredentials,
    #[error("No registered machine available for Pi registration")]
    NoRegisteredMachine,
    #[error("Radius request failed: {status} {message}")]
    Http { status: u16, message: String },
    #[error("invalid Radius URL: {0}")]
    InvalidUrl(#[from] url::ParseError),
    #[error(transparent)]
    Request(#[from] reqwest::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Storage(#[from] StorageError),
}

impl RadiusError {
    pub fn http(status: u16, message: impl Into<String>) -> Self {
        Self::Http {
            status,
            message: message.into(),
        }
    }

    pub fn status(&self) -> Option<u16> {
        match self {
            Self::Http { status, .. } => Some(*status),
            _ => None,
        }
    }

    fn is_not_found(&self) -> bool {
        self.status() == Some(404)
    }
}

#[async_trait]
pub trait RadiusTransport: Send + Sync {
    async fn post(&self, url: &str, access_token: &str, body: Value) -> Result<Value, RadiusError>;
}

pub struct ReqwestRadiusTransport {
    client: reqwest::Client,
}

impl ReqwestRadiusTransport {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
        }
    }
}

impl Default for ReqwestRadiusTransport {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl RadiusTransport for ReqwestRadiusTransport {
    async fn post(&self, url: &str, access_token: &str, body: Value) -> Result<Value, RadiusError> {
        let response = self
            .client
            .post(url)
            .bearer_auth(access_token)
            .json(&body)
            .send()
            .await?;
        let status = response.status();
        let text = response.text().await?;
        if !status.is_success() {
            return Err(RadiusError::http(status.as_u16(), text));
        }
        if text.is_empty() {
            Ok(Value::Null)
        } else {
            Ok(serde_json::from_str(&text)?)
        }
    }
}

#[async_trait]
pub trait RadiusTokenSource: Send + Sync {
    async fn access_token(&self) -> Result<Option<String>, RadiusError>;
}

pub struct FileRadiusTokenSource {
    auth_path: PathBuf,
}

impl FileRadiusTokenSource {
    pub fn new(auth_path: impl Into<PathBuf>) -> Self {
        Self {
            auth_path: auth_path.into(),
        }
    }
}

impl Default for FileRadiusTokenSource {
    fn default() -> Self {
        Self::new(pi_coding_agent::config::get_auth_path())
    }
}

#[async_trait]
impl RadiusTokenSource for FileRadiusTokenSource {
    async fn access_token(&self) -> Result<Option<String>, RadiusError> {
        Ok(
            resolve_access_token_from(&self.auth_path, std::env::var("PI_RADIUS_API_KEY").ok())
                .await,
        )
    }
}

/// Resolve Radius credentials in pi's order: stored OAuth access token, then environment.
pub async fn resolve_access_token_from(
    auth_path: &Path,
    environment_token: Option<String>,
) -> Option<String> {
    let stored = FileCredentialStore::new(auth_path)
        .read(RADIUS_PROVIDER)
        .await;
    if let Ok(Some(Credential::OAuth(credential))) = stored
        && !credential.access.is_empty()
    {
        return Some(credential.access);
    }
    environment_token.filter(|token| !token.is_empty())
}

pub fn get_radius_url() -> String {
    std::env::var("PI_RADIUS_URL")
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| DEFAULT_RADIUS_URL.to_owned())
}

pub fn get_radius_orchestrator_base_url() -> Result<String, RadiusError> {
    if let Some(explicit) = std::env::var("PI_RADIUS_ORCHESTRATOR_URL")
        .ok()
        .filter(|value| !value.is_empty())
    {
        return Ok(explicit);
    }
    Ok(url::Url::parse(&get_radius_url())?
        .join(DEFAULT_ORCHESTRATOR_BASE_PATH)?
        .to_string())
}

/// Node-compatible `process.platform` value for Radius registration.
pub fn node_platform() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "win32",
        "solaris" => "sunos",
        other => other,
    }
}

/// Node-compatible `process.arch` value for Radius registration.
pub fn node_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86" => "ia32",
        "x86_64" => "x64",
        "aarch64" => "arm64",
        "powerpc" => "ppc",
        "powerpc64" => "ppc64",
        other => other,
    }
}

/// Faithful exponential backoff with a caller-supplied `[0, 1)` jitter sample.
pub fn compute_backoff_delay_ms(failure_count: u32, jitter_sample: f64) -> u64 {
    let shift = failure_count.saturating_sub(1).min(63);
    let exponential = HEARTBEAT_BACKOFF_BASE_MS
        .saturating_mul(1_u64 << shift)
        .min(HEARTBEAT_BACKOFF_MAX_MS);
    let jitter_window = 250_u64.max(exponential / 4);
    let sample = jitter_sample.clamp(0.0, 1.0 - f64::EPSILON);
    let jitter = (sample * jitter_window as f64).floor() as u64;
    exponential
        .saturating_add(jitter)
        .min(HEARTBEAT_BACKOFF_MAX_MS)
}

fn random_backoff_delay_ms(failure_count: u32) -> u64 {
    compute_backoff_delay_ms(failure_count, rand::rng().random())
}

#[async_trait]
pub trait PresenceCoordinator: Send + Sync {
    async fn get_live_instance(&self, instance_id: &str) -> Option<InstanceRecord>;
    async fn list_live_instances(&self) -> Vec<InstanceRecord>;
    async fn update_instance(&self, instance: InstanceRecord);
}

#[async_trait]
pub trait Presence: Send + Sync {
    fn set_coordinator(&self, coordinator: Weak<dyn PresenceCoordinator>);
    async fn start(&self, label: Option<String>) -> Result<Option<MachineRecord>, RadiusError>;
    async fn stop(&self) -> Result<(), RadiusError>;
    async fn register_pi(&self, instance: InstanceRecord) -> Result<InstanceRecord, RadiusError>;
    async fn disconnect_pi(&self, instance: &InstanceRecord) -> Result<(), RadiusError>;
}

#[derive(Clone, Debug)]
pub struct RadiusRuntime {
    pub orchestrator_dir: PathBuf,
    pub socket_path: PathBuf,
}

impl RadiusRuntime {
    pub fn from_config() -> Self {
        Self {
            orchestrator_dir: config::get_orchestrator_dir(),
            socket_path: config::get_socket_path(),
        }
    }
}

pub struct RadiusPresence {
    inner: Arc<RadiusInner>,
}

struct RadiusInner {
    transport: Arc<dyn RadiusTransport>,
    tokens: Arc<dyn RadiusTokenSource>,
    storage: Arc<Storage>,
    base_url: String,
    runtime: RadiusRuntime,
    state: Mutex<RadiusState>,
}

#[derive(Default)]
struct RadiusState {
    machine: Option<MachineRecord>,
    machine_timer: Option<JoinHandle<()>>,
    machine_heartbeat_active: bool,
    machine_timer_generation: u64,
    machine_heartbeat_interval_ms: u64,
    machine_consecutive_not_found_count: u32,
    machine_transient_failure_count: u32,
    pi_heartbeats: HashMap<String, PiHeartbeatState>,
    coordinator: Option<Weak<dyn PresenceCoordinator>>,
}

struct PiHeartbeatState {
    timer: Option<JoinHandle<()>>,
    generation: u64,
    interval_ms: u64,
    radius_pi_id: String,
    consecutive_not_found_count: u32,
    transient_failure_count: u32,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct RegistrationResponse {
    id: String,
    #[serde(flatten)]
    registration: RadiusRegistration,
}

impl RadiusPresence {
    pub fn new() -> Result<Self, RadiusError> {
        Ok(Self::with_dependencies(
            Arc::new(ReqwestRadiusTransport::new()),
            Arc::new(FileRadiusTokenSource::default()),
            Arc::new(Storage::from_config()),
            get_radius_orchestrator_base_url()?,
            RadiusRuntime::from_config(),
        ))
    }

    pub fn with_dependencies(
        transport: Arc<dyn RadiusTransport>,
        tokens: Arc<dyn RadiusTokenSource>,
        storage: Arc<Storage>,
        base_url: String,
        runtime: RadiusRuntime,
    ) -> Self {
        Self {
            inner: Arc::new(RadiusInner {
                transport,
                tokens,
                storage,
                base_url,
                runtime,
                state: Mutex::new(RadiusState::default()),
            }),
        }
    }

    pub async fn is_enabled(&self) -> Result<bool, RadiusError> {
        Ok(self.inner.tokens.access_token().await?.is_some())
    }

    pub fn active_timer_count(&self) -> usize {
        let state = self.inner.state();
        usize::from(state.machine_timer.is_some())
            + state
                .pi_heartbeats
                .values()
                .filter(|state| state.timer.is_some())
                .count()
    }

    async fn post(&self, path: &str, body: Value) -> Result<Value, RadiusError> {
        post(&self.inner, path, body).await
    }

    async fn register_machine(
        &self,
        label: Option<String>,
    ) -> Result<RegistrationResponse, RadiusError> {
        register_machine(&self.inner, label).await
    }
}

impl Drop for RadiusPresence {
    fn drop(&mut self) {
        if Arc::strong_count(&self.inner) != 1 {
            return;
        }
        let mut state = self.inner.state();
        if let Some(timer) = state.machine_timer.take() {
            timer.abort();
        }
        for heartbeat in state.pi_heartbeats.values_mut() {
            if let Some(timer) = heartbeat.timer.take() {
                timer.abort();
            }
        }
    }
}

#[async_trait]
impl Presence for RadiusPresence {
    fn set_coordinator(&self, coordinator: Weak<dyn PresenceCoordinator>) {
        self.inner.state().coordinator = Some(coordinator);
    }

    async fn start(&self, label: Option<String>) -> Result<Option<MachineRecord>, RadiusError> {
        if !self.is_enabled().await? {
            return Ok(None);
        }
        let registered = self.register_machine(label).await?;
        self.inner.state().machine_heartbeat_active = true;
        start_machine_heartbeat(&self.inner, registered.registration.heartbeat_interval_ms);
        Ok(self.inner.state().machine.clone())
    }

    async fn stop(&self) -> Result<(), RadiusError> {
        let machine = {
            let mut state = self.inner.state();
            state.machine_heartbeat_active = false;
            state.machine_timer_generation = state.machine_timer_generation.wrapping_add(1);
            if let Some(timer) = state.machine_timer.take() {
                timer.abort();
            }
            for heartbeat in state.pi_heartbeats.values_mut() {
                if let Some(timer) = heartbeat.timer.take() {
                    timer.abort();
                }
            }
            state.pi_heartbeats.clear();
            state.machine.clone()
        };
        if !self.is_enabled().await? {
            return Ok(());
        }
        let Some(machine) = machine else {
            return Ok(());
        };
        match self
            .post(&format!("machines/{}/disconnect", machine.id), json!({}))
            .await
        {
            Err(error) if error.is_not_found() => Ok(()),
            result => result.map(|_| ()),
        }
    }

    async fn register_pi(&self, instance: InstanceRecord) -> Result<InstanceRecord, RadiusError> {
        if !self.is_enabled().await? {
            return Ok(instance);
        }
        let machine = self
            .inner
            .state()
            .machine
            .clone()
            .or(self.inner.storage.load_machine()?)
            .ok_or(RadiusError::NoRegisteredMachine)?;
        let mut body = Map::new();
        body.insert("machineId".into(), Value::String(machine.id));
        if let Some(label) = &instance.label {
            body.insert("label".into(), Value::String(label.clone()));
        }
        body.insert("cwd".into(), Value::String(instance.cwd.clone()));
        body.insert("hostname".into(), Value::String(system_hostname()));
        body.insert("pid".into(), Value::from(std::process::id()));
        body.insert("transport".into(), Value::String("local-rpc".into()));
        body.insert(
            "capabilities".into(),
            json!({ "rpc": true, "relay": false, "iroh": false }),
        );
        if let Some(session_id) = &instance.session_id {
            body.insert("sessionId".into(), Value::String(session_id.clone()));
        }
        let response = self.post("pis/register", Value::Object(body)).await?;
        let registered: RegistrationResponse = serde_json::from_value(response)?;
        let mut registered_instance = instance;
        registered_instance.radius_pi_id = Some(registered.id.clone());
        start_pi_heartbeat(
            &self.inner,
            registered_instance.id.clone(),
            registered.registration.heartbeat_interval_ms,
            registered.id,
        );
        Ok(registered_instance)
    }

    async fn disconnect_pi(&self, instance: &InstanceRecord) -> Result<(), RadiusError> {
        {
            let mut state = self.inner.state();
            if let Some(mut heartbeat) = state.pi_heartbeats.remove(&instance.id)
                && let Some(timer) = heartbeat.timer.take()
            {
                timer.abort();
            }
        }
        if !self.is_enabled().await? {
            return Ok(());
        }
        let Some(radius_pi_id) = &instance.radius_pi_id else {
            return Ok(());
        };
        match self
            .post(&format!("pis/{radius_pi_id}/disconnect"), json!({}))
            .await
        {
            Err(error) if error.is_not_found() => Ok(()),
            result => result.map(|_| ()),
        }
    }
}

impl RadiusInner {
    fn state(&self) -> MutexGuard<'_, RadiusState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

async fn post(inner: &RadiusInner, path: &str, body: Value) -> Result<Value, RadiusError> {
    let token = inner
        .tokens
        .access_token()
        .await?
        .ok_or(RadiusError::MissingCredentials)?;
    let url = url::Url::parse(&inner.base_url)?.join(path)?.to_string();
    inner.transport.post(&url, &token, body).await
}

async fn register_machine(
    inner: &Arc<RadiusInner>,
    label: Option<String>,
) -> Result<RegistrationResponse, RadiusError> {
    let existing = inner
        .state()
        .machine
        .clone()
        .or(inner.storage.load_machine()?);
    let mut body = Map::new();
    if let Some(machine) = &existing {
        body.insert("machineId".into(), Value::String(machine.id.clone()));
    }
    if let Some(label) = &label {
        body.insert("label".into(), Value::String(label.clone()));
    }
    body.insert("hostname".into(), Value::String(system_hostname()));
    body.insert("platform".into(), Value::String(node_platform().into()));
    body.insert("arch".into(), Value::String(node_arch().into()));
    body.insert("version".into(), Value::String(config::VERSION.into()));
    body.insert(
        "capabilities".into(),
        json!({ "spawn": true, "relay": false, "iroh": false }),
    );
    let response = post(inner, "machines/register", Value::Object(body)).await?;
    let registered: RegistrationResponse = serde_json::from_value(response)?;
    let timestamp = now_iso_timestamp();
    let machine = MachineRecord {
        id: registered.id.clone(),
        created_at: existing
            .as_ref()
            .map_or_else(|| timestamp.clone(), |machine| machine.created_at.clone()),
        last_seen_at: Some(timestamp),
        label,
    };
    inner.storage.save_machine(&machine)?;
    let mut state = inner.state();
    state.machine = Some(machine);
    state.machine_consecutive_not_found_count = 0;
    state.machine_transient_failure_count = 0;
    Ok(registered)
}

fn start_machine_heartbeat(inner: &Arc<RadiusInner>, interval_ms: u64) {
    inner.state().machine_heartbeat_interval_ms = interval_ms;
    schedule_machine_heartbeat(inner, interval_ms);
}

fn schedule_machine_heartbeat(inner: &Arc<RadiusInner>, delay_ms: u64) {
    let mut state = inner.state();
    if !state.machine_heartbeat_active {
        return;
    }
    if let Some(timer) = state.machine_timer.take() {
        timer.abort();
    }
    state.machine_timer_generation = state.machine_timer_generation.wrapping_add(1);
    let generation = state.machine_timer_generation;
    let weak = Arc::downgrade(inner);
    state.machine_timer = Some(tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        let Some(inner) = weak.upgrade() else {
            return;
        };
        {
            let mut state = inner.state();
            if state.machine_timer_generation != generation {
                return;
            }
            state.machine_timer.take();
        }
        heartbeat_machine(inner).await;
    }));
}

fn start_pi_heartbeat(
    inner: &Arc<RadiusInner>,
    instance_id: String,
    interval_ms: u64,
    radius_pi_id: String,
) {
    {
        let mut state = inner.state();
        if let Some(existing) = state.pi_heartbeats.get_mut(&instance_id) {
            if let Some(timer) = existing.timer.take() {
                timer.abort();
            }
            existing.interval_ms = interval_ms;
            existing.radius_pi_id = radius_pi_id;
            existing.consecutive_not_found_count = 0;
            existing.transient_failure_count = 0;
        } else {
            state.pi_heartbeats.insert(
                instance_id.clone(),
                PiHeartbeatState {
                    timer: None,
                    generation: 0,
                    interval_ms,
                    radius_pi_id,
                    consecutive_not_found_count: 0,
                    transient_failure_count: 0,
                },
            );
        }
    }
    schedule_pi_heartbeat(inner, instance_id, interval_ms);
}

fn schedule_pi_heartbeat(inner: &Arc<RadiusInner>, instance_id: String, delay_ms: u64) {
    let mut state = inner.state();
    let Some(heartbeat) = state.pi_heartbeats.get_mut(&instance_id) else {
        return;
    };
    if let Some(timer) = heartbeat.timer.take() {
        timer.abort();
    }
    heartbeat.generation = heartbeat.generation.wrapping_add(1);
    let generation = heartbeat.generation;
    let weak = Arc::downgrade(inner);
    let timer_instance_id = instance_id.clone();
    heartbeat.timer = Some(tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        let Some(inner) = weak.upgrade() else {
            return;
        };
        {
            let mut state = inner.state();
            let Some(heartbeat) = state.pi_heartbeats.get_mut(&timer_instance_id) else {
                return;
            };
            if heartbeat.generation != generation {
                return;
            }
            heartbeat.timer.take();
        }
        heartbeat_pi(inner, timer_instance_id).await;
    }));
}

async fn heartbeat_machine(inner: Arc<RadiusInner>) {
    let machine = inner.state().machine.clone();
    let Some(machine) = machine else {
        return;
    };
    if !matches!(inner.tokens.access_token().await, Ok(Some(_))) {
        return;
    }
    let body = json!({
        "cwd": inner.runtime.orchestrator_dir,
        "socketPath": inner.runtime.socket_path,
    });
    match post(&inner, &format!("machines/{}/heartbeat", machine.id), body).await {
        Ok(_) => {
            let interval = {
                let mut state = inner.state();
                state.machine_consecutive_not_found_count = 0;
                state.machine_transient_failure_count = 0;
                state.machine_heartbeat_interval_ms
            };
            schedule_machine_heartbeat(&inner, interval);
        }
        Err(error) if !error.is_not_found() => {
            let failure_count = {
                let mut state = inner.state();
                state.machine_transient_failure_count += 1;
                state.machine_transient_failure_count
            };
            let delay_ms = random_backoff_delay_ms(failure_count);
            log_radius_retry(
                "Radius machine",
                "heartbeat",
                delay_ms,
                failure_count,
                &error,
            );
            schedule_machine_heartbeat(&inner, delay_ms);
        }
        Err(_) => {
            let (not_found_count, interval) = {
                let mut state = inner.state();
                state.machine_transient_failure_count = 0;
                state.machine_consecutive_not_found_count += 1;
                (
                    state.machine_consecutive_not_found_count,
                    state.machine_heartbeat_interval_ms,
                )
            };
            if not_found_count < NOT_FOUND_RETRY_THRESHOLD {
                schedule_machine_heartbeat(&inner, interval);
            } else if let Err(error) = re_register_machine_and_pis(&inner).await {
                let failure_count = {
                    let mut state = inner.state();
                    state.machine_transient_failure_count += 1;
                    state.machine_transient_failure_count
                };
                let delay_ms = random_backoff_delay_ms(failure_count);
                log_radius_retry(
                    "Radius machine",
                    "re-registration",
                    delay_ms,
                    failure_count,
                    &error,
                );
                schedule_machine_heartbeat(&inner, delay_ms);
            }
        }
    }
}

async fn heartbeat_pi(inner: Arc<RadiusInner>, instance_id: String) {
    if !matches!(inner.tokens.access_token().await, Ok(Some(_))) {
        return;
    }
    let radius_pi_id = {
        let state = inner.state();
        let Some(heartbeat) = state.pi_heartbeats.get(&instance_id) else {
            return;
        };
        heartbeat.radius_pi_id.clone()
    };
    match post(&inner, &format!("pis/{radius_pi_id}/heartbeat"), json!({})).await {
        Ok(_) => {
            let interval = {
                let mut state = inner.state();
                let Some(heartbeat) = state.pi_heartbeats.get_mut(&instance_id) else {
                    return;
                };
                heartbeat.consecutive_not_found_count = 0;
                heartbeat.transient_failure_count = 0;
                heartbeat.interval_ms
            };
            schedule_pi_heartbeat(&inner, instance_id, interval);
        }
        Err(error) if !error.is_not_found() => {
            let failure_count = {
                let mut state = inner.state();
                let Some(heartbeat) = state.pi_heartbeats.get_mut(&instance_id) else {
                    return;
                };
                heartbeat.transient_failure_count += 1;
                heartbeat.transient_failure_count
            };
            let delay_ms = random_backoff_delay_ms(failure_count);
            log_radius_retry(
                &format!("Radius Pi {instance_id}"),
                "heartbeat",
                delay_ms,
                failure_count,
                &error,
            );
            schedule_pi_heartbeat(&inner, instance_id, delay_ms);
        }
        Err(_) => {
            let (not_found_count, interval) = {
                let mut state = inner.state();
                let Some(heartbeat) = state.pi_heartbeats.get_mut(&instance_id) else {
                    return;
                };
                heartbeat.transient_failure_count = 0;
                heartbeat.consecutive_not_found_count += 1;
                (heartbeat.consecutive_not_found_count, heartbeat.interval_ms)
            };
            if not_found_count < NOT_FOUND_RETRY_THRESHOLD {
                schedule_pi_heartbeat(&inner, instance_id, interval);
                return;
            }
            match re_register_pi(&inner, &instance_id).await {
                Ok(true) => {}
                Ok(false) => {
                    let delay_ms = random_backoff_delay_ms(1);
                    eprintln!(
                        "Radius Pi {instance_id} re-registration skipped; retrying in {delay_ms}ms"
                    );
                    schedule_pi_heartbeat(&inner, instance_id, delay_ms);
                }
                Err(error) => {
                    let failure_count = {
                        let mut state = inner.state();
                        let Some(heartbeat) = state.pi_heartbeats.get_mut(&instance_id) else {
                            return;
                        };
                        heartbeat.transient_failure_count += 1;
                        heartbeat.transient_failure_count
                    };
                    let delay_ms = random_backoff_delay_ms(failure_count);
                    log_radius_retry(
                        &format!("Radius Pi {instance_id}"),
                        "re-registration",
                        delay_ms,
                        failure_count,
                        &error,
                    );
                    schedule_pi_heartbeat(&inner, instance_id, delay_ms);
                }
            }
        }
    }
}

async fn re_register_machine_and_pis(inner: &Arc<RadiusInner>) -> Result<(), RadiusError> {
    let label = inner
        .state()
        .machine
        .as_ref()
        .and_then(|machine| machine.label.clone());
    let registered = register_machine(inner, label).await?;
    {
        inner.state().machine_heartbeat_interval_ms = registered.registration.heartbeat_interval_ms;
    }
    schedule_machine_heartbeat(inner, registered.registration.heartbeat_interval_ms);
    let coordinator = inner.state().coordinator.as_ref().and_then(Weak::upgrade);
    let instances = match coordinator {
        Some(coordinator) => coordinator.list_live_instances().await,
        None => Vec::new(),
    };
    for instance in instances {
        if let Err(error) = re_register_pi(inner, &instance.id).await {
            eprintln!(
                "Radius Pi {} re-registration failed: {}",
                instance.id,
                format_radius_error(&error)
            );
        }
    }
    Ok(())
}

async fn re_register_pi(inner: &Arc<RadiusInner>, instance_id: &str) -> Result<bool, RadiusError> {
    let coordinator = inner.state().coordinator.as_ref().and_then(Weak::upgrade);
    let instance = match &coordinator {
        Some(coordinator) => coordinator.get_live_instance(instance_id).await,
        None => None,
    };
    let Some(instance) = instance else {
        let mut state = inner.state();
        if let Some(mut heartbeat) = state.pi_heartbeats.remove(instance_id)
            && let Some(timer) = heartbeat.timer.take()
        {
            timer.abort();
        }
        return Ok(false);
    };
    if inner.state().machine.is_none() {
        Box::pin(re_register_machine_and_pis(inner)).await?;
        return Ok(true);
    }
    let presence = RadiusPresence {
        inner: Arc::clone(inner),
    };
    let registered = presence.register_pi(instance).await?;
    if let Some(coordinator) = coordinator {
        coordinator.update_instance(registered).await;
    }
    Ok(true)
}

fn format_radius_error(error: &RadiusError) -> String {
    match error {
        RadiusError::Http { status, .. } => format!("HTTP {status}: {error}"),
        _ => error.to_string(),
    }
}

fn log_radius_retry(
    scope: &str,
    action: &str,
    delay_ms: u64,
    failure_count: u32,
    error: &RadiusError,
) {
    eprintln!(
        "{scope} {action} failed (attempt {failure_count}); retrying in {delay_ms}ms: {}",
        format_radius_error(error)
    );
}

fn system_hostname() -> String {
    hostname::get()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}
