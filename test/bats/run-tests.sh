#!/bin/bash
#
# Test runner for conmon-v3 BATS tests (v3-specific features such as syslog).

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

CONMON_BINARY="${CONMON_BINARY:-$PROJECT_ROOT/target/debug/conmon}"
if [[ ! -x "$CONMON_BINARY" && -x "$PROJECT_ROOT/target/release/conmon" ]]; then
    CONMON_BINARY="$PROJECT_ROOT/target/release/conmon"
fi

if [[ -z "${RUNTIME_BINARY:-}" ]]; then
    if [[ -x /usr/bin/crun ]]; then
        RUNTIME_BINARY=/usr/bin/crun
    else
        RUNTIME_BINARY=/usr/bin/runc
    fi
fi

BATS_OPTIONS="${BATS_OPTIONS:-}"
CONMON_TEST_STRICT="${CONMON_TEST_STRICT:-}"
CONMON_V2_TEST_DIR="${CONMON_V2_TEST_DIR:-$PROJECT_ROOT/conmon-v2/test}"

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

usage() {
    cat <<EOF
Usage: $0 [OPTIONS] [TEST_FILES...]

Run conmon-v3 BATS tests (syslog and other v3-only coverage).

OPTIONS:
    -h, --help              Show this help message
    -c, --conmon BINARY     Path to conmon binary (default: $CONMON_BINARY)
    -r, --runtime BINARY    Path to runtime binary (default: $RUNTIME_BINARY)
    -v, --verbose           Verbose output
    -t, --tap               Output in TAP format
    -j, --jobs N            Run tests in parallel with N jobs
    -s, --strict            Treat skipped tests as failures (for CI)
    --filter PATTERN        Run only tests matching PATTERN

ENVIRONMENT VARIABLES:
    CONMON_BINARY          Path to conmon binary
    RUNTIME_BINARY         Path to runtime binary
    CONMON_V2_TEST_DIR     Path to conmon-v2/test helpers (default: ./conmon-v2/test)
    CONMON_TEST_STRICT     Same as --strict, if set to a non-empty value
    UBI10_MICRO_IMAGE      Image to take the test rootfs from
    BATS_OPTIONS           Additional options to pass to bats
EOF
}

log_info() { echo -e "${GREEN}[INFO]${NC} $*"; }
log_warn() { echo -e "${YELLOW}[WARN]${NC} $*"; }
log_error() { echo -e "${RED}[ERROR]${NC} $*" >&2; }

check_dependencies() {
    local missing_deps=()

    if ! command -v bats >/dev/null 2>&1; then
        missing_deps+=("bats")
    fi

    if [[ ! -x "$CONMON_BINARY" ]]; then
        missing_deps+=("conmon binary at $CONMON_BINARY (run 'make' first)")
    fi

    if [[ ! -x "$RUNTIME_BINARY" ]]; then
        missing_deps+=("runtime binary at $RUNTIME_BINARY")
    fi

    if [[ ! -f "$CONMON_V2_TEST_DIR/test_helper.bash" ]]; then
        missing_deps+=("conmon-v2 test helpers at $CONMON_V2_TEST_DIR (run 'make conmon-v2')")
    fi

    if [[ ${#missing_deps[@]} -gt 0 ]]; then
        log_error "Missing dependencies:"
        printf '  - %s\n' "${missing_deps[@]}"
        return 1
    fi
}

show_environment() {
    log_info "Running conmon-v3 bats with:"
    log_info "  kernel:  $(uname -srm)"
    if [[ -r /etc/os-release ]]; then
        # shellcheck source=/dev/null
        log_info "  distro:  $(. /etc/os-release && echo "$PRETTY_NAME")"
    fi
    log_info "  conmon:  $CONMON_BINARY: $("$CONMON_BINARY" --version 2>&1 | tr '\n' ' ' | sed 's/  *$//')"
    log_info "  runtime: $RUNTIME_BINARY: $("$RUNTIME_BINARY" --version 2>&1 | head -1)"
    log_info "  bats:    $(command -v bats): $(bats --version 2>&1)"
    log_info "  helpers: $CONMON_V2_TEST_DIR"
}

main() {
    local verbose=false
    local tap=false
    local jobs=""
    local filter=""
    local test_files=()

    while [[ $# -gt 0 ]]; do
        case $1 in
        -h | --help)
            usage
            exit 0
            ;;
        -c | --conmon)
            CONMON_BINARY="$2"
            shift 2
            ;;
        -r | --runtime)
            RUNTIME_BINARY="$2"
            shift 2
            ;;
        -v | --verbose)
            verbose=true
            shift
            ;;
        -t | --tap)
            tap=true
            shift
            ;;
        -j | --jobs)
            jobs="$2"
            shift 2
            ;;
        --filter)
            filter="$2"
            shift 2
            ;;
        -s | --strict)
            CONMON_TEST_STRICT=1
            shift
            ;;
        *.bats)
            test_files+=("$1")
            shift
            ;;
        *)
            log_error "Unknown option: $1"
            usage
            exit 1
            ;;
        esac
    done

    local bats_args=()
    [[ "$verbose" == true ]] && bats_args+=("--verbose-run")
    [[ "$tap" == true ]] && bats_args+=("--tap")
    [[ -n "$jobs" ]] && bats_args+=("--jobs" "$jobs")
    [[ -n "$filter" ]] && bats_args+=("--filter" "$filter")

    if [[ -n "$BATS_OPTIONS" ]]; then
        read -ra additional_opts <<<"$BATS_OPTIONS"
        bats_args+=("${additional_opts[@]}")
    fi

    log_info "Checking dependencies..."
    if ! check_dependencies; then
        exit 1
    fi

    if [[ ${#test_files[@]} -eq 0 ]]; then
        mapfile -t test_files < <(find "$SCRIPT_DIR" -maxdepth 1 -name "*.bats" | sort)
    else
        local resolved_files=()
        for file in "${test_files[@]}"; do
            if [[ "$file" =~ ^/ ]]; then
                resolved_files+=("$file")
            else
                resolved_files+=("$SCRIPT_DIR/$(basename "$file")")
            fi
        done
        test_files=("${resolved_files[@]}")
    fi

    for file in "${test_files[@]}"; do
        if [[ ! -f "$file" ]]; then
            log_error "Test file not found: $file"
            exit 1
        fi
    done

    export CONMON_BINARY RUNTIME_BINARY CONMON_TEST_STRICT CONMON_V2_TEST_DIR

    show_environment
    log_info "  test files: ${test_files[*]}"

    log_info "Starting test execution..."
    if bats "${bats_args[@]}" "${test_files[@]}"; then
        log_info "All tests passed!"
        exit 0
    else
        log_error "Some tests failed!"
        exit 1
    fi
}

if [[ "${BASH_SOURCE[0]}" == "${0}" ]]; then
    main "$@"
fi
