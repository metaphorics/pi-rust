use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use async_trait::async_trait;
use pi_orchestrator::radius::{
    Presence, PresenceCoordinator, RadiusError, RadiusPresence, RadiusRuntime, RadiusTokenSource,
    RadiusTransport, compute_backoff_delay_ms, node_arch, node_platform, resolve_access_token_from,
};
use pi_orchestrator::storage::Storage;
use pi_orchestrator::types::{InstanceRecord, InstanceStatus};
use serde_json::{Value, json};

#[derive(Clone, Debug, PartialEq)]
struct Request {
    url: String,
    token: String,
    body: Value,
}

#[derive(Default)]
struct MemoryTransport {
    requests: Mutex<Vec<Request>>,
    responses: Mutex<VecDeque<Result<Value, RadiusError>>>,
}

impl MemoryTransport {
    fn with_responses(responses: Vec<Result<Value, RadiusError>>) -> Self {
        Self {
            requests: Mutex::new(Vec::new()),
            responses: Mutex::new(responses.into()),
        }
    }

    fn requests(&self) -> Vec<Request> {
        lock(&self.requests).clone()
    }
}

#[async_trait]
impl RadiusTransport for MemoryTransport {
    async fn post(&self, url: &str, access_token: &str, body: Value) -> Result<Value, RadiusError> {
        lock(&self.requests).push(Request {
            url: url.to_owned(),
            token: access_token.to_owned(),
            body,
        });
        lock(&self.responses).pop_front().unwrap_or(Ok(Value::Null))
    }
}

struct StaticToken(Option<String>);

#[async_trait]
impl RadiusTokenSource for StaticToken {
    async fn access_token(&self) -> Result<Option<String>, RadiusError> {
        Ok(self.0.clone())
    }
}

#[derive(Default)]
struct MemoryCoordinator {
    instances: Mutex<HashMap<String, InstanceRecord>>,
    updates: Mutex<Vec<InstanceRecord>>,
}

impl MemoryCoordinator {
    fn insert(&self, instance: InstanceRecord) {
        lock(&self.instances).insert(instance.id.clone(), instance);
    }

    fn updates(&self) -> Vec<InstanceRecord> {
        lock(&self.updates).clone()
    }
}

#[async_trait]
impl PresenceCoordinator for MemoryCoordinator {
    async fn get_live_instance(&self, instance_id: &str) -> Option<InstanceRecord> {
        lock(&self.instances).get(instance_id).cloned()
    }

    async fn list_live_instances(&self) -> Vec<InstanceRecord> {
        lock(&self.instances).values().cloned().collect()
    }

