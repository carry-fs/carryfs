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

Windows, from 64-bit PowerShell:

```powershell
powershell -ExecutionPolicy Bypass -File .\install.ps1 -Local
```

The script builds release binaries, asks for confirmation, and requests
administrator permission to install. Eject all CarryFS volumes first.

## Build without installing

Run these commands from the checkout. Binaries are written to `target/release`.
To install the resulting files yourself, follow
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

It runs the format reference check in `spec/verify.py`, installer selection
and release-index tests, then
`cargo fmt --check`, `cargo check`, Clippy with warnings as errors, and the
tests.

To run only the installer checks:

```sh
python3 -m unittest discover -s tests/installers -p 'test_*.py'
```

On Windows, also run `powershell -NoProfile -File tests/installers/selection.ps1`.

The website publisher generates `https://carryfs.com/releases.txt` from
published releases with all three platform packages and `SHA256SUMS` uploaded.
Release publication calls the same publisher after the assets are available.
Tagged runs update only the index, preserving the website's installer scripts.
The index is generated automatically; do not add unreleased versions by hand.
If index publication fails after a release succeeds, rerun the failed
publisher job to rebuild it from the published releases.

The [installer-checks workflow](.github/workflows/installer-checks.yml) runs
selection tests on Ubuntu, macOS, and Windows, then installs a selected
published package on disposable Ubuntu and Windows hosts. Start it manually
and choose the package version. It checks adapter installation, reinstall,
and uninstall; the Ubuntu job removes `jq` first.
[Initial verification](https://github.com/carry-fs/carryfs/actions/runs/40237648047)
passed using an existing published package and the checkout's installers.

### Miri

For changes to low-level core code, run the focused memory-safety checks.
Install nightly Rust and its Miri components once:

```sh
rustup toolchain install nightly --profile minimal --component miri --component rust-src
bash scripts/check-miri.sh
```

The script checks CRC vectors, tree-cache eviction, and transaction-work
accounting. Each exact filter must run one test. Logs go to `build/check-miri/`.
It uses Miri's default isolation and does not cover the entire core or adapter
FFI. Longer volume tests are outside this bounded set.

### Fuzzing

For parser, storage, or state-machine changes, install nightly Rust and
`cargo-fuzz`, then run:

```sh
rustup toolchain install nightly --profile minimal
cargo install cargo-fuzz --locked
bash scripts/check-fuzz.sh
```

The four targets cover parser inputs, publication/crash/reopen sequences,
storage maintenance, and file operations checked against a byte oracle.
Each target requests a 60-second run; an individual input can finish after
that budget. Parser seeds come from `testdata/vectors/`. Logs go to
`build/check-fuzz/`; corpora and failing inputs stay under `fuzz/corpus/` and
`fuzz/artifacts/`. These are bounded runs, not exhaustive verification.

### Linux directory synchronization

On a disposable Linux host with root access, FUSE, udev, and loop-device
tools, build the CLI and adapter, then run:

```sh
cargo build --release --locked -p carryfs-core -p carryfs-fuse
sudo python3 fuse/tests/fsyncdir.py
```

The test creates fresh 256 MiB images and its own loop devices. It checks
create, rename, and unlink after directory fsync and fdatasync, in default
and strict modes, by killing the adapter immediately after synchronization
and checking recovered contents offline. It uses no existing disk or mount
and does not need the system installer or UDisks. Logs and images stay under
`build/fsyncdir-*`. It tests daemon interruption, not physical power loss.

### Occasional CI

The [extended-checks workflow](.github/workflows/extended-checks.yml) runs
these same scripts on Ubuntu 24.04 through manual dispatch and a weekly
schedule, Mondays at 04:23 UTC. Start a manual run from the repository's
Actions tab with **Run workflow**. Miri and fuzzing run without root; the
directory-sync job uses root inside a disposable VM. Jobs have time limits
and upload logs and failing fuzz inputs for seven days. The weekly schedule
takes effect after the workflow reaches the default branch.

[Initial Ubuntu verification](https://github.com/carry-fs/carryfs/actions/runs/40231511907)
passed all three Miri filters, four bounded fuzz runs, and 12 directory-sync
cases, including artifact uploads.

### Other platform checks

Run the additional checks for the adapter you touch:

- Linux: `cargo test --locked -p carryfs-fuse -- --ignored` mounts a volume
  through FUSE and checks the kernel's directory cache. It needs `/dev/fuse`,
  `fusermount3` and root.
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
