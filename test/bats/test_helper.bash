#!/usr/bin/env bash
#
# Shared helpers for conmon-v3 BATS tests.
#
# Reuses the mature helpers from the conmon-v2 test suite (fetched via
# `make conmon-v2`) so v3-specific bats files stay small and consistent.

# shellcheck disable=SC2034,SC2154

CONMON_V3_TEST_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CONMON_V3_ROOT="$(cd "$CONMON_V3_TEST_DIR/../.." && pwd)"
CONMON_V2_TEST_DIR="${CONMON_V2_TEST_DIR:-$CONMON_V3_ROOT/conmon-v2/test}"

if [[ ! -f "$CONMON_V2_TEST_DIR/test_helper.bash" ]]; then
    echo "conmon-v2 test helpers not found at $CONMON_V2_TEST_DIR" >&2
    echo "Run 'make conmon-v2' first (or set CONMON_V2_TEST_DIR)." >&2
    return 1 2>/dev/null || exit 1
fi

# Prefer the v3 binary under test when the caller has not set CONMON_BINARY.
if [[ -z "${CONMON_BINARY:-}" ]]; then
    if [[ -x "$CONMON_V3_ROOT/target/debug/conmon" ]]; then
        CONMON_BINARY="$CONMON_V3_ROOT/target/debug/conmon"
    elif [[ -x "$CONMON_V3_ROOT/target/release/conmon" ]]; then
        CONMON_BINARY="$CONMON_V3_ROOT/target/release/conmon"
    fi
fi
export CONMON_BINARY

# Prefer crun when present (common on Fedora); fall back to runc.
if [[ -z "${RUNTIME_BINARY:-}" ]]; then
    if [[ -x /usr/bin/crun ]]; then
        RUNTIME_BINARY=/usr/bin/crun
    elif [[ -x /usr/bin/runc ]]; then
        RUNTIME_BINARY=/usr/bin/runc
    fi
fi
export RUNTIME_BINARY

# shellcheck source=/dev/null
source "$CONMON_V2_TEST_DIR/test_helper.bash"

# Skip when the local syslog socket / journal tooling is unavailable.
skip_if_no_syslog() {
    if [[ ! -e /dev/log && ! -e /var/run/syslog && ! -e /var/run/log ]]; then
        skip "no local syslog socket (/dev/log)"
    fi
    if ! command -v journalctl >/dev/null 2>&1; then
        skip "journalctl not available to verify syslog delivery"
    fi
}

# True when journalctl has seen $pattern for SYSLOG_IDENTIFIER=$ident.
_syslog_ident_has_pattern() {
    local ident=$1
    local pattern=$2
    journalctl --no-pager -t "$ident" --since "5 min ago" 2>/dev/null | grep -q -- "$pattern"
}

# Wait until a syslog identity carries the expected message.
wait_for_syslog_message() {
    local ident=$1
    local pattern=$2
    local how_long=${3:-15}

    retry "$how_long" 0.2 _syslog_ident_has_pattern "$ident" "$pattern" ||
        die "timed out waiting for '$pattern' under SYSLOG_IDENTIFIER=$ident: $(
            journalctl --no-pager -t "$ident" --since "5 min ago" 2>&1 | tail -20
        )"
}
