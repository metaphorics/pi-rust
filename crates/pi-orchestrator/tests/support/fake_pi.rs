use std::fs;
use std::path::PathBuf;

use pi_orchestrator::rpc_process::RpcProcessOptions;
use uuid::Uuid;

pub struct FakePi {
    root: PathBuf,
    script: PathBuf,
}

impl FakePi {
    pub fn new() -> Self {
        let root = std::env::temp_dir().join(format!("pi-orchestrator-fake-{}", Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let script = root.join("fake_pi.py");
        fs::write(&script, SCRIPT).unwrap();
        Self { root, script }
    }

    pub fn options(&self) -> RpcProcessOptions {
        RpcProcessOptions {
            cwd: self.root.clone(),
            command_override: Some((
                PathBuf::from("python3"),
                vec!["-u".into(), self.script.to_string_lossy().into_owned()],
            )),
        }
    }
}

impl Drop for FakePi {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

const SCRIPT: &str = r#"
import json
import os
import signal
import sys

pairs = []

def write(value):
    sys.stdout.write(json.dumps(value, separators=(",", ":")) + "\n")
    sys.stdout.flush()

def terminate(received_signal, _frame):
    sys.stderr.write("received SIGTERM")
    sys.stderr.flush()
    signal.signal(received_signal, signal.SIG_DFL)
    os.kill(os.getpid(), received_signal)

signal.signal(signal.SIGTERM, terminate)

for raw in sys.stdin:
    command = json.loads(raw)
    kind = command.get("type")
    request_id = command.get("id")
    if kind == "echo":
        write({"type": "response", "id": request_id, "command": kind, "success": True, "data": command.get("value")})
    elif kind == "pair":
        pairs.append(command)
        if len(pairs) == 2:
            for item in reversed(pairs):
                write({"type": "response", "id": item["id"], "command": kind, "success": True, "data": item.get("value")})
            pairs.clear()
    elif kind == "emit":
        write({"type": "agent_event", "value": command.get("value")})
        write({"type": "response", "id": request_id, "command": kind, "success": True})
    elif kind == "ui":
        write({"type": "extension_ui_request", "id": "ui-1", "method": "select"})
        write({"type": "response", "id": request_id, "command": kind, "success": True})
    elif kind == "stderr":
        sys.stderr.write(command.get("value", ""))
        sys.stderr.flush()
        write({"type": "response", "id": request_id, "command": kind, "success": True})
    elif kind == "pending":
        write({"type": "pending_received", "id": request_id})
    elif kind == "exit":
        sys.stderr.write(command.get("stderr", ""))
        sys.stderr.flush()
        raise SystemExit(command.get("code", 1))
    elif kind == "extension_ui_response":
        write({"type": "ui_observed", "value": command.get("value")})
"#;
