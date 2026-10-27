#!/usr/bin/env bash
# Splunk Enterprise helper for conmon HEC tests.
# Commands: start | stop | stop-owned | wait-event
# With SPLUNK_OWNERSHIP_FILE, start claims the create ID immediately and
# refuses a pre-existing name; stop-owned removes only that ID.
set -euo pipefail

CONTAINER_NAME="${SPLUNK_CONTAINER:-conmon-splunk}"
IMAGE="${SPLUNK_IMAGE:-docker.io/splunk/splunk:latest}"
PASSWORD="${SPLUNK_PASSWORD:-Changeme1!}"
HEC_TOKEN="${SPLUNK_HEC_TOKEN:-abcd1234}"
WEB_PORT="${SPLUNK_WEB_PORT:-18000}"
HEC_PORT="${SPLUNK_HEC_PORT:-18088}"
MGMT_PORT="${SPLUNK_MGMT_PORT:-18089}"
HEC_URL="https://127.0.0.1:${HEC_PORT}/services/collector/event"
MGMT_URL="https://127.0.0.1:${MGMT_PORT}"

die() {
    echo "error: $*" >&2
    exit 1
}

command -v podman >/dev/null 2>&1 || die "podman is required"
command -v curl >/dev/null 2>&1 || die "curl is required"

hec_post() {
    curl -fsS -k \
        -H "Authorization: Splunk ${HEC_TOKEN}" \
        -H "Content-Type: application/json" \
        -d "$1" \
        "$HEC_URL"
}

wait_for_hec() {
    echo "Waiting for HEC at ${HEC_URL}..."
    local i
    for ((i = 0; i < 180; i++)); do
        if hec_post '{"event":"conmon-splunk-probe"}' >/dev/null 2>&1; then
            echo "HEC is ready."
            return 0
        fi
        sleep 2
    done
    die "HEC did not become ready; check: podman logs $CONTAINER_NAME"
}

claim_created_id() {
    local id="$1" file="${SPLUNK_OWNERSHIP_FILE:-}"
    [[ -n "$file" ]] || return 0
    [[ -n "$id" ]] || die "refusing to claim an empty container id"
    printf '%s\n' "$id" >"$file"
}

create_container() {
    echo "Starting $IMAGE as $CONTAINER_NAME..."
    local id
    id="$(
        podman run -d --name "$CONTAINER_NAME" \
            -p "${WEB_PORT}:8000" \
            -p "${HEC_PORT}:8088" \
            -p "${MGMT_PORT}:8089" \
            -e SPLUNK_START_ARGS=--accept-license \
            -e SPLUNK_GENERAL_TERMS=--accept-sgt-current-at-splunk-com \
            -e "SPLUNK_PASSWORD=${PASSWORD}" \
            -e "SPLUNK_HEC_TOKEN=${HEC_TOKEN}" \
            "$IMAGE"
    )"
    claim_created_id "$id"
}

cmd_start() {
    if podman inspect "$CONTAINER_NAME" >/dev/null 2>&1; then
        if [[ -n "${SPLUNK_OWNERSHIP_FILE:-}" ]]; then
            die "container $CONTAINER_NAME already exists; refusing to replace it"
        fi
        # Manual reuse when ports match.
        local state
        state="$(podman inspect -f '{{.State.Status}}' "$CONTAINER_NAME")"
        if [[ "$state" != "running" ]]; then
            podman start "$CONTAINER_NAME" >/dev/null
        fi
    else
        create_container
    fi
    wait_for_hec
    echo "HEC URL: $HEC_URL"
}

cmd_stop() {
    if ! podman inspect "$CONTAINER_NAME" >/dev/null 2>&1; then
        echo "No container named $CONTAINER_NAME."
        return 0
    fi
    podman stop -t 20 "$CONTAINER_NAME" >/dev/null 2>&1 || true
    podman rm -f "$CONTAINER_NAME" >/dev/null 2>&1 || true
    echo "Removed $CONTAINER_NAME."
}

# present=0 / absent=1 / indeterminate=2 via `podman container exists`.
owned_container_state() {
    local id="$1" rc=0
    podman container exists "$id" >/dev/null 2>&1 || rc=$?
    case "$rc" in
    1) return 0 ;;
    0) return 1 ;;
    *)
        echo "error: cannot determine state of owned Splunk container ${id:0:12} (exists exit ${rc})" >&2
        return 2
        ;;
    esac
}

cmd_stop_owned() {
    local file="${SPLUNK_OWNERSHIP_FILE:-}"
    if [[ -z "$file" ]]; then
        return 0
    fi
    if [[ ! -s "$file" ]]; then
        rm -f "$file" 2>/dev/null || true
        return 0
    fi

    local expected
    expected="$(tr -d '[:space:]' <"$file")"
    if [[ -z "$expected" ]]; then
        rm -f "$file"
        return 0
    fi

    local state_rc=0
    owned_container_state "$expected" || state_rc=$?
    case "$state_rc" in
    0)
        rm -f "$file"
        return 0
        ;;
    2) return 1 ;;
    esac

    podman stop -t 20 "$expected" >/dev/null 2>&1 || true
    podman rm -f "$expected" >/dev/null 2>&1 || true

    state_rc=0
    owned_container_state "$expected" || state_rc=$?
    case "$state_rc" in
    0)
        rm -f "$file"
        echo "Removed owned container ${expected:0:12}."
        return 0
        ;;
    1)
        echo "error: failed to remove owned Splunk container ${expected:0:12}" >&2
        return 1
        ;;
    *) return 1 ;;
    esac
}

cmd_wait_event() {
    local needle="${1:-}"
    [[ -n "$needle" ]] || die "wait-event requires a search string"
    local i out
    echo "Waiting for Splunk to index '${needle}'..."
    for ((i = 0; i < 60; i++)); do
        out="$(curl -fsS -k -u "admin:${PASSWORD}" \
            "${MGMT_URL}/services/search/jobs/export" \
            --data-urlencode "search=search index=main \"${needle}\"" \
            -d output_mode=csv \
            -d earliest_time=-15m 2>/dev/null || true)"
        if [[ "$out" == *"$needle"* ]]; then
            echo "Found in Splunk: $needle"
            return 0
        fi
        sleep 2
    done
    die "event '${needle}' not found in Splunk"
}

usage() {
    sed -n '2,5p' "$0" | sed 's/^# \?//'
}

main() {
    local cmd="${1:-start}"
    case "$cmd" in
    start) cmd_start ;;
    stop) cmd_stop ;;
    stop-owned) cmd_stop_owned ;;
    wait-event)
        shift
        cmd_wait_event "${1:-}"
        ;;
    -h | --help | help) usage ;;
    *)
        usage >&2
        die "unknown command: $cmd"
        ;;
    esac
}

main "$@"
