#!/bin/bash
# Process-level smoke for the wired `pi` binary (P5-B4): argv/help/version,
# package subcommands, real settings/session fixtures, print/json/rpc against
# a local scripted provider, signals/exit codes, stdout purity, extension
# detection (zero-extension = no Bun; missing Bun degrades gracefully; real
# sidecar startup when Bun is present), and an interactive tmux PTY pass with
# persistent editor history.
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
python3 - "$PORT" <<'EOF' &
import http.server, json, sys

PORT = int(sys.argv[1])

class Handler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def do_POST(self):
        length = int(self.headers.get("content-length", 0))
        self.rfile.read(length)
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
          "input": ["text"],
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

# -ne skips detection entirely even with the extension installed.
out=$(printf 'ext ne' | PI_RUST_BUN=/nonexistent/bun "$BIN" -a -ne -p 2>"$WORK/ne.err"); rc=$?
check "ext -ne exit" 0 "$rc"
check_not_contains "ext -ne no warning" "extensions disabled" "$(cat "$WORK/ne.err")"

# Real sidecar startup when Bun is present (skipped when bun is missing).
if command -v bun >/dev/null 2>&1 && [ -d "$REPO/sidecar/node_modules" ]; then
  out=$(printf 'ext real' | PI_RUST_SIDECAR="$REPO/sidecar" "$BIN" -a -p 2>"$WORK/ext.err"); rc=$?
  check "ext-real exit" 0 "$rc"
  check_contains "ext-real reply" "SMOKE-REPLY" "$out"
  check_not_contains "ext-real no degrade warning" "extensions disabled" "$(cat "$WORK/ext.err")"
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
  check_contains "history.jsonl persisted" "hello interactive" "$(cat "$AGENT/history.jsonl" 2>/dev/null)"

  # Second run: Up arrow recalls the persisted prompt.
  tmux kill-session -t pi-main-smoke 2>/dev/null
  tmux new-session -d -s pi-main-smoke -x 100 -y 30 \
    "env PI_CODING_AGENT_DIR='$AGENT' PI_OFFLINE=1 '$BIN' --no-session"
  sleep 2
  tmux send-keys -t pi-main-smoke Up
  sleep 0.5
  recall=0
  tmux capture-pane -t pi-main-smoke -p 2>/dev/null | grep -q "hello interactive" && recall=1
  check "history recall via Up" 1 "$recall"
  tmux send-keys -t pi-main-smoke C-c
  tmux send-keys -t pi-main-smoke -l "/quit"
  tmux send-keys -t pi-main-smoke Enter
  sleep 1
  tmux kill-session -t pi-main-smoke 2>/dev/null
else
  echo "skip - interactive PTY smoke (tmux missing)"
fi

echo
if [ "$fail" = 0 ]; then echo "main-smoke: ALL PASS"; else echo "main-smoke: FAILURES"; fi
exit "$fail"
