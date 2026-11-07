#!/usr/bin/env bash
# Build the kernel driver (linux/) against installed kernel headers, with
# the rustc that built that kernel, as a Rust module must be.
#
#   scripts/kernel-build.sh [KDIR]
#
# KDIR defaults to the running kernel's build directory, or else the newest
# installed one. Says why when the kernel cannot build Rust modules.
set -euo pipefail
cd "$(dirname "$0")/.."

fail() {
	echo "kernel-build: $*" >&2
	exit 1
}

kdir=${1:-}
if [[ -z $kdir ]]; then
	if [[ -d /lib/modules/$(uname -r)/build ]]; then
		kdir=/lib/modules/$(uname -r)/build
	else
		# Containers run the host's kernel: take the newest headers installed.
		kdir=$(ls -d /lib/modules/*/build /usr/src/kernels/* /usr/src/linux-headers-*-generic \
			2>/dev/null | sort -V | tail -1 || true)
	fi
fi
[[ -n $kdir && -f $kdir/Makefile ]] || fail "no kernel headers found; install your distribution's kernel headers"
kdir=$(readlink -f "$kdir")
config=$kdir/include/config/auto.conf
[[ -f $config ]] || fail "$kdir has no include/config/auto.conf"
echo "kernel-build: headers $kdir"

grep -q '^CONFIG_RUST=y' "$config" || fail "this kernel is built without Rust support (CONFIG_RUST)"
[[ -f $kdir/rust/libkernel.rmeta ]] ||
	fail "the headers lack the kernel's Rust libraries (rust/libkernel.rmeta); Ubuntu ships them as linux-lib-rust-<version>"

# auto.conf leaves the value unquoted; .config quotes it.
wanted=$(sed -n 's/^CONFIG_RUSTC_VERSION_TEXT="\{0,1\}\([^"]*\)"\{0,1\}$/\1/p' "$config")
[[ -n $wanted ]] || fail "$config does not record the rustc that built the kernel"
echo "kernel-build: kernel built with $wanted"

# The exact compiler build: the whole of `rustc --version`, packager suffix
# included. A rustup toolchain of the same version and commit is a different
# build, which the kernel's crates reject. Distributions ship it as rustc, or
# versioned, as Ubuntu's rustc-1.NN; every one on PATH is tried.
version=$(awk '{print $2}' <<<"$wanted")
rustc=
for name in ${RUSTC:-} rustc "rustc-${version%.*}" "rustc-$version"; do
	for candidate in $(type -ap "$name" 2>/dev/null); do
		if [[ $("$candidate" --version) == "$wanted" ]]; then
			rustc=$candidate
			break 2
		fi
	done
done
[[ -n $rustc ]] || fail "no rustc on PATH is \"$wanted\"; install that exact build (first on PATH: $(rustc --version 2>/dev/null || echo none))"
echo "kernel-build: using $rustc"

make -C linux build KDIR="$kdir" RUSTC="$rustc"
modinfo linux/carryfs.ko | grep -E '^(name|vermagic):'
