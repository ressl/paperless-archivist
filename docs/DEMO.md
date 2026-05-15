# Public Demo Plan

The public demo must be safe to publish, clone, screenshot, and discuss. It must
not contain private documents, real customer names, real invoices, private
hosts, secrets, or deployment topology.

## Demo Dataset

Use synthetic documents only:

- invoice from `Example Office Supplies`
- insurance letter from `Example Insurance`
- employment contract excerpt for `Alex Example`
- tax notice from `Example City`
- multilingual receipt samples in English and German

Recommended tags:

- `inbox`
- `finance`
- `insurance`
- `contracts`
- `tax`
- `archivist-ocr`
- `archivist-tags`
- `ai-processed`

Recommended correspondents:

- `Example Office Supplies`
- `Example Insurance`
- `Example City`
- `Example Employer`

Recommended document types:

- `Invoice`
- `Contract`
- `Notice`
- `Receipt`

All names, addresses, account numbers, amounts, and dates must be artificial.

## Local Demo Environment

1. Start Paperless-ngx separately with its own demo database.
2. Import only synthetic PDFs or text-backed PDFs.
3. Start Archivist with Docker Compose:

```bash
cp deploy/compose/.env.example deploy/compose/.env
$EDITOR deploy/compose/.env
docker compose --env-file deploy/compose/.env -f deploy/compose/docker-compose.yml up --build
```

4. Configure Paperless and a local Ollama provider.
5. Run inventory sync.
6. Queue one full pipeline batch in review mode.
7. Approve a few synthetic review items.
8. Switch to full autopilot only after the demo data is validated.

## Demo Safety Checklist

- No real document images.
- No real email addresses except `example.com`.
- No private infrastructure hostnames.
- No screenshots that show access tokens, webhook URLs, OIDC secrets, or API
  keys.
- Browser address bars should use localhost or `example.com`.
- Audit screenshots must not show sensitive metadata.
- Chat screenshots must use synthetic questions and synthetic source content.

## Suggested Demo Flow

1. Dashboard: show 24h default view, live processing, and backlog charts.
2. Settings: show Paperless test, provider test, model dropdown, and first-run
   wizard completion.
3. Prompts: show editable default prompts and info hints.
4. Inventory: show per-document processing status and debug context.
5. Review: approve a synthetic metadata suggestion.
6. Chat: ask a synthetic archive question and show citations.
7. Audit: show a filtered audit event list without secrets.
