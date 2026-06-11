# Migrations

Plain-SQL migrations applied in filename order by `archivist_db::migrate()`
(sqlx migrator; the directory can be overridden via `ARCHIVIST_MIGRATIONS_DIR`).
Target platform is **PostgreSQL 18+** — migrations may (and do) use PG18
features such as `uuidv7()` defaults, generated columns, and `RETURNING
old/new` in application SQL.

## Ground rules

- **Numbering**: zero-padded, strictly increasing (`00NN_short_name.sql`).
  Never renumber or edit an applied migration — append a new one.
- **Fresh-DB proof**: every migration must apply cleanly on an empty database.
  This is enforced by the ignored integration test
  `migrations_apply_on_fresh_postgresql_18_database`
  (`crates/archivist-db/tests/migration_smoke.rs`); run it against a
  disposable PG18 database before merging:

  ```sh
  DATABASE_URL=postgres://...:5434/<throwaway> \
  ARCHIVIST_MIGRATIONS_DIR=$PWD/migrations \
  cargo test -p archivist-db -- --ignored
  ```

- **Drops**: before dropping any column/table/index, grep `crates/`,
  `frontend/` and `openapi/` for remaining references — including string
  literals inside SQL — and note the verification in the migration header
  comment (see 0039/0042/0044 for the pattern).

## Constraint convention: two-step `NOT VALID` + `VALIDATE`

New constraints on the unbounded, write-hot tables (`audit_events`, `jobs`,
`ai_artifacts`, and the `document_inventory` mirrors) must be added in two
steps so existing rows are checked without an exclusive lock blocking
writers:

```sql
-- Step 1: instant catalog-only change (brief ACCESS EXCLUSIVE, no scan).
-- New/updated rows are checked from this moment on.
alter table audit_events
  add constraint audit_events_outcome_check
  check (outcome in ('success', 'retry', ...))
  not valid;

-- Step 2: scan existing rows under SHARE UPDATE EXCLUSIVE —
-- concurrent INSERT/UPDATE/DELETE keep running during the scan.
alter table audit_events validate constraint audit_events_outcome_check;
```

This applies to all three constraint kinds:

- **CHECK** constraints: as above (see 0043 for shipped examples).
- **NOT NULL** additions (PostgreSQL 18 catalogs not-null constraints, so
  they support the same two-step dance — prefer it over a full-validating
  `ALTER COLUMN ... SET NOT NULL` on the big tables):

  ```sql
  alter table ai_artifacts
    add constraint ai_artifacts_input_tokens_not_null
    not null input_tokens
    not valid;
  alter table ai_artifacts validate constraint ai_artifacts_input_tokens_not_null;
  ```

- **FOREIGN KEY** additions: `ADD CONSTRAINT ... FOREIGN KEY ... NOT VALID`
  then `VALIDATE CONSTRAINT` (see 0041).

**Lock nuance**: sqlx wraps each migration file in one transaction by
default, and locks are held until commit — so inside an ordinary migration
the brief ACCESS EXCLUSIVE taken by step 1 persists through the step-2 scan
and the online benefit is lost. For a genuinely online validation on a large
table, start the file with sqlx's `-- no-transaction` marker (first line) so
each statement autocommits, releasing the ADD's lock before the VALIDATE
scan; statements after a failure then need to be idempotent/re-runnable. At
today's table sizes (≤ a few hundred MB) keeping both statements in one
transactional file is an acceptable trade — revisit when `audit_events`
outgrows quick scans.

Small/config tables (`runtime_settings`, `prompts`, `ai_provider_cooldowns`,
...) may keep using immediate, fully-validating constraints — the scan is
negligible there.

Derive allowed value sets from live `DISTINCT`s **unioned with every literal
the code can write** (grep `crates/`), not from assumptions — a value set
that only mirrors current data bricks rarely-taken writers at `VALIDATE`
time or, worse, at the next such write (0043 documents a concrete case:
`audit_events.outcome` had 4 live values but 11 writable ones).

## Index convention

Before adding a NEW single-column index on `jobs`/`runs`/`review_items`/
`audit_events`, check whether an existing composite whose leading column is
low-cardinality (`status`, `stage`, `event_type`, ...) already serves the
query via a PG18 B-tree skip scan (`EXPLAIN` shows `Index Searches: N`).