    async fn update_instance(&self, instance: InstanceRecord) {
        lock(&self.instances).insert(instance.id.clone(), instance.clone());
        lock(&self.updates).push(instance);
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn registration(id: &str, interval_ms: u64) -> Value {
    json!({
        "id": id,
        "heartbeatIntervalMs": interval_ms,
        "expiresInMs": interval_ms * 3
    })
}

fn instance() -> InstanceRecord {
    InstanceRecord {
        id: "instance-1".into(),
        status: InstanceStatus::Online,
        cwd: "/workspace".into(),
        created_at: "2026-07-16T00:00:00.000Z".into(),
        last_seen_at: None,
        label: Some("worker".into()),
        session_id: Some("session-1".into()),
        session_file: None,
        radius_pi_id: None,
    }
}

fn harness(
    temp: &tempfile::TempDir,
    transport: Arc<MemoryTransport>,
    token: Option<&str>,
) -> RadiusPresence {
    let orchestrator_dir = temp.path().join("orchestrator");
    RadiusPresence::with_dependencies(
        transport,
        Arc::new(StaticToken(token.map(str::to_owned))),
        Arc::new(Storage::new(&orchestrator_dir)),
        "https://radius.test/v1/".into(),
        RadiusRuntime {
            socket_path: orchestrator_dir.join("orchestrator.sock"),
            orchestrator_dir,
        },
    )
}

async fn settle() {
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
}

#[tokio::test]
async fn stored_oauth_token_precedes_environment_then_falls_back() {
    let temp = tempfile::tempdir().unwrap();
    let auth_path = temp.path().join("auth.json");
    tokio::fs::write(
        &auth_path,
        r#"{"radius":{"type":"oauth","access":"stored","refresh":"refresh","expires":4102444800000}}"#,
    )
    .await
    .unwrap();

    assert_eq!(
        resolve_access_token_from(&auth_path, Some("environment".into())).await,
        Some("stored".into())
    );

    tokio::fs::write(
        &auth_path,
        r#"{"radius":{"type":"api_key","key":"not-radius-oauth"}}"#,
    )
    .await
    .unwrap();
    assert_eq!(
        resolve_access_token_from(&auth_path, Some("environment".into())).await,
        Some("environment".into())
    );
    assert_eq!(resolve_access_token_from(&auth_path, None).await, None);
}

#[tokio::test]
async fn disabled_presence_short_circuits_without_transport() {
    let temp = tempfile::tempdir().unwrap();
    let transport = Arc::new(MemoryTransport::default());
    let presence = harness(&temp, Arc::clone(&transport), None);
    let original = instance();

    assert_eq!(presence.start(Some("machine".into())).await.unwrap(), None);
    assert_eq!(
        presence.register_pi(original.clone()).await.unwrap(),
        original
    );
    presence.disconnect_pi(&instance()).await.unwrap();
    presence.stop().await.unwrap();

    assert!(transport.requests().is_empty());
    assert_eq!(presence.active_timer_count(), 0);
}

#[test]
fn backoff_matches_exponential_and_jitter_bounds() {
    assert_eq!(compute_backoff_delay_ms(0, 0.0), 1_000);
    assert_eq!(compute_backoff_delay_ms(1, 0.0), 1_000);
    assert_eq!(compute_backoff_delay_ms(1, 0.999_999), 1_249);
    assert_eq!(compute_backoff_delay_ms(2, 0.0), 2_000);
    assert_eq!(compute_backoff_delay_ms(2, 0.999_999), 2_499);
    assert_eq!(compute_backoff_delay_ms(5, 0.5), 18_000);
    assert_eq!(compute_backoff_delay_ms(6, 0.0), 30_000);
    assert_eq!(compute_backoff_delay_ms(32, 0.999_999), 30_000);
}

#[tokio::test(start_paused = true)]
async fn registration_bodies_urls_and_server_intervals_match_radius() {
    let temp = tempfile::tempdir().unwrap();
    let transport = Arc::new(MemoryTransport::with_responses(vec![
        Ok(registration("machine-1", 50)),
        Ok(registration("pi-1", 75)),
        Ok(Value::Null),
        Ok(Value::Null),
    ]));
    let presence = harness(&temp, Arc::clone(&transport), Some("secret"));

    let machine = presence
        .start(Some("laptop".into()))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(machine.id, "machine-1");
    let registered = presence.register_pi(instance()).await.unwrap();
    assert_eq!(registered.radius_pi_id.as_deref(), Some("pi-1"));
    assert_eq!(presence.active_timer_count(), 2);
    settle().await;

    let requests = transport.requests();
    assert_eq!(requests[0].url, "https://radius.test/v1/machines/register");
    assert_eq!(requests[0].token, "secret");
    assert_eq!(requests[0].body["platform"], node_platform());
    assert_eq!(requests[0].body["arch"], node_arch());
    assert_eq!(requests[0].body["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(requests[0].body["label"], "laptop");
    assert_eq!(
        requests[0].body["capabilities"],
        json!({"spawn": true, "relay": false, "iroh": false})
    );
    assert!(requests[0].body["hostname"].as_str().is_some());

    assert_eq!(requests[1].url, "https://radius.test/v1/pis/register");
    assert_eq!(requests[1].body["machineId"], "machine-1");
    assert_eq!(requests[1].body["cwd"], "/workspace");
    assert_eq!(requests[1].body["transport"], "local-rpc");
    assert_eq!(requests[1].body["sessionId"], "session-1");
    assert_eq!(
        requests[1].body["capabilities"],
        json!({"rpc": true, "relay": false, "iroh": false})
    );

    tokio::time::advance(Duration::from_millis(49)).await;
    settle().await;
    assert_eq!(transport.requests().len(), 2);
    tokio::time::advance(Duration::from_millis(1)).await;
    settle().await;
    let requests = transport.requests();
    assert_eq!(
        requests[2].url,
        "https://radius.test/v1/machines/machine-1/heartbeat"
    );
    assert_eq!(
        requests[2].body,
        json!({
            "cwd": temp.path().join("orchestrator"),
            "socketPath": temp.path().join("orchestrator/orchestrator.sock")
        })
    );

    tokio::time::advance(Duration::from_millis(24)).await;
    settle().await;
    assert_eq!(transport.requests().len(), 3);
    tokio::time::advance(Duration::from_millis(1)).await;
    settle().await;
    assert_eq!(
        transport.requests()[3].url,
        "https://radius.test/v1/pis/pi-1/heartbeat"
    );
}

#[tokio::test(start_paused = true)]
async fn three_machine_not_found_heartbeats_trigger_re_registration() {
    let temp = tempfile::tempdir().unwrap();
    let transport = Arc::new(MemoryTransport::with_responses(vec![
        Ok(registration("machine-old", 100)),
        Err(RadiusError::http(404, "gone")),
        Err(RadiusError::http(404, "gone")),
        Err(RadiusError::http(404, "gone")),
        Ok(registration("machine-new", 250)),
    ]));
    let presence = harness(&temp, Arc::clone(&transport), Some("secret"));
    presence.start(Some("laptop".into())).await.unwrap();
    settle().await;

    for _ in 0..2 {
        tokio::time::advance(Duration::from_millis(100)).await;
        settle().await;
        assert_eq!(
            transport
                .requests()
                .iter()
                .filter(|request| request.url.ends_with("/machines/register"))
                .count(),
            1
        );
    }
    tokio::time::advance(Duration::from_millis(100)).await;
    settle().await;

    let registrations: Vec<_> = transport
        .requests()
        .into_iter()
        .filter(|request| request.url.ends_with("/machines/register"))
        .collect();
    assert_eq!(registrations.len(), 2);
    assert_eq!(registrations[1].body["machineId"], "machine-old");
    assert_eq!(registrations[1].body["label"], "laptop");
    assert_eq!(presence.active_timer_count(), 1);
}

#[tokio::test(start_paused = true)]
async fn three_pi_not_found_heartbeats_trigger_re_registration() {
    let temp = tempfile::tempdir().unwrap();
    let transport = Arc::new(MemoryTransport::with_responses(vec![
        Ok(registration("machine-1", 1_000_000)),
        Ok(registration("pi-old", 100)),
        Err(RadiusError::http(404, "gone")),
        Err(RadiusError::http(404, "gone")),
        Err(RadiusError::http(404, "gone")),
        Ok(registration("pi-new", 100)),
    ]));
    let presence = harness(&temp, Arc::clone(&transport), Some("secret"));
    presence.start(None).await.unwrap();
    let registered = presence.register_pi(instance()).await.unwrap();

    let coordinator = Arc::new(MemoryCoordinator::default());
    coordinator.insert(registered);
    let coordinator_dyn: Arc<dyn PresenceCoordinator> = coordinator.clone();
    presence.set_coordinator(Arc::downgrade(&coordinator_dyn));
    settle().await;

    for _ in 0..3 {
        tokio::time::advance(Duration::from_millis(100)).await;
        settle().await;
    }

    let paths: Vec<_> = transport
        .requests()
        .into_iter()
        .map(|request| request.url)
        .collect();
    assert_eq!(
        paths
            .iter()
            .filter(|path| path.ends_with("/pis/pi-old/heartbeat"))
            .count(),
        3
    );
    assert_eq!(paths.last().unwrap(), "https://radius.test/v1/pis/register");
    assert_eq!(
        coordinator.updates()[0].radius_pi_id.as_deref(),
        Some("pi-new")
    );
    assert_eq!(presence.active_timer_count(), 2);
}

#[tokio::test(start_paused = true)]
async fn disconnect_and_stop_cancel_timers_and_use_exact_endpoints() {
    let temp = tempfile::tempdir().unwrap();
    let transport = Arc::new(MemoryTransport::with_responses(vec![
        Ok(registration("machine-1", 100)),
        Ok(registration("pi-1", 100)),
        Ok(Value::Null),
        Ok(Value::Null),
    ]));
    let presence = harness(&temp, Arc::clone(&transport), Some("secret"));
    presence.start(None).await.unwrap();
    let registered = presence.register_pi(instance()).await.unwrap();
    assert_eq!(presence.active_timer_count(), 2);

    presence.disconnect_pi(&registered).await.unwrap();
    assert_eq!(presence.active_timer_count(), 1);
    presence.stop().await.unwrap();
    assert_eq!(presence.active_timer_count(), 0);

    let requests = transport.requests();
    assert_eq!(
        requests[2].url,
        "https://radius.test/v1/pis/pi-1/disconnect"
    );
    assert_eq!(requests[2].body, json!({}));
    assert_eq!(
        requests[3].url,
        "https://radius.test/v1/machines/machine-1/disconnect"
    );
    assert_eq!(requests[3].body, json!({}));

    tokio::time::advance(Duration::from_secs(10)).await;
    settle().await;
    assert_eq!(transport.requests().len(), 4);
}

#[test]
fn runtime_paths_remain_platform_paths() {
    let runtime = RadiusRuntime {
        orchestrator_dir: PathBuf::from("/tmp/pi/orchestrator"),
        socket_path: PathBuf::from("/tmp/pi/orchestrator/orchestrator.sock"),
    };
    assert!(runtime.socket_path.starts_with(&runtime.orchestrator_dir));
}
