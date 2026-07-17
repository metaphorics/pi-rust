//! Process-level CLI smoke: a real `pi-orchestrator serve` on a real Unix
//! socket, driven exclusively through the binary's subcommands, with a fake
//! sibling `pi` child, interrupted with SIGINT at the end.

#![cfg(unix)]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const FAKE_PI: &str = r#"#!/usr/bin/env python3
import json
import os
import sys

with open("pi-pid", "w") as marker:
    marker.write(str(os.getpid()))

session = 0

def write(value):
    sys.stdout.write(json.dumps(value, separators=(",", ":")) + "\n")
    sys.stdout.flush()

for raw in sys.stdin:
    command = json.loads(raw)
    kind = command.get("type")
    request_id = command.get("id")
    if kind == "get_state":
        write({"type": "response", "id": request_id, "command": kind, "success": True, "data": {"sessionId": "session-%d" % session, "sessionFile": "/tmp/session-%d.jsonl" % session}})
    elif kind == "echo":
        write({"type": "response", "id": request_id, "command": kind, "success": True, "data": command.get("value")})
    elif kind == "emit":
        write({"type": "agent_event", "value": command.get("value")})
        write({"type": "response", "id": request_id, "command": kind, "success": True})
    elif kind == "ui":
        write({"type": "extension_ui_request", "id": "ui-1", "method": "select"})
        write({"type": "response", "id": request_id, "command": kind, "success": True})
    elif kind == "extension_ui_response":
        write({"type": "ui_observed", "value": command.get("value")})
    elif kind == "new_session":
        session += 1
        write({"type": "response", "id": request_id, "command": kind, "success": True})
    else:
        write({"type": "response", "id": request_id, "command": kind, "success": True})
"#;

struct Smoke {
    root: tempfile::TempDir,
    bin: PathBuf,
}

impl Smoke {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let bin_dir = root.path().join("bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        // Copy the binary so the sibling `pi` resolution (current_exe parent)
        // lands on our fake child, hermetically.
        let bin = bin_dir.join("pi-orchestrator");
        std::fs::copy(env!("CARGO_BIN_EXE_pi-orchestrator"), &bin).unwrap();
        let fake_pi = bin_dir.join("pi");
        std::fs::write(&fake_pi, FAKE_PI).unwrap();
        std::fs::set_permissions(&fake_pi, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::create_dir_all(root.path().join("orch")).unwrap();
        std::fs::create_dir_all(root.path().join("work")).unwrap();
        std::fs::create_dir_all(root.path().join("agent")).unwrap();
        Self { root, bin }
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(&self.bin);
        command
            .args(args)
            .current_dir(self.root.path())
            .env("PI_ORCHESTRATOR_DIR", self.root.path().join("orch"))
            // Isolate radius credential lookup from the developer machine.
            .env("PI_CODING_AGENT_DIR", self.root.path().join("agent"))
            .env_remove("PI_RADIUS_API_KEY")
            .env_remove("PI_RADIUS_URL")
            .env_remove("PI_RADIUS_ORCHESTRATOR_URL");
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }

    fn run_json(&self, args: &[&str]) -> Value {
        let output = self.run(args);
        assert!(output.status.success(), "{args:?}: {output:?}");
        serde_json::from_slice(&output.stdout)
            .unwrap_or_else(|error| panic!("{args:?} stdout not JSON ({error}): {output:?}"))
    }

    fn work_dir(&self) -> PathBuf {
        self.root.path().join("work")
    }

    fn socket_path(&self) -> PathBuf {
        self.root.path().join("orch").join("orchestrator.sock")
    }
}

struct LineReader {
    receiver: mpsc::Receiver<String>,
}

impl LineReader {
    fn spawn(stream: impl std::io::Read + Send + 'static) -> Self {
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stream).lines() {
                let Ok(line) = line else { break };
                if sender.send(line).is_err() {
                    break;
                }
            }
        });
        Self { receiver }
    }

    fn next_line(&self) -> String {
        self.receiver
            .recv_timeout(Duration::from_secs(10))
            .expect("timed out waiting for output line")
    }
}

