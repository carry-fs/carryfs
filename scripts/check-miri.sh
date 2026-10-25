#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
mkdir -p build/check-miri

for filter in \
    format::tests::crc32c_matches_reference_vectors \
    tree::tests::a_full_cache_drops_leaves_and_keeps_branches \
    tree::tests::discarded_drafts_still_count_toward_transaction_work; do
    log="build/check-miri/${filter##*::}.log"
    cargo +nightly miri test --locked -p carryfs-core --all-features --lib \
        "$filter" -- --exact 2>&1 | tee "$log"
    # A renamed filter must fail instead of silently running zero tests.
    grep -Eq '^test result: ok\. 1 passed; 0 failed;' "$log"
done
