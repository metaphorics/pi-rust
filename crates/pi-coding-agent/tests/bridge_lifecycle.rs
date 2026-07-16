//! Sidecar process lifecycle tests (Phase 6 commit C5).
//!
//! Every scenario runs a REAL subprocess: fake-sidecar bash scripts under
//! `tests/fixtures/sidecar/` speak actual NDJSON over actual pipes, standing
//! in for `bun sidecar/src/main.ts`.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use pi_coding_agent::extensions::{
    BridgeState, ClientConfig, ClientError, DeadReason, ExtensionHost, ExtensionHostConfig,
    HostError, Incoming, LauncherSource, SidecarLauncher, SidecarTimeouts,
};
use pi_ext_protocol::{InitParams, Notification, Request};
use tempfile::TempDir;

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/sidecar")
        .join(name)
}

fn test_init_params() -> InitParams {
    serde_json::from_value(serde_json::json!({
        "cwd": "/tmp",
        "agentDir": "/tmp",
        "sessionDir": "/tmp",
        "configuredPaths": [],
        "mode": "rpc",
        "hasUi": false,
        "flagValues": {},
        "theme": {"name": "dark", "json": {}},
        "session": {
            "epoch": 0,
            "sessionFile": "/tmp/session.jsonl",
            "entries": [],
            "leafId": null
        },
        "state": {
            "idle": true,
            "projectTrusted": true,
            "pendingMessages": false,
            "activeTools": [],
            "allTools": [],
            "commands": [],
            "thinkingLevel": "minimal",
            "systemPrompt": "system",
            "flagValues": {},
            "editorText": "",
            "toolsExpanded": false,
            "theme": {"name": "dark", "json": {}}
        }
    }))
    .expect("valid InitParams fixture")
}

struct Harness {
    dir: TempDir,
    host: ExtensionHost,
}

struct HarnessOptions {
    client: ClientConfig,
    timeouts: SidecarTimeouts,
    extension_count: usize,
}

impl Default for HarnessOptions {
    fn default() -> Self {
        Self {
            client: ClientConfig {
                heartbeat_interval: Duration::from_millis(200),
                ..ClientConfig::default()
            },
            timeouts: SidecarTimeouts {
                handshake: Duration::from_secs(5),
                init: Duration::from_secs(5),
                shutdown_grace: Duration::from_millis(500),
            },
            extension_count: 1,
        }
    }
}

impl Harness {
    fn new(script: &str, options: HarnessOptions) -> Self {
        let dir = TempDir::new().unwrap();
        let mut paths = Vec::new();
        for index in 0..options.extension_count {
            let path = dir.path().join(format!("ext-{index}.ts"));
            std::fs::write(&path, "// fake extension").unwrap();
            paths.push(path);
        }
        let launcher = SidecarLauncher {
            bun: fixture(script),
            entry: PathBuf::from("sidecar-entry.ts"),
            cwd: dir.path().to_path_buf(),
            envs: vec![
                (
                    "FAKE_SIDECAR_SPAWNS".into(),
                    dir.path().join("spawns").into(),
                ),
                ("FAKE_SIDECAR_LOG".into(), dir.path().join("log").into()),
            ],
        };
        let host = ExtensionHost::new(ExtensionHostConfig {
            extension_paths: paths,
            launcher: LauncherSource::Resolved(launcher),
            init: Arc::new(test_init_params),
            timeouts: options.timeouts,
            client: options.client,
        })
        .unwrap();
        Self { dir, host }
    }

    fn spawn_count(&self) -> usize {
        std::fs::read_to_string(self.dir.path().join("spawns"))
            .map(|s| s.lines().count())
            .unwrap_or(0)
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.dir.path().join("log")).unwrap_or_default()
    }
}

#[tokio::test]
async fn clean_lifecycle_load_and_shutdown() {
    let harness = Harness::new("ok.sh", HarnessOptions::default());
    assert_eq!(harness.host.state().await, BridgeState::Detected);
    assert_eq!(harness.spawn_count(), 0, "construction must not spawn");

    let connection = harness.host.ensure_ready().await.unwrap();
    assert_eq!(harness.spawn_count(), 1);
    assert_eq!(connection.hello().pi, "0.80.7");
    let initialized = connection.initialized().expect("initialized recorded");
    assert!(initialized.registrations.tools.is_empty());

    // ensure_ready is idempotent while Ready: same process.
    let again = harness.host.ensure_ready().await.unwrap();
    assert!(Arc::ptr_eq(&connection, &again));
    assert_eq!(harness.spawn_count(), 1);

    // Incremental load round-trips.
    let extra = harness.dir.path().join("extra.ts");
    std::fs::write(&extra, "// extra").unwrap();
    let outcome = harness.host.load_more(&[extra]).await.unwrap();
    assert!(outcome.errors.is_empty());

    // Heartbeats are answered: survive several intervals.
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert!(
        connection.closed().is_none(),
        "heartbeat must keep the connection alive"
    );

    harness.host.shutdown().await;
    assert_eq!(
        harness.host.state().await,
        BridgeState::Dead(DeadReason::Shutdown)
    );
    let status = connection.exit_status().expect("child reaped");
    assert!(
        status.success(),
        "clean voluntary exit, not a kill: {status:?}"
    );

    // Diagnostics captured stderr.
    let diagnostics = connection.diagnostics();
    assert!(
        diagnostics
            .iter()
            .any(|line| line.contains("fake sidecar booted")),
        "stderr line missing from diagnostics: {diagnostics:?}"
    );

    // Shutdown is terminal: no respawn.
    let error = harness.host.ensure_ready().await.unwrap_err();
    assert!(matches!(error, HostError::ShutDown), "{error}");
    assert_eq!(harness.spawn_count(), 1);
}

