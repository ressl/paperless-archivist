//! Processing-failure classification and provider quota cooldowns.

use std::time::Duration;

use anyhow::{Context, Result};
use archivist_ai::{AiProviderError, MetadataContractError};
use archivist_core::{AuditEventInput, RuntimeSettings, Stage};
use archivist_db::{DbPool, JobRecord, release_job_lease_for_cooldown};
use archivist_paperless::PaperlessError;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use serde_json::json;
use tracing::warn;

use crate::provider_name_for_stage;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProcessingFailureClass {
    Transient,
    /// A transient failure of the Paperless *gateway/infrastructure* — the
    /// system of record is briefly unreachable (network, timeout, 5xx, or a
    /// gateway-404 mid-restart, #245). Distinct from `Transient` because an
    /// upstream outage blocks *every* job at once, so failing each document
    /// against its small `max_attempts` budget permanently loses the whole
    /// backlog for an outage longer than ~1 h. Retried against a higher,
    /// bounded ceiling instead so the documents ride the outage out. #305.
    TransientInfra,
    Permanent,
    /// Provider replied with a hard usage-cap signal (Ollama Cloud weekly,
    /// OpenAI tier monthly, …). Not retryable — the worker writes a
    /// per-provider cooldown so subsequent claims of jobs that would route
    /// to the same provider are short-circuited until the cap resets.
    ProviderQuota,
}

impl ProcessingFailureClass {
    pub(crate) fn is_retryable(self) -> bool {
        matches!(self, Self::Transient | Self::TransientInfra)
    }

    /// Retry-budget ceiling for `fail_job`: `None` uses the per-job
    /// `max_attempts`; `TransientInfra` raises it to ride out an upstream
    /// outage (bounded — see [`PAPERLESS_INFRA_RETRY_CEILING`]).
    pub(crate) fn retry_ceiling(self) -> Option<i32> {
        match self {
            Self::TransientInfra => Some(PAPERLESS_INFRA_RETRY_CEILING),
            _ => None,
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Transient => "transient",
            Self::TransientInfra => "transient_infra",
            Self::Permanent => "permanent",
            Self::ProviderQuota => "provider_quota",
        }
    }
}

/// Bounded retry ceiling for a Paperless infrastructure outage
/// (`ProcessingFailureClass::TransientInfra`). With `fail_job`'s exponential
/// backoff capped at ~32 min, 20 attempts span ~8.5 h — long enough to ride
/// out a realistic gateway outage/restart, short enough that a *permanently*
/// broken gateway still surfaces as a failed job instead of looping forever
/// (unlike the provider-cooldown release, which is unbounded by design). #305.
const PAPERLESS_INFRA_RETRY_CEILING: i32 = 20;

/// Decide whether `error` should be retried with backoff (Transient) or marked
/// permanent. The function first walks the error chain looking for typed
/// errors from `archivist-paperless` and `archivist-ai`; those carry an
/// authoritative `is_transient()` classification and bypass substring guesses.
/// Anything else — DB driver errors, `reqwest::Error` raised outside the typed
/// wrappers, third-party HTTP clients — falls through to substring matching as
/// a documented last resort.
pub(crate) fn classify_processing_failure(error: &anyhow::Error) -> ProcessingFailureClass {
    for cause in error.chain() {
        if let Some(paperless_error) = cause.downcast_ref::<PaperlessError>() {
            // A transient Paperless failure is an *infrastructure* outage of the
            // system of record (network/timeout/5xx/gateway-404) — it blocks
            // every job, so grant the higher bounded retry budget rather than
            // burning each document's small `max_attempts`. #305.
            return if paperless_error.is_transient() {
                ProcessingFailureClass::TransientInfra
            } else {
                ProcessingFailureClass::Permanent
            };
        }
        if let Some(ai_error) = cause.downcast_ref::<AiProviderError>() {
            return match ai_error {
                AiProviderError::QuotaExhausted { .. } => ProcessingFailureClass::ProviderQuota,
                e if e.is_transient() => ProcessingFailureClass::Transient,
                _ => ProcessingFailureClass::Permanent,
            };
        }
        if cause.downcast_ref::<MetadataContractError>().is_some() {
            // The provider responded, but violated the metadata schema. Retry
            // because a fresh generation can recover; after the normal job
            // budget is exhausted, fail_job makes the violation visible.
            return ProcessingFailureClass::Transient;
        }
    }

    // Last-resort substring matcher: covers errors that arise *outside* the
    // typed surfaces — sqlx pool errors, reqwest errors from helpers that
    // still use `anyhow!`, raw HTTP responses, etc. Any new error path
    // should prefer adding a typed variant in the originating crate so this
    // table can keep shrinking.
    let message = error
        .chain()
        .map(|cause| cause.to_string().to_ascii_lowercase())
        .collect::<Vec<_>>()
        .join(" | ");
    let transient_markers = [
        "timeout",
        "timed out",
        "connection refused",
        "connection reset",
        "connection closed",
        "temporarily unavailable",
        "service unavailable",
        "internal server error",
        "ollama",
        "runner process no longer running",
        "database",
        "pool timed out",
        "broken pipe",
        "dns",
        "network",
        "502",
        "503",
        "504",
    ];

    if transient_markers
        .iter()
        .any(|marker| message.contains(marker))
    {
        ProcessingFailureClass::Transient
    } else {
        ProcessingFailureClass::Permanent
    }
}

