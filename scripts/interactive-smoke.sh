#!/bin/bash
set -u

# Tmux gate: verify tmux is installed and available
if ! command -v tmux >/dev/null 2>&1; then
    echo "ERROR: tmux is required but not installed." >&2
    exit 1
fi

# Locate the worktree root
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKTREE="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$WORKTREE"

# Redirect TMPDIR to avoid inode exhaustion on /tmp (ZFS/Zpool target dir)
export TMPDIR="$WORKTREE/target/tmp"
mkdir -p "$TMPDIR"

# Clean up tmux sessions on exit
SESSION=""
SUS_SESSION=""
cleanup() {
    if [ -n "${SESSION:-}" ]; then
        tmux kill-session -t "$SESSION" 2>/dev/null || true
    fi
    if [ -n "${SUS_SESSION:-}" ]; then
        tmux kill-session -t "$SUS_SESSION" 2>/dev/null || true
    fi
}
trap cleanup EXIT

# Helpers
pass() {
    echo "PASS $1"
}

fail() {
    echo "FAIL $1"
    if [ -n "${SESSION:-}" ]; then
        echo "=== TMUX PANE CAPTURE ==="
        tmux capture-pane -t "$SESSION" -p -S -
        echo "========================="
    fi
    exit 1
}

wait_for() {
    local regex="$1"
    local timeout_s="$2"
    local max_checks=$((timeout_s * 5))
    local check=0
    while [ "$check" -lt "$max_checks" ]; do
        if tmux capture-pane -pt "$SESSION" -S - | grep -qE "$regex"; then
            return 0
        fi
        sleep 0.2
        check=$((check + 1))
    done
    return 1
}

wait_for_not() {
    local regex="$1"
    local timeout_s="$2"
    local max_checks=$((timeout_s * 5))
    local check=0
    while [ "$check" -lt "$max_checks" ]; do
        if ! tmux capture-pane -pt "$SESSION" | grep -qE "$regex"; then
            return 0
        fi
        sleep 0.2
        check=$((check + 1))
    done
    return 1
}

wait_for_count() {
    local regex="$1"
    local expected_count="$2"
    local timeout_s="$3"
    local max_checks=$((timeout_s * 5))
    local check=0
    while [ "$check" -lt "$max_checks" ]; do
        local actual_count
        actual_count=$(tmux capture-pane -pt "$SESSION" -S - | grep -c "$regex" || true)
        if [ "$actual_count" -eq "$expected_count" ]; then
            return 0
        fi
        sleep 0.2
        check=$((check + 1))
    done
    return 1
}

# (1) build: cargo build --offline -p pi-coding-agent --example interactive_smoke
echo "Step 1: Building example..."
cargo build --offline -p pi-coding-agent --example interactive_smoke || fail "build"
pass "build"

# (2) Launch session and wait for editor/footer frame
echo "Step 2: Starting interactive_smoke in tmux..."
SESSION="pi_smoke_$$"
# Launch with our TMPDIR env var passed to the process inside tmux
TMPDIR="$TMPDIR" tmux new-session -d -s "$SESSION" -x 100 -y 30 "$WORKTREE/target/debug/examples/interactive_smoke" || fail "tmux start"

# Wait for footer marker indicating TUI is loaded and idle
wait_for "thinking off" 10 || fail "startup"
sleep 1.0
pass "startup"

# (3) send text, verify SMOKE-REPLY
echo "Step 3: Sending 'hello smoke'..."
tmux send-keys -t "$SESSION" -l 'hello smoke'
tmux send-keys -t "$SESSION" Enter
wait_for "SMOKE-REPLY" 10 || fail "hello smoke reply"
# Wait for the stream to complete fully
wait_for_count "escape cancellation" 1  10 || fail "stream completion after hello smoke"
sleep 1.0
pass "hello smoke reply"

# (4) /model + Enter; wait for model selector; send C-c; verify editor frame back
echo "Step 4: Testing model selector..."
tmux send-keys -t "$SESSION" -l '/model'
tmux send-keys -t "$SESSION" Enter
wait_for "Only showing models" 10 || fail "model selector UI"
sleep 1.0
tmux send-keys -t "$SESSION" C-c
wait_for_not "Only showing models" 10 || fail "restore editor"
sleep 1.0
pass "restore editor"

# (5) run the tool demo; wait_for progress_demo; wait_for SMOKE-TOOL-DONE
echo "Step 5: Testing tool execution..."
tmux send-keys -t "$SESSION" -l 'run the tool demo'
tmux send-keys -t "$SESSION" Enter
wait_for "progress_demo" 10 || fail "progress_demo box"
wait_for "SMOKE-TOOL-DONE" 10 || fail "tool execution done message"
sleep 1.0
pass "tool execution done message"

# (6) resize mid-stream: send hello smoke + Enter, sleep 0.3, resize, verify no garbled duplication
echo "Step 6: Testing resize mid-stream..."
# Send prompt
tmux send-keys -t "$SESSION" -l 'hello smoke'
tmux send-keys -t "$SESSION" Enter
sleep 0.3

# Resize window/pane
tmux resize-window -t "$SESSION" -x 80 -y 24 2>/dev/null || tmux resize-pane -t "$SESSION" -x 80 -y 24 || fail "resize"

# Wait for stream to complete by waiting for the final word of the response
wait_for_count "escape cancellation" 2 10 || fail "wait for stream complete after resize"

# Assert no garbled duplication: SMOKE-REPLY count must be exactly 2
expected=2
actual=$(tmux capture-pane -t "$SESSION" -p -S - | grep -c 'SMOKE-REPLY' || true)
if [ "$actual" -ne "$expected" ]; then
    fail "resize check: expected $expected SMOKE-REPLY occurrences, got $actual"