#[tokio::test]
async fn zero_extensions_never_spawns_or_detects() {
    let dir = TempDir::new().unwrap();
    let resolved = std::sync::atomic::AtomicBool::new(false);
    let resolved = Arc::new(resolved);
    let flag = Arc::clone(&resolved);
    let cwd = dir.path().to_path_buf();
    let host = ExtensionHost::new(ExtensionHostConfig {
        extension_paths: Vec::new(),
        launcher: LauncherSource::Lazy(Box::new(move || {
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(SidecarLauncher {
                bun: PathBuf::from("/nonexistent"),
                entry: PathBuf::from("/nonexistent"),
                cwd,
                envs: Vec::new(),
            })
        })),
        init: Arc::new(test_init_params),
        timeouts: SidecarTimeouts::default(),
        client: ClientConfig::default(),
    })
    .unwrap();

    assert_eq!(host.state().await, BridgeState::NotNeeded);
    let error = host.ensure_ready().await.unwrap_err();
    assert!(matches!(error, HostError::NotNeeded));
    assert!(
        !resolved.load(std::sync::atomic::Ordering::SeqCst),
        "resolver must never run"
    );
}

#[tokio::test]
async fn launcher_resolution_failure_disables_with_install_command() {
    let dir = TempDir::new().unwrap();
    let ext = dir.path().join("ext.ts");
    std::fs::write(&ext, "// ext").unwrap();
    let host = ExtensionHost::new(ExtensionHostConfig {
        extension_paths: vec![ext],
        launcher: LauncherSource::Lazy(Box::new(|| {
            Err(pi_coding_agent::extensions::BunResolveError::NotFound.into())
        })),
        init: Arc::new(test_init_params),
        timeouts: SidecarTimeouts::default(),
        client: ClientConfig::default(),
    })
    .unwrap();

    let error = host.ensure_ready().await.unwrap_err();
    assert!(
        error
            .to_string()
            .contains(pi_coding_agent::extensions::BUN_INSTALL_COMMAND),
        "{error}"
    );
    assert!(matches!(host.state().await, BridgeState::Disabled(_)));
}

#[tokio::test]
async fn vanished_extension_path_is_a_loud_error() {
    let dir = TempDir::new().unwrap();
    let error = ExtensionHost::new(ExtensionHostConfig {
        extension_paths: vec![dir.path().join("vanished.ts")],
        launcher: LauncherSource::Resolved(SidecarLauncher {
            bun: fixture("ok.sh"),
            entry: PathBuf::from("sidecar-entry.ts"),
            cwd: dir.path().to_path_buf(),
            envs: Vec::new(),
        }),
        init: Arc::new(test_init_params),
        timeouts: SidecarTimeouts::default(),
        client: ClientConfig::default(),
    })
    .unwrap_err();
    assert!(error.to_string().contains("vanished.ts"), "{error}");
}

#[tokio::test]
async fn malformed_frames_and_pollution_after_ready_are_skipped() {
    let harness = Harness::new("noisy.sh", HarnessOptions::default());
    let mut incoming = harness.host.take_incoming().expect("first take");
    let connection = harness.host.ensure_ready().await.unwrap();

    // The valid notification after the garbage line still arrives.
    let item = tokio::time::timeout(Duration::from_secs(5), incoming.recv())
        .await
        .expect("notification within deadline")
        .expect("channel open");
    match item {
        Incoming::Notification(Notification::UiNotify(notify)) => {
            assert_eq!(notify.message, "after-garbage");
        }
        other => panic!("unexpected incoming: {other:?}"),
    }

    // The connection survived the garbage: a request still round-trips.
    let extra = harness.dir.path().join("extra.ts");
    std::fs::write(&extra, "// extra").unwrap();
    harness.host.load_more(&[extra]).await.unwrap();

    let diagnostics = connection.diagnostics();
    assert!(
        diagnostics.iter().any(|line| line.contains("malformed")),
        "expected a malformed-frame diagnostic: {diagnostics:?}"
    );
}

