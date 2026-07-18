# Installation Guide

This guide explains how to install Paperless Archivist for local use,
production-like Docker Compose, generic Kubernetes, and local development.

## Requirements

- Paperless-ngx with API access.
- PostgreSQL 18 or newer.
- Docker with Compose support for the recommended quick start.
- Optional Ollama for local AI.
- Optional OpenAI, Anthropic, Ollama Cloud, or OpenAI-compatible API key.
- Optional SGLang for the exact MiniMax M3 text and vision/OCR integration. Use the
  model/runtime pins and parser requirements in the
  [operations runbook](OPERATIONS.md#sglangminimax-m3-operations); Archivist
  does not install or manage the inference runtime.

Paperless Archivist does not require direct access to the Paperless database.
It only uses the Paperless REST API.

## Docker Compose Quick Start

```bash
cp deploy/compose/.env.example deploy/compose/.env
$EDITOR deploy/compose/.env
docker compose --env-file deploy/compose/.env -f deploy/compose/docker-compose.yml up --build
```

Open:

```text
http://127.0.0.1:8080
```

Log in with `ARCHIVIST_ADMIN_USERNAME` and `ARCHIVIST_ADMIN_PASSWORD` from
`deploy/compose/.env`.

## Docker Compose With Ollama

Use this when you want local inference in the same Compose project:

```bash
docker compose --profile ollama --env-file deploy/compose/.env -f deploy/compose/docker-compose.yml up --build
```

Pull models into the Ollama daemon:

```bash
ollama pull qwen3:8b
ollama pull qwen2.5vl:7b
```

In Settings, set the local Ollama URL to:

```text
http://ollama:11434
```

The model dropdown loads installed models through the Archivist backend.

## Docker Compose With External PostgreSQL 18

Set `EXTERNAL_DATABASE_URL` in `deploy/compose/.env`, then use the override:

```bash
docker compose \
  --env-file deploy/compose/.env \
  -f deploy/compose/docker-compose.yml \
  -f deploy/compose/docker-compose.external-postgres.yml \
  up --build
```

Keep `ARCHIVIST_SECRET_KEY` stable across restarts and restores. It is needed to
decrypt encrypted secret references.

## Reverse Proxy Profile

The public-safe Compose proxy example uses Caddy and example domains:

```bash
docker compose \
  --profile reverse-proxy \
  --env-file deploy/compose/.env \
  -f deploy/compose/docker-compose.yml \
  -f deploy/compose/docker-compose.proxy.yml \
  up --build
```

Replace example hostnames in your private runtime configuration, not in public
documentation.

This overlay is the public HTTPS profile: it forces
`ARCHIVIST_COOKIE_SECURE=true`, redirects port 80 to HTTPS, and returns
`Strict-Transport-Security: max-age=31536000; includeSubDomains`. The value of
`ARCHIVIST_COOKIE_SECURE` in `.env` remains `false` for the separate base-only
localhost profile and is intentionally overridden when the proxy file is used.
Do not publish the base profile directly.

After upgrading an existing proxy deployment, restart the API and sign out and
back in (or revoke existing browser sessions) so cookies issued before the
upgrade are replaced with `Secure` cookies. Only use the HSTS profile when the
hostname and all of its subdomains will remain HTTPS-capable: a browser can
retain the policy for one year, so removing the overlay does not immediately
undo HSTS. A rollback to direct local HTTP may also require clearing the secure
site cookies before logging in at `http://127.0.0.1:8080`.

## Generic Kubernetes

Start with:

```bash
kubectl kustomize deploy/kubernetes/base
```

Then patch:

- image reference and digest
- PostgreSQL connection secret
- `ARCHIVIST_SECRET_KEY`
- admin bootstrap or OIDC configuration
- ingress host
- resource requests/limits
- NetworkPolicy according to your cluster

See [`deploy/kubernetes/README.md`](../deploy/kubernetes/README.md).
In-cluster SGLang or MinerU endpoints require the public-safe
[`custom-ai-egress` component](../deploy/kubernetes/README.md#opt-in-egress-to-custom-ai-providers)
adapted to the real namespace labels, pod labels, and Service target ports in
your private overlay. Do not replace it with unrestricted namespace or private
CIDR egress.

## Local Development

Start PostgreSQL 18 and set:

```bash
export DATABASE_URL=postgres://archivist:archivist@127.0.0.1:5432/archivist
export ARCHIVIST_SECRET_KEY=dev-only-secret-with-at-least-32-bytes
export ARCHIVIST_ADMIN_USERNAME=admin
export ARCHIVIST_ADMIN_PASSWORD=change-me-in-dev
```

Run the API:

```bash
cargo run -p archivist-api
```

Run the worker:

```bash
cargo run -p archivist-worker
```

Run the frontend dev server:

```bash
npm --prefix frontend install
npm --prefix frontend run dev
```

Regenerate OpenAPI types after API contract changes:

```bash
npm --prefix frontend run generate:client
```

## First Configuration

1. Log in as admin.
2. Open Settings.
3. Set the Paperless Base URL that the backend can reach.
4. Enter a Paperless API token.
5. Choose a model provider.
6. Enter API keys for commercial providers, if used.
7. Test Paperless and provider connections.
8. Run Dashboard Sync.
9. Queue one OCR job and one tagging job.
10. Review results before enabling autopilot.

For SGLang/MiniMax M3, follow the dedicated
[Settings procedure](USER_GUIDE.md#sglang-with-minimax-m3-text-and-visionocr), then run
the opt-in live contract before production use.

## Commercial Providers

Default provider records are already created for OpenAI, Anthropic, Ollama
Cloud, and OpenAI-compatible endpoints. In most cases you only need to:

1. enter the API key
2. choose text and vision models
3. save settings
4. click provider test

External providers may receive document text or OCR output. Enable them only
when this matches your data handling policy.
