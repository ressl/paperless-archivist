//! Central status model for pipeline runs, jobs, review items and the
//! `document_inventory` run-status mirror. #439
//!
//! Before #439 the status columns were written from ~15 places with slightly
//! different guards (e.g. `complete_job` reopened `rejected` runs while the
//! review aggregate did not, three review-apply revert paths, several
//! hand-maintained copies of the "active run" list). This module is now the
//! single source for:
//!
//! * the status vocabularies and the shared status lists ("active run",
//!   "active job", "stage done"), both as Rust constants and as SQL-literal
//!   macros so static query strings can embed them via `concat!`;
//! * the run transition table ([`RunTransition`]) and its only writer,
//!   [`transition_runs_tx`], plus the inventory mirror
//!   ([`mirror_run_status_tx`]) that keeps `current_run_status` equal to the
//!   run `last_run_id` points at (#303, #414, #410);
//! * the review transition table ([`ReviewTransition`]) used to validate
//!   review status writes and the unified revert-from-`applying` path.
//!
//! Job status writes stay next to their lease-fenced SQL in `lib.rs`; their
//! allowed transitions are documented and tested in [`JobTransition`].

use anyhow::{Result, anyhow};
use sqlx::{Postgres, Row, Transaction};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// SQL literal lists. `concat!` only accepts literals, so the lists are macros;
// the unit tests below pin each one to its Rust constant.

/// `pipeline_runs` statuses that still own the document (not terminal).
macro_rules! sql_active_run_statuses {
    () => {
        "'queued', 'running', 'waiting_review', 'applying'"
    };
}

/// `pipeline_runs` statuses that are final.
macro_rules! sql_terminal_run_statuses {
    () => {
        "'succeeded', 'rejected', 'failed', 'cancelled'"
    };
}

/// `jobs` statuses that still need work or a decision.
macro_rules! sql_active_job_statuses {
    () => {
        "'queued', 'running', 'waiting_review'"
    };
}

/// Inventory stage statuses that count as done (see
/// [`crate::TERMINAL_STAGE_STATUSES`]). #410
macro_rules! sql_terminal_stage_statuses {
    () => {
        "'succeeded', 'skipped', 'not_needed', 'rejected'"
    };
}

pub(crate) use {
    sql_active_job_statuses, sql_active_run_statuses, sql_terminal_run_statuses,
    sql_terminal_stage_statuses,
};

// ---------------------------------------------------------------------------
// Runs

/// `pipeline_runs.status` (CHECK in migrations 0001/0033; mirrored by
/// `document_inventory.current_run_status`, CHECK in 0043).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RunStatus {
    Queued,
    Running,
    WaitingReview,
    Applying,
    Succeeded,
    Rejected,
    Failed,
    Cancelled,
}

impl RunStatus {
    pub const ALL: &'static [RunStatus] = &[
        RunStatus::Queued,
        RunStatus::Running,
        RunStatus::WaitingReview,
        RunStatus::Applying,
        RunStatus::Succeeded,
        RunStatus::Rejected,
        RunStatus::Failed,
        RunStatus::Cancelled,
    ];
    /// Runs that still own their document. Same set as the partial unique
    /// index from migration 0001 and [`sql_active_run_statuses!`].
    pub const ACTIVE: &'static [RunStatus] = &[
        RunStatus::Queued,
        RunStatus::Running,
        RunStatus::WaitingReview,
        RunStatus::Applying,
    ];
    pub const TERMINAL: &'static [RunStatus] = &[
        RunStatus::Succeeded,
        RunStatus::Rejected,
        RunStatus::Failed,
        RunStatus::Cancelled,
    ];
    /// Runs a worker claim may (re)enter `running` from.
    const CLAIMABLE: &'static [RunStatus] = &[
        RunStatus::Queued,
        RunStatus::Running,
        RunStatus::WaitingReview,
    ];
    /// Runs the operator/stuck-run recovery considers stalled.
    const RECOVERABLE: &'static [RunStatus] =
        &[RunStatus::Queued, RunStatus::Running, RunStatus::Applying];

    pub const fn as_str(self) -> &'static str {
        match self {
            RunStatus::Queued => "queued",
            RunStatus::Running => "running",
            RunStatus::WaitingReview => "waiting_review",
            RunStatus::Applying => "applying",
            RunStatus::Succeeded => "succeeded",
            RunStatus::Rejected => "rejected",
            RunStatus::Failed => "failed",
            RunStatus::Cancelled => "cancelled",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|s| s.as_str() == value)
    }

    pub fn is_active(self) -> bool {
        Self::ACTIVE.contains(&self)
    }
}