#[tokio::test]
async fn pollution_before_hello_fails_the_handshake() {
    let harness = Harness::new("pollute-pre-hello.sh", HarnessOptions::default());
    let error = harness.host.ensure_ready().await.unwrap_err();
    assert!(
        matches!(error, HostError::Failed(DeadReason::HandshakeFailed(_))),
        "{error}"
    );
    assert!(matches!(harness.host.state().await, BridgeState::Dead(_)));
}

#[tokio::test]
async fn version_mismatch_fails_the_handshake() {
    let harness = Harness::new("wrong-version.sh", HarnessOptions::default());
    let error = harness.host.ensure_ready().await.unwrap_err();
    let text = error.to_string();
    assert!(
        text.contains("unsupported extension protocol version"),
        "{text}"
    );
}

#[tokio::test]
async fn crash_mid_request_rejects_pending_then_one_respawn_recovers() {
    let harness = Harness::new("crash-on-load.sh", HarnessOptions::default());
    let connection = harness.host.ensure_ready().await.unwrap();
    assert_eq!(harness.spawn_count(), 1);

    // The sidecar dies while lifecycle/load is pending: the request must be
    // rejected, never left hanging.
    let extra = harness.dir.path().join("extra.ts");
    std::fs::write(&extra, "// extra").unwrap();
    let error = harness
        .host
        .load_more(std::slice::from_ref(&extra))
        .await
        .unwrap_err();
    assert!(
        matches!(error, HostError::Rpc(ClientError::Closed(_))),
        "{error}"
    );
    tokio::time::timeout(Duration::from_secs(5), connection.wait_closed())
        .await
        .unwrap();

    // One respawn attempt is granted; the replacement serves normally.
    let respawned = harness.host.ensure_ready().await.unwrap();
    assert_eq!(harness.spawn_count(), 2);
    assert!(!Arc::ptr_eq(&connection, &respawned));
    harness.host.load_more(&[extra]).await.unwrap();
    assert_eq!(harness.host.state().await, BridgeState::Ready);
}

#[tokio::test]
async fn respawn_failure_disables_permanently() {
    let harness = Harness::new("crash-always.sh", HarnessOptions::default());

    let first = harness.host.ensure_ready().await.unwrap_err();
    assert!(matches!(first, HostError::Failed(_)), "{first}");
    assert!(matches!(harness.host.state().await, BridgeState::Dead(_)));

    let second = harness.host.ensure_ready().await.unwrap_err();
    assert!(matches!(second, HostError::Disabled(_)), "{second}");

    let third = harness.host.ensure_ready().await.unwrap_err();
    assert!(matches!(third, HostError::Disabled(_)), "{third}");
    assert_eq!(
        harness.spawn_count(),
        2,
        "disabled state must stop spawning"
    );
}

#[tokio::test]
async fn partial_frame_at_death_never_satisfies_a_request() {
    let harness = Harness::new("truncate-mid-frame.sh", HarnessOptions::default());
    let connection = harness.host.ensure_ready().await.unwrap();

    let extra = harness.dir.path().join("extra.ts");
    std::fs::write(&extra, "// extra").unwrap();
    let error = harness.host.load_more(&[extra]).await.unwrap_err();
    assert!(
        matches!(error, HostError::Rpc(ClientError::Closed(_))),
        "{error}"
    );

    let reason = tokio::time::timeout(Duration::from_secs(5), connection.wait_closed())
        .await
        .unwrap();
    assert!(
        !matches!(reason, DeadReason::Shutdown),
        "unexpected reason: {reason}"
    );
    let diagnostics = connection.diagnostics();
    assert!(
        diagnostics.iter().any(|line| line.contains("truncated")),
        "expected a truncated-frame diagnostic: {diagnostics:?}"
    );
}

