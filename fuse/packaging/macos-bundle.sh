#!/bin/bash
# Build the carryfs.fs bundle that mounts CarryFS through macFUSE.
#
#   bash fuse/packaging/macos-bundle.sh
#
# DiskArbitration probes a disk with carryfs.util, then runs mount(8), which
# execs Contents/Resources/mount_carryfs from the bundle. diskutil and Disk
# Utility run newfs_carryfs and fsck_carryfs from the same place.
set -euo pipefail
cd "$(dirname "$0")/../.."
cargo build --release -p carryfs-fuse -p carryfs-macos
bundle="$PWD/build/macfuse/carryfs.fs"
rm -rf "$bundle"
mkdir -p "$bundle/Contents/Resources"
cp fuse/packaging/carryfs.fs/Contents/Info.plist "$bundle/Contents/"
cp target/release/carryfs-util "$bundle/Contents/Resources/carryfs.util"
cp target/release/newfs_carryfs target/release/fsck_carryfs "$bundle/Contents/Resources/"
cp target/release/carryfs-fuse "$bundle/Contents/Resources/mount_carryfs"
cat <<EOF
Built $bundle. Install it with:
  sudo rm -rf /Library/Filesystems/carryfs.fs
  sudo cp -R "$bundle" /Library/Filesystems/ && sudo killall storagekitd
EOF
