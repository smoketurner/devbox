# Code Conventions

## Strict No-Panic Policy

The workspace enforces panic-free code via clippy lints in `Cargo.toml`. These are **denied** (not warned):

### Explicit panics
- `unwrap_used`, `expect_used`, `panic`, `unreachable`, `todo`, `unimplemented`, `exit`

### Indexing
- `indexing_slicing`, `string_slice` -- use `.get()` instead of `[]`

### Arithmetic
- `arithmetic_side_effects`, `integer_division`, `modulo_arithmetic`
- Use `checked_*`, `saturating_*`, or `wrapping_*` methods explicitly

### Numeric casts
- `cast_possible_truncation`, `cast_sign_loss`, `cast_possible_wrap`, `cast_precision_loss`, `checked_conversions`
- Use `try_from`, `try_into`, or checked conversions -- never `as` for numeric casts

### Safety
- `unsafe_code` denied at the Rust lint level
- `await_holding_lock`, `large_futures`, `mem_forget` denied

Test modules use `#[expect(clippy::unwrap_used, reason = "...")]` to allow unwrap/expect in test code.

## Error Handling

```rust
// Use anyhow::Result for application errors
pub async fn claim_devbox(store: &DocumentStore, req: ClaimRequest) -> Result<DevboxDoc> { ... }

// Propagate with ?
let doc = store.get::<DevboxDoc>(&id).await?;

// Return explicit HTTP status codes in route handlers
async fn get_devbox(...) -> Result<Json<DevboxResponse>, StatusCode> { ... }
```

## Construction Patterns

```rust
// Prefer struct literals for data types
let doc = DevboxDoc {
    instance_id: None,
    state: DevboxState::Launching,
    instance_type: "m5.large".to_string(),
    ..
};

// Prefer builders for complex configuration
let pool = Pool::connect(&database_url).await?;
```

## Documentation

Document public APIs with:
- Brief description
- `# Errors` section listing error conditions

```rust
/// Launch a new EC2 instance.
///
/// # Errors
///
/// Returns an error if the AWS API call fails or instance type is invalid.
fn launch_instance(&self, instance_type: &str, ami_id: &str, subnet_id: &str) -> Result<String>;
```

## Test Conventions

- Use `#[expect(clippy::unwrap_used, reason = "...")]` on test modules
- Use in-memory SQLite for database tests: `Pool::connect("sqlite::memory:").await?`
- Name tests descriptively: `test_devbox_doc_index_entries_with_owner`
- Keep tests focused on one behavior each

## Formatting and Linting

```bash
make fmt    # cargo fmt --all
make lint   # cargo clippy --all-targets --all-features -- -D warnings
```

Configuration: edition 2024, max width 100, Unix newlines.

## Commit Messages

Use conventional commit prefixes:

```
feat: add pool reconciliation with configurable target size
fix: handle DSQL retry on OC001 error code
chore: update aws-sdk-dsql to 1.60.0
docs: add architecture diagram to README
refactor: extract EC2 client into trait
```
