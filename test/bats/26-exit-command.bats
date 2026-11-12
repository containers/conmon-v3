#!/usr/bin/env bats

load test_helper

setup() {
    check_conmon_binary
    check_runtime_binary
    setup_test_env
    prepare_exit_probe
}

# Marker file and the script that appends its parent pid. Call this after the
# test directory exists; setup_container_env replaces that directory.
prepare_exit_probe() {
    mkdir -p "$TEST_TMPDIR/exits"
    cat >"$TEST_TMPDIR/record-ppid" <<'EOF'
#!/bin/sh
printf '%s\n' "$PPID" >> "$1"
EOF
    chmod +x "$TEST_TMPDIR/record-ppid"
    EXIT_MARKER="$TEST_TMPDIR/exit-ran"
}

teardown() {
    if [[ -n "${PARENT_PID:-}" ]]; then
        kill "$PARENT_PID" 2>/dev/null || true
        wait "$PARENT_PID" 2>/dev/null || true
    fi
    if [[ -n "${CHILD_PID:-}" ]]; then
        kill "$CHILD_PID" 2>/dev/null || true
        wait "$CHILD_PID" 2>/dev/null || true
    fi
    cleanup_test_env
}

# Start conmon in the background. PARENT_PID is the process the caller waits on.
start_conmon_parent() {
    "$CONMON_BINARY" \
        --cid "$CTR_ID" \
        --cuuid "$CTR_ID" \
        --runtime "$1" \
        --bundle "$BUNDLE_PATH" \
        --socket-dir-path "$SOCKET_PATH" \
        --container-pidfile "$PID_FILE" \
        --conmon-pidfile "$CONMON_PID_FILE" \
        --exit-command "$TEST_TMPDIR/record-ppid" \
        --exit-command-arg "$EXIT_MARKER" \
        --exit-dir "$TEST_TMPDIR/exits" \
        "${@:2}" &
    PARENT_PID=$!
}

# Wait for the spawned process. Its status is left in $status, and WAITED_PID
# keeps that pid after PARENT_PID is cleared.
wait_conmon_parent() {
    WAITED_PID=$PARENT_PID
    status=0
    wait "$PARENT_PID" || status=$?
    PARENT_PID=""
}

@test "exit command: pre-fork syslog arg rejection does not run cleanup" {
    run --separate-stderr "$CONMON_BINARY" \
        --cid "$CTR_ID" \
        --cuuid "$CTR_ID" \
        --runtime "$RUNTIME_BINARY" \
        --log-driver syslog \
        --log-driver-opt syslog-bogus=x \
        --exit-command "$TEST_TMPDIR/record-ppid" \
        --exit-command-arg "$EXIT_MARKER" \
        --exit-dir "$TEST_TMPDIR/exits"
    [ "$status" -eq 1 ]
    assert_stderr_contains "conmon:"
    [ ! -s "$EXIT_MARKER" ]
    [ -z "$(ls -A "$TEST_TMPDIR/exits")" ]
}

@test "exit command: post-fork runtime failure still runs cleanup" {
    cat >"$TEST_TMPDIR/fail-runtime" <<'EOF'
#!/bin/sh
exit 1
EOF
    chmod +x "$TEST_TMPDIR/fail-runtime"

    start_conmon_parent "$TEST_TMPDIR/fail-runtime" --log-driver none
    wait_conmon_parent
    [ "$status" -eq 0 ]

    CHILD_PID=$(cat "$CONMON_PID_FILE")
    wait_for_conmon_exit "$CHILD_PID" 20
    [ -s "$EXIT_MARKER" ]
    grep -qx "$CHILD_PID" "$EXIT_MARKER"
    if grep -qx "$WAITED_PID" "$EXIT_MARKER"; then
        die "waited-on parent ran the exit command"
    fi
    CHILD_PID=""
}

@test "exit command: --sync failure still runs cleanup" {
    cat >"$TEST_TMPDIR/fail-runtime" <<'EOF'
#!/bin/sh
exit 1
EOF
    chmod +x "$TEST_TMPDIR/fail-runtime"

    # --sync does not double-fork. The process the caller waits on is the one
    # that runs the runtime, and it still runs the exit command.
    start_conmon_parent "$TEST_TMPDIR/fail-runtime" --log-driver none --sync
    wait_conmon_parent
    [ "$status" -eq 1 ]
    [ -s "$EXIT_MARKER" ]
    grep -qx "$WAITED_PID" "$EXIT_MARKER"
}
