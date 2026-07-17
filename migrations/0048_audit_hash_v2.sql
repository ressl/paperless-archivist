-- #347: make the audit hash format explicit and bind request-origin metadata
-- in v2 without rewriting any existing payload, chain link, or event hash.
--
-- Rows that already have an event_hash were produced by the original v1
-- canonicalizer. Labelling those rows as v1 is metadata about the existing
-- digest; source_ip, user_agent, before/after/metadata and both hash columns
-- remain byte-for-byte untouched. Pre-chain rows stay NULL/NULL.

alter table audit_events add column if not exists hash_version smallint;

update audit_events
   set hash_version = 1
 where event_hash is not null
   and hash_version is null;

-- Rolling-deployment compatibility: a pre-0048 API/worker still writes the
-- v1 event_hash but does not name hash_version in its INSERT. The default
-- labels that digest correctly as v1. New code always writes 2 explicitly.
alter table audit_events alter column hash_version set default 1;

alter table audit_events
  add constraint audit_events_hash_version_check
  check (
    (event_hash is null and hash_version is null)
    or (event_hash is not null and hash_version in (1, 2))
  )
  not valid;

alter table audit_events validate constraint audit_events_hash_version_check;
