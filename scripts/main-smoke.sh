#!/bin/bash
# Process-level smoke for the wired `pi` binary (P5-B4): argv/help/version,
# package subcommands, real settings/session fixtures, print/json/rpc against
# a local scripted provider, signals/exit codes, stdout purity, extension
# detection (zero-extension = no Bun; missing Bun degrades gracefully; real
# sidecar startup when Bun is present), and an interactive tmux PTY pass.
set -u
cd "$(dirname "$0")/.."
REPO=$(pwd)

cargo build -p pi-coding-agent --quiet || exit 1
BIN="$REPO/target/debug/pi"
fail=0

check() { # name expected actual
  if [ "$2" = "$3" ]; then echo "ok   - $1"; else echo "FAIL - $1: expected [$2] got [$3]"; fail=1; fi
}
check_contains() { # name needle haystack
  case "$3" in
    *"$2"*) echo "ok   - $1" ;;
    *) echo "FAIL - $1: [$3] does not contain [$2]"; fail=1 ;;
  esac
}
check_not_contains() { # name needle haystack
  case "$3" in
    *"$2"*) echo "FAIL - $1: output unexpectedly contains [$2]"; fail=1 ;;
    *) echo "ok   - $1" ;;
  esac
}

WORK=$(mktemp -d /tmp/pi-main-smoke.XXXXXX)
SERVER_PID=""
cleanup() {
  [ -n "$SERVER_PID" ] && kill "$SERVER_PID" 2>/dev/null
  tmux kill-session -t pi-main-smoke 2>/dev/null
  rm -rf -- "${WORK:?}"
}
trap cleanup EXIT

AGENT="$WORK/agent"; PROJ="$WORK/proj"
mkdir -p "$AGENT" "$PROJ"
export PI_CODING_AGENT_DIR="$AGENT"
export PI_OFFLINE=1
cd "$PROJ"

# ---------------------------------------------------------------- metadata
out=$("$BIN" --version); rc=$?
check "version exit" 0 "$rc"
check "version shape" "0.80.7" "$out"

out=$("$BIN" --help | head -1 | sed 's/\x1b\[[0-9;]*m//g'); rc=$?
check "help first line" "pi - AI coding assistant with read, bash, edit, write tools" "$out"
out=$("$BIN" install --help); rc=$?
check "install --help exit" 0 "$rc"
check_contains "install --help usage" "pi install <source>" "$out"

# ------------------------------------------------------------------ errors
err=$("$BIN" -x 2>&1 >/dev/null); rc=$?
check "unknown option exit" 1 "$rc"
check_contains "unknown option message" "Error: Unknown option: -x" "$err"

err=$("$BIN" --fork abc --continue 2>&1 >/dev/null); rc=$?
check "fork conflict exit" 1 "$rc"
check_contains "fork conflict message" "fork cannot be combined with --continue" "$err"

err=$(printf '' | "$BIN" --mode rpc @nope.txt 2>&1 >/dev/null); rc=$?
check "rpc @file exit" 1 "$rc"
check_contains "rpc @file message" "@file arguments are not supported in RPC mode" "$err"

err=$(printf 'hi' | "$BIN" -p 2>&1 >/dev/null); rc=$?
check "no models exit" 1 "$rc"
check_contains "no models message" "No models available." "$err"

# --------------------------------------------------------- package manager
mkdir -p "$WORK/local-pkg/skills/demo"
printf -- '---\nname: demo\ndescription: smoke skill\n---\nbody\n' > "$WORK/local-pkg/skills/demo/SKILL.md"
out=$("$BIN" install "$WORK/local-pkg" 2>&1); rc=$?
check "pkg install exit" 0 "$rc"
check_contains "pkg install settings" "local-pkg" "$(cat "$AGENT/settings.json")"
out=$("$BIN" list 2>&1); rc=$?
check "pkg list exit" 0 "$rc"
check_contains "pkg list shows package" "local-pkg" "$out"
out=$("$BIN" remove "$WORK/local-pkg" 2>&1); rc=$?
check "pkg remove exit" 0 "$rc"
check_not_contains "pkg removed from settings" "local-pkg" "$(cat "$AGENT/settings.json")"

# self-update offline: the version check is skipped (PI_OFFLINE), so the
# deterministic outcome is the could-not-determine error with exit 1.
out=$("$BIN" update self 2>&1); rc=$?
check "self-update exit" 1 "$rc"
check_contains "self-update offline message" "Could not determine latest pi version" "$out"

