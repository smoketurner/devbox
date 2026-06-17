# Devbox HTTP API

**Status:** Draft

## Overview

The devbox server exposes a RESTful HTTP API for managing devbox instances. Agents and operators interact with the pool through these endpoints to claim, release, list, and inspect devboxes.

## Motivation

Coding agents need a simple, reliable API to acquire development environments. The API must support:
- Fast claim operations (sub-second when warm pool is available)
- Idempotent release operations (agents may crash and retry)
- List/filter for operational visibility

## Requirements

### Functional

1. **Health check** - `GET /health` returns server and database health
2. **Claim** - `POST /api/v1/devboxes/claim` atomically assigns a Ready devbox to a requester
3. **Release** - `POST /api/v1/devboxes/{id}/release` returns a Claimed devbox to the pool (terminates it)
4. **List** - `GET /api/v1/devboxes` returns all devboxes with optional state filter
5. **Get** - `GET /api/v1/devboxes/{id}` returns a single devbox by ID
6. **Dashboard** - `GET /` serves an HTML dashboard showing pool status

### Non-Functional

1. **Latency** - Claim must complete in < 100ms when warm pool is available
2. **Consistency** - Claims must be atomic (no double-claims)
3. **Idempotency** - Releasing an already-released devbox returns success
4. **Content type** - API endpoints return `application/json`
5. **Error responses** - Use standard HTTP status codes with JSON error bodies

## API Contracts

### GET /health

**Response: 200 OK**
```json
{
  "status": "ok",
  "database": "healthy"
}
```

### POST /api/v1/devboxes/claim

**Request:**
```json
{
  "owner": "agent-abc123",
  "instance_type": "m5.large"  // optional preference
}
```

**Response: 200 OK**
```json
{
  "id": "01914a6b-...",
  "instance_id": "i-0abc123def456",
  "state": "claimed",
  "instance_type": "m5.large",
  "ami_id": "ami-0123456789abcdef0",
  "owner": "agent-abc123",
  "created_at": "2024-12-01T10:00:00Z",
  "claimed_at": "2024-12-01T12:30:00Z"
}
```

**Response: 404 Not Found** (no available instances)
```json
{
  "error": "no_available_devbox",
  "message": "No Ready devbox instances available in the pool"
}
```

### POST /api/v1/devboxes/{id}/release

**Request:**
```json
{
  "owner": "agent-abc123"
}
```

**Response: 200 OK**
```json
{
  "id": "01914a6b-...",
  "instance_id": "i-0abc123def456",
  "state": "terminating",
  "instance_type": "m5.large",
  "ami_id": "ami-0123456789abcdef0",
  "owner": null,
  "created_at": "2024-12-01T10:00:00Z",
  "claimed_at": null
}
```

**Response: 403 Forbidden** (owner mismatch)
```json
{
  "error": "not_owner",
  "message": "Only the current owner can release this devbox"
}
```

### GET /api/v1/devboxes

**Query parameters:**
- `state` (optional): Filter by state (`launching`, `warming`, `ready`, `claimed`, `terminating`)
- `owner` (optional): Filter by owner

**Response: 200 OK**
```json
{
  "devboxes": [
    {
      "id": "01914a6b-...",
      "instance_id": "i-0abc123def456",
      "state": "ready",
      "instance_type": "m5.large",
      "ami_id": "ami-0123456789abcdef0",
      "owner": null,
      "created_at": "2024-12-01T10:00:00Z",
      "claimed_at": null
    }
  ]
}
```

### GET /api/v1/devboxes/{id}

**Response: 200 OK** (same shape as single devbox above)

**Response: 404 Not Found**
```json
{
  "error": "not_found",
  "message": "Devbox not found"
}
```

## Design

### Application State

```rust
pub struct AppState {
    pub store: Arc<DocumentStore>,
}
```

### Router Structure

```rust
Router::new()
    .route("/health", get(health_check))
    .route("/api/v1/devboxes", get(list_devboxes))
    .route("/api/v1/devboxes/{id}", get(get_devbox))
    .route("/api/v1/devboxes/claim", post(claim_devbox))
    .route("/api/v1/devboxes/{id}/release", post(release_devbox))
    .merge(build_ui_router())
    .with_state(state)
```

### Error Handling

Route handlers return `Result<Json<T>, StatusCode>` for simple cases. Future iterations will add a structured error response type.

## Open Questions

1. Should authentication be IAM Signature V4, bearer tokens, or both?
2. Should the API support websocket connections for real-time pool status?
3. Should there be an admin API for force-terminating instances?
4. How should rate limiting be implemented? (per-owner? global?)
