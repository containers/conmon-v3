#!/usr/bin/env bats
#
# Regression: detached -t containers must keep the PTY master across transient
# POLLHUP so later console output (e.g. systemd multi-user.target) reaches logs.
# Matches Podman E2E "podman run container with systemd PID1".

load test_helper

SYSTEMD_IMAGE="${SYSTEMD_IMAGE:-quay.io/libpod/systemd-image:20240124}"

setup() {
    check_conmon_binary
    if ! command -v podman >/dev/null 2>&1; then
        skip "podman required"
    fi
}

teardown() {
    podman rm -f "$CTR_NAME" >/dev/null 2>&1 || true
}

@test "systemd tty: multi-user.target appears in podman logs with -t -i -d" {
    CTR_NAME="conmon-v3-systemd-tty-$$"

    # Same shape as Podman test/e2e/systemd_test.go
    run timeout 60 podman --conmon "$CONMON_BINARY" run \
        --name "$CTR_NAME" \
        --log-driver=k8s-file \
        -t -i -d \
        "$SYSTEMD_IMAGE" \
        /sbin/init
    assert_success

    # Poll logs for up to 30s (same budget as the Podman E2E helper).
    local found=0
    local i
    for i in $(seq 1 30); do
        run timeout 5 podman logs "$CTR_NAME"
        if [[ "$output" == *"Reached target multi-user.target - Multi-User System."* ]]; then
            found=1
            break
        fi
        # Confirm the container itself is healthy while we wait (boot, not hang).
        run timeout 5 podman exec "$CTR_NAME" systemctl is-system-running
        if [[ "$status" -ne 0 && "$output" != *"starting"* && "$output" != *"running"* ]]; then
            echo "systemctl unexpected: $output"
        fi
        sleep 1
    done

    [[ "$found" -eq 1 ]] || {
        echo "FAILED: multi-user.target never appeared in podman logs"
        echo "--- podman logs ---"
        timeout 5 podman logs "$CTR_NAME" || true
        echo "--- systemctl ---"
        timeout 5 podman exec "$CTR_NAME" systemctl status --no-pager || true
        return 1
    }

    # Container should be running systemd, not exited.
    run timeout 5 podman exec "$CTR_NAME" systemctl is-system-running
    assert_success
    [[ "$output" == "running" ]]
}
