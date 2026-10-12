# Contributing to CarryFS

Run the checks before you send a change:

```sh
bash scripts/check.sh
```

It runs the format reference check in `spec/verify.py`, then
`cargo fmt --check`, `cargo check`, Clippy with warnings as errors, and the
tests. It needs Bash, Python 3 and Rust with rustfmt and Clippy.

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
