# Security Guide

This guide explains the practical security model for operators and reviewers.
For vulnerability reporting, see [`../SECURITY.md`](../SECURITY.md). For design
details, see [`SECURITY_DESIGN.md`](SECURITY_DESIGN.md).

## Trust Boundaries

```text
Browser -> Archivist API -> Paperless REST API
                       -> Model providers
                       -> PostgreSQL 18
```

The browser only talks to Archivist. Paperless, Ollama, OpenAI, Anthropic, and
OpenAI-compatible providers are backend-only integrations.

## Paperless Boundary

Archivist never writes directly to the Paperless database. All document
metadata writes use the Paperless REST API. This keeps Paperless as the system
of record and avoids bypassing Paperless permissions, history, and validation.

## Secrets

Paperless tokens, provider API keys, webhook URLs, and OIDC secrets are stored
as secret references. UI-entered secrets are encrypted with
`ARCHIVIST_SECRET_KEY`. Secret values are not returned to the frontend after
save.

Operational rule: back up `ARCHIVIST_SECRET_KEY` with the database. Without it,
encrypted secret references cannot be restored.

## Authentication

Supported authentication:

- local username/password
- OIDC SSO
- optional Paperless login bridge
- scoped API tokens for automation

Local passwords are hashed with Argon2id. Browser sessions use HttpOnly cookies
and CSRF tokens for unsafe requests.

## RBAC

Roles:

- `viewer`
- `reviewer`
- `operator`
- `auditor`
- `admin`

Admin-only areas include settings, users, tokens, security retention, provider
secrets, and Paperless maintenance apply actions.

## Audit Events

Audit events are written for settings, security, prompts, review decisions,
job/run state changes, chat, token lifecycle, Paperless applies, and recovery
actions. Audit payloads redact secret values and avoid raw document content.

## Model Provider Privacy

Local Ollama can keep inference on infrastructure you control. External
providers may receive document text, OCR snippets, metadata context, and user
questions. Enable external providers only when your policy allows that data
flow.

## Safe Defaults

- Review mode by default.
- No automatic full autopilot until configured.
- New business tags disabled by default.
- Existing Paperless metadata protected unless overwrite settings are enabled.
- Provider and Paperless tests run through the backend and show redacted errors.
- Completion/tag reconcile starts as dry-run.

## Operator Checklist

- Use HTTPS behind a trusted proxy in production.
- Set secure cookies when serving over HTTPS.
- Use named users instead of shared admin accounts.
- Rotate API tokens periodically.
- Keep provider keys in a secret manager where possible.
- Review audit events after settings, prompt, and autopilot changes.
- Back up PostgreSQL and `ARCHIVIST_SECRET_KEY` together.
