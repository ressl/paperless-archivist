-- Real monotone counters for /metrics, decoupled from the prunable
-- `audit_events` table. The dashboard/Prometheus counters were previously
-- derived as live COUNT(*) gauges over `audit_events`, so retention pruning made
-- them decrease and corrupted rate() results. These rows are incremented once at
-- each source event and survive audit retention.
create table if not exists metrics_counters (
    name text primary key,
    value bigint not null default 0,
    updated_at timestamptz not null default now()
);

-- Seed the migrated series from the totals currently retained in `audit_events`
-- so the freshly-introduced monotone counters continue from their historical
-- value instead of resetting to zero at deploy. `on conflict do nothing` keeps
-- this idempotent if the migration is ever re-applied.
insert into metrics_counters (name, value)
values
    (
        'apply_success_total',
        (select count(*)::bigint from audit_events
          where event_type = 'document.patch_applied' and outcome = 'success')
    ),
    (
        'apply_failure_total',
        (select count(*)::bigint from audit_events
          where event_type = 'document.patch_apply_failed' and outcome = 'failed')
    ),
    (
        'selector_runs_total',
        (select count(*)::bigint from audit_events
          where event_type = 'workflow.selector_ran')
    ),
    (
        'selector_documents_queued_total',
        coalesce((
            select sum(coalesce((after ->> 'queued')::bigint, 0))::bigint
              from audit_events
             where event_type = 'workflow.selector_ran'
        ), 0)
    ),
    (
        'job_retries_scheduled_total',
        (select count(*)::bigint from audit_events
          where event_type = 'job.retry_scheduled')
    )
on conflict (name) do nothing;
