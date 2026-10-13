#!/usr/bin/env bash
# Fast, unprivileged checks on the current host.
set -euo pipefail
cd "$(dirname "$0")/.."
python3 spec/verify.py
cargo fmt --check
cargo check --locked --workspace --all-targets --all-features
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
cargo test --locked --workspace --all-features --all-targets
cargo test --locked --doc --workspace --all-features
