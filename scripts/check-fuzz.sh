#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
mkdir -p build/check-fuzz fuzz/corpus/parsers fuzz/corpus/ondisk
cp testdata/vectors/*.bin fuzz/corpus/parsers/
# The ondisk target's first byte picks its mode; 0 decodes a page.
for vector in testdata/vectors/*.bin; do
    { printf '\0'; cat "$vector"; } > "fuzz/corpus/ondisk/$(basename "$vector")"
done

# Targets to run, all by default: scripts/check-fuzz.sh [TARGET...]
targets=("$@")
[[ ${#targets[@]} -gt 0 ]] || targets=(parsers commits storage files ondisk)

for target in "${targets[@]}"; do
    case $target in
        parsers) max_len=4096 ;;
        ondisk) max_len=4200 ;;
        commits) max_len=80 ;;
        storage|files) max_len=256 ;;
    esac
    cargo +nightly fuzz run "$target" -- \
        -max_total_time=60 -max_len="$max_len" 2>&1 | tee "build/check-fuzz/$target.log"
done
