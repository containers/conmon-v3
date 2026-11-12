#!/usr/bin/env bats
#
# Generic non-blocking log delivery. Driver-specific protocol coverage lives
# with the syslog and Splunk suites.

load test_helper

setup() {
    check_conmon_binary
    check_runtime_binary
}

teardown() {
    cleanup_test_env
}

@test "non-blocking: file driver delivers container output" {
    setup_container_env "echo nb-file-line"
    run_conmon_with_default_args \
        --log-driver file \
        --log-driver-opt "path=${LOG_PATH}" \
        --log-mode non-blocking \
        --log-max-buffer-size 1m

    wait_for_log_line "$LOG_PATH" "nb-file-line" 15
}

@test "non-blocking: invalid mode is rejected" {
    setup_container_env "true"
    run_conmon_expecting_failure \
        --log-driver none \
        --log-mode sideways
    assert_output_contains "invalid --log-mode"
}

@test "non-blocking: max-buffer-size without non-blocking mode is rejected" {
    setup_container_env "true"
    run_conmon_expecting_failure \
        --log-driver none \
        --log-max-buffer-size 1m
    assert_output_contains "--log-max-buffer-size requires --log-mode non-blocking"
}

@test "non-blocking: mode is not a log-driver-opt" {
    setup_container_env "true"
    run_conmon_expecting_failure \
        --log-driver none \
        --log-driver-opt mode=non-blocking
    assert_output_contains "not a driver option"
}

@test "non-blocking: legacy log-path stays blocking and writes the file" {
    setup_container_env "echo legacy-line"
    run_conmon_with_default_args --log-path "$LOG_PATH"
    wait_for_log_line "$LOG_PATH" "legacy-line" 15
}
