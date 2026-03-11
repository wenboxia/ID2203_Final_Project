#!/usr/bin/env bash
# Run Maelstrom lin-kv workload tests against the OmniPaxos node.
#
# Usage:
#   ./scripts/run_tests.sh [test_name]
#
# Available tests:
#   basic          - Basic linearizability test (no faults)
#   partition      - Network partition test (isolate leader)
#   partition-half - Split-brain partition test
#   kill           - Node crash and restart test
#   all-faults     - Combined partition + kill test
#   all            - Run all tests sequentially
#
# Environment variables:
#   MAELSTROM_DIR  - Path to Maelstrom installation (default: ./maelstrom)
#   NODE_COUNT     - Number of nodes (default: 3)
#   TIME_LIMIT     - Test duration in seconds (default: 60)
#   RATE           - Operations per second (default: 10)
#   CONCURRENCY    - Number of concurrent clients (default: 5)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"

MAELSTROM_DIR="${MAELSTROM_DIR:-${PROJECT_DIR}/maelstrom}"
MAELSTROM="${MAELSTROM_DIR}/maelstrom"
BINARY="${PROJECT_DIR}/target/release/maelstrom-node"
RESULTS_DIR="${PROJECT_DIR}/test-results"

NODE_COUNT="${NODE_COUNT:-3}"
TIME_LIMIT="${TIME_LIMIT:-60}"
RATE="${RATE:-10}"
CONCURRENCY="${CONCURRENCY:-5}"

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

# ---- Preflight checks ----

if [ ! -f "${MAELSTROM}" ]; then
    echo -e "${RED}ERROR: Maelstrom not found at ${MAELSTROM}${NC}"
    echo "Run: ./scripts/install_maelstrom.sh"
    exit 1
fi

# Build the binary
echo -e "${YELLOW}Building maelstrom-node (release)...${NC}"
cd "${PROJECT_DIR}"
cargo build --release --bin maelstrom-node 2>&1
if [ ! -f "${BINARY}" ]; then
    echo -e "${RED}ERROR: Build failed. Binary not found at ${BINARY}${NC}"
    exit 1
fi
echo -e "${GREEN}Build successful.${NC}"

mkdir -p "${RESULTS_DIR}"

# ---- Test functions ----

run_test() {
    local test_name="$1"
    shift
    local extra_args=("$@")

    echo ""
    echo "============================================================"
    echo -e "${YELLOW}Running test: ${test_name}${NC}"
    echo "  Nodes: ${NODE_COUNT}, Time: ${TIME_LIMIT}s, Rate: ${RATE}/s, Concurrency: ${CONCURRENCY}"
    echo "  Extra args: ${extra_args[*]:-none}"
    echo "============================================================"

    local store_dir="${RESULTS_DIR}/${test_name}"
    mkdir -p "${store_dir}"

    "${MAELSTROM}" test \
        -w lin-kv \
        --bin "${BINARY}" \
        --node-count "${NODE_COUNT}" \
        --time-limit "${TIME_LIMIT}" \
        --rate "${RATE}" \
        --concurrency "${CONCURRENCY}" \
        "${extra_args[@]}" \
        2>&1 | tee "${store_dir}/output.log"

    local exit_code=${PIPESTATUS[0]}

    if [ ${exit_code} -eq 0 ]; then
        echo -e "${GREEN}TEST PASSED: ${test_name}${NC}"
    else
        echo -e "${RED}TEST FAILED: ${test_name} (exit code: ${exit_code})${NC}"
    fi

    # Copy Maelstrom results
    if [ -d "${MAELSTROM_DIR}/store/latest" ]; then
        cp -r "${MAELSTROM_DIR}/store/latest" "${store_dir}/maelstrom-results" 2>/dev/null || true
    fi

    return ${exit_code}
}

test_basic() {
    run_test "basic" \
        --nemesis "none"
}

test_partition() {
    # Isolate individual nodes (including potentially the leader)
    run_test "partition" \
        --nemesis "partition"
}

test_partition_half() {
    # Split cluster into two halves (split-brain scenario)
    run_test "partition-half" \
        --nemesis "partition"
}

test_kill() {
    # Kill and restart nodes randomly
    run_test "kill" \
        --nemesis "kill"
}

test_all_faults() {
    # Combined partition + kill
    run_test "all-faults" \
        --nemesis "partition" \
        --nemesis2 "kill"
}

# ---- Main ----

TEST_NAME="${1:-basic}"
OVERALL_RESULT=0

case "${TEST_NAME}" in
    basic)
        test_basic || OVERALL_RESULT=1
        ;;
    partition)
        test_partition || OVERALL_RESULT=1
        ;;
    partition-half)
        test_partition_half || OVERALL_RESULT=1
        ;;
    kill)
        test_kill || OVERALL_RESULT=1
        ;;
    all-faults)
        test_all_faults || OVERALL_RESULT=1
        ;;
    all)
        echo -e "${YELLOW}Running all test suites...${NC}"
        test_basic || OVERALL_RESULT=1
        test_partition || OVERALL_RESULT=1
        test_kill || OVERALL_RESULT=1
        test_all_faults || OVERALL_RESULT=1
        ;;
    *)
        echo "Unknown test: ${TEST_NAME}"
        echo "Available: basic, partition, partition-half, kill, all-faults, all"
        exit 1
        ;;
esac

echo ""
echo "============================================================"
if [ ${OVERALL_RESULT} -eq 0 ]; then
    echo -e "${GREEN}ALL TESTS PASSED${NC}"
else
    echo -e "${RED}SOME TESTS FAILED${NC}"
fi
echo "Results saved to: ${RESULTS_DIR}/"
echo "============================================================"

exit ${OVERALL_RESULT}
