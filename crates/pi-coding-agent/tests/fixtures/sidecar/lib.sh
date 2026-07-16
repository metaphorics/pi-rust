# Shared helpers for fake-sidecar scripts. Sourced, never executed.
#
# These scripts stand in for `bun sidecar/src/main.ts` in lifecycle tests:
# the test constructs a SidecarLauncher whose `bun` is the scenario script,
# so the host spawns a real process speaking real NDJSON over real pipes.
#
# Env contract:
#   FAKE_SIDECAR_SPAWNS  file; one line appended per spawn (spawn counter)
#   FAKE_SIDECAR_LOG     file; receives cancel frames and markers

record_spawn() {
  if [ -n "${FAKE_SIDECAR_SPAWNS:-}" ]; then
    echo spawn >>"$FAKE_SIDECAR_SPAWNS"
  fi
}

spawn_number() {
  if [ -n "${FAKE_SIDECAR_SPAWNS:-}" ] && [ -f "$FAKE_SIDECAR_SPAWNS" ]; then
    wc -l <"$FAKE_SIDECAR_SPAWNS"
  else
    echo 1
  fi
}

emit() {
  printf '%s\n' "$1"
}

hello() {
  emit '{"type":"ev","method":"lifecycle/hello","params":{"protocol":1,"pi":"0.80.7","bun":"fake-1.0"}}'
}

# Envelope key order is deterministic (serde emits tag first, id second), so
# the id can be anchored to the line head.
rid() {
  printf '%s' "$1" | sed -n 's/^{"type":"req","id":\([0-9][0-9]*\).*/\1/p'
}

initialized() {
  emit '{"type":"ev","method":"lifecycle/initialized","params":{"registrations":{"tools":[],"commands":[],"shortcuts":[],"flags":[],"providers":[]},"subscribedEvents":[],"errors":[]}}'
}

res_ok() { # $1 = id, $2 = ok json
  emit "{\"type\":\"res\",\"id\":$1,\"ok\":$2}"
}

load_ok() { # $1 = id
  res_ok "$1" '{"registrations":{"tools":[],"commands":[],"shortcuts":[],"flags":[],"providers":[]},"errors":[]}'
}

pong() { # $1 = the ping line
  emit "${1//lifecycle\/ping/lifecycle\/pong}"
}

log_line() {
  if [ -n "${FAKE_SIDECAR_LOG:-}" ]; then
    printf '%s\n' "$1" >>"$FAKE_SIDECAR_LOG"
  fi
}
