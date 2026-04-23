#!/usr/bin/env bash
# Build and test nexus inside the nexus-v2-dev devcontainer.
#
# Default target is aarch64-unknown-linux-gnu (cross-compiled via the Poky SDK).
# Pass --amd64 to build for x86_64 (native container architecture) instead.
#
# Usage:
#   scripts/build-and-test.sh                            # aarch64 debug build + tests
#   scripts/build-and-test.sh --release                  # aarch64 release build + tests
#   scripts/build-and-test.sh --amd64                    # x86_64 debug build + tests
#   scripts/build-and-test.sh --amd64 --release          # x86_64 release build + tests
#   scripts/build-and-test.sh --test-with-coverage       # debug build + llvm-cov summary
#   scripts/build-and-test.sh --test-coverage            # debug build + full coverage report
#   scripts/build-and-test.sh --rebuild-devcontainer     # rebuild Docker image only

set -euo pipefail

SCRIPT="$(basename "$0")"
IMAGE="nexus-v2-dev"
DOCKERFILE=".devcontainer/Dockerfile"

usage() {
    cat <<EOF
Usage: $SCRIPT [--amd64] [OPTION]

Build and test nexus inside the nexus-v2-dev devcontainer.

Default target: aarch64-unknown-linux-gnu (cross-compiled via the Yocto Poky SDK).
Use --amd64 to target x86_64 (native container architecture).

Target flags:
  (none)                    Cross-compile for aarch64-unknown-linux-gnu (default)
  --amd64                   Build for x86_64-unknown-linux-gnu (native container arch)

Options:
  (none)                    Debug build followed by the full test suite
  --release                 Release build followed by the full test suite
  --test-with-coverage      Debug build followed by llvm-cov summary (native arch)
  --test-coverage           Debug build followed by full per-file coverage report (native arch)
  --rebuild-devcontainer    Rebuild the nexus-v2-dev Docker image only
  -h, --help                Show this help message

Examples:
  $SCRIPT
  $SCRIPT --release
  $SCRIPT --amd64
  $SCRIPT --amd64 --release
  $SCRIPT --test-with-coverage
  $SCRIPT --test-coverage
  $SCRIPT --rebuild-devcontainer
EOF
}

run() {
    echo "+ $*"
    "$@"
}

# Repeat CHAR N times; avoids tr's multi-byte limitation with Unicode chars.
repeat_char() {
    local char="$1" n="$2" s=""
    for (( i=0; i<n; i++ )); do s+="$char"; done
    printf '%s' "$s"
}

