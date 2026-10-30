#!/usr/bin/env bats
# Real Splunk HEC e2e. CONMON_TEST_SPLUNK_REAL=1 to run; otherwise skip.
load test_helper

setup_file() {
    unset SPLUNK_REAL_SKIP_REASON

    case "${CONMON_TEST_SPLUNK_REAL:-}" in
    1 | true | TRUE | yes | YES) ;;
    0 | false | FALSE | no | NO | "")
        export SPLUNK_REAL_SKIP_REASON="CONMON_TEST_SPLUNK_REAL=${CONMON_TEST_SPLUNK_REAL:-unset}; use 1 or Packit job e2e-splunk"
        return 0
        ;;
    *)
        echo "invalid CONMON_TEST_SPLUNK_REAL=${CONMON_TEST_SPLUNK_REAL} (use 0 or 1)" >&2
        return 1
        ;;
    esac

    for bin in podman curl python3; do
        if ! command -v "$bin" >/dev/null 2>&1; then
            echo "$bin is required when CONMON_TEST_SPLUNK_REAL=1" >&2
            return 1
        fi
    done

    local ports
    ports="$(python3 - <<'PY'
import socket
ports = []
for _ in range(3):
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    ports.append(str(s.getsockname()[1]))
    s.close()
print(" ".join(ports))
PY
)"
    # shellcheck disable=SC2086
    set -- $ports
    export SPLUNK_WEB_PORT="${SPLUNK_WEB_PORT:-$1}"
    export SPLUNK_HEC_PORT="${SPLUNK_HEC_PORT:-$2}"
    export SPLUNK_MGMT_PORT="${SPLUNK_MGMT_PORT:-$3}"
    export SPLUNK_CONTAINER="${SPLUNK_CONTAINER:-conmon-splunk-bats-$$-${RANDOM}}"
    export SPLUNK_PASSWORD="${SPLUNK_PASSWORD:-Changeme1!}"
    export SPLUNK_HEC_TOKEN="${SPLUNK_HEC_TOKEN:-abcd1234}"
    export LOCAL_SPLUNK="${LOCAL_SPLUNK:-$CONMON_V3_ROOT/hack/local-splunk.sh}"

    if [[ -z "${SPLUNK_OWNERSHIP_FILE:-}" ]]; then
        SPLUNK_OWNERSHIP_FILE="$(mktemp /tmp/conmon-splunk-own-XXXXXX)"
        export SPLUNK_OWNERSHIP_FILE SPLUNK_OWNERSHIP_FILE_CREATED=1
    fi

    if [[ ! -x "$LOCAL_SPLUNK" ]]; then
        echo "missing executable Splunk helper: $LOCAL_SPLUNK" >&2
        return 1
    fi

    # start refuses pre-existing names when SPLUNK_OWNERSHIP_FILE is set.
    if ! "$LOCAL_SPLUNK" start; then
        echo "failed to start Splunk Enterprise / HEC for container '${SPLUNK_CONTAINER}'" >&2
        return 1
    fi
}

teardown_file() {
    local clean_rc=0
    if [[ -n "${LOCAL_SPLUNK:-}" && -x "${LOCAL_SPLUNK}" ]]; then
        if ! "$LOCAL_SPLUNK" stop-owned; then
            echo "# error: stop-owned failed; ownership retained for retry" >&2
            clean_rc=1
        fi
    fi
    if [[ "${SPLUNK_OWNERSHIP_FILE_CREATED:-0}" == 1 ]]; then
        if [[ ! -e "${SPLUNK_OWNERSHIP_FILE:-}" || ! -s "${SPLUNK_OWNERSHIP_FILE}" ]]; then
            rm -f "${SPLUNK_OWNERSHIP_FILE:-}"
        fi
    fi
    return "$clean_rc"
}

setup() {
    [[ -z "${SPLUNK_REAL_SKIP_REASON:-}" ]] || return 0
    check_conmon_binary
    check_runtime_binary
}

teardown() {
    [[ -z "${SPLUNK_REAL_SKIP_REASON:-}" ]] || return 0
    cleanup_test_env
}

@test "splunk real: conmon delivers stdout and stderr to Splunk Enterprise HEC" {
    if [[ -n "${SPLUNK_REAL_SKIP_REASON:-}" ]]; then
        skip "$SPLUNK_REAL_SKIP_REASON"
    fi

    local tag stdout_line stderr_line
    tag="splunk-bats-$$-$RANDOM"
    stdout_line="conmon-splunk-stdout-${tag}"
    stderr_line="conmon-splunk-stderr-${tag}"

    setup_container_env "echo ${stdout_line}; echo ${stderr_line} >&2"
    run_conmon_with_default_args \
        --log-driver splunk \
        --log-driver-opt "splunk-url=https://127.0.0.1:${SPLUNK_HEC_PORT}/services/collector/event" \
        --log-driver-opt "splunk-token=${SPLUNK_HEC_TOKEN}" \
        --log-driver-opt splunk-insecureskipverify=true \
        --log-driver-opt splunk-index=main \
        --log-tag "$tag"

    "$LOCAL_SPLUNK" wait-event "$stdout_line"
    "$LOCAL_SPLUNK" wait-event "$stderr_line"
}
