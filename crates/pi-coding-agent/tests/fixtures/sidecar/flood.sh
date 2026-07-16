#!/usr/bin/env bash
# Floods 2000 ordered notifications DURING init handling (before the init
# response), then serves normally. Exercises bounded inbound delivery with a
# live control plane: init must still complete while the queue backpressures.
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
      i=1
      while [ "$i" -le 2000 ]; do
        emit "{\"type\":\"ev\",\"method\":\"ui/notify\",\"params\":{\"message\":\"flood-$i\",\"level\":\"info\"}}"
        i=$((i + 1))
      done
      res_ok "$id" '{}'
      ;;
    *'"method":"lifecycle/shutdown"'*)
      id=$(rid "$line")
      res_ok "$id" '{}'
      exit 0
      ;;
  esac
done
exit 0
