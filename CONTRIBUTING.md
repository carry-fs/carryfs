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

It runs the format reference check in `spec/verify.py`, then
`cargo fmt --check`, `cargo check`, Clippy with warnings as errors, and the
tests.

Each platform adapter has one more check that the script does not run. Run
the one for the adapter you touch:

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
