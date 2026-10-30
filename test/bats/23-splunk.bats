#!/usr/bin/env bats
#
# Splunk HEC log-driver coverage. A local Python server stands in for Splunk.

load test_helper

setup() {
    check_conmon_binary
    check_runtime_binary
    if ! command -v python3 >/dev/null 2>&1; then
        skip "python3 not available"
    fi
    HEC_PID=""
    HEC_PORT=""
    HEC_LOG=""
}

teardown() {
    if [[ -n "${HEC_PID:-}" ]]; then
        kill "$HEC_PID" 2>/dev/null || true
        wait "$HEC_PID" 2>/dev/null || true
    fi
    cleanup_test_env
}

start_hec() {
    local mode="$1"
    HEC_LOG="$TEST_TMPDIR/hec.log"
    : >"$HEC_LOG"
    HEC_PORT=$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1]); s.close()')
    python3 - "$HEC_PORT" "$HEC_LOG" "$mode" <<'PY' &
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

port = int(sys.argv[1])
log_path = sys.argv[2]
mode = sys.argv[3]

class Handler(BaseHTTPRequestHandler):
    def do_POST(self):
        length = int(self.headers.get("Content-Length", "0"))
        body = self.rfile.read(length)
        with open(log_path, "a", encoding="utf-8") as fh:
            auth = self.headers.get("Authorization", "")
            fh.write(f"Authorization: {auth}\n")
            fh.write(body.decode("utf-8", "replace") + "\n")
        if mode == "fail":
            payload = b'{"text":"Error","code":8}'
            status = 500
        else:
            payload = b'{"text":"Success","code":0}'
            status = 200
        self.send_response(status)
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def log_message(self, fmt, *args):
        return

ThreadingHTTPServer(("127.0.0.1", port), Handler).serve_forever()
PY
    HEC_PID=$!
    for _ in $(seq 1 50); do
        python3 -c 'import socket,sys; s=socket.create_connection(("127.0.0.1", int(sys.argv[1])), 0.2); s.close()' "$HEC_PORT" && return
        sleep 0.05
    done
    die "HEC test server did not accept connections on $HEC_PORT"
}

@test "splunk: missing token is rejected before the container starts" {
    setup_container_env "echo should-not-run"
    run_conmon_expecting_failure \
        --log-driver splunk \
        --log-driver-opt splunk-url=http://127.0.0.1:9/services/collector/event
    assert_output_contains "missing splunk-token"
}

@test "splunk: stdout and stderr are delivered to the HEC endpoint" {
    setup_container_env "echo splunk-out; echo splunk-err >&2"
    start_hec ok
    run_conmon_with_default_args \
        --log-driver splunk \
        --log-driver-opt "splunk-url=http://127.0.0.1:${HEC_PORT}/services/collector/event" \
        --log-driver-opt splunk-token=test-token \
        --log-driver-opt splunk-source=conmon-test \
        --log-driver-opt splunk-sourcetype=conmon:line \
        --log-driver-opt splunk-index=main \
        --log-tag splunk-tag

    local log
    log=$(cat "$HEC_LOG")
    [[ "$log" == *"Authorization: Splunk test-token"* ]]
    [[ "$log" == *'"line":"splunk-out"'* ]]
    [[ "$log" == *'"source":"stdout"'* ]]
    [[ "$log" == *'"line":"splunk-err"'* ]]
    [[ "$log" == *'"source":"stderr"'* ]]
    [[ "$log" == *'"tag":"splunk-tag"'* ]]
    [[ "$log" == *'"source":"conmon-test"'* ]]
    [[ "$log" == *'"sourcetype":"conmon:line"'* ]]
    [[ "$log" == *'"index":"main"'* ]]
}

@test "splunk: a down collector does not stop the container" {
    setup_container_env "echo still-ran"
    local port
    port=$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1]); s.close()')
    run_conmon_with_default_args \
        --log-driver splunk \
        --log-driver-opt "splunk-url=http://127.0.0.1:${port}/services/collector/event" \
        --log-driver-opt splunk-token=test-token
}

@test "splunk: HTTP 500 does not stop the container" {
    setup_container_env "echo still-ran"
    start_hec fail
    run_conmon_with_default_args \
        --log-driver splunk \
        --log-driver-opt "splunk-url=http://127.0.0.1:${HEC_PORT}/services/collector/event" \
        --log-driver-opt splunk-token=test-token
}