/// How a transition treats `pipeline_runs.finished_at`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FinishedAt {
    Keep,
    Now,
    /// Recovery paths keep an already recorded finish time.
    CoalesceNow,
    /// Reopening a terminal run.
    Clear,
}

impl FinishedAt {
    const fn as_str(self) -> &'static str {
        match self {
            FinishedAt::Keep => "keep",
            FinishedAt::Now => "now",
            FinishedAt::CoalesceNow => "coalesce",
            FinishedAt::Clear => "clear",
        }
    }
}

/// Every event that changes `pipeline_runs.status`. The table
/// ([`RunTransition::from`] / [`RunTransition::to`]) is enforced by the
/// WHERE clause of [`transition_runs_tx`]: a run outside the `from` set is
/// left untouched and not reported as transitioned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RunTransition {
    /// `claim_jobs` leased one of the run's jobs.
    Claimed,
    /// A stage finished and a later stage is still pending
    /// (`complete_job`, review aggregate).
    StageAdvanced,
    /// The last active job finished (`complete_job`, review aggregate).
    Succeeded,
    /// A job failed permanently (`fail_job`, #402 exhausted stale lease).
    Failed,
    /// A stage produced review items (`create_review_item`).
    AwaitingReview,
    /// Every review item of the reviewed stage was rejected.
    Rejected,
    /// The only running job was released for a provider cooldown (#253/#303).
    LeaseReleased,
    /// Operator requeue of expired leases (`recover_stale_leases`).
    StaleLeaseRequeued,
    /// Operator/stuck-run recovery: all jobs succeeded.
    RecoveredSucceeded,
    /// Operator/stuck-run recovery: no active jobs left.
    RecoveredFailed,
    /// Startup repair: vision-crash requeue gives a failed run one more try (#406).
    VisionCrashRequeued,
    /// Startup repair: OCR-only run reopened for the backfilled metadata stage.
    MetadataBackfilled,
    /// Startup repair: `running` run without a running job but with pending work.
    StuckRunningRequeued,
    /// Startup repair: `running` run whose jobs all settled.
    StuckRunningSucceeded,
}

