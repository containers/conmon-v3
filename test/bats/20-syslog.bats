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
}

teardown() {
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
