#!/usr/bin/env bash
# Handshakes and inits but ignores pings. Exercises heartbeat death.
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
      ;;
    *) : ;; # ignore everything else, including pings
  esac
done
exit 0
