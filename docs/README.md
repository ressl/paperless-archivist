# Paperless Archivist Documentation

Start here when you need to understand, operate, extend, or review Paperless
Archivist.

## Best Entry Point By Audience

| Audience | Start with | Purpose |
| --- | --- | --- |
| New user | [Installation Guide](INSTALLATION.md), then [User Guide](USER_GUIDE.md) | Install, first login, daily workflows, model providers, review, autopilot, chat |
| Operator | [Operations Guide](OPERATIONS.md) | Deployment, environment, secrets, backups, metrics, maintenance |
| Developer | [Development Guide](DEVELOPMENT.md) | Local setup, commands, code workflow |
| AI coding agent | [AI Agent Guide](AI_AGENT_GUIDE.md) | Repository map, safety boundaries, implementation checklist |
| API integrator | [API Reference](API_REFERENCE.md) | Endpoints, auth, request/response behavior |
| Security reviewer | [Security Guide](SECURITY.md), then [Security Design](SECURITY_DESIGN.md) | Auth, RBAC, sessions, CSRF, secrets, audit boundaries |
| Prompt author | [Prompt Pack](PROMPTS.md) | Default prompts, output contracts, upgrade behavior |
| Troubleshooter | [Troubleshooting](TROUBLESHOOTING.md), then [FAQ](FAQ.md) | Common Paperless, Ollama, worker, review, and autopilot issues |

## Product And Architecture

- [Project Overview](PROJECT_OVERVIEW.md)
- [Feature List](FEATURES.md)
- [Feature Reference](FEATURE_REFERENCE.md)
- [Solution Design](SOLUTION_DESIGN.md)
- [Architecture Decisions](ARCHITECTURE_DECISIONS.md)
- [PostgreSQL 18 Design](POSTGRESQL_18_DESIGN.md)
- [Frontend Design](FRONTEND_DESIGN.md)
- [Branding](BRANDING.md)
- [Prompt Pack](PROMPTS.md)
- [Installation Guide](INSTALLATION.md)
- [Troubleshooting](TROUBLESHOOTING.md)
- [FAQ](FAQ.md)
- [Stability Policy](STABILITY.md)
- [Migration Guide](MIGRATIONS.md)
- [Performance Guide](PERFORMANCE.md)
- [Accessibility Audit](ACCESSIBILITY.md)
- [Release Checklist](RELEASE_CHECKLIST.md)

## How To Read The Docs

If you are evaluating the project, read:

1. [README](../README.md)
2. [Installation Guide](INSTALLATION.md)
3. [User Guide](USER_GUIDE.md)
4. [Feature Reference](FEATURE_REFERENCE.md)
5. [Security Guide](SECURITY.md)

If you are operating a deployment, read:

1. [Operations Guide](OPERATIONS.md)
2. [Migration Guide](MIGRATIONS.md)
3. [Performance Guide](PERFORMANCE.md)
4. [Troubleshooting](TROUBLESHOOTING.md)

If you are changing code, read:

1. [AI Agent Guide](AI_AGENT_GUIDE.md)
2. [Development Guide](DEVELOPMENT.md)
3. [Project Overview](PROJECT_OVERVIEW.md)
4. The relevant design document for the area you are changing

## Core Safety Rules

- Paperless remains the system of record.
- Archivist writes to Paperless through the Paperless REST API only.
- The frontend only talks to the Archivist API.
- AI provider calls happen in backend or worker code only.
- Secrets are encrypted references, never returned to the frontend.
- Model output is validated before review or apply.
- Settings, security, review, apply, and chat actions are auditable.
