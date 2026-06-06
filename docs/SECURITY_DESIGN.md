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

### 4.1 CSRF Threat Model

The CSRF token is minted on session establishment and stored both as a
`SameSite=Lax` cookie and inside the session record. Browser writes must
echo the token via the `X-CSRF-Token` header; the API rehashes the
incoming value and compares it against the stored hash with
`subtle::ConstantTimeEq` so that comparison time does not depend on
which byte first differs.

Known limitations of the current design (accepted for v1.x):

- CSRF tokens persist for the lifetime of the session (up to 12 hours
  by default) and are not rotated on privilege change. A leaked token
  remains valid until the session expires or the user logs out.
- There is no binding to client IP address or user-agent. A token
  exfiltrated to another network is still accepted while the session
  lives.
- Bearer-token (machine) requests bypass CSRF entirely; the bearer
  token itself is the only secret.

Mitigations: short session lifetime, `SameSite=Lax` and `Secure`
cookies in production, and audit-logged logout. Future hardening is
tracked in the roadmap (CSRF rotation, optional IP/UA binding).

### 4.2 Cookie `Secure` Attribute

The `ARCHIVIST_COOKIE_SECURE` flag is set to `false` by default so that
local development against `http://localhost` works without TLS. **Every
production deployment is required to set `ARCHIVIST_COOKIE_SECURE=true`**
(or `cookie_secure = true` in the equivalent settings file) so that the
session and CSRF cookies are only transmitted over HTTPS. The flag is
read once at startup; changing it requires a process restart. Operators
deploying behind a TLS-terminating reverse proxy should additionally set
`ARCHIVIST_TRUST_PROXY=true` so the originating client IP — not the
proxy — is recorded in the audit log.

### 4.3 SSRF Threat Model for Outbound Requests

Every outbound request to an operator-influenceable URL — the admin "test"
endpoints (`POST /api/settings/test/paperless`,
`POST /api/settings/test/provider`, `POST /api/settings/test/notification`,
`GET /api/settings/providers/{name}/models`) **and** the worker data path
(Paperless downloads / metadata, AI provider calls, notification webhooks) —
is guarded against SSRF. To stop a session hijacker who briefly holds
`WriteSettings` from turning those requests into a host-side scanner, two
layers cooperate:

1. **Up-front check (test endpoints).** Each test path runs
   `validate_outbound_url()` before opening a socket, for an early, friendly
   rejection of an obviously dangerous URL.
2. **Connection-time guard (everywhere).** Every outbound `reqwest` client is
   built with a shared `SsrfGuardResolver` (a custom DNS resolver) plus a
   no-redirect policy. The resolver applies the address policy below to the
   exact IPs reqwest is about to dial, so a hostname cannot be rebound to an
   internal address in the gap between the up-front check and the connect
   (DNS-rebinding TOCTOU). This applies to the worker data path too, not just
   the UI test handlers. (IP-literal hosts bypass the resolver — they carry no
   rebinding risk and are screened up front by `validate_outbound_url`.)

The validator is intentionally **narrow**, not paranoid: Paperless
Archivist is routinely deployed inside Kubernetes / Docker Compose /
on-prem networks where Paperless-ngx and Ollama live on private
addresses. A validator that rejected all of RFC1918 / RFC6598 / RFC4193
would make the in-UI Test buttons unusable in every realistic deployment
even though the targets are operator-trusted internal services.

Hard reject (no legitimate operator target):

- Loopback (`127.0.0.0/8`, `::1`, v4-mapped forms of `::ffff:127.0.0.1`)
- Link-local incl. cloud-metadata IMDS (`169.254.0.0/16`, `fe80::/10`).
  The IMDS endpoint `169.254.169.254` would expose AWS/Azure/GCP IAM
  credentials if reachable.
- Unspecified (`0.0.0.0`, `::`)
- Broadcast (`255.255.255.255`)
- Multicast
- URLs containing userinfo (`http://user:pass@host/`) — these leak the
  embedded credentials into the request and serve no purpose for a
  configured integration.
- Schemes other than `http` and `https` (no `file://`, `gopher://`,
  `dict://`, etc.)

Explicitly allowed (operator-trusted):

- RFC1918 (`10.0.0.0/8`, `172.16.0.0/12`, `192.168.0.0/16`)
- RFC6598 shared-address space (`100.64.0.0/10`)
- RFC4193 unique-local IPv6 (`fc00::/7`)
- Any public unicast address (the original public-internet case)

The threat model assumes the operator setting an `Ollama` or `Paperless`
URL deliberately points the integration at the right private service.
The SSRF guard targets the abuse cases that no operator would ever
deliberately configure — loopback to scan the API pod itself, IMDS to
exfiltrate cloud credentials, link-local to probe the host network.

Implementation: the up-front check is
`crates/archivist-api/src/main.rs::validate_outbound_url` (invoked by
`test_paperless`, `test_provider`, `test_notification`,
`model_provider_models`), covered by unit tests in the same file. The shared
address policy and connection-time resolver live in
`crates/archivist-core/src/ssrf.rs` (`is_ssrf_dangerous_ip`,
`SsrfGuardResolver`) and are installed on every Paperless / AI-provider /
webhook client across the API and worker.

## 5. API Tokens and Service Accounts

Support API tokens for automation.

Token requirements:

- generated once, shown once
- stored hashed
- scoped permissions
- expiry policy with default and maximum TTL
- last-used timestamp
- revocation
- rotation that revokes the old token and returns the raw replacement once
- audit event on creation/rotation/revocation

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

Audit logs must be append-only from application perspective. New audit events
are linked with `prev_event_hash` and `event_hash` so operators can verify the
current hash chain. Admins can define retention and apply it explicitly; the
retention action writes its own audit event with deleted row counts. Ordinary UI
actions must not edit past audit events.

## 8. Data Protection

Document data is sensitive.

Rules:

- do not log full document text by default
- do not log AI prompts containing full document text by default
- store AI request/response artifacts with configurable retention
- support artifact storage modes: full, redacted, metadata-only
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

### 14.1 Prompt Injection Threat Model

Document text — including OCR output — is interpolated into model
prompts without scrubbing prompt-injection markers ("ignore previous
instructions", role-impersonation tokens, embedded system-prompt
syntaxes, etc.). The threat is real: any actor who can upload a
document into Paperless can attempt to subvert downstream AI stages
through crafted content.

Accepted residual risk for v1.x: all model outputs that affect document
state are funnelled through the typed validators in
`archivist-core::validate_*` (`validate_tag_suggestion`,
`validate_title`, `validate_correspondent`, `validate_document_date`,
`validate_fields`, etc.). Those validators enforce:

- closed allowlists for tag names and document types (rejecting any
  output that doesn't match a configured taxonomy)
- length, character class and date-range checks for titles,
  correspondents and dates
- workflow-tag protection so the AI cannot toggle the trigger /
  completion / failure tags that drive the pipeline

A successful prompt-injection attack therefore degrades into one of:
(a) a validator rejection, surfaced as a failed stage with a typed
error and a review item; or (b) an output that is already constrained
to the configured allowlist, no different in effect than a tagging
mistake. The system does not currently strip injection markers from
prompt text — the constraint is enforced at the *output* boundary, not
the input. Hardening the input path (sanitization, dual-LLM
classification, separation of trusted/untrusted context blocks) is
tracked in the security roadmap.

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
