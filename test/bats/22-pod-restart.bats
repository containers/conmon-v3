#!/usr/bin/env bats
#
# Regression: rapid pod stop -t0 / start with an always-init container must not
# leave the pod systemd slice loaded such that the next StartTransientUnit fails
# with "Unit ... was already loaded or has a fragment file".
#
# Reproduces the Podman E2E shape from
# test/e2e/pod_initcontainers_test.go ("ensure always init containers always run").

load test_helper

FEDORA_MINIMAL="${FEDORA_MINIMAL:-quay.io/libpod/testimage:20241011}"

setup() {
    check_conmon_binary
    if ! command -v podman >/dev/null 2>&1; then
        skip "podman required"
    fi
    # Point Podman at this build (global --conmon works for run; create/pod use conf).
    mkdir -p "$HOME/.config/containers/containers.conf.d"
    cat > "$HOME/.config/containers/containers.conf.d/99-conmon-v3-bats.conf" << EOF
[engine]
conmon_path = ["$CONMON_BINARY"]
cgroup_manager = "systemd"
EOF
    POD_NAME="conmon-v3-pod-restart-$$"
    podman pod rm -f "$POD_NAME" >/dev/null 2>&1 || true
}

teardown() {
    podman pod rm -f "$POD_NAME" >/dev/null 2>&1 || true
    rm -f "$HOME/.config/containers/containers.conf.d/99-conmon-v3-bats.conf"
}

@test "pod restart: always-init + stop -t0 + start keeps slice reusable" {
    run timeout 60 podman create --init-ctr always --pod "new:$POD_NAME" \
        "$FEDORA_MINIMAL" /bin/sh -c 'date +%T.%N > /dev/shm/initstamp'
    assert_success

    # Use the same image as the init ctr so /bin/sh and shared /dev/shm work for verify.
    run timeout 60 podman create --pod "$POD_NAME" -t "$FEDORA_MINIMAL" top
    assert_success

    run timeout 60 podman pod start "$POD_NAME"
    assert_success

    PODID=$(podman pod inspect "$POD_NAME" --format '{{.Id}}')
    if [ "$(id -u)" -eq 0 ]; then
        SLICE="machine-libpod_pod_${PODID}.slice"
        SYSTEMCTL=(systemctl)
    else
        SLICE="user-libpod_pod_${PODID}.slice"
        SYSTEMCTL=(systemctl --user)
    fi

    # Confirm we are actually using the conmon under test (ignore unrelated leftover execs).
    run bash -c "pgrep -af \"$CONMON_BINARY\" | grep -v exit-delay | grep -E 'conmon-v3-pod-restart|libpod_pod_${PODID}|--cid' || pgrep -af \"$CONMON_BINARY\" | head -5"
    [[ "$status" -eq 0 ]] || skip "could not observe conmon for this pod"

    run timeout 60 podman pod stop -t0 "$POD_NAME"
    assert_success

    # Diagnostics for CI: slice/cgroup/conmon state right after stop.
    echo "=== after stop ==="
    "${SYSTEMCTL[@]}" show "$SLICE" -p LoadState -p ActiveState -p SubState -p Job || true
    pgrep -af "$CONMON_BINARY" | grep -F "$PODID" || echo "no conmon processes for this pod id"
    "${SYSTEMCTL[@]}" list-units --all "libpod-conmon-*.scope" --no-legend 2>/dev/null | grep -F "$PODID" || true

    # Immediate restart — the failure mode under investigation.
    run timeout 60 podman pod start "$POD_NAME"
    if [ "$status" -ne 0 ]; then
        echo "=== restart failed; slice/journal diagnostics ==="
        echo "$output"
        "${SYSTEMCTL[@]}" status "$SLICE" --no-pager -l || true
        "${SYSTEMCTL[@]}" --state=failed --no-legend || true
        journalctl --no-pager -n 80 || true
        return 1
    fi

    # Init container must have rewritten the stamp (always-init ran again).
    TOP=$(podman ps --filter "pod=$POD_NAME" --filter "status=running" --format '{{.ID}} {{.Command}}' | awk '$2 ~ /top/ {print $1; exit}')
    [[ -n "$TOP" ]]
    run timeout 30 podman exec "$TOP" /bin/sh -c 'cat /dev/shm/initstamp'
    assert_success
    [[ -n "$output" ]]
}
