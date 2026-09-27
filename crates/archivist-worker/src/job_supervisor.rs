//! In-flight job supervision for the worker: panic capture (#402), the
//! lease-based no-progress watchdog and progress-aware liveness (#407), and the
//! lease keepalive for long pre-page OCR setup (#413).

use std::any::Any;
use std::collections::HashMap;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::sync::Mutex;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use anyhow::{Result, anyhow};
use chrono::{DateTime, Utc};
use futures::FutureExt;
use tokio::time::sleep;
use tracing::warn;
use uuid::Uuid;

/// Tracks when the claim loop last completed and, per in-flight job, the
/// latest instant by which it must show progress again. The liveness
/// heartbeat is only written while both are fresh, so a wedged claim loop or a
/// job that can neither progress nor be aborted stops the heartbeat. #407
#[derive(Debug)]
pub(crate) struct JobSupervisor {
    claim_cycle_at: AtomicI64,
    jobs: Mutex<HashMap<Uuid, i64>>,
}

impl JobSupervisor {
    pub(crate) fn new(now: i64) -> Self {
        Self {
            claim_cycle_at: AtomicI64::new(now),
            jobs: Mutex::new(HashMap::new()),
        }
    }

    fn jobs(&self) -> std::sync::MutexGuard<'_, HashMap<Uuid, i64>> {
        // A panic while holding this lock cannot leave the map inconsistent
        // (single insert/remove calls), so recover from poisoning.
        self.jobs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Register a job (or record progress for it): it must show progress
    /// again before `deadline` (unix seconds).
    pub(crate) fn touch(&self, job_id: Uuid, deadline: i64) {
        self.jobs().insert(job_id, deadline);
    }

    pub(crate) fn finish(&self, job_id: Uuid) {
        self.jobs().remove(&job_id);
    }

    pub(crate) fn in_flight(&self) -> usize {
        self.jobs().len()
    }

    pub(crate) fn claim_cycle_completed(&self, now: i64) {
        self.claim_cycle_at.store(now, Ordering::Release);
    }

    /// Healthy while the claim loop completed within `claim_stall_limit`
    /// seconds and no in-flight job is past its progress deadline.
    pub(crate) fn is_healthy(&self, now: i64, claim_stall_limit: i64) -> bool {
        let claim_fresh = now - self.claim_cycle_at.load(Ordering::Acquire) <= claim_stall_limit;
        claim_fresh && self.jobs().values().all(|deadline| *deadline >= now)
    }
}

/// Removes a job from the supervisor when its task ends — including on panic
/// unwinding and on `abort()`, so capacity accounting cannot leak.
pub(crate) struct InFlightGuard<'a> {
    pub(crate) supervisor: &'a JobSupervisor,
    pub(crate) job_id: Uuid,
}

impl Drop for InFlightGuard<'_> {
    fn drop(&mut self) {
        self.supervisor.finish(self.job_id);
    }
}

fn panic_message(payload: &(dyn Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_owned()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "non-string panic payload".to_owned()
    }
}

/// Turn a panic inside `job` into an error so the caller can `fail_job` it
/// with a message instead of losing it as a `JoinError` and leaving the lease
/// to expire. Other jobs are unaffected because each runs in its own task. #402
pub(crate) async fn catch_job_panic<F>(job: F) -> Result<()>
where
    F: Future<Output = Result<()>>,
{
    match AssertUnwindSafe(job).catch_unwind().await {
        Ok(result) => result,
        Err(payload) => Err(anyhow!(
            "job processing panicked: {}",
            panic_message(payload.as_ref())
        )),
    }
}

/// Outcome of [`watch_job_lease`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WatchdogVerdict {
    /// The job task finished on its own.
    Finished,
    /// The lease expired (plus grace) without renewal: the job made no
    /// progress within a whole lease window and must be aborted.
    Stalled,
    /// The lease now belongs to someone else (or was released).
    LeaseLost,
}

/// Poll the job's lease every `interval`. Every lease renewal is a unit of
/// progress, so a live lease reports progress via `on_progress` while a lease
/// that lapsed by more than `grace` means the job hung (e.g. a subprocess or
/// network call without its own bound). Probe errors (DB hiccup) neither
/// abort nor count as progress. #407
pub(crate) async fn watch_job_lease<Probe, ProbeFut, Done, Progress>(
    mut probe: Probe,
    done: Done,
    mut on_progress: Progress,
    interval: Duration,
    grace: chrono::Duration,
) -> WatchdogVerdict
where
    Probe: FnMut() -> ProbeFut,
    ProbeFut: Future<Output = Result<Option<DateTime<Utc>>>>,
    Done: Fn() -> bool,
    Progress: FnMut(),
{
    loop {
        sleep(interval).await;
        if done() {
            return WatchdogVerdict::Finished;
        }
        match probe().await {
            Ok(Some(lease_until)) if lease_until + grace < Utc::now() => {
                return if done() {
                    WatchdogVerdict::Finished
                } else {
                    WatchdogVerdict::Stalled
                };
            }
            Ok(Some(_)) => on_progress(),
            Ok(None) => {
                return if done() {
                    WatchdogVerdict::Finished
                } else {
                    WatchdogVerdict::LeaseLost
                };
            }
            Err(error) => warn!(error = %error, "job lease watchdog probe failed"),
        }
    }
}

