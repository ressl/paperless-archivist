# Stability And Support Policy

Status: v1.0 GA contract

This document defines what Paperless Archivist treats as stable for the v1.0
series. It is written for operators, contributors, packagers, and AI agents.

## Supported Deployment Modes

The v1.0 series supports these deployment modes:

| Mode | Status | Notes |
| --- | --- | --- |
| Docker Compose with bundled PostgreSQL 18 | Supported | Best local and small-server path. |
| Docker Compose with external PostgreSQL 18 | Supported | Recommended when PostgreSQL is already operated separately. |
| Generic Kubernetes package | Supported baseline | Public-safe Kustomize package intended to be patched by the operator. |
| Local development with Cargo and Vite | Supported for contributors | Requires a reachable PostgreSQL 18 database. |
| Direct binary deployment | Best effort | Use the same environment variables and run API plus worker separately. |

Unsupported:

- PostgreSQL versions older than 18.
- Direct writes to the Paperless-ngx database.
- Frontend-to-Paperless or frontend-to-provider integrations.
- Running workers against a schema that has not been migrated by the API.

## Database Versions

PostgreSQL 18 or newer is required. The first migration checks
`server_version_num` and fails early on older servers. The schema uses
PostgreSQL UUID v7 defaults and indexes tuned for dashboard, queue, inventory,
chat, audit, and Paperless sync queries.

## Upgrade Policy

Patch releases in the v1.0 series may add:

- backward-compatible settings fields
- new optional API response fields
- new migrations
- new model catalog entries
- documentation and packaging improvements

Patch releases must not:

- remove an existing public API endpoint without a deprecation period
- rename existing settings fields
- change the meaning of existing roles or scopes without migration notes
- require direct Paperless database access
- expose secret values through API responses, logs, or audit metadata

Minor releases may introduce larger feature changes, but should preserve
existing documented deployment and upgrade paths.

## API Compatibility

The OpenAPI document in `openapi/openapi.yaml` is the frontend/backend contract.
For v1.0.x:

- existing request fields remain accepted unless explicitly deprecated
- new request fields are optional by default
- response objects may gain new fields
- enum additions are allowed when clients can ignore unknown values
- browser-only session endpoints continue to require CSRF

Generated frontend types must be regenerated after OpenAPI changes.

## Settings Compatibility

Runtime settings are stored under the `runtime` settings key. New settings must
be normalized in Rust so older installations get safe defaults after upgrade.
Secret values must be stored as secret references, never as plain runtime
settings.

## Security Support

Security reports should include:

- affected version or commit
- deployment mode
- whether the issue requires authenticated access
- whether document content, credentials, audit data, or Paperless metadata can
  be exposed or modified
- reproduction steps without real secrets

Security fixes take priority over feature work. Public documentation and issue
comments must avoid real secret values, private hostnames, credential URLs, and
private deployment topology.

## Release Checklist

Before tagging a v1.0.x release:

- `cargo fmt --all -- --check`
- `cargo test --workspace --locked`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `npm --prefix frontend run generate:client`
- `npm --prefix frontend run accessibility:check`
- `npm --prefix frontend run typecheck`
- `npm --prefix frontend run build`
- `docker compose --env-file deploy/compose/.env.example -f deploy/compose/docker-compose.yml config`
- `kubectl kustomize deploy/kubernetes/base`
- public-boundary scan for tree, history, and tags
- fresh public clone verification after export
- update release notes and migration notes
