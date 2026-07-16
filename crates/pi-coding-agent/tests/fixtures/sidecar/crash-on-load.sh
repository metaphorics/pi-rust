#!/usr/bin/env bash
# Spawn 1: dies without responding when lifecycle/load arrives (crash while a
# request is in flight). Spawn 2+: delegates to ok.sh (successful respawn).
set -u
. "$(dirname "$0")/lib.sh"
record_spawn
if [ "$(spawn_number)" -ge 2 ]; then
  FAKE_SIDECAR_SPAWNS="" exec "$(dirname "$0")/ok.sh" "$@"
fi

hello
while IFS= read -r line; do
  case "$line" in
    *'"method":"lifecycle/ping"'*) pong "$line" ;;
    *'"method":"lifecycle/init"'*)
      id=$(rid "$line")
      initialized
      res_ok "$id" '{}'
      ;;
    *'"method":"lifecycle/load"'*) exit 1 ;; # crash mid-request
  esac
done
exit 0
