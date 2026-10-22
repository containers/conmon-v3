#!/usr/bin/env bats

load test_helper

setup() {
    check_conmon_binary
    check_runtime_binary
    setup_container_env
}

teardown() {
    cleanup_test_env
}

@test "runtime: simple runtime test" {
    run_conmon_with_default_args --log-path "k8s-file:$LOG_PATH"

    # Check that log file was created
    [ -f "$LOG_PATH" ]
    run cat "$LOG_PATH"
    assert "${output}" =~ "hello from ubi10" "'hello from ubi10' found in the log"
}

@test "runtime: container execution with different log drivers" {
    # Test with journald log driver
    run_conmon_with_default_args --log-path "journald:"

    run journalctl --user CONTAINER_ID_FULL="$CTR_ID"
    assert "${output}" =~ "hello from ubi10" "'hello from ubi10' found in the journald"
}

@test "runtime: container execution with multiple log drivers" {
    # Test with both k8s-file and journald log drivers
    run_conmon_with_default_args --log-path "k8s-file:$LOG_PATH" --log-path "journald:"

    # Check that log file was created
    [ -f "$LOG_PATH" ]

    run cat "$LOG_PATH"
    assert "${output}" =~ "hello from ubi10" "'hello from ubi10' found in the log"

    run journalctl --user CONTAINER_ID_FULL="$CTR_ID"
    assert "${output}" =~ "hello from ubi10" "'hello from ubi10' found in the journald"
}

@test "runtime: container with log size limit" {
    # Test container execution with log rotation
    # This effectively keeps just the last line at max.
    local log_size_max=10

    run_conmon_with_default_args --log-path "k8s-file:$LOG_PATH" --log-size-max "$log_size_max"

    # Check that log file was created
    [ -f "$LOG_PATH" ]

    run cat "$LOG_PATH"
    assert "${output}" !~ "hello from ubi10 11" "'hello from ubi10 11' not in the logs"
}

@test "runtime: invalid runtime binary should fail" {
    # Test with non-existent runtime binary
    run_conmon \
        --cid "$CTR_ID" \
        --cuuid "$CTR_ID" \
        --runtime "/nonexistent/runtime" \
        --log-path "k8s-file:$LOG_PATH" \
        --bundle "$BUNDLE_PATH" \
        --socket-dir-path "$SOCKET_PATH" \
        --container-pidfile "$PID_FILE" \
        --conmon-pidfile "$CONMON_PID_FILE"

    assert_failure
}

@test "runtime: configuration validation works" {
    # Test that conmon can validate its configuration
    # This is a basic smoke test for the runtime integration
    run_conmon --version
    assert_success
    assert_output_contains "conmon version"
}

@test "runtime: simple test with _OCI_SYNCPIPE defined" {
    start_oci_sync_pipe_reader
    run_conmon_with_default_args \
        --log-path "k8s-file:$LOG_PATH" 6>"$OCI_SYNCPIPE_PATH"

    # Check that the pid is sent to the sync pipe.
    wait_for_syncpipe_output 1
    run cat "$TEST_TMPDIR/syncpipe-output"
    CONTAINER_PID=$(cat "$CONTAINER_PIDFILE")
    assert_json "${output}" =~ "\"pid\": $CONTAINER_PID"
}

@test "runtime: runc error with _OCI_SYNCPIPE defined" {
    if [[ $(basename "$RUNTIME_BINARY") != "runc" ]]; then
        skip "test requires runc"
    fi
    # This trailing " results in wrong config.json. We expect the runtime
    # failure.
    setup_container_env '"'
    start_oci_sync_pipe_reader
    run_conmon \
        --cid "$CTR_ID" \
        --cuuid "$CTR_ID" \
        --runtime "$RUNTIME_BINARY" \
        --log-path "k8s-file:$LOG_PATH" \
        --bundle "$BUNDLE_PATH" \
        --socket-dir-path "$SOCKET_PATH" \
        --syslog \
        --container-pidfile "$PID_FILE" \
        --conmon-pidfile "$CONMON_PID_FILE" 6>"$OCI_SYNCPIPE_PATH"

    # Give conmon some time to run the runtime and fail.
    sleep 1

    assert_file_exists "$CONMON_PID_FILE"
    CONMON_PID=$(cat "$CONMON_PID_FILE")
    wait_for_conmon_exit "$CONMON_PID"

    # Check that the error is sent to the sync pipe.
    wait_for_syncpipe_output 1
    run cat "$TEST_TMPDIR/syncpipe-output"
    assert_json "${output}" =~ "\"pid\": -1"
    assert_json "${output}" =~ "\"message\":"
    assert_json "${output}" =~ "runc create failed"
}

# Without _OCI_SYNCPIPE and without --sync, conmon double-forks: the process
# bats waits on exits immediately while the child stays in create. If that
# child then fails waiting for the console fd, the runtime must be killed or
# it is reparented to PID 1. A stub runtime connects without sending an
# SCM_RIGHTS fd and would otherwise keep sleeping.
@test "runtime: --terminal create without sync pipe kills runtime on console failure" {
    if ! command -v python3 >/dev/null 2>&1; then
        skip "python3 required for the stub runtime console-socket client"
    fi

    local fake_runtime="$TEST_TMPDIR/stuck-console-runtime"
    export FAKE_RUNTIME_PIDFILE="$TEST_TMPDIR/fake-runtime.pid"

    cat >"$fake_runtime" <<'EOF'
#!/usr/bin/env python3
"""Stub runtime: fail the console handshake, then sleep like a stuck create."""
import os
import socket
import sys
import time

open(os.environ["FAKE_RUNTIME_PIDFILE"], "w", encoding="utf-8").write(str(os.getpid()))

console = None
args = sys.argv[1:]
i = 0
while i < len(args):
    if args[i] == "--console-socket" and i + 1 < len(args):
        console = args[i + 1]
        i += 2
    else:
        i += 1

if console:
    for _ in range(50):
        if os.path.exists(console):
            break
        time.sleep(0.1)
    sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    sock.connect(console)
    # Byte without SCM_RIGHTS: conmon rejects; keep the socket open briefly so
    # the recv completes, then sleep so we would be orphaned without kill_runtime.
    sock.send(b"x")
    time.sleep(300)
else:
    time.sleep(300)
EOF
    chmod +x "$fake_runtime"

    # Bare create: no sync pipe, no --sync (double-fork). Do not use the
    # create/start helpers; those wait on the child and hide this path.
    run_conmon \
        --cid "$CTR_ID" \
        --cuuid "$CTR_ID" \
        --runtime "$fake_runtime" \
        --log-path "k8s-file:$LOG_PATH" \
        --bundle "$BUNDLE_PATH" \
        --socket-dir-path "$SOCKET_PATH" \
        --container-pidfile "$PID_FILE" \
        --conmon-pidfile "$CONMON_PID_FILE" \
        --terminal

    # Outer process exited; the pidfile names the double-forked child.
    assert_file_exists "$CONMON_PID_FILE"
    CONMON_PID=$(cat "$CONMON_PID_FILE")
    wait_for_conmon_exit "$CONMON_PID" 15

    assert_file_exists "$FAKE_RUNTIME_PIDFILE"
    local runtime_pid
    runtime_pid=$(cat "$FAKE_RUNTIME_PIDFILE")
    if kill -0 "$runtime_pid" 2>/dev/null; then
        kill -9 "$runtime_pid" 2>/dev/null || true
        die "runtime pid $runtime_pid still running after console-socket failure"
    fi
}
