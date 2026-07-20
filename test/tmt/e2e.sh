#!/usr/bin/env bash

set -exo pipefail

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=test/tmt/debug-common.sh
source "$SCRIPT_DIR/debug-common.sh"

BATS_LOG="${BATS_LOG:-/tmp/conmon-e2e-bats.log}"

# Cleanup function to kill lingering processes
cleanup() {
    echo "Running cleanup..."
    pkill -9 conmon || true
    pkill -9 runc || true
    rm -rf /var/run/crio/* || true
}

on_exit() {
    local rc=$?
    cleanup
    collect_failure_diagnostics "$rc" "$BATS_LOG"
}
trap on_exit EXIT

print_test_environment

uname -r

# Clone conmon-v2 repository for e2e tests
CONMON_V2_DIR="conmon-v2"
CONMON_V2_URL="https://github.com/containers/conmon.git"

# Clone conmon-v2 if not already present
if [ ! -d "$CONMON_V2_DIR" ]; then
    git clone "$CONMON_V2_URL" "$CONMON_V2_DIR"
fi

# Create directory required by conmon-v2 tests
mkdir -p /var/run/crio

# Show installed package versions
rpm -q conmon-v3 runc

setup_conmon_for_tests /usr/bin/conmon-v3
export CONMON_BINARY="/usr/bin/conmon-v3"

# Run e2e tests using the installed conmon-v3 binary
# Use timeout to ensure we don't hang waiting for background processes
cd "$CONMON_V2_DIR/test"
set +e
timeout 840 env CONMON_BINARY="$CONMON_BINARY" \
    CONMON_LOG_LEVEL="$CONMON_LOG_LEVEL" \
    CONMON_LOG_PATH="$CONMON_DEBUG_LOG" \
    bats --tap . 2>&1 | tee "$BATS_LOG"
rc=${PIPESTATUS[0]}
set -e

if [ "$rc" -eq 124 ]; then
    echo "BATS timed out after 14 minutes" >&2
fi
exit "$rc"
