#!/bin/sh
# Compare the layout!() values in windows/src/ffi.rs with WinFsp's headers.
# The probe is a Windows program. RUN must execute a command on a Windows host,
# and COPY must copy LOCAL to REMOTE in the directory RUN starts in. Wrappers
# around ssh and scp work.
# Requires x86_64-w64-mingw32-gcc.
set -eu
root=$(cd "$(dirname "$0")/../.." && pwd)
out=$root/build/windows-adapter/layout
tag=v2.1
mkdir -p "$out/winfsp"
for header in winfsp.h fsctl.h; do
    [ -f "$out/winfsp/$header" ] ||
        curl -sSLf -o "$out/winfsp/$header" "https://raw.githubusercontent.com/winfsp/winfsp/$tag/inc/winfsp/$header"
done
x86_64-w64-mingw32-gcc -std=c11 -include assert.h -I"$out" -o "$out/layout.exe" "$root/windows/tests/layout.c"
: "${RUN:?set RUN to a command that runs a Windows program}"
: "${COPY:?set COPY to a command taking LOCAL REMOTE that copies a file to the Windows host}"
$COPY "$out/layout.exe" layout.exe
$RUN ./layout.exe | tr -d '\r' | sort > "$out/c.txt"
# rustfmt may wrap a layout!() call, so join lines before matching.
tr -s ' \n' ' ' < "$root/windows/src/ffi.rs" |
    grep -o 'layout!( *"[^"]*", [^;]*, [0-9]*,\? *);' |
    sed 's/^layout!( *"\([^"]*\)", .*, \([0-9]*\),\{0,1\} *);$/\1 \2/' | sort > "$out/rust.txt"
if diff -u "$out/c.txt" "$out/rust.txt"; then
    echo "layout matches WinFsp $tag: $(wc -l < "$out/rust.txt") values"
else
    echo "layout differs from WinFsp $tag" >&2
    exit 1
fi
