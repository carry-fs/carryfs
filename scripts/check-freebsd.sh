#!/bin/sh
# Native FreeBSD checks, including actual FUSE mounts on disposable md devices.
set -eu
cd "$(dirname "$0")/.."
[ "$(uname -s)" = FreeBSD ] || { echo 'Run this on FreeBSD.' >&2; exit 1; }
cargo fmt --check
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
cargo test --locked --workspace --all-features
cargo build --locked -p carryfs-core -p carryfs-fuse --features carryfs-fuse/fault-injection
if [ "$(id -u)" = 0 ]; then
    python3 fuse/tests/freebsd.py
else
    sudo python3 fuse/tests/freebsd.py
fi
