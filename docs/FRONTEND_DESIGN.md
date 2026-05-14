# Frontend Design

Status: draft  
Decision: React + TypeScript frontend

## 1. Purpose

Paperless Archivist needs a rich operational UI:

- backlog dashboard
- review queue with diffs
- batch processing controls
- prompt editor and prompt testing
- model/provider configuration
- workflow tag configuration
- run/job timelines
- user and role management
- audit log exploration
- future document chat/RAG

This complexity is better served by a React + TypeScript frontend than by
server-rendered HTML alone.

## 2. Architecture

```text
frontend/ React + TypeScript
        |
        | generated OpenAPI client
        v
crates/archivist-api Rust/Axum API
        |
        v
PostgreSQL 18 / Paperless / AI providers
```

Rules:

- Frontend calls only Archivist `/api/*` endpoints.
- Frontend never talks directly to Paperless-ngx.
- Frontend never talks directly to Ollama, OpenAI, Claude, or other providers.
- Frontend validation is for user experience only.
- Backend Rust validation is authoritative.
- Security decisions are enforced by the backend.

## 3. Recommended Stack

Required:

- React
- TypeScript
- Vite
- pnpm
- generated OpenAPI client

Recommended:

- TanStack Query for API cache and server state
- TanStack Table for backlog, audit, runs, jobs
- React Hook Form for forms
- Zod for client-side schema validation
- React Router or TanStack Router for routing
- Monaco editor later for advanced prompt editing if needed

Styling:

- start with plain CSS or a small utility approach
- avoid a heavy component framework until UI needs are clearer
- prioritize dense, operational, enterprise-style screens

## 4. Deployment Model

MVP:

- frontend is built during container build
- built static assets are copied into the Rust API image
- Rust API serves frontend assets and `/api/*`
- worker remains a separate Rust process/deployment

Optional later:

- separate static frontend deployment
- same OpenAPI contract
- same authentication/session model

## 5. OpenAPI Contract

The backend owns the API contract.

Requirements:

- generate OpenAPI from Rust routes/types or maintain checked OpenAPI spec
- generate TypeScript client from OpenAPI
- CI fails if frontend client is stale
- no handwritten duplicate API types where generated types exist

## 6. Authentication UI

Required screens:

- login
- logout
- change password
- user list
- user detail
- role assignment
- API token management
- session revocation

Frontend must support:

- cookie-based session auth
- CSRF tokens for state-changing requests
- clear expired-session handling
- no token storage in localStorage for normal browser sessions

## 7. Main Screens

### 7.1 Dashboard

Shows:

- total documents
- fully processed documents
- OCR missing
- tagging missing
- failed jobs
- waiting review
- currently running
- recent failures
- throughput over time

### 7.2 Backlog

Features:

- filter by missing stage
- filter by failure
- filter by tag/correspondent/document type
- bulk queue OCR
- bulk queue tagging
- bulk queue full pipeline
- retry failed subset
- export CSV/JSON

### 7.3 Review Queue

Features:

- current metadata vs suggested metadata
- diff view
- raw AI output view if allowed
- approve
- reject
- edit and approve
- retry with selected model/prompt

### 7.4 Prompt Editor

Features:

- list prompts by stage
- edit prompt
- version prompt
- activate prompt version
- test prompt against sample document
- compare outputs across prompt versions

### 7.5 Provider and Model Settings

Features:

- configure Ollama
- configure OpenAI
- configure Claude
- configure OpenAI-compatible endpoints
- test provider connection
- choose default text model
- choose default vision model
- override model per stage

### 7.6 Audit Log

Features:

- filter by actor
- filter by document
- filter by event type
- filter by time range
- inspect before/after JSON
- export

## 8. Security Requirements

- no direct provider API keys in frontend state after save
- no secrets in localStorage
- no full document text in browser logs
- CSP-compatible build
- no inline scripts unless nonce-based
- dependency scanning in CI
- generated source maps disabled or protected in production
- all state-changing calls include CSRF protection

## 9. Build and CI

Frontend CI:

- install with pnpm
- typecheck
- lint
- test
- build
- verify generated OpenAPI client is current

Container build:

1. build frontend assets with Node/pnpm
2. build Rust API/worker binaries
3. copy assets and binaries into minimal runtime image

## 10. Non-Goals for MVP

- offline-first UI
- mobile app
- complex drag-and-drop workflow builder
- direct Paperless document viewer replacement
- frontend-only business rules