/// Default cooldown applied when a provider returns a quota-exhausted
/// signal without a `Retry-After` header. Ollama Cloud's weekly cap and
/// most "monthly tier" quotas don't reset in single-digit hours, so the
/// default is deliberately long — the cost of being wrong (a few hours
/// of idle worker) is much smaller than burning the queue against an
/// upgrade-or-wait quota. Operators can lift it early via the dashboard
/// "Entsperren" action.
const DEFAULT_PROVIDER_QUOTA_COOLDOWN: Duration = Duration::from_secs(24 * 60 * 60);
/// Floor/ceiling applied to a provider-supplied `Retry-After` so a tiny value
/// can't thrash the claim loop and a huge one can't park a provider for weeks.
const MIN_PROVIDER_QUOTA_COOLDOWN: Duration = Duration::from_secs(5 * 60);
const MAX_PROVIDER_QUOTA_COOLDOWN: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// Walk `error` for an `AiProviderError::QuotaExhausted` and persist a
/// cooldown row keyed on its `provider` field. When the provider supplied a
/// `Retry-After`, honor it (clamped to [MIN, MAX]); otherwise default to
/// `DEFAULT_PROVIDER_QUOTA_COOLDOWN`. Previously `retry_after.max(DEFAULT)`
/// meant a `Retry-After: 60` still produced a 24 h cooldown — a short throttle
/// mis-read as a hard cap parked the provider (and every claimed job's
/// run_after) for a day. #292. Falls back to the job's stage provider name if
/// no typed quota error is found in the chain.
/// Resolve the cooldown duration from a provider-supplied `Retry-After`: honor
/// it clamped to [MIN, MAX], or default when absent. Pulled out for unit
/// testing. #292
fn quota_cooldown_duration(retry_after_secs: Option<u64>) -> Duration {
    match retry_after_secs {
        Some(secs) => Duration::from_secs(secs)
            .clamp(MIN_PROVIDER_QUOTA_COOLDOWN, MAX_PROVIDER_QUOTA_COOLDOWN),
        None => DEFAULT_PROVIDER_QUOTA_COOLDOWN,
    }
}

/// Returns the EFFECTIVE cooldown end — when an existing longer cooldown
/// wins over the requested one, that is what the caller parks the
/// triggering job's `run_after` on. #317
pub(crate) async fn record_quota_cooldown_for_failure(
    pool: &DbPool,
    settings: &RuntimeSettings,
    job: &JobRecord,
    error: &anyhow::Error,
) -> Result<DateTime<Utc>> {
    let (provider_name, retry_after_secs, message) =
        extract_quota_signal(error).unwrap_or_else(|| {
            (
                provider_name_for_stage(settings, job.stage).unwrap_or_else(|_| "unknown".into()),
                None,
                error.to_string(),
            )
        });
    let cooldown = quota_cooldown_duration(retry_after_secs);
    let cooldown_until = Utc::now() + ChronoDuration::from_std(cooldown).unwrap_or_default();
    let reason = format!(
        "{} (job {}, stage {})",
        truncate_for_audit(&message, 240),
        job.id,
        job.stage
    );
    // The upsert keeps the longer of (existing, requested) cooldown and
    // reports which case happened (fresh / extended / already covered), so
    // the log and audit trail show whether this 429 actually moved the
    // window — and the job is parked on the EFFECTIVE expiry, not on a
    // requested value an existing longer cooldown overrules. #317
    let upsert =
        archivist_db::upsert_provider_cooldown(pool, &provider_name, cooldown_until, &reason)
            .await?;
    warn!(
        provider = %provider_name,
        until = %upsert.effective_until,
        outcome = upsert.outcome.as_str(),
        previous_until = ?upsert.previous_until,
        retry_after_secs,
        "provider quota exhausted; persisted cooldown — claim cycles will skip this provider until expiry"
    );
    let _ = archivist_db::append_audit(
        pool,
        AuditEventInput {
            event_type: "ai.provider_quota_exhausted".to_owned(),
            actor_type: "worker".to_owned(),
            actor_id: None,
            run_id: Some(job.run_id),
            job_id: Some(job.id),
            paperless_document_id: Some(job.paperless_document_id),
            before: None,
            after: Some(json!({
                "provider": provider_name,
                "cooldown_until": upsert.effective_until,
                "requested_cooldown_until": cooldown_until,
                "previous_cooldown_until": upsert.previous_until,
                "cooldown_outcome": upsert.outcome.as_str(),
                "retry_after_secs": retry_after_secs,
            })),
            metadata: None,
            outcome: "failed".to_owned(),
            error_message: Some(truncate_for_audit(&message, 1024)),
            source_ip: None,
            user_agent: None,
        },
    )
    .await;
    Ok(upsert.effective_until)
}

