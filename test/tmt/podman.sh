#!/usr/bin/env bash

set -exo pipefail

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=test/tmt/debug-common.sh
source "$SCRIPT_DIR/debug-common.sh"

on_exit() {
    local rc=$?
    collect_failure_diagnostics "$rc" "$BATS_LOG"
    if [[ "$rc" -ne 0 ]]; then
        rerun_failed_podman_tests "$BATS_LOG"
    fi
}
trap on_exit EXIT

print_test_environment

# Install dependencies
dnf install -y podman-tests bats conmon-v3

# Show installed package versions
rpm -q conmon-v3 podman containers-common-extra crun runc || true

setup_conmon_for_tests /usr/bin/conmon-v3
verify_conmon_version /usr/bin/conmon-v3

# Do not set _PODMAN_TEST_OPTS here. setup_conmon_for_tests() symlinks
# /usr/bin/conmon and /usr/sbin/conmon to conmon-v3, which is how local podman
# finds conmon. Passing --conmon breaks podman-remote tests (CONTAINER_HOST,
# --remote) because the remote client does not accept that flag. Likewise,
# --log-level=debug pollutes run_podman $output because Bats merges stderr.

# Run Podman system tests; capture full output for post-failure analysis.
run_bats_with_logging "$BATS_LOG" bats --tap /usr/share/podman/test/system/
