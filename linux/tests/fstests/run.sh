#!/bin/bash
# Run fstests against the CarryFS kernel driver on two loop images.
# Usage: run.sh FSTESTS_DIR [check arguments, default -g quick]
# Needs root, the module loaded, and release builds of carryfs and
# carryfs-fuse in target/release.
set -eu
fstests=$(realpath "$1")
shift
here=$(dirname "$(realpath "$0")")
export CARRYFS_BIN=$(realpath "$here/../../../target/release")
# Images live on disk: /tmp is often a small tmpfs, and a full backing
# file fails the loop device's writes.
work=$(mktemp -d "${CARRYFS_FSTESTS_DIR:-/var/tmp}/carryfs-fstests.XXXXXX")
truncate -s 2G "$work/test.img" "$work/scratch.img"
export TEST_DEV=$(losetup -f --show "$work/test.img")
export SCRATCH_DEV=$(losetup -f --show "$work/scratch.img")
export TEST_DIR=$work/test SCRATCH_MNT=$work/scratch
mkdir -p "$TEST_DIR" "$SCRATCH_MNT"
cleanup() {
	umount "$TEST_DIR" "$SCRATCH_MNT" 2>/dev/null || true
	losetup -d "$TEST_DEV" "$SCRATCH_DEV"
	rm -rf "$work"
}
trap cleanup EXIT
export PATH=$here/bin:$PATH
# Golden outputs expect GNU od's lowercase hex; Ubuntu's default uutils od
# prints uppercase (generic/029, generic/030).
if [ -x /usr/bin/gnuod ]; then
	mkdir "$work/bin"
	ln -s /usr/bin/gnuod "$work/bin/od"
	export PATH=$work/bin:$PATH
fi
"$here/bin/mkfs.carryfs" "$TEST_DEV" >/dev/null
cp "$here/local.config" "$fstests/local.config"
cd "$fstests"
[ $# -gt 0 ] || set -- -g quick
./check "$@"
