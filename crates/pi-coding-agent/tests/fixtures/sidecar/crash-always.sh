#!/usr/bin/env bash
# Dies immediately on every spawn, before any handshake.
set -u
. "$(dirname "$0")/lib.sh"
record_spawn
exit 7
