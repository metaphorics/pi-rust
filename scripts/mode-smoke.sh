#!/bin/bash
# Process-level smoke for the wire modes (print/json/rpc) via the
# examples/mode_smoke driver: framing, exit codes, and signal handling
# against real stdin/stdout. Binary wiring (src/main.rs dispatch) lands with
# the CLI unit; this exercises the REAL mode handlers.
set -u
cd "$(dirname "$0")/.."

cargo build --example mode_smoke --quiet || exit 1
BIN=target/debug/examples/mode_smoke
fail=0

check() { # name expected actual
  if [ "$2" = "$3" ]; then echo "ok   - $1"; else echo "FAIL - $1: expected [$2] got [$3]"; fail=1; fi
}

# --- print mode: final assistant text only, exit 0 -------------------------
out=$("$BIN" print "hello" < /dev/null); rc=$?
check "print stdout" "done" "$out"
check "print exit" "0" "$rc"

# --- json mode: SessionHeader first, agent_settled last, all-JSON lines ----
"$BIN" json "hello" < /dev/null > /tmp/mode-smoke-json.out; rc=$?
check "json exit" "0" "$rc"
check "json header first" "session" "$(head -1 /tmp/mode-smoke-json.out | sed -n 's/^{"type":"\([a-z_]*\)".*/\1/p')"
check "json settled last" '{"type":"agent_settled"}' "$(tail -1 /tmp/mode-smoke-json.out)"

# --- rpc: id echo, unknown-command survival, parse envelope, EOF exit 0 ----
printf '{"id":"1","type":"get_state"}\n{"id":"9","type":"bogus"}\n{oops\n' \
  | "$BIN" rpc > /tmp/mode-smoke-rpc.out; rc=$?
check "rpc eof exit" "0" "$rc"
check "rpc id echo" "1" "$(grep -c '^{"id":"1","type":"response","command":"get_state","success":true,' /tmp/mode-smoke-rpc.out)"
check "rpc unknown command" '{"id":"9","type":"response","command":"bogus","success":false,"error":"Unknown command: bogus"}' "$(sed -n 2p /tmp/mode-smoke-rpc.out)"
check "rpc parse envelope" "1" "$(grep -c '^{"type":"response","command":"parse","success":false,"error":"Failed to parse command: ' /tmp/mode-smoke-rpc.out)"

# --- rpc prompt: preflight response emitted even on immediate EOF ----------
out=$(printf '{"id":"p1","type":"prompt","message":"go"}\n' | "$BIN" rpc | head -1)
check "rpc prompt preflight" '{"id":"p1","type":"response","command":"prompt","success":true}' "$out"

# --- signals: SIGTERM -> 143 (no flush), SIGHUP -> 129 ----------------------
signal_check() { # name signal expected
  local fifo pid rc
  fifo=$(mktemp -u /tmp/mode-smoke-fifo.XXXXXX)
  mkfifo "$fifo"
  # Hold the write end open so stdin never EOFs before the signal.
  exec 3<>"$fifo"
  "$BIN" rpc < "$fifo" > /dev/null 2>&1 & pid=$!
  sleep 1
  if ! kill -0 "$pid" 2>/dev/null; then
    check "$1 (process alive)" "alive" "dead"
  else
    kill "-$2" "$pid"
    wait "$pid"; rc=$?
    check "$1" "$3" "$rc"
  fi
  exec 3>&-
  rm -f "$fifo"
}
signal_check "rpc SIGTERM exit" TERM 143
signal_check "rpc SIGHUP exit" HUP 129

exit "$fail"
