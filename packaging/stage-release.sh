#!/bin/bash
# Stage a release payload in the layout install.sh and install.ps1 verify.
#
#   packaging/stage-release.sh PLATFORM OUTDIR
#
# PLATFORM is linux-x86_64, macos-arm64, or windows-x86_64. Run it after
# building the release binaries; the cargo commands mirror install.sh --local.
set -euo pipefail
[ "$#" = 2 ] || { echo "usage: $0 PLATFORM OUTDIR" >&2; exit 2; }
cd "$(dirname "$0")/.."
platform=$1 out=$2
version=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -n 1)
target=${CARGO_TARGET_DIR:-target}/release
mkdir -p "$out"
case $platform in
    linux-x86_64)
        cp "$target/carryfs" "$target/carryfs-fuse" fuse/packaging/61-carryfs.rules \
            fuse/packaging/mount_options.conf "$out/"
        cp packaging/install-unix.sh "$out/install-system.sh"
        ;;
    macos-arm64)
        # The bundle layout of fuse/packaging/macos-bundle.sh.
        resources=$out/carryfs.fs/Contents/Resources
        mkdir -p "$resources"
        cp "$target/carryfs" "$out/"
        cp fuse/packaging/carryfs.fs/Contents/Info.plist "$out/carryfs.fs/Contents/"
        cp "$target/carryfs-util" "$resources/carryfs.util"
        cp "$target/newfs_carryfs" "$target/fsck_carryfs" "$resources/"
        cp "$target/carryfs-fuse" "$resources/mount_carryfs"
        cp packaging/install-unix.sh "$out/install-system.sh"
        ;;
    windows-x86_64)
        cp "$target/carryfs.exe" "$target/carryfs-windows.exe" "$out/"
        cp windows/packaging/install.ps1 "$out/install-system.ps1"
        ;;
    *) echo "unknown platform: $platform" >&2; exit 2 ;;
esac
printf '%s\n' "$version" > "$out/VERSION"
