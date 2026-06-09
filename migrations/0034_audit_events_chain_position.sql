-- #254: order the audit hash chain by a monotonic sequence instead of the
-- writing process's wall clock.
--
-- append_audit_tx serializes appends with pg_advisory_xact_lock, so the
-- prev_event_hash linkage follows commit order. But created_at / UUIDv7 come
-- from whichever process (API pod vs worker pod) holds the lock, and
-- verify_audit_integrity replayed by (created_at, id). Cross-host clock skew
-- larger than the inter-event gap made the replay order diverge from the
-- append order, producing a false "previous event hash does not match chain"
-- failure on a perfectly intact chain.
--
-- chain_position is assigned by a sequence at INSERT time, under the advisory
-- lock, so its order equals commit/append order regardless of clock skew. It
-- is NOT part of the event hash (existing hashes stay valid); it is purely an
-- ordering key for the prev-hash lookup and the verification replay.

alter table audit_events add column if not exists chain_position bigint;

-- Backfill existing rows in their established (created_at, id) order so the
-- chain order is preserved across the upgrade.
with ordered as (
  select id, row_number() over (order by created_at asc, id asc) as rn
    from audit_events
)
update audit_events ae
   set chain_position = ordered.rn
  from ordered
 where ae.id = ordered.id
   and ae.chain_position is null;

create sequence if not exists audit_events_chain_position_seq
  owned by audit_events.chain_position;

select setval(
  'audit_events_chain_position_seq',
  coalesce((select max(chain_position) from audit_events), 0) + 1,
  false
);

alter table audit_events
  alter column chain_position set default nextval('audit_events_chain_position_seq');

alter table audit_events
  alter column chain_position set not null;

create unique index if not exists audit_events_chain_position_idx
  on audit_events (chain_position);
