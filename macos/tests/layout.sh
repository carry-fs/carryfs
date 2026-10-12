#!/bin/bash
# Check the helper ioctl constants against the macOS SDK, then compile the helpers.
set -euo pipefail
cd "$(dirname "$0")/.."
out="$(mktemp -d)"
trap 'rm -rf "$out"' EXIT
cc -std=c11 -Wall -Werror tests/layout.c -o "$out/layout"
"$out/layout"
cargo check -p carryfs-macos
printf 'layout: helper ioctl constants match the macOS SDK; helpers compile\n'