fn wait_until(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn pid_alive(pid: &str) -> bool {
    Command::new("kill")
        .args(["-0", pid])
        .stderr(Stdio::null())
        .status()
        .unwrap()
        .success()
}

fn wait_for_exit(child: &mut Child) -> std::process::ExitStatus {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        assert!(Instant::now() < deadline, "process did not exit");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn cli_smoke_end_to_end() {
    let smoke = Smoke::new();
    let version = env!("CARGO_PKG_VERSION");

    // --version / --help / no args (exact fixtures).
    let output = smoke.run(&["--version"]);
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        format!("{version}\n")
    );

    let expected_help = format!(
        "pi-orchestrator v{version}\n\nUsage:\n  pi-orchestrator serve\n  pi-orchestrator list\n  pi-orchestrator spawn [--cwd <path>] [--label <label>]\n  pi-orchestrator status <instance-id>\n  pi-orchestrator stop <instance-id>\n  pi-orchestrator rpc <instance-id> <json-command>\n  pi-orchestrator rpc-stream <instance-id>\n  pi-orchestrator --help\n  pi-orchestrator --version\n\nRPC stream stdin expects JSONL RpcCommand or extension_ui_response messages.\n"
    );
    let output = smoke.run(&["--help"]);
    assert!(output.status.success());
    assert_eq!(String::from_utf8_lossy(&output.stdout), expected_help);
    let output = smoke.run(&[]);
    assert!(output.status.success());
    assert_eq!(String::from_utf8_lossy(&output.stdout), expected_help);

    // Usage errors exit 1 on stderr.
    let output = smoke.run(&["status"]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        String::from_utf8_lossy(&output.stderr),
        "Usage: pi-orchestrator status <instance-id>\n"
    );
    let output = smoke.run(&["stop"]);
    assert_eq!(output.status.code(), Some(1));
    let output = smoke.run(&["rpc", "some-id"]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        String::from_utf8_lossy(&output.stderr),
        "Usage: pi-orchestrator rpc <instance-id> <json-command>\n"
    );
    let output = smoke.run(&["bogus"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).starts_with("Unknown command: bogus\n"));
    assert_eq!(String::from_utf8_lossy(&output.stdout), expected_help);

    // Client commands fail cleanly with no server.
    let output = smoke.run(&["list"]);
    assert_eq!(output.status.code(), Some(1));

    // Serve: banners, then ready.
    let mut serve = smoke
        .command(&["serve"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let serve_stdout = LineReader::spawn(serve.stdout.take().unwrap());
    assert_eq!(
        serve_stdout.next_line(),
        "radius integration disabled: login radius in ~/.pi/agent/auth.json or set PI_RADIUS_API_KEY"
    );
    assert_eq!(
        serve_stdout.next_line(),
        format!(
            "orchestrator listening on {}",
            smoke.socket_path().display()
        )
    );
    assert!(smoke.socket_path().exists());

    // One server per socket.
    let output = smoke.run(&["serve"]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        String::from_utf8_lossy(&output.stderr),
        format!(
            "orchestrator is already running: {}\n",
            smoke.socket_path().display()
        )
    );

    // spawn (option precedence: --cwd and --label override defaults).
    let work = smoke.work_dir();
    let spawn = smoke.run_json(&["spawn", "--cwd", work.to_str().unwrap(), "--label", "smoke"]);
    assert_eq!(spawn["type"], json!("spawn_result"));
    assert_eq!(spawn["ok"], json!(true));
    let instance = &spawn["instance"];
    assert_eq!(instance["status"], json!("online"));
    assert_eq!(instance["cwd"], json!(work.to_str().unwrap()));
    assert_eq!(instance["label"], json!("smoke"));
    assert_eq!(instance["sessionId"], json!("session-0"));
    let instance_id = instance["id"].as_str().unwrap().to_owned();
    let child_pid = std::fs::read_to_string(work.join("pi-pid")).unwrap();
    assert!(pid_alive(&child_pid));

    // spawn without --cwd defaults to the CLI's working directory.
    let default_spawn = smoke.run_json(&["spawn"]);
    assert_eq!(
        default_spawn["instance"]["cwd"],
        json!(smoke.root.path().to_str().unwrap())
    );
    let default_id = default_spawn["instance"]["id"].as_str().unwrap().to_owned();

    // list / status.
    let list = smoke.run_json(&["list"]);
    assert_eq!(list["type"], json!("list_result"));
    assert_eq!(list["instances"].as_array().unwrap().len(), 2);
    let status = smoke.run_json(&["status", &instance_id]);
    assert_eq!(status["type"], json!("status_result"));
    assert_eq!(status["instance"]["id"], json!(instance_id.clone()));

    // Unknown instance: exact pretty-printed fixture.
    let output = smoke.run(&["status", "nope"]);
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "{\n  \"type\": \"error\",\n  \"ok\": false,\n  \"error\": \"Unknown instance: nope\"\n}\n"
    );

    // rpc bridges a one-shot command.
    let rpc = smoke.run_json(&[
        "rpc",
        &instance_id,
        r#"{"type":"echo","id":"cli-1","value":{"n":5}}"#,
    ]);
    assert_eq!(
        rpc,
        json!({
            "type": "rpc_result",
            "ok": true,
            "response": {"type":"response","id":"cli-1","command":"echo","success":true,"data":{"n":5}}
        })
    );
    let output = smoke.run(&["rpc", &instance_id, "not json"]);
    assert_eq!(output.status.code(), Some(1));

    // rpc-stream: raw passthrough of rpc_ready, events, ui round-trip.
    let mut stream = smoke
        .command(&["rpc-stream", &instance_id])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stream_stdout = LineReader::spawn(stream.stdout.take().unwrap());
    let stream_stderr = LineReader::spawn(stream.stderr.take().unwrap());
    assert_eq!(
        stream_stderr.next_line(),
        format!(
            "connected to rpc stream {instance_id}; send JSONL RpcCommand or extension_ui_response messages on stdin"
        )
    );
    let ready: Value = serde_json::from_str(&stream_stdout.next_line()).unwrap();
    assert_eq!(ready["type"], json!("rpc_ready"));
    assert_eq!(ready["instance"]["id"], json!(instance_id.clone()));

    let mut stream_stdin = stream.stdin.take().unwrap();
    stream_stdin
        .write_all(b"{\"type\":\"emit\",\"id\":\"s-1\",\"value\":\"ping\"}\n")
        .unwrap();
    stream_stdin.flush().unwrap();
    let mut types = [
        serde_json::from_str::<Value>(&stream_stdout.next_line()).unwrap(),
        serde_json::from_str::<Value>(&stream_stdout.next_line()).unwrap(),
    ];
    types.sort_by_key(|value| value["type"].as_str().unwrap().to_owned());
    assert_eq!(types[0], json!({"type":"agent_event","value":"ping"}));
    assert_eq!(
        types[1],
        json!({"type":"response","id":"s-1","command":"emit","success":true})
    );

    stream_stdin
        .write_all(b"{\"type\":\"ui\",\"id\":\"s-2\"}\n")
        .unwrap();
    stream_stdin.flush().unwrap();
    let mut seen_ui = false;
    let mut seen_response = false;
    while !(seen_ui && seen_response) {
        let message: Value = serde_json::from_str(&stream_stdout.next_line()).unwrap();
        match message["type"].as_str().unwrap() {
            "extension_ui_request" => seen_ui = true,
            "response" => seen_response = true,
            other => panic!("unexpected stream line type {other}: {message}"),
        }
    }
    stream_stdin
        .write_all(b"{\"type\":\"extension_ui_response\",\"id\":\"ui-1\",\"value\":\"picked\"}\n")
        .unwrap();
    stream_stdin.flush().unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&stream_stdout.next_line()).unwrap(),
        json!({"type":"ui_observed","value":"picked"})
    );
    // Stdin EOF keeps the stream open: events still flow. Reopen is not
    // possible on a closed pipe, so verify liveness via a one-shot rpc that
    // makes the child emit an event observed on this stream.
    drop(stream_stdin);
    smoke.run_json(&[
        "rpc",
        &instance_id,
        r#"{"type":"emit","id":"after-eof","value":"still-here"}"#,
    ]);
    wait_until("post-EOF event on stream", || {
        matches!(
            stream_stdout.receiver.recv_timeout(Duration::from_secs(5)),
            Ok(line) if line.contains("still-here")
        )
    });
    stream.kill().unwrap();
    let _ = stream.wait();

    // stop removes the second instance and SIGTERMs nothing else.
    let stop = smoke.run_json(&["stop", &default_id]);
    assert_eq!(
        stop,
        json!({"type":"stop_result","ok":true,"instanceId": default_id})
    );
    let list = smoke.run_json(&["list"]);
    assert_eq!(list["instances"].as_array().unwrap().len(), 1);

    // SIGINT the server: children reaped, socket unlinked, exit 0.
    let serve_pid = serve.id().to_string();
    assert!(
        Command::new("kill")
            .args(["-INT", &serve_pid])
            .status()
            .unwrap()
            .success()
    );
    let status = wait_for_exit(&mut serve);
    assert!(status.success(), "serve exited with {status:?}");
    assert!(
        !smoke.socket_path().exists(),
        "socket file survived shutdown"
    );
    wait_until("fake pi child to die", || !pid_alive(&child_pid));

    // The stored record for the still-running instance was removed by the
    // shutdown stopInstance sweep.
    let instances_json =
        std::fs::read_to_string(smoke.root.path().join("orch").join("instances.json")).unwrap();
    assert_eq!(instances_json.trim(), "[]");
}
