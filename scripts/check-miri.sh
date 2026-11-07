#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
mkdir -p build/check-miri

# PACKAGE TEST [RUSTFLAGS]: run one test under Miri.
run() {
    local log="build/check-miri/${2##*::}.log"
    RUSTFLAGS=${3:-} cargo +nightly miri test --locked -p "$1" --all-features --lib \
        "$2" -- --exact 2>&1 | tee "$log"
    # A renamed test must fail instead of silently running zero tests.
    grep -Eq '^test result: ok\. 1 passed; 0 failed;' "$log"
}

run carryfs-ondisk crc::tests::crc32c_matches_reference_vectors
# With SSE4.2 and PCLMUL enabled, Miri runs the hardware CRC path's intrinsics.
run carryfs-ondisk crc::tests::hardware_crc32c_matches_the_software_tables \
    "-C target-feature=+sse4.2,+pclmulqdq"
run carryfs-core tree::tests::a_full_cache_drops_leaves_and_keeps_branches
run carryfs-core tree::tests::discarded_drafts_still_count_toward_transaction_work
