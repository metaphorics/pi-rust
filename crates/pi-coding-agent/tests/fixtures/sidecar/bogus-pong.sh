#!/usr/bin/env bash
# Forges a far-future pong nonce right after init and never answers real
# pings. Strict nonce matching: the forgery must not defeat wedge detection.
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
      emit '{"type":"ev","method":"lifecycle/pong","params":{"nonce":18446744073709551615}}'
      ;;
    *) : ;; # ignore everything else, including real pings
  esac
done
exit 0
