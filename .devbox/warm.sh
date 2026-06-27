#!/usr/bin/env bash
#
# Warm this repo so a freshly seeded /workspace runs `make build` instantly: build
# exactly what `make build` builds (minified CSS + the release binaries) so the
# generated CSS and target/release ride the snapshot, then compile target/debug so
# `make test` and rust-analyzer are warm too.
# (RUSTUP_HOME, CARGO_HOME, and target/ must live on the /workspace volume for the
# pinned toolchain and warmed caches to survive into the claimant's session.)
#
# Compile, never run, the tests: warming only needs the artifacts on the snapshot,
# and a flaky / credential / network-dependent test must not abort warming and leave
# a cold snapshot. `make build` (release) runs first so it is warmed even if the
# test compile later fails.

set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.." || exit 1

make build
cargo test --all-features --no-run
