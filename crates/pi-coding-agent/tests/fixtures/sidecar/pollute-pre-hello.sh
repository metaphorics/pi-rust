#!/usr/bin/env bash
# Pollutes stdout before any hello. Strict handshake: this must be fatal.
set -u
. "$(dirname "$0")/lib.sh"
record_spawn
printf 'npm warn deprecated something@1.0.0\n'
hello
sleep 30
exit 0
