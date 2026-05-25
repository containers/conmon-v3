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

# Run e2e tests using the installed conmon-v3 binary
# Use timeout to ensure we don't hang waiting for background processes
cd "$CONMON_V2_DIR/test" && timeout 840 env CONMON_BINARY="/usr/bin/conmon-v3" bats . || {
    rc=$?
    if [ $rc -eq 124 ]; then
        echo "BATS timed out after 14 minutes" >&2
    fi
    exit $rc
}
