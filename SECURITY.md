# Security Policy

Paperless Archivist handles private document metadata and may process document
content through local or external AI providers. Security reports are taken
seriously.

## Supported Versions

The current supported line is v1.0.x once v1.0.0 is tagged. Pre-GA versions are
best-effort.

## Reporting A Vulnerability

Please report vulnerabilities privately to the project maintainers. Include:

- affected version or commit
- deployment mode
- whether authentication is required
- impact on document content, Paperless metadata, credentials, audit data, or
  availability
- reproduction steps using example data only

Do not include real document content, API keys, cookies, access tokens, private
hostnames, or credential URLs in public issues.

## Security Boundaries

- The frontend talks only to the Archivist backend.
- Archivist uses the Paperless REST API and never writes directly to the
  Paperless database.
- Provider keys and Paperless tokens are secret references and are not returned
  after save.
- Browser mutations use sessions plus CSRF.
- Passwords are hashed with Argon2id.
- Roles and API token scopes are enforced by the backend.
- Important settings, security, prompt, review, job, chat, and apply actions are
  audited.

See `docs/SECURITY_DESIGN.md` for the full design.
