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
The overlay always sets `ARCHIVIST_COOKIE_SECURE=true` for the API, regardless
of the localhost-oriented value in `.env`, so new session and CSRF cookies are
HTTPS-only. Caddy redirects HTTP to HTTPS and emits HSTS with a one-year
`max-age` and `includeSubDomains`.

Use the base file without the proxy overlay for direct local HTTP. That profile
keeps `ARCHIVIST_COOKIE_SECURE=false` so browsers can use cookies at
`http://127.0.0.1:8080`; do not expose it as a public deployment.

Verify both rendered profiles after changing Compose files:

```bash
pnpm --dir frontend contract:compose:rendered
```

Because HSTS includes subdomains, enable the public profile only on a hostname
whose full subdomain tree is available over HTTPS. Browsers can retain the HSTS
policy for up to one year after a rollback.

## Hardening Notes

- API and worker run as the non-root user from the image.
- API and worker use read-only root filesystems and a `/tmp` tmpfs.
- Linux capabilities are dropped for API and worker.
- API exposes a Compose health check against `/healthz`.
- The worker starts after the API health check, so migrations have a chance to
  run before jobs are claimed.
- Services communicate on the private `archivist-internal` network.
- Resource reservations and limits are configurable through `.env`.
- The Caddy profile owns the external HSTS header and replaces the API's
  equivalent value so clients receive one unambiguous policy.

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
