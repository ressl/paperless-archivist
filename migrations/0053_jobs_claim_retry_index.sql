-- #412: back the retry-bias pass of claim_jobs with its own ordered index.
--
-- claim_jobs used to OR `queued`/`running` in WHERE and lead its ORDER BY with
-- a CASE retry-bias expression, so `idx_jobs_claim` could not serve the sort
-- and every poll sorted the whole queued backlog. The claim now runs as
-- separate index-ordered passes (stale leases via `jobs_lease_until_idx`,
-- queued retries via this index, the rest via `idx_jobs_claim`).
--
-- Retries are a small subset of the queue, so this partial index stays tiny.
-- Its predicate must match the retry pass filter in
-- `archivist_db::claim_jobs_candidate_sql(ClaimPass::Retry)`.
create index if not exists idx_jobs_claim_retry
  on jobs (priority, stage_priority, run_after, created_at)
  where status = 'queued' and error_message is not null and attempts > 0;
