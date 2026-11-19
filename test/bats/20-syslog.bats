#!/usr/bin/env bats
#
# conmon-v3 syslog log-driver coverage.
# Lives in the conmon-v3 tree because syslog is a v3-only container log plugin.

load test_helper

setup() {
    check_conmon_binary
    check_runtime_binary
    PRIVATE_RSYSLOG_PID=""
    PRIVATE_RSYSLOG_CONF=""
    STALL_PID=""
}

teardown() {
    unset CONMON_LOG_PATH CONMON_LOG_LEVEL
    if [[ -n "${STALL_PID:-}" ]]; then
        kill "$STALL_PID" 2>/dev/null || true
        wait "$STALL_PID" 2>/dev/null || true
    fi
    stop_private_rsyslogd
    cleanup_test_env
}

@test "syslog: --log-driver syslog is accepted" {
    setup_container_env "true"
    run_conmon_with_default_args --log-driver "syslog"
}

@test "syslog: rejects --log-label" {
    setup_container_env "true"
    run_conmon_expecting_failure \
        --log-driver "syslog" \
        --log-label "FOO=bar"

    assert_output_contains "syslog doesn't support --log-label"
}

@test "syslog: container stdout is delivered with --log-tag identity" {
    skip_if_no_syslog

    local tag="cslog_${RANDOM}_$$"
    # CTR_ID is assigned inside setup_container_env; build the marker afterwards.
    setup_container_env "echo syslog-hello"
    local marker="syslog-hello"

    run_conmon_with_default_args \
        --log-driver "syslog" \
        --log-tag "$tag"

    wait_for_syslog_message "$tag" "$marker"
}

@test "syslog: stderr uses error priority by default" {
    skip_if_no_syslog

    local tag="cslog_err_${RANDOM}_$$"
    local marker="syslog-stderr-msg"

    # Write only to stderr so the default priority is LOG_ERR (3).
    setup_container_env "echo ${marker} 1>&2"
    run_conmon_with_default_args \
        --log-driver "syslog" \
        --log-tag "$tag"

    wait_for_syslog_message "$tag" "$marker"

    # journald stores the syslog priority; require PRIORITY=3 for stderr.
    run journalctl --no-pager -t "$tag" --since "5 min ago" -o json
    assert_success
    assert_output_contains "\"PRIORITY\":\"3\""
    assert_output_contains "$marker"
}

@test "syslog: priority prefix in the message is honored" {
    skip_if_no_syslog

    local tag="cslog_pri_${RANDOM}_$$"
    local marker="syslog-prio-msg"

    setup_container_env "echo '<1>${marker}'"
    run_conmon_with_default_args \
        --log-driver "syslog" \
        --log-tag "$tag"

    wait_for_syslog_message "$tag" "$marker"

    run journalctl --no-pager -t "$tag" --since "5 min ago" -o json
    assert_success
    assert_output_contains "\"PRIORITY\":\"1\""
    assert_output_contains "$marker"
}

@test "syslog: non-blocking tcp delivery reaches private rsyslogd" {
    skip_if_no_rsyslogd

    local tag="rsys_nb_${RANDOM}_$$"
    setup_container_env "true"
    start_private_rsyslogd

    "$RUNTIME_BINARY" delete -f "$CTR_ID" >/dev/null 2>&1 || true
    CTR_ID=$(generate_ctr_id)
    generate_runtime_config "$BUNDLE_PATH" "$ROOTFS" "false" "echo rsyslog-nb-marker"
    run_conmon_with_default_args \
        --log-driver syslog \
        --log-tag "$tag" \
        --log-mode non-blocking \
        --log-max-buffer-size 1m \
        --log-driver-opt "syslog-address=tcp://127.0.0.1:${PRIVATE_RSYSLOG_TCP_PORT}" \
        --log-driver-opt syslog-facility=local0

    wait_for_log_line "$PRIVATE_RSYSLOG_TCP_LOG" "rsyslog-nb-marker" 15
}

