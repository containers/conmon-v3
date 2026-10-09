#!/usr/bin/env bats
#
# conmon-v3 syslog log-driver coverage.
# Lives in the conmon-v3 tree because syslog is a v3-only container log plugin.

load test_helper

setup() {
    check_conmon_binary
    check_runtime_binary
}

teardown() {
    cleanup_test_env
}

@test "syslog: --log-path syslog is accepted" {
    setup_container_env "true"
    run_conmon_with_default_args --log-path "syslog"
}

@test "syslog: --log-path syslog: is accepted" {
    setup_container_env "true"
    run_conmon_with_default_args --log-path "syslog:"
}

@test "syslog: rejects --log-label" {
    setup_container_env "true"
    run_conmon_expecting_failure \
        --log-path "syslog" \
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
        --log-path "syslog" \
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
        --log-path "syslog" \
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
        --log-path "syslog" \
        --log-tag "$tag"

    wait_for_syslog_message "$tag" "$marker"

    run journalctl --no-pager -t "$tag" --since "5 min ago" -o json
    assert_success
    assert_output_contains "\"PRIORITY\":\"1\""
    assert_output_contains "$marker"
}
