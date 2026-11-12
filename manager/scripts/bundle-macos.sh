#!/bin/bash
set -euo pipefail
root=$(cd "$(dirname "$0")/../.." && pwd)
target="$root/manager/target"
cargo build --manifest-path "$root/manager/Cargo.toml" --target-dir "$target" --locked --bins
stage=$(mktemp -d "$target/bundle.XXXXXX")
trap 'rm -rf "$stage"' EXIT
bundle="$target/CarryFS Manager.app"
mkdir -p "$stage/MacOS" "$bundle/Contents/MacOS"
for binary in carryfs-manager carryfs; do
    cp "$target/debug/$binary" "$stage/MacOS/$binary"
    test -x "$stage/MacOS/$binary"
    test -s "$stage/MacOS/$binary"
done
cat >"$stage/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleExecutable</key><string>carryfs-manager</string>
<key>CFBundleIdentifier</key><string>dev.carryfs.manager</string>
<key>CFBundleName</key><string>CarryFS Manager</string>
<key>CFBundlePackageType</key><string>APPL</string>
<key>CFBundleVersion</key><string>1</string>
<key>NSHighResolutionCapable</key><true/>
</dict></plist>
PLIST
# Replace files instead of copying through a previous development symlink.
for binary in carryfs-manager carryfs; do
    mv -f "$stage/MacOS/$binary" "$bundle/Contents/MacOS/$binary"
done
mv -f "$stage/Info.plist" "$bundle/Contents/Info.plist"
printf '%s\n' "$bundle"
