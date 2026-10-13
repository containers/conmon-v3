#!/usr/bin/env bash

set -exo pipefail

# Cleanup function to kill lingering processes
cleanup() {
    echo "Running cleanup..."
    pkill -9 conmon || true
    pkill -9 runc || true
    rm -rf /var/run/crio/* || true
}

# Set trap to ensure cleanup runs on exit
trap cleanup EXIT

uname -r

# Create directory required by e2e tests
mkdir -p /var/run/crio

# Show installed package versions
rpm -q conmon-v3 runc crun podman

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BATS_DIR="${SCRIPT_DIR}/../bats"

# Run e2e tests using the installed conmon-v3 binary
# Use timeout to ensure we don't hang waiting for background processes
cd "$BATS_DIR" && timeout 840 env CONMON_BINARY="/usr/bin/conmon-v3" bats . || {
    rc=$?
    if [ $rc -eq 124 ]; then
        echo "BATS timed out after 14 minutes" >&2
    fi
    exit $rc
}
