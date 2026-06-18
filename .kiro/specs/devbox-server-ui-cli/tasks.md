# Implementation Plan: Devbox Server UI & CLI

## Overview

This plan implements the devbox server API handlers (wiring routes to the DocumentStore), dashboard UI templates with detail and action pages, CLI output formatting, and supporting modules (error handling, conversion). Tasks are ordered so that foundational modules (error, conversion) come first, followed by API handlers, then UI templates, and finally CLI formatting. Property-based tests use the `proptest` crate.

## Tasks

- [x] 1. Create foundational modules (error and conversion)
  - [x] 1.1 Create the error module (`src/error.rs`)
    - Create `devbox-server/src/error.rs` with the `AppError` enum (BadRequest, Forbidden, NotFound, Conflict, Internal) and `ErrorBody` struct
    - Implement `IntoResponse` for `AppError` producing JSON `{ "error": "..." }` with correct HTTP status codes
    - Implement `From<anyhow::Error>` for `AppError` converting to `Internal`
    - Register the module in `src/lib.rs` with `pub mod error;`
    - _Requirements: 9.1, 9.2, 9.3, 9.4, 9.5, 9.6_

  - [x] 1.2 Create the conversion module (`src/convert.rs`)
    - Create `devbox-server/src/convert.rs` with `impl From<Document<DevboxDoc>> for DevboxResponse`
    - Map fields: doc.id → id, doc.data.instance_id → instance_id, doc.data.state → state, doc.data.instance_type → instance_type, doc.data.ami_id → ami_id, doc.data.owner → owner, doc.created_at (RFC 3339) → created_at, doc.data.claimed_at → claimed_at
    - Register the module in `src/lib.rs` with `pub mod convert;`
    - _Requirements: 10.1, 10.2_

  - [x] 1.3 Create the custom `JsonBody<T>` extractor
    - Add a `JsonBody<T>` struct in `src/error.rs` (or a new `src/extractors.rs`) that wraps `axum::Json<T>` but converts rejection to `AppError::BadRequest` instead of 422
    - Implement `FromRequest` for `JsonBody<T>`
    - _Requirements: 9.2_

  - [ ]* 1.4 Write property tests for AppError (Property 15)
    - **Property 15: All error responses have JSON "error" field**
    - Test that each `AppError` variant produces correct status code and JSON body with non-empty `error` field
    - **Validates: Requirements 9.1**

  - [ ]* 1.5 Write property tests for conversion (Properties 16, 17)
    - **Property 16: Document-to-DevboxResponse field mapping**
    - **Property 17: DevboxResponse serialization round-trip**
    - Add `proptest` as a dev-dependency to `devbox-server`
    - Create test generators (`prop_compose!`) for `DevboxDoc` and `Document<DevboxDoc>`
    - **Validates: Requirements 10.1, 10.2**

- [x] 2. Checkpoint - Ensure foundational modules compile
  - Ensure all tests pass, ask the user if questions arise.

- [x] 3. Implement API route handlers
  - [x] 3.1 Implement `list_devboxes` handler
    - Replace the placeholder in `src/routes.rs` to call `state.store.list_all::<DevboxDoc>()` and convert results via `DevboxResponse::from`
    - Change return type to `Result<Json<DevboxListResponse>, AppError>`
    - _Requirements: 1.1, 1.2, 1.3_

  - [x] 3.2 Implement `get_devbox` handler
    - Replace the placeholder to call `state.store.get::<DevboxDoc>(&id)` and return 404 via `AppError::NotFound` if absent
    - Change return type to `Result<Json<DevboxResponse>, AppError>`
    - _Requirements: 2.1, 2.2, 2.3_

  - [x] 3.3 Implement `claim_devbox` handler
    - Replace the placeholder with the optimistic concurrency loop: query `find_all("state", "ready")`, sort by preferred instance_type, loop with `compare_and_update`
    - Validate that `owner` is non-empty (return `AppError::BadRequest`)
    - Return 409 via `AppError::Conflict` when no devboxes available
    - Use `JsonBody<ClaimRequest>` instead of `Json<ClaimRequest>` for 400 on malformed bodies
    - _Requirements: 3.1, 3.2, 3.3, 3.4, 3.5_

  - [x] 3.4 Implement `release_devbox` handler
    - Replace the placeholder: verify devbox exists (404), is Claimed (409), owner matches (403), then update state to Terminating and clear owner
    - Use `JsonBody<ReleaseRequest>` instead of `Json<ReleaseRequest>`
    - _Requirements: 4.1, 4.2, 4.3, 4.4_

  - [ ]* 3.5 Write property tests for list and get (Properties 1, 2)
    - **Property 1: List returns all inserted documents**
    - **Property 2: Get by ID returns the correct document**
    - Use in-memory SQLite DocumentStore for testing
    - **Validates: Requirements 1.1, 2.1**

  - [ ]* 3.6 Write property tests for claim (Properties 3, 4)
    - **Property 3: Claim transitions Ready to Claimed with correct fields**
    - **Property 4: Claim prefers matching instance type**
    - **Validates: Requirements 3.1, 3.3**

  - [ ]* 3.7 Write property tests for release (Properties 5, 6, 7)
    - **Property 5: Release transitions Claimed to Terminating**
    - **Property 6: Release rejects non-Claimed state**
    - **Property 7: Release rejects wrong owner**
    - **Validates: Requirements 4.1, 4.3, 4.4**

