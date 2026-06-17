# Build and Test

## Prerequisites

- Rust 1.96+ (pinned in `rust-toolchain.toml`)
- TailwindCSS v4 standalone CLI (not npm) for CSS compilation
- Docker for container builds
- System packages (Linux): `clang`, `pkg-config` (for `aws-lc-rs`)

## Key Commands

| Task | Command |
|------|---------|
| Build (release, includes CSS) | `make build` |
| Format | `make fmt` |
| Lint | `make lint` |
| Check | `make check` |
| Unit tests | `make test` |
| Run server | `make run-server` |
| Build CSS | `make css-build` |
| Watch CSS | `make css-dev` |
| Docker build | `make docker-build` |
| Docker run | `make docker-run` |
| Build musl (Docker Bake) | `make bake-all` |
| Clean | `make clean` |

## Running Specific Tests

```bash
cargo test test_name -- --nocapture   # Single test with output
cargo test --package devbox-server    # Server tests only
cargo test --package devbox-common    # Common crate tests only
```

## Server Environment (Minimum Viable)

```bash
DATABASE_URL="sqlite:devbox-dev.db?mode=rwc" \
RUST_LOG=info,devbox_server=debug \
cargo run --bin devbox-server
```

The server binds to `[::]:3000` by default.

## Environment Variables

| Variable | Purpose | Default |
|----------|---------|---------|
| `DATABASE_URL` | SQLite or Postgres/DSQL connection string | `sqlite::memory:` |
| `RUST_LOG` | Log level filter | `info` |
| `PORT` | Server listen port | `3000` |

## Gotchas

- First build is slow (~2-3 min) due to `aws-lc-rs` compilation. Incremental builds are fast.
- Static assets (CSS) are embedded at compile time via `rust-embed`. After changing CSS, rebuild the binary.
- The `.env` file at repo root is loaded by the Makefile for server runs.
- Database tests use in-memory SQLite and do not require any external services.
- The `cargo test` command does not require TailwindCSS (the embedded static CSS is a placeholder `.gitkeep`).
