#!/usr/bin/env bash
#
# Warm this repo's cargo caches so a freshly seeded /workspace builds fast.
# (RUSTUP_HOME, CARGO_HOME, and target/ must live on the /workspace volume for the
# pinned toolchain and warmed caches to survive into the claimant's session.)

set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.." || exit 1

cargo build --all-targets --all-features
cargo clippy --all-targets --all-features
