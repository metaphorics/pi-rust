#!/usr/bin/env bash
# Speaks protocol version 99. The handshake must fail.
set -u
. "$(dirname "$0")/lib.sh"
record_spawn
emit '{"type":"ev","method":"lifecycle/hello","params":{"protocol":99,"pi":"9.99.9","bun":"fake-1.0"}}'
sleep 30
exit 0
