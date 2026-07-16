#!/usr/bin/env bash
# Emits a malformed stdout line and then a valid notification after init;
# the host must skip the garbage and keep the connection alive.
set -u
. "$(dirname "$0")/lib.sh"
record_spawn

hello
while IFS= read -r line; do
  case "$line" in
    *'"method":"lifecycle/ping"'*) pong "$line" ;;
    *'"method":"lifecycle/init"'*)
      id=$(rid "$line")
      initialized
      res_ok "$id" '{}'
      emit 'npm warn this is not a protocol frame'
      emit '{"type":"ev","method":"ui/notify","params":{"message":"after-garbage","level":"info"}}'
      ;;
    *'"method":"lifecycle/load"'*)
      id=$(rid "$line")
      load_ok "$id"
      ;;
    *'"method":"lifecycle/shutdown"'*)
      id=$(rid "$line")
      res_ok "$id" '{}'
      exit 0
      ;;
  esac
done
exit 0
