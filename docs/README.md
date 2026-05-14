# Paperless Archivist Documentation

Start here when you need to understand, operate, extend, or review Paperless
Archivist.

## Best Entry Point By Audience

| Audience | Start with | Purpose |
| --- | --- | --- |
| New user | [User Guide](USER_GUIDE.md) | Daily workflows, setup, model providers, review, autopilot, chat, troubleshooting |
| Operator | [Operations Guide](OPERATIONS.md) | Deployment, environment, secrets, backups, metrics, maintenance |
| Developer | [Development Guide](DEVELOPMENT.md) | Local setup, commands, code workflow |
| AI coding agent | [AI Agent Guide](AI_AGENT_GUIDE.md) | Repository map, safety boundaries, implementation checklist |
| API integrator | [API Reference](API_REFERENCE.md) | Endpoints, auth, request/response behavior |
| Security reviewer | [Security Design](SECURITY_DESIGN.md) | Auth, RBAC, sessions, CSRF, secrets, audit boundaries |

## Product And Architecture

- [Project Overview](PROJECT_OVERVIEW.md)
- [Feature List](FEATURES.md)
- [Solution Design](SOLUTION_DESIGN.md)
- [Architecture Decisions](ARCHITECTURE_DECISIONS.md)
- [PostgreSQL 18 Design](POSTGRESQL_18_DESIGN.md)
- [Frontend Design](FRONTEND_DESIGN.md)
- [Branding](BRANDING.md)

## How To Read The Docs

If you are evaluating the project, read:

1. [README](../README.md)
2. [User Guide](USER_GUIDE.md)
3. [Project Overview](PROJECT_OVERVIEW.md)
4. [Security Design](SECURITY_DESIGN.md)

If you are operating a deployment, read:

1. [Operations Guide](OPERATIONS.md)
2. [User Guide](USER_GUIDE.md)
3. [API Reference](API_REFERENCE.md)

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
