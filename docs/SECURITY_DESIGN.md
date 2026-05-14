# Security Design

Status: draft  
Goal: make Paperless Archivist secure by default and suitable for serious
self-hosted and enterprise environments.

## 1. Security Principles

1. Secure by default.
   A fresh deployment must not expose an unauthenticated admin UI.

2. Least privilege.
   Users, API tokens, service accounts, database roles, and Kubernetes access
   should have the smallest useful permissions.

3. Paperless remains authoritative for documents.
   Archivist does not bypass Paperless-ngx permissions when reading or writing
   document data.

4. AI output is untrusted.
   Model responses must never bypass validation, review policy, or audit.

5. Secrets are not normal settings.
   Secrets need separate storage, redaction, rotation, and audit behavior.

6. Every sensitive action is auditable.
   Authentication events, configuration changes, review decisions, job retries,
   and applied document changes must be logged.

7. Enterprise integrations should be possible without weakening local installs.

## 2. Authentication

Paperless Archivist must include user login.

### 2.1 MVP Authentication

MVP should support local users stored in PostgreSQL 18:

- username or email
- password hash
- role assignment
- enabled/disabled flag
- last login timestamp
- failed login counter
- password changed timestamp

Password hashing:

- Argon2id
- per-user salt
- configurable cost parameters
- 12-character minimum password length in the MVP
- 15-minute lockout after 10 failed login attempts

Session handling:

- secure HTTP-only cookies
- SameSite=Lax or Strict
- CSRF protection for state-changing browser requests
- configurable session lifetime
- session revocation on password change
- logout invalidates the server-side session

### 2.2 Paperless-ngx Login Bridge

Optional mode:

- authenticate users against Paperless-ngx token endpoint
- create a separate `paperless-*` Archivist local account on first successful
  bridge login
- grant viewer role by default; admins can later assign stronger Archivist roles
- issue a normal Archivist server-side session

This is useful for homelabs because users do not need a second password.

Important:

- Paperless auth bridge must not store Paperless passwords.
- Paperless API tokens returned by the login endpoint are not stored.
- Passwords are forwarded only to the Paperless login endpoint.
- Bridge accounts are prefixed so a Paperless username cannot take over an
  existing local admin username.
- Failed login behavior must avoid leaking whether Paperless or Archivist
  rejected the user.

### 2.3 Enterprise SSO

Enterprise-ready support:

- OIDC
- OAuth2
- SAML later if needed
- group/claim to role mapping
- SCIM later if needed

OIDC is implemented for ZITADEL using Authorization Code + PKCE. The backend
validates ID tokens, nonce, and access token hashes where present, then issues a
normal Archivist server-side session. Role assignment is controlled by local
configuration: an admin allowlist receives admin/operator/reviewer/auditor, and
other new SSO users receive default roles.

## 3. Authorization

Use role-based access control.

Required roles:

```text
viewer
reviewer
operator
admin
auditor
```

Role permissions:

| Role | Permissions |
| --- | --- |
| viewer | view dashboard, runs, non-sensitive config |
| reviewer | approve/reject/edit review items |
| operator | start/pause/retry jobs and batches |
| admin | manage settings, providers, prompts, users, secrets |
| auditor | view audit logs and security events |

Fine-grained permissions should be represented internally even if MVP exposes
only roles. This keeps the design ready for custom enterprise roles later.

## 4. API Security

All API endpoints must require authentication unless explicitly public:

Public endpoints:

```text
GET /healthz
GET /readyz
```

Protected endpoints:

```text
/api/*
/admin/*
/reviews/*
/settings/*
```

API requirements:

- CSRF protection for cookie-authenticated browser writes
- bearer tokens for automation
- token scopes
- token expiry
- token revocation
- request size limits
- timeout limits
- consistent error responses
- no stack traces in production responses

## 5. API Tokens and Service Accounts

Support API tokens for automation.

Token requirements:

- generated once, shown once
- stored hashed
- scoped permissions
- optional expiry
- last-used timestamp
- revocation
- audit event on creation/use/revocation

Example scopes:

```text
runs:read
runs:write
reviews:read
reviews:write
settings:read
settings:write
users:manage
inventory:read
batches:write
```

## 6. Secret Management

Secrets include:

- Paperless API token
- Ollama auth token if used
- OpenAI API key
- Anthropic API key
- OpenAI-compatible provider keys
- OIDC client secret
- SMTP password later

Kubernetes:

- prefer Kubernetes Secrets
- allow secret references in UI settings
- avoid storing plain secret values in PostgreSQL

Docker Compose:

- support Docker secrets
- support mounted secret files
- support environment variables for bootstrap

Optional enterprise mode:

- external secret manager integration later
- Vault-compatible secret references later

