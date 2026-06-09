# Migration And Rollback Guide

Status: v1.0 GA readiness

The API service owns database migrations. Workers must start after the API has
applied migrations and must not run migrations themselves.

## Fresh Install Smoke Test

Run the smoke test against an isolated PostgreSQL 18 container:

```bash
scripts/verify/migration_smoke.sh
```

The test applies all migrations through the Rust migrator and verifies the
required GA tables.

## Upgrade Procedure

1. Read the release notes for the target version.
2. Stop workers or scale them to zero.
3. Back up PostgreSQL.
4. Back up runtime secret material and deployment configuration.
5. Deploy the new API image.
6. Wait until API readiness succeeds and migrations are complete.
7. Start workers.
8. Open the dashboard and confirm live status, backlog counts, and audit access.
9. Run Paperless consistency check after major upgrades.

## Backup Checklist

Before migration:

```bash
pg_dump --format=custom --file=archivist-before-upgrade.dump "$DATABASE_URL"
```

Also preserve:

- `ARCHIVIST_SECRET_KEY`
- external secret manager entries
- Paperless API token source
- model provider API key source
- deployment manifests or Compose environment file

Without the same `ARCHIVIST_SECRET_KEY`, encrypted secret references cannot be
read after restore.

## Rollback Boundaries

Supported rollback:

- application binary/image rollback when no new migration was applied
- restore database backup and previous application version

Unsupported rollback:

- running an older binary against a newer migrated schema unless the release
  notes explicitly say it is compatible
- manually deleting migration rows
- editing Paperless metadata directly to undo Archivist changes

When a migration has already run, treat database restore as the rollback path.
Paperless remains the system of record, so verify any metadata writes with the
Paperless audit/history available to your deployment.

## Destructive Migration Policy

A migration must not irreversibly delete operator-authored content (prompts,
settings, mappings). Migration `0028_drop_legacy_prompt_stages.sql` deleted
customised prompt rows and is the cautionary example. For future schema
changes that retire such data:

- copy the affected rows into an `_archive` table (or a JSON column) before
  removing them, or
- deactivate rather than delete (e.g. an `active = false` flag), so the
  operator's content is recoverable after an upgrade.

Index/constraint cleanups and pruning of regenerable/operational data
(snapshots, caches, telemetry) are exempt — those carry no operator content.

## Migration Smoke Test Details

The ignored Rust integration test lives at
`crates/archivist-db/tests/migration_smoke.rs`. The shell wrapper starts
PostgreSQL 18 and runs:

```bash
DATABASE_URL=postgres://... cargo test -p archivist-db --test migration_smoke -- --ignored --nocapture
```

Use this before release tags and after modifying migrations.
