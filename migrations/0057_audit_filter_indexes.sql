-- 0057 (#448): indexes for the filtered, keyset-paginated audit log.
--
-- GET /api/audit now filters by document, actor and event type and pages by
-- the (created_at, id) keyset in descending order. 0042 dropped the old
-- document/actor indexes because nothing filtered on them at the time; the
-- new filters do, so they come back in keyset shape (created_at desc, id desc
-- as trailing columns), which lets one index scan serve both the filter and
-- the ORDER BY ... LIMIT without a sort. Partial on "is not null": most audit
-- rows carry no document, and system events carry no actor id.
create index if not exists audit_events_document_keyset_idx
  on audit_events (paperless_document_id, created_at desc, id desc)
  where paperless_document_id is not null;

create index if not exists audit_events_actor_keyset_idx
  on audit_events (actor_id, created_at desc, id desc)
  where actor_id is not null;

-- Replace (event_type, created_at desc) from 0001 with its keyset superset.
-- Every existing reader (metrics/dashboard aggregates filtering on
-- event_type + created_at, verified by grep over crates/) is served by the
-- wider index as well.
create index if not exists audit_events_type_keyset_idx
  on audit_events (event_type, created_at desc, id desc);
drop index if exists audit_events_type_created_idx;
