#!/usr/bin/env bash
# Run the native checks in a disposable VM with a matching kernel and module.
set -euo pipefail
cd "$(dirname "$0")/../.."
repo=$PWD
kernel_image=${KERNEL_IMAGE:-/usr/lib/modules/$(uname -r)/vmlinuz}
[[ -r $kernel_image ]] || { echo "cannot read kernel image: $kernel_image" >&2; exit 1; }
[[ -f linux/carryfs.ko && -x target/release/carryfs && -x target/release/carryfs-fuse ]] || {
    echo "build linux/carryfs.ko, target/release/carryfs, and target/release/carryfs-fuse first" >&2
    exit 1
}
[[ -x build/check-kernel/replay-log ]] || {
    echo "build the fstests replay-log tool in build/check-kernel/ first; see linux/README.md" >&2
    exit 1
}
mkdir -p build/check-kernel
work=$(mktemp -d "$repo/build/check-kernel/vm.XXXXXX")
trap 'python3 -c '\''import shutil,sys; shutil.rmtree(sys.argv[1])'\'' "$work"' EXIT
truncate -s 6G "$work/store.img"
guest=${1:-"$repo/linux/tests/guest.sh"}
[[ $# == 0 ]] || shift
printf -v command '%q ' "$guest" "$@"
args=(--kimg "$kernel_image" --mods auto --memory 4G --cpus 4 --pwd --force-9p
      --disable-microvm --kopt panic=-1 --script-sh
      "set -eu; mkfs.ext4 -q -F /dev/disk/by-id/virtio-carryfs-test-store; mount /dev/disk/by-id/virtio-carryfs-test-store /var/tmp; exec $command")
if [[ ${CARRYFS_VM_TCG:-0} == 1 ]]; then
    args+=(--disable-kvm)
fi
args+=(--qemu-opts -drive "file=$work/store.img,if=none,format=raw,id=store"
       -device virtio-blk-pci,drive=store,serial=carryfs-test-store)
timeout --foreground "${CARRYFS_VM_TIMEOUT:-1800}" virtme-run "${args[@]}" \
    2>&1 | tee "$work.log"
