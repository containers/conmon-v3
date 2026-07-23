#!/usr/bin/env bash

# Shared helpers for TMT/Packit test jobs. Source from e2e.sh and podman.sh.

CONMON_DEBUG_LOG="${CONMON_DEBUG_LOG:-/tmp/conmon-v3-test.log}"
BATS_LOG="${BATS_LOG:-/tmp/conmon-bats.log}"

setup_conmon_for_tests() {
    local conmon_bin="${1:-/usr/bin/conmon-v3}"

    ln -sf "$conmon_bin" /usr/bin/conmon
    ln -sf "$conmon_bin" /usr/sbin/conmon

    export CONMON_LOG_LEVEL="${CONMON_LOG_LEVEL:-debug}"
    export CONMON_LOG_PATH="$CONMON_DEBUG_LOG"
    : >"$CONMON_DEBUG_LOG"

    export RUST_BACKTRACE="${RUST_BACKTRACE:-1}"
    export RUST_LIB_BACKTRACE="${RUST_LIB_BACKTRACE:-1}"
}

verify_conmon_version() {
    local conmon_bin="${1:-/usr/bin/conmon-v3}"

    local version
    version=$("$conmon_bin" --version)
    echo "$version"
    grep -qE '^conmon version 3(\.|$)' <<< "$version"

    version=$(/usr/bin/conmon --version)
    echo "via /usr/bin/conmon: $version"
    grep -qE '^conmon version 3(\.|$)' <<< "$version"
}

print_test_environment() {
    echo "=== Test environment ==="
    date -Is
    uname -a
    echo

    echo "=== Package versions ==="
    rpm -q conmon-v3 podman containers-common-extra crun runc 2>/dev/null || true
    echo

    echo "=== conmon binaries ==="
    ls -l /usr/bin/conmon /usr/bin/conmon-v3 /usr/sbin/conmon 2>/dev/null || true
    readlink -f /usr/bin/conmon /usr/sbin/conmon 2>/dev/null || true
    echo

    echo "=== podman ==="
    if command -v podman >/dev/null; then
        podman version 2>/dev/null || true
        echo
        podman info 2>/dev/null | head -80 || true
    fi
    echo

    echo "=== Debug env ==="
    env | sort | grep -E '^(CONMON_|PODMAN_|CONTAINERS_|RUST_|BATS_|_PODMAN_)' || true
    echo
}

collect_conmon_debug_log() {
    echo "=== conmon log ($CONMON_DEBUG_LOG) ==="
    if [[ -s "$CONMON_DEBUG_LOG" ]]; then
        tail -500 "$CONMON_DEBUG_LOG"
    else
        echo "(empty or missing)"
    fi
    echo
}

collect_journal_conmon_hints() {
    echo "=== Recent conmon-related journal entries ==="
    if command -v journalctl >/dev/null; then
        journalctl --no-pager -n 200 --grep=conmon 2>/dev/null || \
            journalctl --no-pager -n 200 2>/dev/null | grep -i conmon || \
            echo "(no conmon journal entries found)"
    else
        echo "(journalctl not available)"
    fi
    echo
}

collect_exit_artifacts() {
    echo "=== Recent libpod exit files ==="
    find /run /var/run "$HOME/.local/share/containers" \
        \( -path '*/libpod/tmp/exits/*' -o -path '*/libpod/tmp/persist/*/exit' \) \
        -type f -mmin -120 2>/dev/null |
        head -20 |
        while read -r f; do
            echo "--- $f ---"
            cat "$f" 2>/dev/null || true
        done
    echo
}

collect_failure_diagnostics() {
    local rc=$1
    local log_file=${2:-$BATS_LOG}

    [[ "$rc" -eq 0 ]] && return 0

    echo
    echo "!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!"
    echo "!!! Tests failed with exit code $rc — collecting debug info"
    echo "!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!"
    echo

    print_test_environment
    collect_conmon_debug_log
    collect_journal_conmon_hints
    collect_exit_artifacts

    if [[ -f "$log_file" ]]; then
        echo "=== Failed BATS tests ==="
        grep '^not ok' "$log_file" || echo "(none matched '^not ok')"
        echo

        echo "=== Last 120 lines of BATS log ==="
        tail -120 "$log_file"
        echo
    fi
}

# Re-run failed podman system tests with shell tracing and verbose podman logs.
rerun_failed_podman_tests() {
    local log_file=$1
    local system_dir=${2:-/usr/share/podman/test/system}

    [[ -f "$log_file" ]] || return 0

    # Debug logs go to stderr and are merged into Bats $output, so only enable
    # them when re-running individual failing tests for diagnosis. Do not pass
    # --conmon here; it breaks podman-remote subtests.
    export _PODMAN_TEST_OPTS="--log-level=debug"

    local line test_file test_name
    while IFS= read -r line; do
        test_file=$(sed -n 's/^not ok [0-9]\+ \[\([0-9]\+\)\].*/\1/p' <<< "$line")
        test_name=$(sed -n 's/^not ok [0-9]\+ \[[0-9]\+\] //p' <<< "$line")
        [[ -n "$test_file" && -n "$test_name" ]] || continue

        local bats_file="$system_dir/${test_file}-*.bats"
        bats_file=$(compgen -G "$bats_file" | head -1)
        [[ -n "$bats_file" ]] || continue

        echo "=== Re-running failed test with trace: [$test_file] $test_name ==="
        bats -t "$bats_file" -f "$test_name" || true
        echo
    done < <(grep '^not ok' "$log_file" | tail -5)
}

run_bats_with_logging() {
    local log_file=$1
    shift

    : >"$log_file"
    set +e
    "$@" 2>&1 | tee "$log_file"
    local rc=${PIPESTATUS[0]}
    set -e
    return "$rc"
}