impl RunTransition {
    pub const ALL: &'static [RunTransition] = &[
        RunTransition::Claimed,
        RunTransition::StageAdvanced,
        RunTransition::Succeeded,
        RunTransition::Failed,
        RunTransition::AwaitingReview,
        RunTransition::Rejected,
        RunTransition::LeaseReleased,
        RunTransition::StaleLeaseRequeued,
        RunTransition::RecoveredSucceeded,
        RunTransition::RecoveredFailed,
        RunTransition::VisionCrashRequeued,
        RunTransition::MetadataBackfilled,
        RunTransition::StuckRunningRequeued,
        RunTransition::StuckRunningSucceeded,
    ];

    /// Source statuses the transition may fire from.
    pub const fn from(self) -> &'static [RunStatus] {
        match self {
            RunTransition::Claimed => RunStatus::CLAIMABLE,
            // #439: `complete_job` used to exclude only succeeded/failed/
            // cancelled here (so it could reopen a `rejected` run) while the
            // review aggregate also excluded `rejected`; both now share the
            // active set. The live `Succeeded`/`Failed`/`AwaitingReview`
            // writes had no guard at all and could overwrite a terminal run.
            RunTransition::StageAdvanced
            | RunTransition::Succeeded
            | RunTransition::Failed
            | RunTransition::AwaitingReview
            | RunTransition::Rejected => RunStatus::ACTIVE,
            RunTransition::LeaseReleased
            | RunTransition::StaleLeaseRequeued
            | RunTransition::StuckRunningRequeued
            | RunTransition::StuckRunningSucceeded => &[RunStatus::Running],
            RunTransition::RecoveredSucceeded | RunTransition::RecoveredFailed => {
                RunStatus::RECOVERABLE
            }
            RunTransition::VisionCrashRequeued => &[RunStatus::Failed],
            RunTransition::MetadataBackfilled => &[RunStatus::Succeeded],
        }
    }

    pub const fn to(self) -> RunStatus {
        match self {
            RunTransition::Claimed => RunStatus::Running,
            RunTransition::StageAdvanced
            | RunTransition::LeaseReleased
            | RunTransition::StaleLeaseRequeued
            | RunTransition::VisionCrashRequeued
            | RunTransition::MetadataBackfilled
            | RunTransition::StuckRunningRequeued => RunStatus::Queued,
            RunTransition::Succeeded
            | RunTransition::RecoveredSucceeded
            | RunTransition::StuckRunningSucceeded => RunStatus::Succeeded,
            RunTransition::Failed | RunTransition::RecoveredFailed => RunStatus::Failed,
            RunTransition::AwaitingReview => RunStatus::WaitingReview,
            RunTransition::Rejected => RunStatus::Rejected,
        }
    }

    const fn finished_at(self) -> FinishedAt {
        match self {
            RunTransition::Succeeded
            | RunTransition::Failed
            | RunTransition::Rejected
            | RunTransition::StuckRunningSucceeded => FinishedAt::Now,
            RunTransition::RecoveredSucceeded | RunTransition::RecoveredFailed => {
                FinishedAt::CoalesceNow
            }
            RunTransition::VisionCrashRequeued | RunTransition::MetadataBackfilled => {
                FinishedAt::Clear
            }
            RunTransition::Claimed
            | RunTransition::StageAdvanced
            | RunTransition::AwaitingReview
            | RunTransition::LeaseReleased
            | RunTransition::StaleLeaseRequeued
            | RunTransition::StuckRunningRequeued => FinishedAt::Keep,
        }
    }

    /// Whether the transition also requires that none of the run's jobs is
    /// still `running` (the cooldown release and the stuck-running repair).
    const fn requires_no_running_job(self) -> bool {
        matches!(
            self,
            RunTransition::LeaseReleased
                | RunTransition::StuckRunningRequeued
                | RunTransition::StuckRunningSucceeded
        )
    }

    /// Whether the transition writes `error_message` (`Some`) and with what:
    /// failures record the caller's error, a requeue clears it.
    const fn writes_error(self) -> bool {
        matches!(
            self,
            RunTransition::Failed
                | RunTransition::RecoveredFailed
                | RunTransition::VisionCrashRequeued
        )
    }

    /// Pure table lookup used by the tests and by debug assertions.
    pub fn allows(self, from: RunStatus) -> bool {
        self.from().contains(&from)
    }
}

/// Error message recorded by [`RunTransition::RecoveredFailed`].
pub const RECOVERED_STUCK_RUN_ERROR: &str = "Recovered stuck run with no active jobs";

/// The only writer of `pipeline_runs.status`. Applies `event` to every run in
/// `run_ids` whose current status is in `event.from()`, and returns the ids
/// that actually transitioned. `error` is recorded for failure events (and
/// ignored otherwise); a requeue clears the previous error. Does not touch
/// `document_inventory` — callers follow up with [`mirror_run_status_tx`] for
/// the runs whose badge should follow (usually the same ids). #439
pub(crate) async fn transition_runs_tx(
    tx: &mut Transaction<'_, Postgres>,
    run_ids: &[Uuid],
    event: RunTransition,
    error: Option<&str>,
) -> Result<Vec<Uuid>> {
    if run_ids.is_empty() {
        return Ok(Vec::new());
    }
    let from: Vec<&str> = event.from().iter().map(|s| s.as_str()).collect();
    let error = match event {
        RunTransition::Failed => Some(error.ok_or_else(|| anyhow!("run failure without error"))?),
        RunTransition::RecoveredFailed => Some(error.unwrap_or(RECOVERED_STUCK_RUN_ERROR)),
        _ => None,
    };
    let rows = sqlx::query(
        r#"
        update pipeline_runs r
           set status = $2,
               started_at = case when $2 = 'running' then coalesce(r.started_at, now()) else r.started_at end,
               finished_at = case $4
                 when 'now' then now()
                 when 'coalesce' then coalesce(r.finished_at, now())
                 when 'clear' then null
                 else r.finished_at
               end,
               error_message = case when $6 then $7 else r.error_message end,
               updated_at = now()
         where r.id = any($1)
           and r.status = any($3)
           and (not $5 or not exists (
             select 1 from jobs j where j.run_id = r.id and j.status = 'running'
           ))
        returning r.id
        "#,
    )
    .bind(run_ids)
    .bind(event.to().as_str())
    .bind(&from)
    .bind(event.finished_at().as_str())
    .bind(event.requires_no_running_job())
    .bind(event.writes_error())
    .bind(error)
    .fetch_all(&mut **tx)
    .await?;
    rows.iter()
        .map(|row| row.try_get::<Uuid, _>("id").map_err(Into::into))
        .collect()
}

