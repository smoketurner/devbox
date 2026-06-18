# Requirements Document

## Introduction

This feature fleshes out the devbox orchestration service from its current placeholder state into working functionality across three components: the server API handlers (wiring routes to the DocumentStore for CRUD on devboxes), the TailwindCSS/Askama web dashboard (showing real data, detail views, and in-browser claim/release actions), and the CLI (formatted output and improved UX). The pool reconciliation loop and EC2 client implementation are out of scope for this feature.

## Glossary

- **Server**: The devbox-server Axum HTTP application
- **Dashboard**: The TailwindCSS/Askama web UI served at the root path
- **CLI**: The devbox-cli command-line binary
- **DocumentStore**: The database abstraction layer providing typed CRUD over devbox documents
- **DevboxDoc**: The document type representing a devbox instance in the store
- **Devbox**: A managed development environment instance record
- **Pool**: The set of all devbox instances managed by the service
- **Owner**: A string identifier representing the user or agent that has claimed a devbox

## Requirements

### Requirement 1: List Devboxes API

**User Story:** As an API consumer, I want to list all devboxes in the pool, so that I can see the current state of all managed instances.

#### Acceptance Criteria

1. WHEN a GET request is received at `/api/v1/devboxes`, THE Server SHALL query the DocumentStore for all DevboxDoc documents and return them as a JSON array in the DevboxListResponse format with HTTP status 200.
2. WHEN the DocumentStore contains no devbox documents, THE Server SHALL return an empty devboxes array with HTTP status 200.
3. IF the DocumentStore query fails, THEN THE Server SHALL return HTTP status 500 with a JSON error message describing the failure.

### Requirement 2: Get Devbox by ID API

**User Story:** As an API consumer, I want to retrieve a single devbox by its ID, so that I can inspect its current state and metadata.

#### Acceptance Criteria

1. WHEN a GET request is received at `/api/v1/devboxes/{id}` with a valid devbox ID, THE Server SHALL return the matching DevboxDoc as a DevboxResponse JSON with HTTP status 200.
2. WHEN a GET request is received at `/api/v1/devboxes/{id}` with an ID that does not exist in the DocumentStore, THE Server SHALL return HTTP status 404 with a JSON error message.
3. IF the DocumentStore query fails, THEN THE Server SHALL return HTTP status 500 with a JSON error message.

### Requirement 3: Claim Devbox API

**User Story:** As an API consumer, I want to claim an available devbox from the pool, so that I get exclusive access to a development environment.

#### Acceptance Criteria

1. WHEN a POST request is received at `/api/v1/devboxes/claim` with a valid ClaimRequest containing an owner field, THE Server SHALL find a devbox in the Ready state, transition it to the Claimed state, set the owner field, record the claimed_at timestamp, persist the update, and return the updated DevboxResponse with HTTP status 200.
2. WHEN a POST request is received at `/api/v1/devboxes/claim` and no devbox is in the Ready state, THE Server SHALL return HTTP status 409 with a JSON error message indicating no devboxes are available.
3. WHEN a POST request is received at `/api/v1/devboxes/claim` with an optional instance_type preference, THE Server SHALL prefer a Ready devbox matching that instance type, falling back to any Ready devbox if no match exists.
4. THE Server SHALL use optimistic concurrency (compare-and-update) when transitioning the devbox state to prevent two concurrent claims from succeeding on the same devbox.
5. IF the compare-and-update fails due to a version conflict, THEN THE Server SHALL retry by selecting the next available Ready devbox.

### Requirement 4: Release Devbox API

**User Story:** As an API consumer, I want to release a devbox I previously claimed, so that it can be terminated and replaced.

#### Acceptance Criteria

1. WHEN a POST request is received at `/api/v1/devboxes/{id}/release` with a valid ReleaseRequest, THE Server SHALL verify the devbox exists and is in the Claimed state, transition it to the Terminating state, clear the owner field, persist the update, and return the updated DevboxResponse with HTTP status 200.
2. WHEN a release request is received for a devbox that does not exist, THE Server SHALL return HTTP status 404 with a JSON error message.
3. WHEN a release request is received for a devbox that is not in the Claimed state, THE Server SHALL return HTTP status 409 with a JSON error message indicating the devbox cannot be released from its current state.
4. WHEN a release request is received with an owner field that does not match the current owner of the devbox, THE Server SHALL return HTTP status 403 with a JSON error message indicating ownership mismatch.

### Requirement 5: Dashboard Devbox List View

**User Story:** As a platform operator, I want to view all devboxes on the web dashboard, so that I can monitor pool health at a glance.

#### Acceptance Criteria

