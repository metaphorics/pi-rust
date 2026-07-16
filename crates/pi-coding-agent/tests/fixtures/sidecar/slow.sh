#!/usr/bin/env bash
# Handshakes fine but never answers lifecycle/load; logs cancel frames.
# Exercises request timeout + cancel emission.
set -u
. "$(dirname "$0")/lib.sh"
record_spawn

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
    *'"method":"lifecycle/load"'*) : ;; # swallow: never respond
    *'"method":"lifecycle/shutdown"'*)
      id=$(rid "$line")
      res_ok "$id" '{}'
      exit 0
      ;;
  esac
done
exit 0
