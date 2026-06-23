# devbox-common

Shared types for [devbox](../../README.md), used by both `devbox-server` and
`devbox-cli` so the API wire format stays in sync. Pure library — no I/O.

## Contents

- **Identifiers** — `DevboxId` (UUIDv7) and strongly-typed newtypes over AWS ids:
  `InstanceType`, `AmiId`, `SubnetId`, `SecurityGroupId`.
- **`DevboxState`** — the lifecycle enum: `Launching → Warming → Ready → Claimed
  → Terminating`.
- **API request/response types** — `ClaimRequest`, `DevboxResponse`,
  `DevboxListResponse`, `HealthResponse`, `PoolMetricsResponse`,
  `ProtectedResourceMetadata`. Release takes no body — the owner is the
  authenticated principal, so there is no `ReleaseRequest`.
- **Config structs** — `ServerConfig`, `DatabaseConfig`.

All types derive `serde::{Serialize, Deserialize}`; the AWS-id newtypes are
`#[serde(transparent)]`, so they serialize as plain strings. Dependencies are
`serde` + `uuid` only.
