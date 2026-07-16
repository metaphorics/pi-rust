#!/usr/bin/env bash
# Handshakes and inits, then wedges completely: stops reading stdin and never
# writes again. Exercises writer backpressure and bounded shutdown-by-kill.
set -u
. "$(dirname "$0")/lib.sh"
record_spawn

hello
while IFS= read -r line; do
  case "$line" in
    *'"method":"lifecycle/init"'*)
      id=$(rid "$line")
      initialized
      res_ok "$id" '{}'
      # Wedge: keep the process alive without consuming stdin.
      exec sleep 600
      ;;
  esac
done
exit 0
