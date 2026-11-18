#!/bin/sh
# Native NetBSD checks, including actual PUFFS mounts on disposable vnd disks.
set -eu
cd "$(dirname "$0")/.."
[ "$(uname -s)" = NetBSD ] || { echo 'Run this on NetBSD.' >&2; exit 1; }
export PATH="$HOME/.cargo/bin:/usr/pkg/bin:/usr/pkg/sbin:/sbin:/usr/sbin:$PATH"
# NetBSD tmpfs reserves sparse-file lengths; recovery tests need a disk filesystem.
export TMPDIR=/var/tmp
layout=$(mktemp -d)
trap 'rm -rf "$layout"' EXIT
cc -std=c11 -Wall -Wextra -Werror fuse/tests/netbsd-layout.c -o "$layout/layout"
"$layout/layout"
PYTHON=$(command -v python3 || command -v python3.12)
export PYTHON
workspace='--workspace --exclude carryfs-macos --exclude carryfs-windows'
cargo fmt --check
cargo clippy --locked $workspace --all-targets --all-features -- -D warnings
cargo test --locked $workspace --all-features
cargo build --locked --profile test -p carryfs-core -p carryfs-fuse --all-features
"$PYTHON" -m unittest discover -s tests/installers -p 'test_bsd_install.py'
if [ "$(id -u)" = 0 ]; then
    "$PYTHON" fuse/tests/netbsd.py
else
    sudo env PATH="$PATH" "$PYTHON" fuse/tests/netbsd.py
fi