/// Single-run convenience wrapper around [`transition_runs_tx`]; returns
/// whether the run transitioned.
pub(crate) async fn transition_run_tx(
    tx: &mut Transaction<'_, Postgres>,
    run_id: Uuid,
    event: RunTransition,
    error: Option<&str>,
) -> Result<bool> {
    Ok(!transition_runs_tx(tx, &[run_id], event, error)
        .await?
        .is_empty())
}

/// Copy each run's status onto the inventory row whose `last_run_id` points at
/// it — the #303 mirror invariant, guarded on `last_run_id` so a superseded
/// run never overwrites the badge of the run that now owns the row (#414).
/// `complete` is re-derived from the Paperless completion tag on the same
/// write (#410). `failed_error` (when given) is copied into `last_error` for
/// rows whose run is `failed`. Rows are locked in `paperless_document_id`
/// order, the order the batched sync upserts use, so the two cannot deadlock
/// (#408). Returns the number of inventory rows changed.
pub(crate) async fn mirror_run_status_tx(
    tx: &mut Transaction<'_, Postgres>,
    run_ids: &[Uuid],
    failed_error: Option<&str>,
) -> Result<u64> {
    if run_ids.is_empty() {
        return Ok(0);
    }
    let updated = sqlx::query(
        r#"
        with locked as (
          select di.paperless_document_id
            from document_inventory di
           where di.last_run_id = any($1)
           order by di.paperless_document_id
           for update
        )
        update document_inventory di
           set current_run_status = pr.status,
               complete = di.has_full_completion_tag,
               last_error = case
                 when pr.status = 'failed' and $2::text is not null then $2
                 else di.last_error
               end,
               updated_at = now()
          from locked, pipeline_runs pr
         where di.paperless_document_id = locked.paperless_document_id
           and pr.id = di.last_run_id
           and (
             di.current_run_status is distinct from pr.status
             or di.complete is distinct from di.has_full_completion_tag
             or (pr.status = 'failed' and $2::text is not null
                 and di.last_error is distinct from $2)
           )
        "#,
    )
    .bind(run_ids)
    .bind(failed_error)
    .execute(&mut **tx)
    .await?;
    Ok(updated.rows_affected())
}

// ---------------------------------------------------------------------------
// Jobs

/// `jobs.status` (CHECK in migration 0001).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum JobStatus {
    Queued,
    Running,
    WaitingReview,
    Succeeded,
    Failed,
    Cancelled,
}

impl JobStatus {
    pub const ALL: &'static [JobStatus] = &[
        JobStatus::Queued,
        JobStatus::Running,
        JobStatus::WaitingReview,
        JobStatus::Succeeded,
        JobStatus::Failed,
        JobStatus::Cancelled,
    ];
    /// Jobs that still need work or a decision ([`sql_active_job_statuses!`]).
    pub const ACTIVE: &'static [JobStatus] = &[
        JobStatus::Queued,
        JobStatus::Running,
        JobStatus::WaitingReview,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            JobStatus::Queued => "queued",
            JobStatus::Running => "running",
            JobStatus::WaitingReview => "waiting_review",
            JobStatus::Succeeded => "succeeded",
            JobStatus::Failed => "failed",
            JobStatus::Cancelled => "cancelled",
        }
    }
}