1. WHEN a GET request is received at `/`, THE Dashboard SHALL query the DocumentStore for all DevboxDoc documents and render them in an HTML table showing ID, state, instance type, instance ID, owner, and creation time.
2. WHEN the DocumentStore contains no devbox documents, THE Dashboard SHALL display a message indicating no devboxes are found.
3. THE Dashboard SHALL apply a color-coded CSS class to each devbox state value (launching, warming, ready, claimed, terminating) for visual distinction.
4. IF the DocumentStore query fails, THEN THE Dashboard SHALL render an error message within the page body.

### Requirement 6: Dashboard Devbox Detail View

**User Story:** As a platform operator, I want to click on a devbox in the dashboard to see its full details, so that I can inspect all metadata for a specific instance.

#### Acceptance Criteria

1. WHEN a GET request is received at `/devboxes/{id}`, THE Dashboard SHALL query the DocumentStore for the specified DevboxDoc and render a detail page showing all fields: ID, state, instance type, AMI ID, subnet ID, instance ID, EBS volume ID, owner, claimed_at, and created_at.
2. WHEN a GET request is received at `/devboxes/{id}` with an ID that does not exist, THE Dashboard SHALL render a 404 page with a message indicating the devbox was not found.
3. THE Dashboard SHALL provide a link back to the main devbox list from the detail page.
4. WHILE the devbox is in the Claimed state, THE Dashboard SHALL display a "Release" button on the detail page that triggers a release action.
5. WHILE the devbox is in the Ready state, THE Dashboard SHALL display a "Claim" button on the detail page that navigates to a claim form.

### Requirement 7: Dashboard Claim and Release Actions

**User Story:** As a platform operator, I want to claim and release devboxes directly from the dashboard, so that I can manage the pool without using the CLI.

#### Acceptance Criteria

1. WHEN the operator submits the claim form on the Dashboard, THE Dashboard SHALL POST to `/api/v1/devboxes/claim` with the provided owner value and redirect to the detail page of the claimed devbox on success.
2. WHEN the operator clicks the "Release" button on a devbox detail page, THE Dashboard SHALL POST to `/api/v1/devboxes/{id}/release` with the current owner and redirect to the detail page showing the updated state.
3. IF a claim or release action fails, THEN THE Dashboard SHALL display the error message returned by the API on the same page.

### Requirement 8: CLI Formatted Output

**User Story:** As a CLI user, I want nicely formatted output from devbox commands, so that I can quickly scan devbox information.

#### Acceptance Criteria

1. WHEN the `list` command succeeds, THE CLI SHALL display devboxes in a formatted table with columns for ID (truncated to 8 characters), state, instance type, instance ID (truncated to 19 characters), and owner.
2. WHEN the `status` command succeeds, THE CLI SHALL display all devbox fields in a labeled key-value format with aligned values.
3. WHEN the `claim` command succeeds, THE CLI SHALL display the claimed devbox ID, instance ID, instance type, and a suggested SSM connection command.
4. WHEN the `release` command succeeds, THE CLI SHALL display a confirmation message with the released devbox ID and its new state.
5. IF any command fails with a non-success HTTP status, THEN THE CLI SHALL display the HTTP status code and error message from the response body to standard error and exit with a non-zero status code.

### Requirement 9: API Error Response Format

**User Story:** As an API consumer, I want consistent error responses, so that I can programmatically handle failures.

#### Acceptance Criteria

1. THE Server SHALL return all error responses as JSON objects with an `error` field containing a human-readable message string.
2. THE Server SHALL use HTTP status 400 for malformed request bodies.
3. THE Server SHALL use HTTP status 404 for resources that do not exist.
4. THE Server SHALL use HTTP status 409 for state conflict errors (no available devboxes, invalid state transitions).
5. THE Server SHALL use HTTP status 403 for ownership verification failures.
6. THE Server SHALL use HTTP status 500 for internal errors (database failures, serialization errors).

### Requirement 10: DevboxDoc to DevboxResponse Conversion

**User Story:** As a developer, I want a consistent mapping from internal document representation to API response format, so that all endpoints return uniform data.

#### Acceptance Criteria

1. THE Server SHALL convert each Document<DevboxDoc> to a DevboxResponse by mapping: document ID to `id`, DevboxDoc.instance_id to `instance_id`, DevboxDoc.state to `state`, DevboxDoc.instance_type to `instance_type`, DevboxDoc.ami_id to `ami_id`, DevboxDoc.owner to `owner`, document.created_at formatted as RFC 3339 to `created_at`, and DevboxDoc.claimed_at formatted as RFC 3339 to `claimed_at`.
2. FOR ALL Document<DevboxDoc> values, converting to DevboxResponse and serializing to JSON then deserializing back SHALL produce a valid DevboxResponse with all fields preserved (round-trip property).