fn extract_quota_signal(error: &anyhow::Error) -> Option<(String, Option<u64>, String)> {
    for cause in error.chain() {
        if let Some(AiProviderError::QuotaExhausted {
            provider,
            retry_after,
            message,
        }) = cause.downcast_ref::<AiProviderError>()
        {
            return Some((provider.clone(), *retry_after, message.clone()));
        }
    }
    None
}

fn truncate_for_audit(value: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for (idx, ch) in value.chars().enumerate() {
        if idx >= max_chars {
            out.push('…');
            break;
        }
        out.push(ch);
    }
    out
}

/// Resolve the active provider for a stage and look up its cooldown row.
/// Returns the cooldown record only if it is still active (cooldown_until
/// in the future). On stage configuration errors we log and return None
/// rather than failing — a misconfigured stage should fall through to the
/// existing error path, not get masked as a cooldown.
pub(crate) async fn active_cooldown_for_stage(
    pool: &DbPool,
    settings: &RuntimeSettings,
    stage: Stage,
) -> Result<Option<archivist_db::AiProviderCooldown>> {
    let provider = match provider_name_for_stage(settings, stage) {
        Ok(name) => name,
        Err(error) => {
            warn!(error = %error, "could not resolve provider for stage cooldown check");
            return Ok(None);
        }
    };
    archivist_db::get_active_provider_cooldown(pool, &provider).await
}

