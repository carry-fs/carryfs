# Contributing to CarryFS

Run these checks before you send a change:

```sh
cargo fmt --check
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
cargo test --locked --workspace --all-features --all-targets
cargo test --locked --doc --workspace --all-features
```

## Contribution license

By default, contributions intentionally submitted for inclusion in CarryFS
are licensed under `Apache-2.0 OR GPL-2.0-only`, without additional terms or
conditions. By submitting a contribution, you agree to grant both license
alternatives.
