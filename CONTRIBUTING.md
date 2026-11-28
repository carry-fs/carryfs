# Contributing to CarryFS

For release packages and runtime setup, see [INSTALLATION.md](INSTALLATION.md).

## Build requirements

Install Git and [Rust](https://rustup.rs/) 1.89 or later, including Cargo.
Rust also needs a native linker:

- Linux: GCC or Clang; Ubuntu provides the toolchain in `build-essential`.
- macOS: Xcode Command Line Tools, installed with `xcode-select --install`.
- Windows: the MSVC toolchain and Windows SDK. Follow the
  [Rust MSVC setup guide](https://rust-lang.github.io/rustup/installation/windows-msvc.html).

To install or mount the filesystem adapter, also set up the
[platform framework](INSTALLATION.md#requirements). The CLI can work with disk
images without mounting them.

## Build and install from source

Clone the repository:

```sh
git clone https://github.com/carry-fs/carryfs.git
cd carryfs
```

Linux and macOS:

```sh
./install.sh --local
```

On Linux, `./install.sh --local --native` also builds and installs the native
module for the running kernel. FUSE stays the default.
`./install.sh --uninstall` removes both adapters, including the modules it
recorded for older kernels. [Native installation](INSTALLATION.md#fuse-adapter-or-native-driver)
lists the requirements and how to load the module.

Windows, from 64-bit PowerShell:

```powershell
powershell -ExecutionPolicy Bypass -File .\install.ps1 -Local
```

The script builds release binaries, asks for confirmation, and requests
administrator permission to install. Eject all CarryFS volumes first.

## Build without installing

Run these commands from the checkout. Cargo writes the binaries to
`target/release`. To install them yourself, follow
[install by hand](INSTALLATION.md#install-by-hand).

### Linux

```sh
cargo build --release --locked -p carryfs-core -p carryfs-fuse
```

### macOS

```sh
cargo build --release --locked -p carryfs-core
bash fuse/packaging/macos-bundle.sh
```

The bundle script builds the adapter and disk helpers, then assembles
`build/macfuse/carryfs.fs` for DiskArbitration, `diskutil`, and Disk Utility.

### Windows

Run in a normal PowerShell:

```powershell
cargo build --release --locked -p carryfs-core -p carryfs-windows
```

## Run checks

The checks also need Bash, Python 3, rustfmt, and Clippy. On Windows, run them
from a Bash shell with these tools available. With rustup, install the Rust
components with:

```sh
rustup component add rustfmt clippy
```

Run the checks before you send a change:

```sh
bash scripts/check.sh
```

It runs the format reference check in `spec/verify.py`, the installer
selection and release-index tests, `cargo fmt --check`, `cargo check`, Clippy
with warnings as errors, and the tests.

To run only the installer checks:

```sh
python3 -m unittest discover -s tests/installers -p 'test_*.py'
```

On Windows, also run `powershell -NoProfile -File tests/installers/selection.ps1`.

The [installer-checks workflow](.github/workflows/installer-checks.yml) runs
the selection tests on Ubuntu, macOS, and Windows for installer PRs. On Linux
it also checks native installation, reinstall, rollback, and cleanup across
kernel versions in a temporary filesystem tree. None of this needs a published
release. To test a published package on disposable Ubuntu and Windows hosts,
start the workflow by hand and choose the version. The Ubuntu job removes `jq`
first, to prove the installer works without it.

### Miri

For changes to low-level core code, run the focused memory-safety checks.
Install nightly Rust and its Miri components once:

```sh
rustup toolchain install nightly --profile minimal --component miri --component rust-src
bash scripts/check-miri.sh
```

The script checks CRC vectors, tree-cache eviction, and transaction-work
accounting. Each filter must match exactly one test. Logs go to
`build/check-miri/`. It runs under Miri's default isolation and skips the rest
of the core, the adapters' FFI, and the longer volume tests.

### Fuzzing

For parser, storage, or state-machine changes, install nightly Rust and
`cargo-fuzz`, then run:

```sh
rustup toolchain install nightly --profile minimal
cargo install cargo-fuzz --locked
bash scripts/check-fuzz.sh
```

The four targets cover parser inputs, publication, crash, and reopen
sequences, storage maintenance, and file operations checked against a byte
oracle. Each target runs for about 60 seconds; the input in progress can run
past that. Parser seeds come from `testdata/vectors/`. Logs go to
`build/check-fuzz/`. Corpora stay in `fuzz/corpus/` and failing inputs in
`fuzz/artifacts/`.

### Allocation and unnamed-file checks

Core tests cover reservations across remounts, conversion interruptions,
truncation, hole punching, and writes after another file fills the volume.
Windows adapter tests cover allocation at create, overwrite, size changes,
and last cleanup.

On Linux with `/dev/fuse` and permission to mount:

```sh
cargo test --locked -p carryfs-fuse tmpfile -- --include-ignored
```

On macOS with macFUSE, after building the CLI and adapter:

```sh
cargo build --locked -p carryfs-core -p carryfs-fuse
python3 fuse/tests/preallocate_macos.py
```

The macOS test checks `F_PREALLOCATE` return values and persistent and
temporary reservations, then runs `check` and `scrub` on the image after
unmounting. macFUSE 5.5 reports allocation through `stat` only up to EOF, so
the on-disk checker verifies reservations beyond EOF.

### Linux directory synchronization

On a disposable Linux host with root access, FUSE, udev, and loop-device
tools, build the CLI and adapter, then run:

```sh
cargo build --release --locked -p carryfs-core -p carryfs-fuse
sudo python3 fuse/tests/fsyncdir.py
```

The test creates fresh 256 MiB images on its own loop devices. For create,
rename, and unlink, in default and strict modes, it calls directory fsync or
fdatasync, kills the adapter right after, and checks the recovered contents
offline. It touches no existing disk or mount and needs neither the system
installer nor UDisks. Logs and images stay in `build/fsyncdir-*`. A killed
daemon is not a power cut, so this does not test physical power loss.

### Occasional CI

The [extended-checks workflow](.github/workflows/extended-checks.yml) runs
these scripts on Ubuntu 24.04 every Monday at 04:23 UTC. To start it by hand,
open the repository's Actions tab and choose **Run workflow**. Miri and
fuzzing run without root. The directory-sync job runs as root inside a
disposable VM. Every job has a time limit, and the workflow keeps logs and
failing fuzz inputs for seven days.

### Other platform checks

Run the additional checks for the adapter you touch:

- Linux: `cargo test --locked -p carryfs-fuse -- --ignored` mounts a volume
  through FUSE and checks the kernel's directory cache. It needs `/dev/fuse`,
  `fusermount3` and root.
- FreeBSD: load `fusefs`, then run `sh scripts/check-freebsd.sh`. It runs
  workspace tests and actual mount, durability, raw-format, and device-lock
  tests against disposable md devices. The mount tests need root or sudo.
  CI runs this on FreeBSD 14.5 and 15.1, on amd64 and arm64. From a Mac, edit
  locally, sync the checkout into a FreeBSD QEMU VM, and build and run the
  script there. Keep the checkout on the guest filesystem.
- NetBSD: install the stable Rust toolchain through rustup, with rustfmt and
  Clippy, and `python312` through pkgin, then run `make check`.
  It runs formatting, Clippy, and the workspace and installer tests, then the
  mounted tests shared with FreeBSD, through `perfused` on disposable vnd
  disks. The mounted tests need root or passwordless sudo. CI tests NetBSD
  10.2 and 11.1 on amd64. Keep the checkout and the Cargo target directory on
  the guest filesystem.
- macOS: `bash macos/tests/layout.sh` checks the ioctl constants in
  `macos/src/raw.rs` against the macOS SDK.
- Windows: `windows/tests/layout.sh` compares the struct layouts in
  `windows/src/ffi.rs` with the WinFsp headers. It needs mingw-w64, curl and a
  Windows host to run the probe on. The script header says how to point it at
  the host.

## Contribution license

By default, contributions intentionally submitted for inclusion in CarryFS
are licensed under `Apache-2.0 OR GPL-2.0-only`, without additional terms or
conditions. By submitting a contribution, you agree to grant both license
alternatives.
