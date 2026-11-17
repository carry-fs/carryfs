#!/bin/sh
# Native FreeBSD checks, including actual FUSE mounts on disposable md devices.
set -eu
cd "$(dirname "$0")/.."
[ "$(uname -s)" = FreeBSD ] || { echo 'Run this on FreeBSD.' >&2; exit 1; }
# The macOS and Windows crates are built and tested on their own CI jobs.
workspace='--workspace --exclude carryfs-macos --exclude carryfs-windows'
# --emulated: skip what the x86-64 jobs already cover, for slow emulated guests.
# Core is single-threaded and arch-independent; extended-checks runs it in full.
if [ "${1:-}" = --emulated ]; then
    cargo test --locked -p carryfs-ondisk -p carryfs-fuse --all-features
else
    cargo fmt --check
    cargo clippy --locked $workspace --all-targets --all-features -- -D warnings
    cargo test --locked $workspace --all-features
fi
cargo build --locked -p carryfs-core -p carryfs-fuse --features carryfs-fuse/fault-injection
if [ "$(id -u)" = 0 ]; then
    python3 fuse/tests/freebsd.py
else
    sudo python3 fuse/tests/freebsd.py
fi
