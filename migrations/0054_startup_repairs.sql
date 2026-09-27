-- #443: persisted markers for the worker's one-shot startup repairs.
--
-- The worker used to run every historical repair (Ollama num_ctx floors,
-- vision-crash requeue, OCR-only metadata backfill, backfill priority
-- rebalance, stuck-running run reset) on every boot. A crash-looping pod
-- therefore repeated them without bound and nothing recorded when they had
-- actually run. Each repair is now keyed by (name, version): the worker skips
-- a repair whose marker exists and records the marker (with the app version
-- and the repair's summary) after a successful pass. Bumping a repair's
-- version in `archivist_db::StartupRepair` makes it run once more.
--
-- Small, append-only operational table: no NOT VALID dance needed.
create table if not exists startup_repairs (
  name text not null,
  version integer not null check (version > 0),
  app_version text not null,
  applied_at timestamptz not null default now(),
  details jsonb not null default '{}'::jsonb,
  primary key (name, version)
);
