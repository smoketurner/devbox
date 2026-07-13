# File Locations

## Quick Reference

| Need | Location |
|------|----------|
| Shared types (DevboxId, DevboxState, API types) | `crates/devbox-common/src/lib.rs` |
| CLI binary + subcommands | `crates/devbox-cli/src/main.rs` (Clap definitions), `crates/devbox-cli/src/command.rs` (handlers), `crates/devbox-cli/src/state.rs`, `crates/devbox-cli/src/auth.rs`, `crates/devbox-cli/src/session.rs` |
| Server binary entry point | `crates/devbox-server/src/main.rs` |
| On-host agent (principals / owner-sync / warmup / checkout / doctor / session-watch) | `crates/devbox-agent/src/` |
| Server library root | `crates/devbox-server/src/lib.rs` |
| HTTP route handlers | `crates/devbox-server/src/routes.rs` (HTTP layer), `crates/devbox-server/src/service.rs` (domain logic) |
| UI (HTML dashboard) routes | `crates/devbox-server/src/ui.rs` |
| Pool reconciliation (loop, tick, config, lock) | `crates/devbox-server/src/reconcile/` |
| Compute trait (ASG + instance ops) | `crates/devbox-server/src/compute/mod.rs` |
| Compute AWS impl + test mock | `crates/devbox-server/src/compute/ec2.rs`, `crates/devbox-server/src/compute/mock.rs` |
| Leader-lock document | `crates/devbox-server/src/documents/leader_lock.rs` |
| AWS type conversions | `crates/devbox-server/src/convert.rs` |
| Database module root | `crates/devbox-server/src/db/mod.rs` |
| Pool enum + macros | `crates/devbox-server/src/db/pool.rs` |
| DocumentStore (generic CRUD) | `crates/devbox-server/src/db/store.rs` |
| DocumentType trait | `crates/devbox-server/src/db/document_type.rs` |
| DSQL endpoint parsing + IAM auth | `crates/devbox-server/src/db/dsql.rs` |
| Migration runner | `crates/devbox-server/src/db/migrations.rs` |
| Database tests | `crates/devbox-server/src/db/tests.rs` |
| DevboxDoc document type | `crates/devbox-server/src/documents/devbox.rs` |
| SessionDoc document type | `crates/devbox-server/src/documents/session.rs` |
| Session archive pack/restore | `crates/devbox-agent/src/session.rs` |
| Session-watch service (archives on release --keep) | `crates/devbox-agent/src/session_watch.rs` |
| Presigned URL access to session bucket | `crates/devbox-server/src/sessions.rs` |
| SQLite migrations | `crates/devbox-server/migrations/sqlite/` |
| Postgres migrations | `crates/devbox-server/migrations/postgres/` |
| HTML templates | `crates/devbox-server/templates/` |
| CSS source (TailwindCSS input) | `crates/devbox-server/styles/input.css` |
| Static assets (compiled CSS) | `crates/devbox-server/static/css/` |
| TailwindCSS config | `crates/devbox-server/tailwind.config.js` |
| Docker config | `Dockerfile`, `Dockerfile.build`, `docker-bake.hcl` |
| Dependency audit | `deny.toml` |
| Makefile | `Makefile` |
| Workspace config | `Cargo.toml` (root) |

## Adding New Components

### New Document Type
1. Create file in `crates/devbox-server/src/documents/`
2. Define struct and implement `DocumentType` trait
3. Add module to `crates/devbox-server/src/documents/mod.rs`
4. Add tests for serde roundtrip and index entries

### New API Endpoint
1. Add request/response types to `crates/devbox-common/src/lib.rs`
2. Add domain logic function in `crates/devbox-server/src/service.rs`
3. Add HTTP handler in `crates/devbox-server/src/routes.rs` and register in `build_router()`
4. Add CLI subcommand in `crates/devbox-cli/src/main.rs` and handler in `crates/devbox-cli/src/command.rs`

### New Database Migration
1. Add `.sql` file in `crates/devbox-server/migrations/sqlite/` with next sequence number
2. Add corresponding `.sql` file in `crates/devbox-server/migrations/postgres/`
3. Keep DDL compatible between SQLite and Postgres

### New Compute Operation
1. Add method to the `Compute` trait in `crates/devbox-server/src/compute/mod.rs`
2. Implement it in the AWS client (`compute/ec2.rs`) and the test mock (`compute/mock.rs`)
3. Call from the reconciliation tick (`reconcile/tick.rs`) or a route handler