#[tokio::test]
async fn request_timeout_sends_cancel_frame() {
    let harness = Harness::new("slow.sh", HarnessOptions::default());
    let connection = harness.host.ensure_ready().await.unwrap();

    let request = Request::LifecycleLoad(pi_ext_protocol::LoadParams { paths: Vec::new() });
    let error = connection
        .request_timeout(request, Duration::from_millis(300))
        .await
        .unwrap_err();
    assert!(matches!(error, ClientError::Timeout(_)), "{error}");

    // The cancel frame for the abandoned id (init=1, load=2) reaches the wire.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let log = harness.log();
        if log.contains("{\"type\":\"cancel\",\"id\":2}") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "cancel frame never observed; log: {log}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[tokio::test]
async fn heartbeat_declares_a_silent_sidecar_dead() {
    let options = HarnessOptions {
        client: ClientConfig {
            heartbeat_interval: Duration::from_millis(100),
            ..ClientConfig::default()
        },
        ..HarnessOptions::default()
    };
    let harness = Harness::new("never-pong.sh", options);
    let connection = harness.host.ensure_ready().await.unwrap();

    let reason = tokio::time::timeout(Duration::from_secs(5), connection.wait_closed())
        .await
        .expect("heartbeat must trip within the deadline");
    assert_eq!(reason, DeadReason::HeartbeatMissed);
    assert!(matches!(harness.host.state().await, BridgeState::Dead(_)));
}

#[tokio::test]
async fn forged_future_pong_nonce_does_not_defeat_the_heartbeat() {
    let options = HarnessOptions {
        client: ClientConfig {
            heartbeat_interval: Duration::from_millis(100),
            ..ClientConfig::default()
        },
        ..HarnessOptions::default()
    };
    let harness = Harness::new("bogus-pong.sh", options);
    let connection = harness.host.ensure_ready().await.unwrap();

    // The sidecar forged a u64::MAX pong before any ping; strict nonce
    // matching must still detect the missing real pongs.
    let reason = tokio::time::timeout(Duration::from_secs(5), connection.wait_closed())
        .await
        .expect("heartbeat must trip despite the forged pong");
    assert_eq!(reason, DeadReason::HeartbeatMissed);
    let diagnostics = connection.diagnostics();
    assert!(
        diagnostics
            .iter()
            .any(|line| line.contains("ignoring pong")),
        "expected a forged-pong diagnostic: {diagnostics:?}"
    );
}

#[tokio::test]
async fn boot_flood_is_bounded_ordered_and_lossless() {
    let options = HarnessOptions {
        client: ClientConfig {
            incoming_queue: 8, // far smaller than the 2000-frame flood
            heartbeat_interval: Duration::from_millis(200),
            ..ClientConfig::default()
        },
        ..HarnessOptions::default()
    };
    let harness = Harness::new("flood.sh", options);
    let mut incoming = harness.host.take_incoming().expect("first take");

    // Drain concurrently: the flood happens DURING init, and init must still
    // complete because control frames never queue behind application frames.
    let consumer = tokio::spawn(async move {
        let mut seen = 0u32;
        while let Some(item) = incoming.recv().await {
            if let Incoming::Notification(Notification::UiNotify(notify)) = item {
                seen += 1;
                assert_eq!(
                    notify.message,
                    format!("flood-{seen}"),
                    "frames must stay ordered"
                );
                if seen == 2000 {
                    break;
                }
            }
        }
        seen
    });

    harness.host.ensure_ready().await.unwrap();
    let seen = tokio::time::timeout(Duration::from_secs(30), consumer)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        seen, 2000,
        "every flood frame must be delivered exactly once, in order"
    );
    harness.host.shutdown().await;
}

#[tokio::test]
async fn writer_backpressure_blocks_producers_and_shutdown_stays_bounded() {
    let options = HarnessOptions {
        client: ClientConfig {
            writer_queue: 4,
            ..ClientConfig::default()
        },
        ..HarnessOptions::default()
    };
    let harness = Harness::new("stall.sh", options);
    let connection = harness.host.ensure_ready().await.unwrap();

    // Blast notifications at a sidecar that stopped reading stdin. The pipe
    // and the 4-slot writer queue fill; the producer must block (bounded
    // memory), never drop or error while the connection lives.
    let producer_connection = Arc::clone(&connection);
    let producer = tokio::spawn(async move {
        let payload = "x".repeat(256);
        for _ in 0..20_000 {
            let notification = Notification::UiSetTitle(pi_ext_protocol::TextParams {
                text: payload.clone(),
            });
            if producer_connection.notify(notification).await.is_err() {
                return false; // connection died (expected at shutdown)
            }
        }
        true
    });

    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(
        !producer.is_finished(),
        "producer must be backpressured, not draining 5MB"
    );

    // Shutdown must stay bounded even with the writer wedged: the shutdown
    // request itself cannot be admitted, so the grace deadline + kill apply.
    let started = Instant::now();
    harness.host.shutdown().await;
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(3),
        "shutdown took {elapsed:?}"
    );
    assert_eq!(
        harness.host.state().await,
        BridgeState::Dead(DeadReason::Shutdown)
    );
    let status = connection.exit_status().expect("child reaped after kill");
    assert!(
        !status.success(),
        "the wedged sidecar must have been killed"
    );

    // The blocked producer observes the close instead of hanging.
    let produced_all = tokio::time::timeout(Duration::from_secs(5), producer)
        .await
        .expect("producer must unblock after close")
        .unwrap();
    assert!(
        !produced_all,
        "producer should have been interrupted by the close"
    );
}
