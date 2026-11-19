#!/usr/bin/env bash
# Real Splunk HEC e2e (CONMON_TEST_SPLUNK_REAL=1). Ownership file is claimed
# at create; EXIT trap retries stop-owned. Do not enable shell tracing.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BATS_DIR="${SCRIPT_DIR}/../bats"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
LOCAL_SPLUNK="${REPO_ROOT}/hack/local-splunk.sh"

# Unique per invocation so concurrent runs cannot share names.
export SPLUNK_CONTAINER="conmon-splunk-bats-$$-${RANDOM}-$(date +%s)"
export LOCAL_SPLUNK
# Empty until local-splunk.sh claims a container ID at create time.
export SPLUNK_OWNERSHIP_FILE="$(mktemp /tmp/conmon-splunk-own-XXXXXX)"
export CONMON_BINARY="/usr/bin/conmon-v3"
export CONMON_TEST_SPLUNK_REAL=1

# Set by the main body; EXIT cleanup must not hide a Bats failure.
SPLUNK_TEST_RC=0

cleanup() {
    local clean_rc=0
    # Retry/safety-net cleanup by recorded ID (not by name).
    if [[ -x "${LOCAL_SPLUNK}" ]]; then
        if ! "$LOCAL_SPLUNK" stop-owned; then
            echo "error: stop-owned failed; Splunk container may still be running" >&2
            clean_rc=1
        fi
    fi
    if [[ -n "${SPLUNK_OWNERSHIP_FILE:-}" ]]; then
        if [[ -s "$SPLUNK_OWNERSHIP_FILE" ]]; then
            echo "error: ownership file retained after cleanup; container may still be running" >&2
            clean_rc=1
            # Keep the file for operator inspection / manual stop-owned retry.
        else
            rm -f "$SPLUNK_OWNERSHIP_FILE"
        fi
    fi
    # Surface cleanup failure only when the test itself succeeded.
    if [[ "$clean_rc" -ne 0 && "${SPLUNK_TEST_RC}" -eq 0 ]]; then
        exit 1
    fi
}

trap cleanup EXIT

uname -r
rpm -q conmon-v3 runc crun podman

# Pulling splunk/splunk and waiting for HEC can take well over 15 minutes.
# Do not enable shell tracing: credentials may be in the environment.
cd "$BATS_DIR" && timeout 2400 bats 24-splunk-real.bats || {
    SPLUNK_TEST_RC=$?
    if [ "$SPLUNK_TEST_RC" -eq 124 ]; then
        echo "Splunk BATS timed out after 40 minutes" >&2
    fi
}

exit "$SPLUNK_TEST_RC"