- [x] 4. Checkpoint - Ensure API handlers work
  - Ensure all tests pass, ask the user if questions arise.

- [x] 5. Implement dashboard UI templates and handlers
  - [x] 5.1 Update `DashboardTemplate` and `dashboard` handler for real data
    - Add `error: Option<String>` field to `DashboardTemplate`
    - Update the `dashboard` handler to query `store.list_all::<DevboxDoc>()` and populate `DashboardDevbox` entries from real documents
    - Render error inline if store query fails
    - Update `templates/index.html` to show error banner if `error` is Some, and make rows clickable (link to `/devboxes/{id}`)
    - _Requirements: 5.1, 5.2, 5.3, 5.4_

  - [x] 5.2 Create `DevboxDetailTemplate` and `devbox_detail` handler
    - Add `DevboxDetail` struct with all fields (id, state, instance_type, ami_id, subnet_id, instance_id, ebs_volume_id, owner, claimed_at, created_at)
    - Add `impl From<Document<DevboxDoc>> for DevboxDetail`
    - Create `templates/detail.html` showing all fields, a back-link to `/`, and conditional "Release" button (if Claimed) or "Claim" link (if Ready)
    - Register route `GET /devboxes/{id}` in `build_ui_router()`
    - _Requirements: 6.1, 6.2, 6.3, 6.4, 6.5_

  - [x] 5.3 Create `ClaimFormTemplate` and claim/release UI handlers
    - Create `templates/claim_form.html` with a form (owner field, optional instance_type)
    - Create `templates/error.html` for generic error display
    - Implement `claim_form` handler (GET `/devboxes/claim`) rendering the form
    - Implement `submit_claim` handler (POST `/devboxes/claim`) that calls the API claim endpoint internally and redirects to detail on success or re-renders form with error on failure
    - Implement `submit_release` handler (POST `/devboxes/{id}/release`) that calls release and redirects to detail
    - Register routes in `build_ui_router()`
    - _Requirements: 7.1, 7.2, 7.3_

  - [ ]* 5.4 Write property tests for dashboard and detail templates (Properties 8, 9, 10)
    - **Property 8: Dashboard renders all devbox fields with correct state CSS class**
    - **Property 9: Detail page renders all document fields**
    - **Property 10: Detail page shows correct action button per state**
    - Render templates directly with `askama::Template::render()` and assert on HTML content
    - **Validates: Requirements 5.1, 5.3, 6.1, 6.4, 6.5**

- [x] 6. Checkpoint - Ensure UI templates render correctly
  - Ensure all tests pass, ask the user if questions arise.

- [x] 7. Implement CLI output formatter
  - [x] 7.1 Create CLI format module (`devbox-cli/src/format.rs`)
    - Create `format.rs` with functions: `format_list_table`, `format_status`, `format_claim_success`, `format_release_success`
    - Implement column-aligned table output for list with truncation (ID ≤ 8 chars, instance_id ≤ 19 chars)
    - Implement labeled key-value output for status
    - Implement claim success output with SSM command hint
    - Implement release confirmation output
    - Declare `mod format;` in `devbox-cli/src/main.rs`
    - _Requirements: 8.1, 8.2, 8.3, 8.4_

  - [x] 7.2 Update CLI `main.rs` to use format module
    - Replace inline `println!` calls in each command branch with calls to the format module functions
    - Write error messages to stderr (`eprintln!`) and exit with non-zero code on failure
    - _Requirements: 8.1, 8.2, 8.3, 8.4, 8.5_

  - [ ]* 7.3 Write property tests for CLI formatting (Properties 11, 12, 13, 14)
    - **Property 11: CLI list format includes columns with correct truncation**
    - **Property 12: CLI status format includes all labeled fields**
    - **Property 13: CLI claim format includes devbox info and SSM command**
    - **Property 14: CLI release format includes ID and state**
    - Add `proptest` as a dev-dependency to `devbox-cli`
    - **Validates: Requirements 8.1, 8.2, 8.3, 8.4**

- [x] 8. Final checkpoint - Ensure all tests pass
  - Ensure all tests pass, ask the user if questions arise.

## Notes

- Tasks marked with `*` are optional and can be skipped for faster MVP
- Each task references specific requirements for traceability
- Checkpoints ensure incremental validation
- Property tests validate universal correctness properties from the design document
- Unit tests validate specific examples and edge cases
- The `proptest` crate is used for property-based testing in both `devbox-server` and `devbox-cli`
- All API handlers use `AppError` for uniform error responses
- The `JsonBody<T>` extractor ensures malformed JSON returns 400 instead of Axum's default 422

## Task Dependency Graph

```json
{
  "waves": [
    { "id": 0, "tasks": ["1.1", "1.2"] },
    { "id": 1, "tasks": ["1.3", "1.4", "1.5"] },
    { "id": 2, "tasks": ["3.1", "3.2"] },
    { "id": 3, "tasks": ["3.3", "3.4"] },
    { "id": 4, "tasks": ["3.5", "3.6", "3.7", "5.1"] },
    { "id": 5, "tasks": ["5.2", "7.1"] },
    { "id": 6, "tasks": ["5.3", "7.2"] },
    { "id": 7, "tasks": ["5.4", "7.3"] }
  ]
}
```
