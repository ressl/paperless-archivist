# Security Policy

## Supported Versions

Security fixes are handled on the default branch until versioned releases are
introduced. Production deployments should track reviewed release images and pin
immutable image digests.

## Reporting Vulnerabilities

Do not open public issues for suspected vulnerabilities or leaked secrets.
Report privately to the project maintainers through the security contact listed
on the project hosting platform or another established private contact channel.

Include:

- affected version or commit
- impact and reproduction steps
- logs or screenshots with secrets redacted
- whether the issue may already be exposed in a public mirror

## Scope

In scope:

- authentication, session, CSRF, RBAC, and API token flaws
- secret handling and redaction issues
- Paperless REST API write safety
- SSRF or prompt-injection paths that can disclose secrets or alter documents
- container, Kubernetes, and CI/CD misconfiguration in this repository

Out of scope:

- vulnerabilities in Paperless-ngx, PostgreSQL, ZITADEL, Ollama, or commercial
  model providers unless this project integrates them unsafely
- rate limiting or abuse concerns for non-public internal deployments
