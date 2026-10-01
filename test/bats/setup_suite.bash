#!/usr/bin/env bash
#
# Suite-wide setup for conmon-v3 BATS tests.
# Mirrors conmon-v2/test/setup_suite.bash: pull and export a rootfs tarball once.

load test_helper

suite_fail() {
    echo "# FAIL: $*" >&3
    return 1
}

setup_suite() {
    if ! command -v podman >/dev/null 2>&1; then
        suite_fail "podman is required to prepare the test rootfs"
        return 1
    fi

    export CONMON_TEST_ROOTFS_TAR="$BATS_SUITE_TMPDIR/rootfs.tar"

    if ! timeout 300 podman pull "$UBI10_MICRO_IMAGE"; then
        suite_fail "failed to pull $UBI10_MICRO_IMAGE (timed out?)"
        return 1
    fi

    local ctr
    if ! ctr=$(podman create "$UBI10_MICRO_IMAGE"); then
        suite_fail "failed to create a container from $UBI10_MICRO_IMAGE"
        return 1
    fi
    if ! podman export "$ctr" >"$CONMON_TEST_ROOTFS_TAR"; then
        podman rm "$ctr" >/dev/null 2>&1 || true
        suite_fail "failed to export the rootfs from $UBI10_MICRO_IMAGE"
        return 1
    fi
    podman rm "$ctr" >/dev/null 2>&1 || true
}
