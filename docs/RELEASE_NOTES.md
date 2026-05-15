# Release Notes

## v1.0.0 GA

Paperless Archivist v1.0.0 is the first GA-ready release of the secure AI
automation layer for Paperless-ngx.

### Major Capabilities

- Rust API and worker with PostgreSQL 18 storage.
- React + TypeScript frontend.
- Paperless REST API integration only; no direct Paperless database writes.
- OCR, title, correspondent, document type, document date, tag, and custom-field
  extraction stages.
- Review mode, auto-select with review, and full autopilot.
- Completion tags and trigger-tag cleanup.
- Document inventory, backlog dashboard, live processing status, and recovery
  tools.
- Document Chat/RAG with citations to Paperless documents.
- UI-managed runtime settings, model providers, local Ollama model discovery,
  prompt workbench, users, sessions, and scoped API tokens.
- Local login, Argon2id, sessions, CSRF, RBAC, OIDC SSO, audit log, secret
  redaction, encrypted secret references, and audit integrity checks.
- Hardened Docker Compose profiles and generic Kubernetes package.

### Upgrade Notes

- PostgreSQL 18 or newer is required.
- Stop workers before upgrading.
- Back up PostgreSQL and `ARCHIVIST_SECRET_KEY`.
- Start the API first and wait for migrations/readiness.
- Start workers after the API is healthy.
- Run Paperless consistency check after upgrade.

### Rollback Notes

Rollback to an older version after migrations requires restoring a database
backup from before the upgrade. Do not run older binaries against a newer schema
unless that release explicitly documents compatibility.

### Known Limitations

- Non-English UI languages beyond English and German use the English text
  fallback until translated catalogs are added.
- Public Kubernetes manifests are generic and must be patched for the target
  cluster, secrets, image registry, ingress, and storage policy.
- Benchmark results are synthetic and should be repeated on the operator's
  PostgreSQL storage for very large archives.
