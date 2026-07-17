# Issue #354 Axum Router/OpenAPI Parity Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Keep every runtime Axum route discoverable in OpenAPI and prevent path or method drift in CI.

**Architecture:** A Node contract scanner discovers the local Axum `Router::new()` graph, follows `.nest(...)` and `.merge(...)` edges from the top-level app, applies their runtime prefixes, and compares every resulting method/path pair bidirectionally with the OpenAPI paths map. Missing endpoint contracts use closed reusable schemas and shared JSON error responses. A compile-time TypeScript assertion proves that the regenerated client exposes the newly documented routes.

**Tech Stack:** Axum/Rust source, OpenAPI 3.1, Node.js/YAML, openapi-typescript, TypeScript 6.

## Global Constraints

- Implement GitLab issue #354 only; endpoint runtime behavior remains unchanged.
- Derive the runtime route set from `crates/archivist-api/src/main.rs`, not from a manually duplicated manifest.
- Compare both directions so stale OpenAPI paths fail alongside undocumented runtime paths.
- Treat a route as internal only through an explicit source annotation that the verifier validates against a real runtime route.
- Document authentication, request bodies, successful responses, and relevant error responses for every newly added path.
- Keep generated client output deterministic and committed.

### Task 1: Establish a failing router/OpenAPI contract

**Files:**
- Add: `scripts/verify/router_openapi_contract.mjs`
- Modify: `frontend/package.json`
- Modify: `.gitlab-ci.yml`

- [x] Implement balanced source scanning that discovers and traverses the mounted router graph.
- [x] Parse OpenAPI YAML and compare normalized path/method pairs in both directions.
- [x] Demonstrate the current contract fails with the exact undocumented runtime routes.
- [x] Wire the contract into the frontend OpenAPI validation job.

### Task 2: Document every missing runtime route

**Files:**
- Modify: `openapi/openapi.yaml`

- [x] Add prompt experiments and inventory duplicate contracts.
- [x] Add selected and failed batch rerun contracts.
- [x] Add review auto-fix preview, bulk, and single-item contracts.
- [x] Add unblock, provider cooldown read/clear, and scheduled-retry release contracts.
- [x] Add the shared-secret Paperless webhook contract outside cookie/bearer security.
- [x] Define closed request/response/error schemas with correct required, nullable, bounds, UUID, date-time, and authentication semantics.

### Task 3: Regenerate and verify the client contract

**Files:**
- Modify: `frontend/src/api/schema.ts`
- Add: `frontend/src/api/schema.contract.ts`

- [x] Add compile-time assertions for every newly documented generated path/method.
- [x] Regenerate `schema.ts` and prove a second generation is diff-free.
- [x] Run route/settings contracts, TypeScript typecheck, frontend tests/build, Rust tests/format, and repository diff checks.
- [x] Obtain independent review, commit, push, and close #354 only after the MR pipeline is green.

### Task 4: Exercise the contract in the authoritative CI pipeline

**Files and settings:**
- Modify: `frontend/package.json`
- Configure: non-secret Node project-directory CI setting

- [x] Expose conventional lint and test scripts so generic Node CI executes the route, settings, generated-client, and runtime test gates.
- [x] Configure the nested frontend project directory outside the public source tree.
- [x] Verify the authoritative MR pipeline runs and passes the Node lint, typecheck, test, build, and audit jobs.