/// Release a claimed lease back to the queue without burning an attempt
/// — used when the worker discovers the active provider for the job's
/// stage is in cooldown. `attempts` is decremented to undo the increment
/// performed by `claim_jobs`, so the per-job retry budget is preserved
/// for the next cycle. `run_after` is set to the cooldown expiry so the
/// job is not re-claimed before the provider is plausibly back.
///
/// Delegates to [`release_job_lease_for_cooldown`], which also flips the
/// run back to `queued` and mirrors `document_inventory.current_run_status`
/// in the same transaction — the worker-local variant only updated `jobs`,
/// which is how cooldown releases used to strand runs on `running` and
/// (via the startup repair) drift the inventory mirror. #303.
pub(crate) async fn release_lease_for_cooldown(
    pool: &DbPool,
    job: &JobRecord,
    lease_owner: &str,
    cooldown_until: DateTime<Utc>,
) -> Result<()> {
    let released = release_job_lease_for_cooldown(pool, job, lease_owner, cooldown_until)
        .await
        .context("release lease for provider cooldown")?;
    if !released {
        warn!(
            job_id = %job.id,
            "skipped cooldown lease release: lease no longer owned by this worker"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;

    #[test]
    fn typed_ollama_4xx_is_permanent_despite_ollama_in_message() {
        // A typed Client 404 from the Ollama client (carrying the word
        // "ollama" via the context) must classify Permanent, not Transient —
        // the substring table treats "ollama" as a transient marker, so before
        // typing this it burned the whole retry budget. #294
        let err = anyhow::Error::new(AiProviderError::Client {
            status: 404,
            body: "model not found".to_owned(),
        })
        .context("Ollama vision call");
        assert_eq!(
            classify_processing_failure(&err),
            ProcessingFailureClass::Permanent
        );

        // A typed 503 still classifies Transient.
        let server = anyhow::Error::new(AiProviderError::Server {
            status: 503,
            body: "unavailable".to_owned(),
        })
        .context("Ollama chat call");
        assert_eq!(
            classify_processing_failure(&server),
            ProcessingFailureClass::Transient
        );
    }

    #[test]
    fn quota_cooldown_honors_and_clamps_retry_after() {
        // Absent Retry-After -> the long default.
        assert_eq!(
            quota_cooldown_duration(None),
            DEFAULT_PROVIDER_QUOTA_COOLDOWN
        );
        // A short Retry-After is honored (clamped up to the floor), NOT widened
        // to the 24h default as before.
        assert_eq!(
            quota_cooldown_duration(Some(60)),
            MIN_PROVIDER_QUOTA_COOLDOWN
        );
        // A mid value passes through.
        assert_eq!(
            quota_cooldown_duration(Some(3600)),
            Duration::from_secs(3600)
        );
        // An absurd value is capped.
        assert_eq!(
            quota_cooldown_duration(Some(60 * 24 * 60 * 60)),
            MAX_PROVIDER_QUOTA_COOLDOWN
        );
    }

    #[test]
    fn typed_paperless_errors_drive_classification() {
        // A transient Paperless failure is an infrastructure outage of the
        // system of record: classified as TransientInfra so it retries against
        // the higher, bounded ceiling instead of each document's small budget. #305.
        let transient: anyhow::Error =
            anyhow::Error::new(PaperlessError::Timeout("waiting for paperless".to_owned()))
                .context("higher-level wrap that does not mention transient keywords");
        let class = classify_processing_failure(&transient);
        assert_eq!(class, ProcessingFailureClass::TransientInfra);
        assert!(class.is_retryable(), "an upstream outage is retryable");
        assert_eq!(
            class.retry_ceiling(),
            Some(PAPERLESS_INFRA_RETRY_CEILING),
            "infra failures ride the outage out on the elevated ceiling"
        );

        let permanent: anyhow::Error = anyhow::Error::new(PaperlessError::Client {
            status: 422,
            body: "no transient keyword here".to_owned(),
        });
        let permanent_class = classify_processing_failure(&permanent);
        assert_eq!(permanent_class, ProcessingFailureClass::Permanent);
        assert_eq!(
            permanent_class.retry_ceiling(),
            None,
            "a permanent client error keeps the normal (no-override) budget"
        );
    }

    #[test]
    fn typed_ai_errors_drive_classification() {
        let transient: anyhow::Error =
            anyhow::Error::new(AiProviderError::RunnerUnavailable("ollama".to_owned()));
        assert!(matches!(
            classify_processing_failure(&transient),
            ProcessingFailureClass::Transient
        ));

        let permanent: anyhow::Error = anyhow::Error::new(AiProviderError::InvalidResponse(
            "unexpected shape".to_owned(),
        ));
        assert!(matches!(
            classify_processing_failure(&permanent),
            ProcessingFailureClass::Permanent
        ));
    }

    #[test]
    fn fallback_substring_matching_still_classifies_untyped_errors() {
        let transient: anyhow::Error = anyhow!("pool timed out waiting for connection");
        assert!(matches!(
            classify_processing_failure(&transient),
            ProcessingFailureClass::Transient
        ));
        let permanent: anyhow::Error = anyhow!("invalid configuration: missing field");
        assert!(matches!(
            classify_processing_failure(&permanent),
            ProcessingFailureClass::Permanent
        ));
    }

    #[test]
    fn classifies_integration_interruptions_as_transient() {
        let cases = [
            anyhow!("Paperless request timed out while downloading original"),
            anyhow!(
                "Ollama vision returned 500 Internal Server Error: runner process no longer running"
            ),
            anyhow!("PostgreSQL database pool timed out while claiming jobs"),
        ];

        for error in cases {
            assert_eq!(
                classify_processing_failure(&error),
                ProcessingFailureClass::Transient
            );
        }
    }

    #[test]
    fn classifies_validation_and_configuration_errors_as_permanent() {
        let cases = [
            anyhow!("Paperless returned 406 Not Acceptable"),
            anyhow!("model response did not contain valid JSON"),
            anyhow!("unknown allowed tag returned by model"),
            anyhow!("OCR produced no text after layout markup normalization"),
            anyhow!("OCR layout markup exceeded normalization limits"),
        ];

        for error in cases {
            assert_eq!(
                classify_processing_failure(&error),
                ProcessingFailureClass::Permanent
            );
        }
    }
}
