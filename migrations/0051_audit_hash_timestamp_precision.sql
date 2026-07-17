-- Persist the original sub-microsecond timestamp suffix for audit hashes.
--
-- Writers before v1.17 hashed the host's nanosecond timestamp before
-- PostgreSQL stored it at microsecond precision. Existing rows remain NULL
-- until the integrity verifier reconstructs the finite 0..999 suffix and
-- validates it against the immutable event_hash. New writers store 0 because
-- they hash the already-canonicalized timestamp.

alter table audit_events add column if not exists hash_created_at_ns_suffix smallint;

alter table audit_events
  add constraint audit_events_hash_created_at_ns_suffix_check
  check (
    hash_created_at_ns_suffix is null
    or (
      event_hash is not null
      and hash_created_at_ns_suffix between 0 and 999
    )
  )
  not valid;

alter table audit_events validate constraint audit_events_hash_created_at_ns_suffix_check;