/// Allowed job status changes and their writers. Job writes stay inline in
/// `lib.rs` because each one is fenced on lease ownership inside its own
/// UPDATE; this table documents and tests the guards those statements use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum JobTransition {
    /// `claim_jobs` (queued, or running with an expired lease).
    Claimed,
    /// `complete_job` (lease-fenced).
    Completed,
    /// `fail_job` with retry budget left.
    RetryScheduled,
    /// `fail_job` without retry budget; #402 exhausted stale lease.
    Failed,
    /// `release_job_lease_for_cooldown`, `recover_stale_leases`.
    LeaseReleased,
    /// `create_review_item`.
    AwaitingReview,
    /// Review aggregate: at least one item applied.
    ReviewSucceeded,
    /// Review aggregate (all rejected), permanent sibling failure.
    Cancelled,
    /// Startup vision-crash requeue (#406) for the failed OCR job and its
    /// cancelled siblings.
    Requeued,
}

impl JobTransition {
    pub const ALL: &'static [JobTransition] = &[
        JobTransition::Claimed,
        JobTransition::Completed,
        JobTransition::RetryScheduled,
        JobTransition::Failed,
        JobTransition::LeaseReleased,
        JobTransition::AwaitingReview,
        JobTransition::ReviewSucceeded,
        JobTransition::Cancelled,
        JobTransition::Requeued,
    ];

    pub const fn from(self) -> &'static [JobStatus] {
        match self {
            JobTransition::Claimed => &[JobStatus::Queued, JobStatus::Running],
            // `complete_job`/`fail_job` fence on the lease owner; a leased
            // job is `running`, or `waiting_review` once a review item exists.
            JobTransition::Completed | JobTransition::RetryScheduled | JobTransition::Failed => {
                &[JobStatus::Running, JobStatus::WaitingReview]
            }
            JobTransition::LeaseReleased => &[JobStatus::Running],
            JobTransition::AwaitingReview => &[JobStatus::Running, JobStatus::WaitingReview],
            JobTransition::ReviewSucceeded => &[JobStatus::WaitingReview],
            JobTransition::Cancelled => JobStatus::ACTIVE,
            JobTransition::Requeued => &[JobStatus::Failed, JobStatus::Cancelled],
        }
    }

    pub const fn to(self) -> JobStatus {
        match self {
            JobTransition::Claimed => JobStatus::Running,
            JobTransition::Completed | JobTransition::ReviewSucceeded => JobStatus::Succeeded,
            JobTransition::RetryScheduled
            | JobTransition::LeaseReleased
            | JobTransition::Requeued => JobStatus::Queued,
            JobTransition::Failed => JobStatus::Failed,
            JobTransition::AwaitingReview => JobStatus::WaitingReview,
            JobTransition::Cancelled => JobStatus::Cancelled,
        }
    }

    pub fn allows(self, from: JobStatus) -> bool {
        self.from().contains(&from)
    }
}

// ---------------------------------------------------------------------------
// Review items

/// `review_items.status` (CHECK in migration 0033).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReviewStatus {
    Pending,
    Approved,
    Edited,
    Rejected,
    Applying,
    Applied,
}

impl ReviewStatus {
    pub const ALL: &'static [ReviewStatus] = &[
        ReviewStatus::Pending,
        ReviewStatus::Approved,
        ReviewStatus::Edited,
        ReviewStatus::Rejected,
        ReviewStatus::Applying,
        ReviewStatus::Applied,
    ];
    /// Final review states; the job aggregate settles once every sibling is
    /// in one of them.
    pub const TERMINAL: &'static [ReviewStatus] = &[ReviewStatus::Applied, ReviewStatus::Rejected];

    pub const fn as_str(self) -> &'static str {
        match self {
            ReviewStatus::Pending => "pending",
            ReviewStatus::Approved => "approved",
            ReviewStatus::Edited => "edited",
            ReviewStatus::Rejected => "rejected",
            ReviewStatus::Applying => "applying",
            ReviewStatus::Applied => "applied",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|s| s.as_str() == value)
    }
}

/// Every event that changes `review_items.status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReviewTransition {
    /// Human decision (`review_decision`): approve, edit or reject.
    Decided(ReviewStatus),
    /// Human apply claim (`claim_review_for_apply`).
    ClaimedForApply,
    /// Autopilot drain claim straight from `pending`.
    ClaimedForDrain,
    /// Paperless PATCH confirmed (`mark_review_applied[_auto]`).
    Applied,
    /// Apply failed or hit an optimistic-concurrency conflict; the single
    /// revert path for human, drain and recovery (#388, #439).
    ApplyReverted(ReviewStatus),
    /// Stale sweep (`reset_stale_applying_reviews`, #253/#388).
    StaleReset,
}

