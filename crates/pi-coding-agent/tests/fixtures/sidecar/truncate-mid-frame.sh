#!/usr/bin/env bash
# Dies mid-frame: on lifecycle/load it writes HALF of a valid response (no
# newline) and exits. Strict NDJSON: the partial frame must never satisfy the
# pending request.
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
      ;;
    *'"method":"lifecycle/load"'*)
      id=$(rid "$line")
      printf '{"type":"res","id":%s,"ok":{"registrations"' "$id"
      exit 1
      ;;
  esac
done
exit 0
