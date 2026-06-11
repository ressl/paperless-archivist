-- 0038: one-shot reconcile of document_inventory.current_run_status. #303
--
-- current_run_status is a hand-maintained mirror of pipeline_runs.status:
-- every write site that moves a run is expected to mirror the new status onto
-- the inventory row in the same transaction (create_run_with_jobs_on_tx,
-- claim_jobs, complete_job, fail_job, recover_stale_leases, recover_stuck_runs,
-- the review apply/reject paths, ...). Two sites broke that discipline:
--
--   * the worker's provider-cooldown lease release only updated `jobs`,
--     stranding the run (and the mirror) on 'running' with zero running jobs;
--   * the startup repair reset_stuck_running_pipeline_runs then flipped those
--     runs to 'queued'/'succeeded' WITHOUT mirroring.
--
-- In production ~10% of inventory rows (~600/5996) showed a stale 'running'
-- badge while the actual run was 'queued', polluting the inventory run-status
-- filter, the dashboard stage running counts and the running KPI. Both write
-- sites mirror as of this release (release_job_lease_for_cooldown and
-- reset_stuck_running_pipeline_runs in archivist-db); this migration repairs
-- the rows the old code already drifted.
--
-- Approach note: fully dropping the column in favour of deriving the active
-- run via pipeline_runs_one_active_per_document_idx was considered, but the
-- column is load-bearing across the API payload, openapi schema, frontend and
-- the queue_missing double-enqueue guards — too invasive for one step.
-- Restoring the mirror discipline plus this one-shot repair keeps semantics
-- identical everywhere; column removal can follow as its own migration.
--
-- Repair rule: copy the status of the run each inventory row points at.
-- last_run_id is written on every run creation (single insert site) and on
-- stage completion, so it is the authoritative link; the audited production
-- drift rows were all last_run_id-consistent. Rows without a run are left
-- alone. `is distinct from` keeps already-correct rows untouched.
update document_inventory di
   set current_run_status = pr.status,
       updated_at = now()
  from pipeline_runs pr
 where pr.id = di.last_run_id
   and di.current_run_status is distinct from pr.status;
