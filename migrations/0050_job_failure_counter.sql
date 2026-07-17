-- A mutable count of currently failed jobs cannot be used with Prometheus
-- increase()/rate(). Track permanent job.failed transitions in the monotone
-- counter table so recent failure-rate alerts remain correct across retries,
-- reprocessing, and audit retention.
insert into metrics_counters (name, value)
values (
    'job_failures_total',
    (select count(*)::bigint from audit_events where event_type = 'job.failed')
)
on conflict (name) do nothing;
