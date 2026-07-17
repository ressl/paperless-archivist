# Issue #349 Session Listing Cookie Boundary Implementation Plan

**Goal:** Prevent every API-token principal from reading browser-session metadata while preserving the existing own-session and administrator views for interactive cookie sessions.

**Security contract:** `/api/auth/sessions` requires an authenticated cookie session before roles are evaluated. API-token scopes and the token owner's roles cannot satisfy this boundary. A cookie administrator lists all active sessions; any other cookie user lists only sessions owned by that user.

## Task 1: Add failing authorization coverage

- [x] Add a handler-policy test that rejects bearer-token contexts for multiple scope sets, including `users:manage`.
- [x] Assert the rejection is HTTP 403 and the serialized error body contains no session metadata.
- [x] Add positive tests for a regular cookie user and an administrator cookie session.
- [x] Assert the regular user filter is their own user id and the administrator filter remains unrestricted.

## Task 2: Enforce the interactive-session boundary

- [x] Require a cookie/user session before evaluating session-list visibility.
- [x] Preserve the existing own-session and administrator query behavior.
- [x] Keep session revocation bound to an interactive session.
- [x] Document the cookie-only boundary in OpenAPI and the API reference.

## Task 3: Verify compatibility and delivery

- [x] Run focused API tests, formatting, Clippy, workspace tests, audit, deny, and migration smoke.
- [x] Obtain independent review, commit, push, inspect the MR pipeline, document evidence, and close #349 only after the remote gate is green.
