# Dependency Management

## General Rules

- Add dependencies sparingly -- each is attack surface
- All workspace dependencies are declared in the root `Cargo.toml` under `[workspace.dependencies]`
- Pin to exact versions with `default-features = false`
- Crates reference them with `dep.workspace = true`

## Before Adding a Dependency

1. Check maintenance status
2. Review security advisories (`cargo audit`)
3. Consider size impact
4. Prefer pure Rust over C bindings when possible

## Preferred Crates

| Purpose | Use | Do NOT use |
|---------|-----|------------|
| Time | `jiff` | `chrono` |
| Crypto/TLS | `aws-lc-rs` + `rustls` | `ring`, OpenSSL |
| HTTP client | `reqwest` + `rustls` | OpenSSL |
| HTML templates | `askama` (compile-time checked) | Tera, Handlebars |
| Embedded assets | `rust-embed` | bundling via build.rs |
| CLI parsing | `clap` (derive) | structopt |
| Web framework | `axum` | actix-web, rocket |
| Serialization | `serde` + `serde_json` | |
| Async runtime | `tokio` | async-std |
| Error types | `anyhow` (application code) | |
| Database | `sqlx` (compile-time optional) | diesel, sea-orm |
| Query building | `sea-query` + `sea-query-sqlx` | raw SQL strings |
| AWS SDK | `aws-config` + `aws-sdk-*` | rusoto |
| Unique IDs | `uuid` (v7, time-ordered) | ulid, nanoid |
| Logging | `tracing` + `tracing-subscriber` | log, env_logger |
| Graceful shutdown | `tokio-util` (CancellationToken) | manual channels |

## Frontend (Server UI)

**Allowed:**
- TailwindCSS (standalone CLI, self-hosted output)
- Askama templates (compile-time checked HTML)
- Vanilla JavaScript (minimal interactivity)

**Not allowed:**
- React, Vue, Angular, Svelte
- Any npm/node.js runtime dependency
- External CDN links
- jQuery

Rationale: The server UI is a simple dashboard for pool status. Askama templates + TailwindCSS keeps it auditable and dependency-light.

## AWS SDK Crates

AWS SDK crates should use these features:
```toml
aws-config = { features = ["rt-tokio", "default-https-client", "behavior-version-latest"] }
aws-sdk-dsql = { features = ["rt-tokio", "default-https-client", "behavior-version-latest"] }
```