/// Drive `work` while renewing the job lease every `interval`, so a long
/// pre-page phase (Paperless download up to 10x the HTTP timeout, pdfinfo +
/// pdftoppm render budget) can never outlive the lease without a renewal.
/// Returns `Ok(None)` when a renewal reports the lease lost; `work` is then
/// dropped (cancelling it, and killing `kill_on_drop` subprocesses). #413
pub(crate) async fn with_lease_keepalive<T, Work, Renew, RenewFut>(
    work: Work,
    mut renew: Renew,
    interval: Duration,
) -> Result<Option<T>>
where
    Work: Future<Output = T>,
    Renew: FnMut() -> RenewFut,
    RenewFut: Future<Output = Result<bool>>,
{
    tokio::pin!(work);
    loop {
        tokio::select! {
            output = &mut work => return Ok(Some(output)),
            _ = sleep(interval) => {
                if !renew().await? {
                    return Ok(None);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize};

    #[tokio::test]
    async fn panic_in_one_job_becomes_an_error_and_spares_the_others() {
        let panicking = tokio::spawn(catch_job_panic(async {
            if true {
                panic!("boom on page 3");
            }
            Ok(())
        }));
        let healthy = tokio::spawn(catch_job_panic(async { Ok(()) }));
        let error = panicking
            .await
            .expect("task itself must not report a JoinError")
            .expect_err("panic is surfaced as an error");
        assert_eq!(error.to_string(), "job processing panicked: boom on page 3");
        healthy
            .await
            .expect("join")
            .expect("sibling job is unaffected");
    }

    #[test]
    fn in_flight_guard_releases_the_slot_on_panic() {
        let supervisor = JobSupervisor::new(0);
        let job_id = Uuid::now_v7();
        supervisor.touch(job_id, 100);
        let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
            let _guard = InFlightGuard {
                supervisor: &supervisor,
                job_id,
            };
            panic!("unwind");
        }));
        assert!(result.is_err());
        assert_eq!(supervisor.in_flight(), 0);
    }

    #[test]
    fn liveness_requires_a_fresh_claim_cycle_and_progressing_jobs() {
        let supervisor = JobSupervisor::new(1_000);
        assert!(supervisor.is_healthy(1_100, 600), "idle worker is healthy");
        let job = Uuid::now_v7();
        supervisor.touch(job, 1_400);
        assert!(supervisor.is_healthy(1_300, 600));
        // The job did not progress before its deadline -> unhealthy.
        assert!(!supervisor.is_healthy(1_401, 600));
        supervisor.touch(job, 2_000);
        supervisor.claim_cycle_completed(1_401);
        assert!(supervisor.is_healthy(1_500, 600));
        // Claim loop wedged for longer than the limit -> unhealthy.
        assert!(!supervisor.is_healthy(2_002, 600));
        supervisor.finish(job);
        supervisor.claim_cycle_completed(2_002);
        assert!(supervisor.is_healthy(2_003, 600));
    }

    #[tokio::test]
    async fn watchdog_flags_an_expired_lease_as_stalled() {
        let progress = AtomicUsize::new(0);
        let calls = AtomicUsize::new(0);
        let verdict = watch_job_lease(
            || {
                // First probe: live lease; afterwards the lease has lapsed.
                let call = calls.fetch_add(1, Ordering::SeqCst);
                async move {
                    Ok(Some(if call == 0 {
                        Utc::now() + chrono::Duration::seconds(60)
                    } else {
                        Utc::now() - chrono::Duration::seconds(60)
                    }))
                }
            },
            || false,
            || {
                progress.fetch_add(1, Ordering::SeqCst);
            },
            Duration::from_millis(5),
            chrono::Duration::seconds(30),
        )
        .await;
        assert_eq!(verdict, WatchdogVerdict::Stalled);
        assert_eq!(progress.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn watchdog_reports_lost_lease_and_finished_jobs() {
        let verdict = watch_job_lease(
            || async { Ok(None) },
            || false,
            || {},
            Duration::from_millis(1),
            chrono::Duration::seconds(30),
        )
        .await;
        assert_eq!(verdict, WatchdogVerdict::LeaseLost);

        let finished = AtomicBool::new(true);
        let verdict = watch_job_lease(
            || async { Ok(Some(Utc::now() - chrono::Duration::hours(1))) },
            || finished.load(Ordering::SeqCst),
            || {},
            Duration::from_millis(1),
            chrono::Duration::seconds(30),
        )
        .await;
        assert_eq!(verdict, WatchdogVerdict::Finished);
    }

    #[tokio::test]
    async fn keepalive_renews_during_long_setup_and_stops_on_lost_lease() {
        let renewals = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&renewals);
        let output = with_lease_keepalive(
            async {
                sleep(Duration::from_millis(60)).await;
                7
            },
            move || {
                counter.fetch_add(1, Ordering::SeqCst);
                async { Ok(true) }
            },
            Duration::from_millis(10),
        )
        .await
        .expect("keepalive");
        assert_eq!(output, Some(7));
        assert!(
            renewals.load(Ordering::SeqCst) >= 2,
            "setup longer than the renewal interval must renew the lease"
        );

        let output = with_lease_keepalive(
            sleep(Duration::from_secs(60)),
            || async { Ok(false) },
            Duration::from_millis(5),
        )
        .await
        .expect("keepalive");
        assert!(output.is_none(), "lost lease cancels the setup work");
    }
}