# ------------------------------------------------- local scripted provider
PORT=$(python3 - <<'EOF'
import socket
s = socket.socket()
s.bind(("127.0.0.1", 0))
print(s.getsockname()[1])
s.close()
EOF
)
python3 - "$PORT" "$WORK/requests.log" <<'EOF' &
import http.server, json, sys

PORT = int(sys.argv[1])
REQUEST_LOG = sys.argv[2]

class Handler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def do_POST(self):
        length = int(self.headers.get("content-length", 0))
        body = self.rfile.read(length)
        with open(REQUEST_LOG, "ab") as log:
            log.write(body + b"\n")
        chunks = []
        for word in ["Here", " is", " the", " reply:", " SMOKE-REPLY"]:
            chunks.append({"choices": [{"index": 0, "delta": {"content": word}}]})
        chunks.append({"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
                       "usage": {"prompt_tokens": 3, "completion_tokens": 5, "total_tokens": 8}})
        body = "".join(f"data: {json.dumps(c)}\n\n" for c in chunks) + "data: [DONE]\n\n"
        payload = body.encode()
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.send_header("content-length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def log_message(self, *args):
        pass

http.server.ThreadingHTTPServer(("127.0.0.1", PORT), Handler).serve_forever()
EOF
SERVER_PID=$!
sleep 0.3

cat > "$AGENT/models.json" <<EOF
{
  "providers": {
    "local": {
      "baseUrl": "http://127.0.0.1:$PORT",
      "api": "openai-completions",
      "apiKey": "smoke-key",
      "models": [
        {
          "id": "smoke-model",
          "name": "Smoke Model",
          "reasoning": false,
          "input": ["text", "image"],
          "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0},
          "contextWindow": 32000,
          "maxTokens": 1024
        }
      ]
    }
  }
}
EOF

# --list-models sees the custom provider.
out=$("$BIN" --list-models 2>/dev/null); rc=$?
check "list-models exit" 0 "$rc"
check_contains "list-models row" "smoke-model" "$out"

# ------------------------------------------------------------- print mode
out=$(printf 'say hi' | "$BIN" -p 2>"$WORK/print.err"); rc=$?
check "print exit" 0 "$rc"
check_contains "print reply" "SMOKE-REPLY" "$out"

# Session persisted by the print run; --continue appends to the same file.
SESSION_FILE=$(ls "$AGENT"/sessions/*/*.jsonl 2>/dev/null | head -1)
check "session file written" 1 "$(ls "$AGENT"/sessions/*/*.jsonl 2>/dev/null | wc -l)"
LINES_BEFORE=$(wc -l < "$SESSION_FILE")
out=$("$BIN" --continue -p 'again' 2>/dev/null); rc=$?
check "continue exit" 0 "$rc"
LINES_AFTER=$(wc -l < "$SESSION_FILE")
if [ "$LINES_AFTER" -gt "$LINES_BEFORE" ]; then
  echo "ok   - continue appended to session"
else
  echo "FAIL - continue appended to session: $LINES_BEFORE -> $LINES_AFTER"; fail=1
fi

# --session by id prefix re-opens the same session.
SESSION_ID=$(basename "$SESSION_FILE" .jsonl | cut -d_ -f2-)
out=$("$BIN" --session "${SESSION_ID:0:12}" -p 'third' 2>/dev/null); rc=$?
check "session-by-id exit" 0 "$rc"

# --------------------------------------------------------------- json mode
"$BIN" --mode json -p 'hi json' >"$WORK/json.out" 2>"$WORK/json.err"; rc=$?
check "json exit" 0 "$rc"
first_type=$(head -1 "$WORK/json.out" | sed -n 's/^{"type":"\([a-z_]*\)".*/\1/p')
check "json header first" "session" "$first_type"
check "json all lines are json" 0 "$(grep -cv '^{' "$WORK/json.out")"
check "json agent_end present" 1 "$(grep -c '"type":"agent_end"' "$WORK/json.out")"

# ---------------------------------------------------------------- rpc mode
printf '{"type":"get_state","id":"1"}\n{"type":"bogus","id":"9"}\nnot json\n' \
  | "$BIN" --mode rpc >"$WORK/rpc.out" 2>"$WORK/rpc.err"; rc=$?
check "rpc eof exit" 0 "$rc"
check "rpc id echo" 1 "$(grep -c '^{"id":"1","type":"response","command":"get_state","success":true,' "$WORK/rpc.out")"
check "rpc state model" 1 "$(grep -c '"smoke-model"' "$WORK/rpc.out")"
check "rpc unknown command" '{"id":"9","type":"response","command":"bogus","success":false,"error":"Unknown command: bogus"}' "$(sed -n 2p "$WORK/rpc.out")"
check "rpc parse envelope" 1 "$(grep -c '^{"type":"response","command":"parse","success":false,"error":"Failed to parse command: ' "$WORK/rpc.out")"

# ------------------------------------------------------------------ signals
signal_check() { # name signal expected
  local fifo pid rc
  fifo=$(mktemp -u "$WORK/fifo.XXXXXX")
  mkfifo "$fifo"
  exec 3<>"$fifo"
  "$BIN" --mode rpc < "$fifo" > /dev/null 2>&1 & pid=$!
  sleep 1
  if ! kill -0 "$pid" 2>/dev/null; then
    check "$1 (process alive)" "alive" "dead"
  else
    kill "-$2" "$pid"
    wait "$pid"; rc=$?
    check "$1" "$3" "$rc"
  fi
  exec 3>&-
  rm -f -- "$fifo"
}
signal_check "rpc SIGTERM exit" TERM 143
signal_check "rpc SIGHUP exit" HUP 129

# ------------------------------------------------ extensions: zero vs some
# Zero extensions: a run with an unusable Bun override must succeed —
# nothing may even attempt to resolve Bun (I6).
out=$(printf 'zero ext' | PI_RUST_BUN=/nonexistent/bun "$BIN" -p 2>"$WORK/zero.err"); rc=$?
check "zero-ext exit (Bun never needed)" 0 "$rc"
check_contains "zero-ext reply" "SMOKE-REPLY" "$out"
check_not_contains "zero-ext no sidecar warning" "extensions disabled" "$(cat "$WORK/zero.err")"

# Extension installed but Bun unusable: detection + bind run, startup fails,
# the agent degrades with the hint and still completes the turn.
mkdir -p "$PROJ/.pi/extensions"
cat > "$PROJ/.pi/extensions/smoke-ext.ts" <<'EOF'
export default function (pi) {
    pi.registerCommand("smoke-ext", { description: "smoke", handler: async () => {} });
}
EOF
out=$(printf 'ext no bun' | PI_RUST_BUN=/nonexistent/bun "$BIN" -a -p 2>"$WORK/nobun.err"); rc=$?
check "ext-no-bun exit" 0 "$rc"
check_contains "ext-no-bun reply" "SMOKE-REPLY" "$out"
check_contains "ext-no-bun warning" "extensions disabled" "$(cat "$WORK/nobun.err")"
check_contains "ext-no-bun hint" 'Start without extensions using "pi -ne"' "$(cat "$WORK/nobun.err")"

# -ne suppresses auto-discovery even with the extension installed.
out=$(printf 'ext ne' | PI_RUST_BUN=/nonexistent/bun "$BIN" -a -ne -p 2>"$WORK/ne.err"); rc=$?
check "ext -ne exit" 0 "$rc"
check_not_contains "ext -ne no warning" "extensions disabled" "$(cat "$WORK/ne.err")"

# -ne -e <path>: explicit paths still bind — with a broken Bun the bind is
# attempted and degrades (proof -ne did not skip it).
out=$(printf 'ext ne -e' | PI_RUST_BUN=/nonexistent/bun "$BIN" -a -ne -e "$PROJ/.pi/extensions/smoke-ext.ts" -p 2>"$WORK/ne-e.err"); rc=$?
check "ext -ne -e exit" 0 "$rc"
check_contains "ext -ne -e attempts bind" "extensions disabled" "$(cat "$WORK/ne-e.err")"

# Real sidecar startup when Bun is present (skipped when bun is missing).
if command -v bun >/dev/null 2>&1 && [ -d "$REPO/sidecar/node_modules" ]; then
  out=$(printf 'ext real' | PI_RUST_SIDECAR="$REPO/sidecar" "$BIN" -a -p 2>"$WORK/ext.err"); rc=$?
  check "ext-real exit" 0 "$rc"
  check_contains "ext-real reply" "SMOKE-REPLY" "$out"
  check_not_contains "ext-real no degrade warning" "extensions disabled" "$(cat "$WORK/ext.err")"

  # -ne -e with a REAL sidecar: the explicit extension actually loads —
  # its registered command is visible over RPC get_commands.
  printf '{"type":"get_commands","id":"1"}\n' \
    | PI_RUST_SIDECAR="$REPO/sidecar" "$BIN" -a -ne -e "$PROJ/.pi/extensions/smoke-ext.ts" --mode rpc \
    >"$WORK/ne-e-real.out" 2>"$WORK/ne-e-real.err"; rc=$?
  check "ext -ne -e real exit" 0 "$rc"
  check_not_contains "ext -ne -e real no degrade" "extensions disabled" "$(cat "$WORK/ne-e-real.err")"
  check_contains "ext -ne -e real registers command" "smoke-ext" "$(cat "$WORK/ne-e-real.out")"
else
  echo "skip - real sidecar smoke (bun or sidecar/node_modules missing)"
fi
rm -rf -- "${PROJ:?}/.pi"

# --------------------------------------------------- interactive PTY (tmux)
if command -v tmux >/dev/null 2>&1; then
  tmux kill-session -t pi-main-smoke 2>/dev/null
  tmux new-session -d -s pi-main-smoke -x 100 -y 30 \
    "env PI_CODING_AGENT_DIR='$AGENT' PI_OFFLINE=1 '$BIN'"
  sleep 2
  tmux send-keys -t pi-main-smoke -l "hello interactive"
  tmux send-keys -t pi-main-smoke Enter
  ok=0
  for _ in $(seq 1 40); do
    if tmux capture-pane -t pi-main-smoke -p 2>/dev/null | grep -q "SMOKE-REPLY"; then ok=1; break; fi
    sleep 0.25
  done
  check "interactive streamed reply" 1 "$ok"
  tmux send-keys -t pi-main-smoke -l "/quit"
  tmux send-keys -t pi-main-smoke Enter
  for _ in $(seq 1 20); do
    tmux has-session -t pi-main-smoke 2>/dev/null || break
    sleep 0.25
  done
  tmux kill-session -t pi-main-smoke 2>/dev/null
  # @image attachment: the initial interactive prompt must deliver the
  # image to the provider (oracle initialImages → session.prompt images).
  python3 - "$PROJ/pic.png" <<'EOF'
import base64, sys
PNG_1PX = base64.b64decode(
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg=="
)
with open(sys.argv[1], "wb") as f:
    f.write(PNG_1PX)
EOF
  : > "$WORK/requests.log"
  tmux new-session -d -s pi-main-smoke -x 100 -y 30 \
    "env PI_CODING_AGENT_DIR='$AGENT' PI_OFFLINE=1 '$BIN' --no-session @pic.png 'describe the picture'"
  img_ok=0
  for _ in $(seq 1 40); do
    if tmux capture-pane -t pi-main-smoke -p 2>/dev/null | grep -q "SMOKE-REPLY"; then img_ok=1; break; fi
    sleep 0.25
  done
  check "interactive @image streamed reply" 1 "$img_ok"
  check_contains "model received image content" 'data:image/png;base64,iVBORw0KGgo' "$(cat "$WORK/requests.log" 2>/dev/null)"
  check_contains "model received image prompt text" 'describe the picture' "$(cat "$WORK/requests.log" 2>/dev/null)"
  tmux send-keys -t pi-main-smoke -l "/quit"
  tmux send-keys -t pi-main-smoke Enter
  sleep 1
  tmux kill-session -t pi-main-smoke 2>/dev/null
  rm -f -- "$PROJ/pic.png"
  # ---------------------------------------------------- pi config (tmux)
  # Non-interactive surfaces first: help text and error exits.
  out=$("$BIN" config --help); rc=$?
  check "config --help exit" 0 "$rc"
  check_contains "config --help usage" "pi config [-l] [--approve|--no-approve]" "$out"
  err=$("$BIN" config -x 2>&1 >/dev/null); rc=$?
  check "config unknown option exit" 1 "$rc"
  check_contains "config unknown option text" 'Unknown option -x for "config".' "$err"

  # Interactive selector: reinstall the local package so a resource row
  # exists, open the TUI, toggle it off with Space, close with Esc.
  "$BIN" install "$WORK/local-pkg" >/dev/null 2>&1
  # quietStartup suppresses the startup header/resources, while --verbose
  # overrides it and expands resource entries to source paths.
  python3 - "$AGENT/settings.json" <<'EOF'
import json, sys
path = sys.argv[1]
with open(path, encoding="utf-8") as f:
    settings = json.load(f)
settings["quietStartup"] = True
with open(path, "w", encoding="utf-8") as f:
    json.dump(settings, f, indent=2)
    f.write("\n")
EOF
  tmux new-session -d -s pi-main-smoke -x 120 -y 80 \
    "env PI_CODING_AGENT_DIR='$AGENT' PI_OFFLINE=1 '$BIN' --verbose --no-session"
  sleep 2
  verbose_pane=$(tmux capture-pane -t pi-main-smoke -p 2>/dev/null)
  check_contains "verbose overrides quietStartup header" "pi v0.80.7" "$verbose_pane"
  check_contains "verbose overrides quietStartup resources" "[Skills]" "$verbose_pane"
  check_contains "verbose expands resource paths" "SKILL.md" "$verbose_pane"
  tmux kill-session -t pi-main-smoke 2>/dev/null
  tmux new-session -d -s pi-main-smoke -x 120 -y 40 \
    "env PI_CODING_AGENT_DIR='$AGENT' PI_OFFLINE=1 '$BIN' --no-session"
  sleep 2
  quiet_pane=$(tmux capture-pane -t pi-main-smoke -p 2>/dev/null)
  check_not_contains "quietStartup hides header" "pi v0.80.7" "$quiet_pane"
  check_not_contains "quietStartup hides resources" "[Skills]" "$quiet_pane"
  tmux kill-session -t pi-main-smoke 2>/dev/null
  tmux kill-session -t pi-config-smoke 2>/dev/null
  tmux new-session -d -s pi-config-smoke -x 100 -y 30 \
    "env PI_CODING_AGENT_DIR='$AGENT' PI_OFFLINE=1 '$BIN' config; echo config-exit=\$?; sleep 30"
  cfg_ok=0
  for _ in $(seq 1 40); do
    if tmux capture-pane -t pi-config-smoke -p 2>/dev/null | grep -q "Global Resources"; then cfg_ok=1; break; fi
    sleep 0.25
  done
  check "config TUI shows Global Resources" 1 "$cfg_ok"
  tmux capture-pane -t pi-config-smoke -p 2>/dev/null | grep -q "demo" && cfg_demo=1 || cfg_demo=0
  check "config TUI lists installed skill" 1 "$cfg_demo"
  tmux send-keys -t pi-config-smoke Space
  sleep 0.5
  tmux send-keys -t pi-config-smoke Escape
  cfg_exit=""
  for _ in $(seq 1 20); do
    cfg_exit=$(tmux capture-pane -t pi-config-smoke -p 2>/dev/null | sed -n 's/.*config-exit=\([0-9]*\).*/\1/p' | tail -1)
    [ -n "$cfg_exit" ] && break
    sleep 0.25
  done
  check "config Esc exits 0" 0 "$cfg_exit"
  check_contains "config toggle persisted disable pattern" '"-skills/demo/SKILL.md"' "$(cat "$AGENT/settings.json")"
  tmux kill-session -t pi-config-smoke 2>/dev/null
  # -l -a: project write scope — header starts in project mode, Space pins
  # an unload override into $PROJ/.pi/settings.json (global file untouched).
  GLOBAL_BEFORE=$(cat "$AGENT/settings.json")
  tmux new-session -d -s pi-config-smoke -x 100 -y 30 \
    "env PI_CODING_AGENT_DIR='$AGENT' PI_OFFLINE=1 '$BIN' config -l -a; echo config-exit=\$?; sleep 30"
  cfg_local=0
  for _ in $(seq 1 40); do
    if tmux capture-pane -t pi-config-smoke -p 2>/dev/null | grep -q "Project Local Resources"; then cfg_local=1; break; fi
    sleep 0.25
  done
  check "config -l shows Project Local Resources" 1 "$cfg_local"
  tmux send-keys -t pi-config-smoke Space
  sleep 0.5
  tmux send-keys -t pi-config-smoke Escape
  cfg_exit=""
  for _ in $(seq 1 20); do
    cfg_exit=$(tmux capture-pane -t pi-config-smoke -p 2>/dev/null | sed -n 's/.*config-exit=\([0-9]*\).*/\1/p' | tail -1)
    [ -n "$cfg_exit" ] && break
    sleep 0.25
  done
  check "config -l Esc exits 0" 0 "$cfg_exit"
  check_contains "config -l writes project override" '"+skills/demo/SKILL.md"' "$(cat "$PROJ/.pi/settings.json" 2>/dev/null)"
  check "config -l leaves global settings untouched" "$GLOBAL_BEFORE" "$(cat "$AGENT/settings.json")"
  tmux kill-session -t pi-config-smoke 2>/dev/null
  rm -rf -- "${PROJ:?}/.pi"
  "$BIN" remove "$WORK/local-pkg" >/dev/null 2>&1
else
  echo "skip - interactive PTY smoke (tmux missing)"
fi

echo
if [ "$fail" = 0 ]; then echo "main-smoke: ALL PASS"; else echo "main-smoke: FAILURES"; fi
exit "$fail"