Secret UI behavior:

- show whether a secret is configured
- never display full secret after save
- allow replace/rotate
- test connection without revealing secret
- write audit event on change

## 7. Audit Logging

Audit events are mandatory.

Security audit events:

- login success
- login failure
- logout
- session revoked
- password changed
- user created/disabled/deleted
- role changed
- API token created/revoked
- provider secret changed
- settings changed
- prompt changed/activated
- batch started/paused/cancelled
- job retried/cancelled
- review approved/rejected/edited
- document patch applied

Audit fields:

- event ID
- timestamp
- actor type (`user`, `api_token`, `worker`, `system`)
- actor ID
- source IP
- user agent
- run ID
- document ID if applicable
- before/after JSON
- outcome
- error message if failed

Audit logs must be append-only from application perspective. Admins can define
retention, but ordinary UI actions must not edit past audit events.

## 8. Data Protection

Document data is sensitive.

Rules:

- do not log full document text by default
- do not log AI prompts containing full document text by default
- store AI request/response artifacts with configurable retention
- support artifact redaction mode
- allow disabling raw request storage
- allow storing only hashes and normalized output
- redact API keys and authorization headers everywhere
- temp files must be deleted after processing
- temp directory must be configurable

Retention policies:

- audit events: long retention
- AI raw artifacts: shorter retention
- rendered page images: deleted immediately unless debug mode
- failed job payloads: retained, but redacted

## 9. Enterprise Features

Enterprise-ready capabilities to design for:

- OIDC SSO
- RBAC and future custom roles
- audit export
- SIEM-friendly JSON logs
- Prometheus metrics
- OpenTelemetry traces
- configurable retention
- backup/restore documentation
- HA-ready stateless services
- horizontal worker scaling
- external PostgreSQL 18
- external secret references
- network policy templates
- admin lockout recovery procedure
- security headers
- rate limiting
- TLS behind ingress/reverse proxy

## 10. Web Security

Required headers:

```text
Content-Security-Policy
X-Content-Type-Options: nosniff
X-Frame-Options: DENY
Referrer-Policy
Permissions-Policy
```

Cookie requirements:

- HttpOnly
- Secure when HTTPS
- SameSite=Lax or Strict
- server-side session storage

UI requirements:

- CSRF tokens for form submissions
- no inline scripts unless CSP nonce is used
- HTML escaping by template engine
- safe markdown handling if markdown is ever supported

## 11. Kubernetes Security

Kubernetes deployment should include:

- run as non-root
- read-only root filesystem where practical
- drop Linux capabilities
- resource requests and limits
- NetworkPolicies
- separate service accounts
- no Kubernetes API access unless needed
- secret mounts as files where possible
- pod disruption budgets for API
- graceful shutdown for workers

Network policy intent:

- API/UI can reach PostgreSQL and Paperless.
- Worker can reach PostgreSQL, Paperless, and configured AI providers.
- Ingress can reach API/UI.
- No broad egress by default in hardened mode.

## 12. Docker Compose Security

Compose deployment should support:

- non-root containers
- secrets through files
- local-only default bind where practical
- documented reverse proxy TLS setup
- explicit external network configuration
- secure default admin bootstrap

## 13. Database Security

PostgreSQL 18 requirements:

- SCRAM authentication
- dedicated database
- dedicated role
- no superuser application role
- migrations run with controlled privileges
- application role has only required table privileges
- backups documented
- page checksums enabled for new clusters

Optional split roles later:

- migrator role
- application runtime role
- read-only reporting role

## 14. AI Provider Security

External AI providers are opt-in.

UI must clearly show:

- whether document text leaves the local network
- which provider/model is used per stage
- whether raw prompts/responses are stored

Provider rules:

- local Ollama is the default
- API keys are secret-managed
- provider calls have timeouts
- provider errors are redacted
- per-stage external provider use is auditable

## 15. Supply Chain Security

Required:

- Cargo.lock committed
- dependency scanning in CI
- container image scanning
- minimal runtime image
- SBOM generation later
- signed container images later

Rust crates:

- avoid unmaintained crates for security-sensitive logic
- prefer well-known crates for auth/session/password hashing
- review transitive dependencies before first release

## 16. MVP Security Acceptance Criteria

MVP is not acceptable unless:

- UI requires login
- initial admin bootstrap is documented
- passwords use Argon2id
- sessions are server-side and revocable
- CSRF protection exists
- roles exist at least for admin/operator/reviewer/viewer
- all settings changes create audit events
- provider secret changes create audit events
- review/apply actions create audit events
- API tokens are hashed and scoped
- no full secrets are displayed after save
- Docker Compose and Kubernetes deployments run non-root
- document text is not logged by default
- external AI provider use is visibly configured and auditable
