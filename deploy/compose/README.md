# Docker Compose Deployment

This directory contains public-safe Compose profiles for local evaluation and
small self-hosted installations.

## Profiles

Minimal stack with PostgreSQL 18, API/UI, and worker:

```bash
cp deploy/compose/.env.example deploy/compose/.env
$EDITOR deploy/compose/.env
docker compose --env-file deploy/compose/.env -f deploy/compose/docker-compose.yml up --build
```

Add local Ollama:

```bash
docker compose --profile ollama --env-file deploy/compose/.env -f deploy/compose/docker-compose.yml up --build
```

Use an external PostgreSQL 18 database:

```bash
docker compose \
  --env-file deploy/compose/.env \
  -f deploy/compose/docker-compose.yml \
  -f deploy/compose/docker-compose.external-postgres.yml \
  up --build
```

Add the Caddy reverse proxy profile:

```bash
docker compose \
  --profile reverse-proxy \
  --env-file deploy/compose/.env \
  -f deploy/compose/docker-compose.yml \
  -f deploy/compose/docker-compose.proxy.yml \
  up --build
```

Set `ARCHIVIST_PUBLIC_HOST` and `ACME_EMAIL` before exposing the reverse proxy.

## Hardening Notes

- API and worker run as the non-root user from the image.
- API and worker use read-only root filesystems and a `/tmp` tmpfs.
- Linux capabilities are dropped for API and worker.
- API exposes a Compose health check against `/healthz`.
- The worker starts after the API health check, so migrations have a chance to
  run before jobs are claimed.
- Services communicate on the private `archivist-internal` network.
- Resource reservations and limits are configurable through `.env`.

## Secrets

Use strong values for:

- `POSTGRES_PASSWORD`
- `ARCHIVIST_SECRET_KEY`
- `ARCHIVIST_ADMIN_PASSWORD`
- OIDC client secrets when OIDC is enabled

Do not reuse the example values. For production-like single-host deployments,
store `.env` outside backups that are shared with other systems, restrict file
permissions, and rotate the bootstrap admin password after creating named admin
accounts.

## Backups

The Compose-managed database stores Archivist operational state only; Paperless
documents remain in Paperless-ngx and must be backed up there.

For the Compose PostgreSQL volume:

```bash
docker compose --env-file deploy/compose/.env -f deploy/compose/docker-compose.yml exec postgres \
  pg_dump -U "$POSTGRES_USER" "$POSTGRES_DB" > archivist-backup.sql
```

Restore into a fresh PostgreSQL 18 database before starting the API, then let
the API run any pending migrations.