impl ReviewTransition {
    pub fn from(self) -> &'static [ReviewStatus] {
        match self {
            ReviewTransition::Decided(_) | ReviewTransition::ClaimedForDrain => {
                &[ReviewStatus::Pending]
            }
            ReviewTransition::ClaimedForApply => &[ReviewStatus::Approved, ReviewStatus::Edited],
            ReviewTransition::Applied | ReviewTransition::ApplyReverted(_) => {
                &[ReviewStatus::Applying]
            }
            ReviewTransition::StaleReset => &[
                ReviewStatus::Applying,
                ReviewStatus::Approved,
                ReviewStatus::Edited,
            ],
        }
    }

    /// Target status, or `None` when the event's parameter is not a legal
    /// target (e.g. deciding `applied`, reverting to `rejected`).
    pub fn to(self) -> Option<ReviewStatus> {
        match self {
            ReviewTransition::Decided(
                to @ (ReviewStatus::Approved | ReviewStatus::Edited | ReviewStatus::Rejected),
            ) => Some(to),
            ReviewTransition::Decided(_) => None,
            ReviewTransition::ClaimedForApply | ReviewTransition::ClaimedForDrain => {
                Some(ReviewStatus::Applying)
            }
            ReviewTransition::Applied => Some(ReviewStatus::Applied),
            // `approved`/`edited` stay accepted for legacy intents persisted
            // with that `review_revert_status` (#388).
            ReviewTransition::ApplyReverted(
                to @ (ReviewStatus::Pending | ReviewStatus::Approved | ReviewStatus::Edited),
            ) => Some(to),
            ReviewTransition::ApplyReverted(_) => None,
            ReviewTransition::StaleReset => Some(ReviewStatus::Pending),
        }
    }

    pub fn allows(self, from: ReviewStatus) -> bool {
        self.to().is_some() && self.from().contains(&from)
    }
}

/// Parse and validate the target of a revert from `applying`.
pub(crate) fn review_revert_target(to_status: &str) -> Result<ReviewStatus> {
    ReviewStatus::parse(to_status)
        .filter(|to| ReviewTransition::ApplyReverted(*to).to().is_some())
        .ok_or_else(|| anyhow!("invalid review revert status {to_status}"))
}

