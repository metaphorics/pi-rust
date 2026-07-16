#!/usr/bin/env bash
# Well-behaved sidecar: full handshake, ping/pong, load, clean shutdown exit.
set -u
. "$(dirname "$0")/lib.sh"
record_spawn
printf 'fake sidecar booted\n' >&2

hello
while IFS= read -r line; do
  case "$line" in
    *'"method":"lifecycle/ping"'*) pong "$line" ;;
    '{"type":"cancel"'*) log_line "$line" ;;
    *'"method":"lifecycle/init"'*)
      id=$(rid "$line")
      initialized
      res_ok "$id" '{}'
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
    '{"type":"req"'*)
      id=$(rid "$line")
      res_ok "$id" 'null'
      ;;
  esac
done
exit 0
