#!/usr/bin/env bash
# Handshakes and inits, admits one post-init request, then wedges without
# reading stdin. This lets tests saturate the writer while that request waits.
set -u
. "$(dirname "$0")/lib.sh"
record_spawn

hello
initialized_done=false
while IFS= read -r line; do
  if [ "$initialized_done" = false ]; then
    case "$line" in
      *'"method":"lifecycle/init"'*)
        id=$(rid "$line")
        initialized
        res_ok "$id" '{}'
        initialized_done=true
        ;;
    esac
    continue
  fi

  case "$line" in
    *'"type":"req"'*)
      log_line 'request-admitted'
      exec sleep 600
      ;;
  esac
done
exit 0
