#!/usr/bin/env bash
#
# Warm this repo so a freshly seeded /workspace runs `make build` instantly: build
# exactly what `make build` builds (minified CSS + the release binaries) so the
# generated CSS and target/release ride the snapshot.
# (RUSTUP_HOME, CARGO_HOME, and target/ must live on the /workspace volume for the
# pinned toolchain and warmed caches to survive into the claimant's session.)

set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.." || exit 1

make build