fi
sleep 1.0
pass "resize mid-stream"

# (7) abort: send hello smoke + Enter, sleep 0.2, send Escape; wait_for 'Operation aborted'
echo "Step 7: Testing abort/interrupt..."
# Send prompt
tmux send-keys -t "$SESSION" -l 'hello smoke'
tmux send-keys -t "$SESSION" Enter
sleep 0.2

# Send Escape to interrupt (using the Kitty protocol CSI sequence for Escape)
tmux send-keys -t "$SESSION" -l $'\x1b[27u'
wait_for_count "Operation aborted" 1 10 || fail "abort"
sleep 1.0
pass "abort"

# (8) double-escape on empty editor: two quick Escapes open the tree selector
echo "Step 8: Testing double-escape tree selector..."
tmux send-keys -t "$SESSION" -l $'\x1b[27u'
tmux send-keys -t "$SESSION" -l $'\x1b[27u'
wait_for "Session Tree" 10 || fail "double-escape tree selector"
sleep 0.6
# One Escape cancels the selector and restores the editor
tmux send-keys -t "$SESSION" -l $'\x1b[27u'
wait_for_not "Session Tree" 10 || fail "double-escape selector close"
sleep 1.0
pass "double-escape tree selector"

# (9) Ctrl+Z suspend: under a job-controlling shell the process must stop
# (SIGTSTP to its own process group), resume on fg, and repaint with
# working input. A dedicated session is used because a direct pane command
# is an orphaned process group whose SIGTSTP is discarded by the kernel.
echo "Step 9: Testing Ctrl+Z suspend/resume..."
SUS_SESSION="pi_smoke_sus_$$"
tmux new-session -d -s "$SUS_SESSION" -x 100 -y 30 bash || fail "suspend session start"
tmux send-keys -t "$SUS_SESSION" -l "TMPDIR='$TMPDIR' '$WORKTREE/target/debug/examples/interactive_smoke'"
tmux send-keys -t "$SUS_SESSION" Enter

sus_wait() {
    local regex="$1"; local timeout_s="$2"; local check=0
    while [ "$check" -lt $((timeout_s * 5)) ]; do
        if tmux capture-pane -pt "$SUS_SESSION" -S - | grep -qE "$regex"; then
            return 0
        fi
        sleep 0.2; check=$((check + 1))
    done
    return 1
}

sus_wait "thinking off" 15 || fail "suspend: startup under shell"
sleep 1.0
SHELL_PID=$(tmux list-panes -t "$SUS_SESSION" -F '#{pane_pid}')
SMOKE_PID=$(pgrep -P "$SHELL_PID" -f interactive_smoke | head -1)
[ -n "$SMOKE_PID" ] || fail "suspend: smoke pid not found"

tmux send-keys -t "$SUS_SESSION" C-z
stop_poll=0
while [ "$stop_poll" -lt 50 ]; do
    state=$(ps -o stat= -p "$SMOKE_PID" 2>/dev/null || true)
    case "$state" in *T*) break ;; esac
    sleep 0.2
    stop_poll=$((stop_poll + 1))
done
case "$(ps -o stat= -p "$SMOKE_PID" 2>/dev/null || true)" in
    *T*) : ;;
    *) fail "suspend: process did not stop on Ctrl+Z" ;;
esac
# The shell got the terminal back and reports the stopped job.
sus_wait "Stopped|suspended" 5 || fail "suspend: shell did not report stopped job"

# Resume with fg
tmux send-keys -t "$SUS_SESSION" -l 'fg'
tmux send-keys -t "$SUS_SESSION" Enter
run_poll=0
while [ "$run_poll" -lt 50 ]; do
    state=$(ps -o stat= -p "$SMOKE_PID" 2>/dev/null || true)
    case "$state" in *T*) sleep 0.2; run_poll=$((run_poll + 1)) ;; *) break ;; esac
done
case "$(ps -o stat= -p "$SMOKE_PID" 2>/dev/null || true)" in
    *T*) fail "resume: process still stopped after fg" ;;
esac
# UI must repaint (footer marker in the CURRENT viewport) and accept input.
resume_poll=0
while [ "$resume_poll" -lt 50 ]; do
    if tmux capture-pane -pt "$SUS_SESSION" | grep -qE "thinking off"; then break; fi
    sleep 0.2
    resume_poll=$((resume_poll + 1))
done
tmux capture-pane -pt "$SUS_SESSION" | grep -qE "thinking off" || fail "resume: UI did not repaint"
tmux send-keys -t "$SUS_SESSION" -l 'resumed-input-check'
sus_wait "resumed-input-check" 10 || fail "resume: input not accepted"
tmux send-keys -t "$SUS_SESSION" -l '/quit'
tmux send-keys -t "$SUS_SESSION" Enter
sleep 1.0
tmux kill-session -t "$SUS_SESSION" 2>/dev/null || true
SUS_SESSION=""
pass "suspend/resume"

# (10) /quit + Enter; poll until tmux has-session -t "$SESSION" fails
echo "Step 10: Testing quit..."
tmux send-keys -t "$SESSION" -l '/quit'
tmux send-keys -t "$SESSION" Enter

# Poll until tmux session is closed (up to 10s)
max_poll=50
poll=0
while [ "$poll" -lt "$max_poll" ]; do
    if ! tmux has-session -t "$SESSION" 2>/dev/null; then
        SESSION="" # Clear session so cleanup doesn't try to kill it again
        break
    fi
    sleep 0.2
    poll=$((poll + 1))
done

if [ "$poll" -eq "$max_poll" ]; then
    fail "quit session"
fi
pass "quit session"

echo "All steps completed successfully!"