/// The single revert-from-`applying` write used by the human apply failure
/// path, the autopilot drain failure path, the intent recovery and the
/// optimistic-concurrency conflict path (which additionally records the
/// conflicting fields). A revert to `pending` clears `reviewed_at` so the
/// stale sweep timer restarts from the next decision. Returns the reverted
/// row's `(run_id, job_id, paperless_document_id, stage)` or `None` when the
/// review was no longer `applying`. #439
pub(crate) async fn revert_review_from_applying_tx(
    tx: &mut Transaction<'_, Postgres>,
    review_id: Uuid,
    to: ReviewStatus,
    conflict_fields: Option<&serde_json::Value>,
) -> Result<Option<sqlx::postgres::PgRow>> {
    if ReviewTransition::ApplyReverted(to).to().is_none() {
        return Err(anyhow!("invalid review revert status {}", to.as_str()));
    }
    let row = sqlx::query(
        r#"
        update review_items
           set status = $2,
               reviewed_at = case when $2 = 'pending' then null else reviewed_at end,
               conflict_fields = coalesce($3, conflict_fields),
               conflicted_at = case when $3 is null then conflicted_at else now() end
         where id = $1 and status = 'applying'
        returning run_id, job_id, paperless_document_id, stage
        "#,
    )
    .bind(review_id)
    .bind(to.as_str())
    .bind(conflict_fields)
    .fetch_optional(&mut **tx)
    .await?;
    Ok(row)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sql_list(values: &[&str]) -> String {
        values
            .iter()
            .map(|v| format!("'{v}'"))
            .collect::<Vec<_>>()
            .join(", ")
    }

    #[test]
    fn sql_list_macros_match_rust_constants() {
        let runs: Vec<&str> = RunStatus::ACTIVE.iter().map(|s| s.as_str()).collect();
        assert_eq!(sql_active_run_statuses!(), sql_list(&runs));
        let terminal: Vec<&str> = RunStatus::TERMINAL.iter().map(|s| s.as_str()).collect();
        assert_eq!(sql_terminal_run_statuses!(), sql_list(&terminal));
        let jobs: Vec<&str> = JobStatus::ACTIVE.iter().map(|s| s.as_str()).collect();
        assert_eq!(sql_active_job_statuses!(), sql_list(&jobs));
        assert_eq!(
            sql_terminal_stage_statuses!(),
            sql_list(crate::TERMINAL_STAGE_STATUSES)
        );
    }

    #[test]
    fn run_status_partition_is_complete() {
        for status in RunStatus::ALL {
            assert_ne!(
                RunStatus::ACTIVE.contains(status),
                RunStatus::TERMINAL.contains(status),
                "{status:?} must be exactly one of active/terminal"
            );
            assert_eq!(RunStatus::parse(status.as_str()), Some(*status));
        }
        assert_eq!(RunStatus::parse("bogus"), None);
    }

    /// Expected run transition table, written out independently of the
    /// `from`/`to` implementation: (event, from, to).
    const EXPECTED_RUN_TABLE: &[(RunTransition, &[&str], &str)] = &[
        (
            RunTransition::Claimed,
            &["queued", "running", "waiting_review"],
            "running",
        ),
        (
            RunTransition::StageAdvanced,
            &["queued", "running", "waiting_review", "applying"],
            "queued",
        ),
        (
            RunTransition::Succeeded,
            &["queued", "running", "waiting_review", "applying"],
            "succeeded",
        ),
        (
            RunTransition::Failed,
            &["queued", "running", "waiting_review", "applying"],
            "failed",
        ),
        (
            RunTransition::AwaitingReview,
            &["queued", "running", "waiting_review", "applying"],
            "waiting_review",
        ),
        (
            RunTransition::Rejected,
            &["queued", "running", "waiting_review", "applying"],
            "rejected",
        ),
        (RunTransition::LeaseReleased, &["running"], "queued"),
        (RunTransition::StaleLeaseRequeued, &["running"], "queued"),
        (
            RunTransition::RecoveredSucceeded,
            &["queued", "running", "applying"],
            "succeeded",
        ),
        (
            RunTransition::RecoveredFailed,
            &["queued", "running", "applying"],
            "failed",
        ),
        (RunTransition::VisionCrashRequeued, &["failed"], "queued"),
        (RunTransition::MetadataBackfilled, &["succeeded"], "queued"),
        (RunTransition::StuckRunningRequeued, &["running"], "queued"),
        (
            RunTransition::StuckRunningSucceeded,
            &["running"],
            "succeeded",
        ),
    ];

    #[test]
    fn run_transition_table_allows_and_forbids_exactly_the_expected_edges() {
        assert_eq!(EXPECTED_RUN_TABLE.len(), RunTransition::ALL.len());
        for (event, from, to) in EXPECTED_RUN_TABLE {
            assert_eq!(event.to().as_str(), *to, "{event:?} target");
            for status in RunStatus::ALL {
                assert_eq!(
                    event.allows(*status),
                    from.contains(&status.as_str()),
                    "{event:?} from {status:?}"
                );
            }
        }
    }

    #[test]
    fn no_live_run_transition_leaves_a_terminal_status() {
        // Only the two startup repairs may reopen a terminal run; everything
        // else must fire from an active run. #439
        for event in RunTransition::ALL {
            let reopens = event.from().iter().any(|s| !s.is_active());
            assert_eq!(
                reopens,
                matches!(
                    event,
                    RunTransition::VisionCrashRequeued | RunTransition::MetadataBackfilled
                ),
                "{event:?}"
            );
        }
        for terminal in RunStatus::TERMINAL {
            for event in [
                RunTransition::Claimed,
                RunTransition::StageAdvanced,
                RunTransition::Succeeded,
                RunTransition::Failed,
                RunTransition::AwaitingReview,
                RunTransition::Rejected,
            ] {
                assert!(!event.allows(*terminal), "{event:?} from {terminal:?}");
            }
        }
    }

    #[test]
    fn failure_transitions_finish_and_requeues_reopen() {
        assert_eq!(RunTransition::Failed.finished_at(), FinishedAt::Now);
        assert_eq!(
            RunTransition::RecoveredFailed.finished_at(),
            FinishedAt::CoalesceNow
        );
        assert_eq!(
            RunTransition::VisionCrashRequeued.finished_at(),
            FinishedAt::Clear
        );
        assert!(RunTransition::Failed.writes_error());
        assert!(RunTransition::VisionCrashRequeued.writes_error());
        assert!(!RunTransition::StageAdvanced.writes_error());
        for event in RunTransition::ALL {
            if event.to().is_active() {
                assert_ne!(event.finished_at(), FinishedAt::Now, "{event:?}");
            }
        }
    }

    #[test]
    fn job_transition_table() {
        use JobStatus::*;
        let expected: &[(JobTransition, &[JobStatus], JobStatus)] = &[
            (JobTransition::Claimed, &[Queued, Running], Running),
            (
                JobTransition::Completed,
                &[Running, WaitingReview],
                Succeeded,
            ),
            (
                JobTransition::RetryScheduled,
                &[Running, WaitingReview],
                Queued,
            ),
            (JobTransition::Failed, &[Running, WaitingReview], Failed),
            (JobTransition::LeaseReleased, &[Running], Queued),
            (
                JobTransition::AwaitingReview,
                &[Running, WaitingReview],
                WaitingReview,
            ),
            (JobTransition::ReviewSucceeded, &[WaitingReview], Succeeded),
            (
                JobTransition::Cancelled,
                &[Queued, Running, WaitingReview],
                Cancelled,
            ),
            (JobTransition::Requeued, &[Failed, Cancelled], Queued),
        ];
        assert_eq!(expected.len(), JobTransition::ALL.len());
        for (event, from, to) in expected {
            assert_eq!(event.to(), *to, "{event:?}");
            for status in JobStatus::ALL {
                assert_eq!(
                    event.allows(*status),
                    from.contains(status),
                    "{event:?} {status:?}"
                );
            }
        }
        // A succeeded job is never touched again.
        for event in JobTransition::ALL {
            assert!(!event.allows(Succeeded), "{event:?}");
        }
    }

    #[test]
    fn review_transition_table() {
        use ReviewStatus::*;
        let cases: &[(ReviewTransition, &[ReviewStatus], Option<ReviewStatus>)] = &[
            (
                ReviewTransition::Decided(Approved),
                &[Pending],
                Some(Approved),
            ),
            (ReviewTransition::Decided(Edited), &[Pending], Some(Edited)),
            (
                ReviewTransition::Decided(Rejected),
                &[Pending],
                Some(Rejected),
            ),
            (ReviewTransition::Decided(Applied), &[], None),
            (ReviewTransition::Decided(Pending), &[], None),
            (
                ReviewTransition::ClaimedForApply,
                &[Approved, Edited],
                Some(Applying),
            ),
            (
                ReviewTransition::ClaimedForDrain,
                &[Pending],
                Some(Applying),
            ),
            (ReviewTransition::Applied, &[Applying], Some(Applied)),
            (
                ReviewTransition::ApplyReverted(Pending),
                &[Applying],
                Some(Pending),
            ),
            (
                ReviewTransition::ApplyReverted(Approved),
                &[Applying],
                Some(Approved),
            ),
            (
                ReviewTransition::ApplyReverted(Edited),
                &[Applying],
                Some(Edited),
            ),
            (ReviewTransition::ApplyReverted(Rejected), &[], None),
            (ReviewTransition::ApplyReverted(Applied), &[], None),
            (
                ReviewTransition::StaleReset,
                &[Applying, Approved, Edited],
                Some(Pending),
            ),
        ];
        for (event, from, to) in cases {
            assert_eq!(event.to(), *to, "{event:?}");
            for status in ReviewStatus::ALL {
                assert_eq!(
                    event.allows(*status),
                    from.contains(status),
                    "{event:?} {status:?}"
                );
            }
        }
        // Terminal review states never move again.
        for terminal in ReviewStatus::TERMINAL {
            for (event, _, _) in cases {
                assert!(!event.allows(*terminal), "{event:?} from {terminal:?}");
            }
        }
        assert!(review_revert_target("pending").is_ok());
        assert!(review_revert_target("approved").is_ok());
        assert!(review_revert_target("rejected").is_err());
        assert!(review_revert_target("applied").is_err());
        assert!(review_revert_target("bogus").is_err());
    }
}