# Right-justify a percentage string in WIDTH columns, coloured by value.
#   ≥ 90% → green   70–89% → yellow   < 70% → red   "-" → no colour
fmt_pct() {
    local val="$1" width="$2"
    local pad=$(( width - ${#val} ))
    local prefix="" suffix=""
    if [[ -t 1 && "$val" != "-" ]]; then
        local num="${val%\%}"
        if awk -v n="$num" 'BEGIN { exit !(n+0 >= 90) }'; then
            prefix='\033[32m'; suffix='\033[0m'
        elif awk -v n="$num" 'BEGIN { exit !(n+0 >= 70) }'; then
            prefix='\033[33m'; suffix='\033[0m'
        else
            prefix='\033[31m'; suffix='\033[0m'
        fi
    fi
    printf "%${pad}s${prefix}%s${suffix}" "" "$val"
}

# Parse cargo test output from TMPFILE and print a test suite summary table.
print_test_summary() {
    local tmpfile="$1"
    local suite=""
    local -a suites=() col_passed=() col_failed=() col_time=()
    local total_passed=0 total_failed=0 total_time_ms=0

    while IFS= read -r line; do
        # "     Running unittests src/lib.rs (...)" or "Running tests/foo.rs (...)"
        if [[ "$line" =~ [[:space:]]*Running[[:space:]]+(unittests[[:space:]]+)?([^[:space:]]+\.rs) ]]; then
            suite="${BASH_REMATCH[2]}"
        # "   Doc-tests nexusd"
        elif [[ "$line" =~ [[:space:]]*Doc-tests[[:space:]]+(.+)$ ]]; then
            suite="doc-tests (${BASH_REMATCH[1]})"
        # "test result: ok. 88 passed; 0 failed; ... finished in 0.01s"
        elif [[ "$line" =~ ^test\ result:.*[[:space:]]([0-9]+)\ passed\;\ ([0-9]+)\ failed.*finished\ in\ ([0-9.]+s) ]]; then
            local p="${BASH_REMATCH[1]}" f="${BASH_REMATCH[2]}" t="${BASH_REMATCH[3]}"
            [[ "$p" -eq 0 && "$f" -eq 0 ]] && continue
            suites+=("$suite")
            col_passed+=("$p")
            col_failed+=("$f")
            col_time+=("$t")
            total_passed=$(( total_passed + p ))
            total_failed=$(( total_failed + f ))
            local ms
            ms=$(awk -v t="${t%s}" 'BEGIN { printf "%d", t * 1000 }')
            total_time_ms=$(( total_time_ms + ms ))
        fi
    done < "$tmpfile"

    [[ ${#suites[@]} -eq 0 ]] && return

    local suite_w=5
    for s in "${suites[@]}" "Total"; do
        (( ${#s} > suite_w )) && suite_w=${#s} || true
    done
    local pass_w=6 fail_w=6 time_w=8

    local hr_suite hr_pass hr_fail hr_time
    hr_suite=$(repeat_char '─' "$suite_w")
    hr_pass=$(repeat_char  '─' "$pass_w")
    hr_fail=$(repeat_char  '─' "$fail_w")
    hr_time=$(repeat_char  '─' "$time_w")

    local green="" red="" reset=""
    if [[ -t 1 ]]; then
        green="\033[32m"; red="\033[31m"; reset="\033[0m"
    fi

    echo ""
    echo "Test Summary"
    printf '┌─%s─┬─%s─┬─%s─┬─%s─┐\n' "$hr_suite" "$hr_pass" "$hr_fail" "$hr_time"
    printf '│ %-*s │ %*s │ %*s │ %*s │\n' \
        "$suite_w" "Suite" "$pass_w" "Passed" "$fail_w" "Failed" "$time_w" "Time"
    printf '├─%s─┼─%s─┼─%s─┼─%s─┤\n' "$hr_suite" "$hr_pass" "$hr_fail" "$hr_time"

    for i in "${!suites[@]}"; do
        local row_color="$green"
        [[ "${col_failed[$i]}" -gt 0 ]] && row_color="$red"
        printf "│ %-*s │ ${row_color}%*s${reset} │ ${row_color}%*s${reset} │ %*s │\n" \
            "$suite_w" "${suites[$i]}" \
            "$pass_w"  "${col_passed[$i]}" \
            "$fail_w"  "${col_failed[$i]}" \
            "$time_w"  "${col_time[$i]}"
    done

    printf '├─%s─┼─%s─┼─%s─┼─%s─┤\n' "$hr_suite" "$hr_pass" "$hr_fail" "$hr_time"

    local total_color="$green"
    [[ "$total_failed" -gt 0 ]] && total_color="$red"
    local total_time
    total_time=$(awk -v ms="$total_time_ms" 'BEGIN { printf "%.2fs", ms / 1000 }')
    printf "│ %-*s │ ${total_color}%*s${reset} │ ${total_color}%*s${reset} │ %*s │\n" \
        "$suite_w" "Total" \
        "$pass_w"  "$total_passed" \
        "$fail_w"  "$total_failed" \
        "$time_w"  "$total_time"

    printf '└─%s─┴─%s─┴─%s─┴─%s─┘\n' "$hr_suite" "$hr_pass" "$hr_fail" "$hr_time"
}

# Parse cargo llvm-cov output from TMPFILE and print a per-file coverage table.
# Columns: File, Lines %, Funcs %
print_coverage_summary() {
    local tmpfile="$1"
    local -a files=() col_lines=() col_funcs=()
    local total_lines="" total_funcs=""

    while IFS= read -r line; do
        [[ "$line" =~ ^[[:space:]]*-+[[:space:]]*$ || -z "${line// }" ]] && continue
        # Coverage data rows contain a percentage field; skip header and other lines.
        [[ "$line" =~ [0-9]+\.[0-9]+% ]] || continue

        local -a fields
        # Strip leading whitespace before splitting.
        read -ra fields <<< "${line#"${line%%[![:space:]]*}"}"
        local fname="${fields[0]}"
        # llvm-cov columns: $1=file $2=regions $3=missed $4=regions%
        #                   $5=funcs $6=missed $7=funcs%
        #                   $8=lines $9=missed $10=lines%
        if [[ "$fname" == "TOTAL" ]]; then
            total_funcs="${fields[6]}"
            total_lines="${fields[9]}"
        elif [[ "$fname" == *".rs" ]]; then
            files+=("$fname")
            col_funcs+=("${fields[6]}")
            col_lines+=("${fields[9]}")
        fi
    done < "$tmpfile"

    [[ ${#files[@]} -eq 0 && -z "$total_lines" ]] && return

    local file_w=4
    for f in "${files[@]}" "TOTAL"; do
        (( ${#f} > file_w )) && file_w=${#f} || true
    done
    local pct_w=9  # wide enough for "100.00%"

    local hr_file hr_pct
    hr_file=$(repeat_char '─' "$file_w")
    hr_pct=$(repeat_char  '─' "$pct_w")

    echo ""
    echo "Coverage Summary"
    printf '┌─%s─┬─%s─┬─%s─┐\n' "$hr_file" "$hr_pct" "$hr_pct"
    printf '│ %-*s │ %*s │ %*s │\n' "$file_w" "File" "$pct_w" "Lines %" "$pct_w" "Funcs %"
    printf '├─%s─┼─%s─┼─%s─┤\n' "$hr_file" "$hr_pct" "$hr_pct"

    for i in "${!files[@]}"; do
        local lc fc
        lc=$(fmt_pct "${col_lines[$i]}" "$pct_w")
        fc=$(fmt_pct "${col_funcs[$i]}" "$pct_w")
        printf "│ %-*s │ %b │ %b │\n" "$file_w" "${files[$i]}" "$lc" "$fc"
    done

    if [[ -n "$total_lines" ]]; then
        printf '├─%s─┼─%s─┼─%s─┤\n' "$hr_file" "$hr_pct" "$hr_pct"
        local tlc tfc
        tlc=$(fmt_pct "$total_lines" "$pct_w")
        tfc=$(fmt_pct "$total_funcs" "$pct_w")
        printf "│ %-*s │ %b │ %b │\n" "$file_w" "TOTAL" "$tlc" "$tfc"
    fi

    printf '└─%s─┴─%s─┴─%s─┘\n' "$hr_file" "$hr_pct" "$hr_pct"
}

# Run cargo test and print the test summary table.
run_tests() {
    local tmpfile exit_code=0
    tmpfile=$(mktemp)
    echo "+ ${DOCKER_BASE[*]} cargo test $*"
    "${DOCKER_BASE[@]}" cargo test "$@" 2>&1 | tee "$tmpfile" || exit_code=$?
    print_test_summary "$tmpfile"
    rm -f "$tmpfile"
    return "$exit_code"
}

# Run cargo llvm-cov --summary-only and print the test summary table.
run_coverage_summary() {
    local tmpfile exit_code=0
    tmpfile=$(mktemp)
    echo "+ ${DOCKER_BASE[*]} cargo llvm-cov --summary-only"
    "${DOCKER_BASE[@]}" cargo llvm-cov --summary-only 2>&1 | tee "$tmpfile" || exit_code=$?
    print_test_summary "$tmpfile"
    rm -f "$tmpfile"
    return "$exit_code"
}

# Run cargo llvm-cov (full report) and print both the test and coverage tables.
run_coverage() {
    local tmpfile exit_code=0
    tmpfile=$(mktemp)
    echo "+ ${DOCKER_BASE[*]} cargo llvm-cov"
    "${DOCKER_BASE[@]}" cargo llvm-cov 2>&1 | tee "$tmpfile" || exit_code=$?
    print_test_summary "$tmpfile"
    print_coverage_summary "$tmpfile"
    rm -f "$tmpfile"
    return "$exit_code"
}

build_image() {
    local no_cache="${1:-}"
    echo "==> Building devcontainer image '${IMAGE}'"
    if [[ -n "${no_cache}" ]]; then
        run docker build --no-cache -t "${IMAGE}" -f "${DOCKERFILE}" .
    else
        run docker build -t "${IMAGE}" -f "${DOCKERFILE}" .
    fi
    echo ""
}

image_present() {
    docker image inspect "${IMAGE}" >/dev/null 2>&1
}

ensure_image() {
    if ! image_present; then
        echo "info: Docker image '${IMAGE}' not found — building it now."
        echo ""
        build_image
    fi
}

WORKSPACE="$(cd "$(dirname "$0")/.." && pwd)"
cd "${WORKSPACE}"

DOCKER_BASE=(docker run --rm -v "${WORKSPACE}:/workspace" -w /workspace "${IMAGE}")

# Poky SDK environment script used for aarch64 cross-compilation.
POKY_ENV="/opt/astrax/environment-setup-armv8a-poky-linux"
AARCH64_TARGET="aarch64-unknown-linux-gnu"

# Default build target is aarch64; --amd64 switches to the native container arch.
BUILD_TARGET="${AARCH64_TARGET}"
MODE=""

while [[ $# -gt 0 ]]; do
    case "$1" in
        --amd64)
            BUILD_TARGET="x86_64-unknown-linux-gnu"
            shift
            ;;
        --release|--test-with-coverage|--test-coverage|--rebuild-devcontainer|-h|--help)
            MODE="$1"
            shift
            ;;
        "")
            shift
            ;;
        *)
            echo "error: unknown option '$1'" >&2
            echo "" >&2
            usage >&2
            exit 1
            ;;
    esac
done

# Run `cargo build` with the appropriate target.
#   $@ — extra cargo build flags (e.g. --release)
cargo_build() {
    if [[ "$BUILD_TARGET" == "$AARCH64_TARGET" ]]; then
        local cargo_cmd="cargo build --target ${AARCH64_TARGET}"
        for arg in "$@"; do
            cargo_cmd+=" $arg"
        done
        echo "+ ${DOCKER_BASE[*]} bash -c '. ${POKY_ENV} && ${cargo_cmd}'"
        "${DOCKER_BASE[@]}" bash -c ". ${POKY_ENV} && ${cargo_cmd}"
    else
        run "${DOCKER_BASE[@]}" cargo build "$@"
    fi
}

# Human-readable label for the current target.
target_label() {
    if [[ "$BUILD_TARGET" == "$AARCH64_TARGET" ]]; then
        echo "aarch64"
    else
        echo "amd64"
    fi
}

case "${MODE}" in
    --rebuild-devcontainer)
        build_image --no-cache
        ;;
    "")
        ensure_image
        echo "==> Debug build ($(target_label))"
        cargo_build
        echo ""
        echo "==> Running tests"
        run_tests
        ;;
    --release)
        ensure_image
        echo "==> Release build ($(target_label))"
        cargo_build --release
        echo ""
        echo "==> Running tests (release)"
        run_tests --release
        ;;
    --test-with-coverage)
        ensure_image
        echo "==> Debug build ($(target_label))"
        cargo_build
        echo ""
        echo "==> Running tests with coverage"
        run_coverage_summary
        ;;
    --test-coverage)
        ensure_image
        echo "==> Debug build ($(target_label))"
        cargo_build
        echo ""
        echo "==> Running tests with coverage"
        run_coverage
        ;;
    -h|--help)
        usage
        ;;
    *)
        echo "error: unknown option '${MODE}'" >&2
        echo "" >&2
        usage >&2
        exit 1
        ;;
esac
