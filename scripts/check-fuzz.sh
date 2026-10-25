#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
mkdir -p build/check-fuzz fuzz/corpus/parsers
cp testdata/vectors/*.bin fuzz/corpus/parsers/

for target in parsers commits storage files; do
    case $target in
        parsers) max_len=4096 ;;
        commits) max_len=80 ;;
        storage|files) max_len=256 ;;
    esac
    cargo +nightly fuzz run "$target" -- \
        -max_total_time=60 -max_len="$max_len" 2>&1 | tee "build/check-fuzz/$target.log"
done