# A peer that accepts and never reads. The kernel still copies sent bytes
# into the local TCP send buffer, up to tcp_wmem's maximum, so a small
# SO_RCVBUF does not make send() block. The container therefore writes more
# than that maximum plus the queue. Once the send buffer is full the worker
# blocks in its 1s write timeout, the 64KiB queue overflows, and later
# chunks are dropped. The queue stays small so shutdown is not a long series
# of timed-out sends.
#
# One syslog frame is at most the 8KiB line buffer, and a failed write is
# retried on a new connection, so a timeout is not required to finish the
# retry. This test does not look for "failed to send". It requires a
# dropped-message warning with a non-zero count, unread bytes sitting in the
# accepted socket, and conmon exiting anyway.
@test "syslog: non-blocking mode drops output instead of stalling the container" {
    if ! command -v python3 >/dev/null 2>&1; then
        skip "python3 not available"
    fi

    local wmem_max queue produce reps
    wmem_max=$(awk '{print $3}' /proc/sys/net/ipv4/tcp_wmem 2>/dev/null || true)
    if ! [[ "$wmem_max" =~ ^[0-9]+$ ]]; then
        wmem_max=4194304
    fi
    # A huge sysctl would make the container write an enormous payload.
    if ((wmem_max > 8388608)); then
        wmem_max=8388608
    fi
    queue=65536
    produce=$((wmem_max + queue + 262144))
    reps=$(((produce + 63) / 64))

    setup_container_env "i=0; while [ \$i -lt ${reps} ]; do printf 'xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx'; i=\$((i+1)); done; echo"
    local portfile="$TEST_TMPDIR/stall.port"
    local statusfile="$TEST_TMPDIR/stall.status"
    local conmon_log="$TEST_TMPDIR/conmon-stall.log"
    export CONMON_LOG_PATH="$conmon_log"
    export CONMON_LOG_LEVEL=debug

    python3 "$CONMON_V3_TEST_DIR/stall_tcp_peer.py" serve "$portfile" "$statusfile" &
    STALL_PID=$!
    local port=""
    for _ in $(seq 1 50); do
        if [[ -s "$portfile" ]]; then
            port=$(cat "$portfile")
            break
        fi
        sleep 0.05
    done
    [[ -n "$port" ]] || die "stalled syslog peer did not listen"
    start_conmon_with_default_args \
        --log-driver syslog \
        --log-mode non-blocking \
        --log-max-buffer-size "$queue" \
        --log-driver-opt "syslog-address=tcp://127.0.0.1:${port}"
    wait_for_runtime_status "$CTR_ID" stopped
    # One send can sit in SO_SNDTIMEO (1s) and be retried once. The wait is
    # only an upper bound; the assertions below are the pass condition.
    wait_for_conmon_exit "$CONMON_PID" 30

    [[ -s "$conmon_log" ]] || die "conmon log was not written: $conmon_log"
    local dropped_total=0 n
    while read -r n; do
        dropped_total=$((dropped_total + n))
    done < <(grep -oE 'dropped [0-9]+ message' "$conmon_log" | awk '{print $2}')
    if ((dropped_total <= 0)); then
        die "expected dropped messages, log was: $(cat "$conmon_log")"
    fi

    local unread=""
    for _ in $(seq 1 50); do
        if [[ -s "$statusfile" ]]; then
            unread=$(sed -n 's/^unread=//p' "$statusfile" | tail -1)
            if [[ "$unread" =~ ^[0-9]+$ ]] && ((unread > 0)); then
                break
            fi
        fi
        sleep 0.05
    done
    if ! [[ "$unread" =~ ^[0-9]+$ ]] || ((unread <= 0)); then
        die "stalled peer had no unread bytes (status: $(cat "$statusfile" 2>/dev/null || true))"
    fi
    grep -q '^reads=0$' "$statusfile" || die "stalled peer read application data"
}

@test "syslog: non-blocking mode does not wait on a closed port" {
    if ! command -v python3 >/dev/null 2>&1; then
        skip "python3 not available"
    fi
    setup_container_env "echo still-ran"
    local port
    port=$(python3 "$CONMON_V3_TEST_DIR/stall_tcp_peer.py" free-port)
    run_conmon_with_default_args \
        --log-driver syslog \
        --log-mode non-blocking \
        --log-max-buffer-size 4k \
        --log-driver-opt "syslog-address=tcp://127.0.0.1:${port}"
}

@test "syslog: rejects unknown syslog- driver opt" {
    setup_container_env "true"
    run_conmon_expecting_failure \
        --log-driver "syslog" \
        --log-driver-opt "syslog-bogus=x"

    assert_output_contains "unknown log opt"
}

# One private rsyslogd with four listeners covers every remote address
# scheme conmon accepts: tcp://, udp://, unix://, and unixgram://.
@test "syslog: private rsyslogd accepts tcp, udp, unix, and unixgram" {
    skip_if_no_rsyslogd

    local tag
    tag="rsys_${RANDOM}_$$"
    setup_container_env "true"
    start_private_rsyslogd

    deliver_to_private_rsyslogd \
        "rsyslog-tcp-marker" \
        "tcp://127.0.0.1:${PRIVATE_RSYSLOG_TCP_PORT}" \
        "${tag}-tcp" \
        "$PRIVATE_RSYSLOG_TCP_LOG"
    deliver_to_private_rsyslogd \
        "rsyslog-udp-marker" \
        "udp://127.0.0.1:${PRIVATE_RSYSLOG_UDP_PORT}" \
        "${tag}-udp" \
        "$PRIVATE_RSYSLOG_UDP_LOG"
    deliver_to_private_rsyslogd \
        "rsyslog-unix-marker" \
        "unix://${PRIVATE_RSYSLOG_UNIX_SOCK}" \
        "${tag}-unix" \
        "$PRIVATE_RSYSLOG_UNIX_LOG"
    deliver_to_private_rsyslogd \
        "rsyslog-unixgram-marker" \
        "unixgram://${PRIVATE_RSYSLOG_UNIXGRAM_SOCK}" \
        "${tag}-unixgram" \
        "$PRIVATE_RSYSLOG_UNIXGRAM_LOG"
}
