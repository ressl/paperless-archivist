//! Job-lease sizing and lease-renewal fencing helpers.

use std::future::Future;
use std::time::Duration;

use anyhow::Result;
use archivist_core::{AiProviderKind, RuntimeSettings, StructuredOutputMode};

/// Baseline job-lease window in seconds. Claims and heartbeat bumps never
/// grant less than this.
const BASE_JOB_LEASE_SECONDS: i64 = 300;

/// Margin added on top of the slowest configured AI high-level call budget
/// when the lease is derived from it: heartbeats run BETWEEN AI calls, so one
/// lease window must also cover the non-AI work around a maximal-length call
/// (Paperless round-trips, page rendering, DB writes).
const JOB_LEASE_TIMEOUT_MARGIN_SECONDS: i64 = 60;

/// Lease window for `claim_jobs` / `bump_job_lease`, coupled to the AI
/// request timeout: `max(300, slowest enabled provider call budget + margin)`.
/// OpenAI-compatible `structured_output=auto` may make two sequential HTTP
/// requests (strict schema, then the bounded 400 compatibility fallback), so
/// its high-level call budget is twice the per-request timeout.
///
/// `request_timeout_seconds` is operator-configurable (prod runs 600s for
/// slow local models) while the lease used to be a hard-coded 300s — a
/// single in-flight call could outlive the lease, letting a second replica
/// reclaim and double-process the job mid-call. The lease follows the
/// timeout (rather than clamping the timeout below the lease) because the
/// configurable timeout exists precisely so calls may run long. Jobs are
/// claimed before stage→provider resolution, so size the window for the
/// slowest enabled provider rather than per-stage. #308
pub(crate) fn job_lease_seconds(settings: &RuntimeSettings) -> i64 {
    let slowest_call_budget = settings
        .ai
        .providers
        .iter()
        .filter(|provider| provider.enabled)
        .map(|provider| {
            // Mirror `provider_for_stage`: 0/unset inherits the built-in default.
            let request_timeout = i64::from(
                provider
                    .tuning
                    .request_timeout_seconds
                    .filter(|secs| *secs > 0)
                    .unwrap_or(archivist_core::DEFAULT_AI_REQUEST_TIMEOUT_SECS),
            );
            let schema_retry_possible = matches!(
                provider.kind,
                AiProviderKind::Openai | AiProviderKind::OpenaiCompatible
            ) && provider.tuning.structured_output.unwrap_or_default()
                == StructuredOutputMode::Auto;
            if schema_retry_possible {
                request_timeout.saturating_mul(2)
            } else {
                request_timeout
            }
        })
        .max()
        .unwrap_or(i64::from(archivist_core::DEFAULT_AI_REQUEST_TIMEOUT_SECS));
    BASE_JOB_LEASE_SECONDS.max(slowest_call_budget + JOB_LEASE_TIMEOUT_MARGIN_SECONDS)
}

/// Renew an owner-scoped job lease before polling a potentially slow network
/// future. Keeping the call future unpolled until the renewal succeeds is the
/// fencing guarantee: once another worker owns the job, this worker cannot
/// start the next provider request and later reach cache/apply completion.
pub(crate) async fn run_after_lease_renewal<T, Renewal, Call>(
    renewal: Renewal,
    call: Call,
) -> Result<Option<T>>
where
    Renewal: Future<Output = Result<bool>>,
    Call: Future<Output = T>,
{
    if !renewal.await? {
        return Ok(None);
    }
    Ok(Some(call.await))
}

/// Renewal cadence for [`with_lease_keepalive`]: a third of the lease window,
/// so even a renewal that itself stalls for a while lands before expiry. #413
pub(crate) fn lease_keepalive_interval(lease_seconds: i64) -> Duration {
    Duration::from_secs((lease_seconds / 3).max(1) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claim_loop::resolve_target_concurrency;

    #[test]
    fn ocr_setup_lease_keepalive_renews_well_within_the_lease() {
        // #413: every pre-page phase runs under the keepalive, so the longest
        // unrenewed stretch is one keepalive interval — a third of the lease —
        // regardless of the download (10x HTTP timeout) or render budget.
        for lease in [BASE_JOB_LEASE_SECONDS, 420, 3600] {
            let interval = lease_keepalive_interval(lease).as_secs() as i64;
            assert!(interval * 3 <= lease && interval >= 1, "lease {lease}");
        }
        // The #407 watchdog only fires after a full lease window plus grace
        // without renewal, so it never races a working keepalive.
        let lease = job_lease_seconds(&RuntimeSettings::default());
        assert!((lease_keepalive_interval(lease).as_secs() as i64) < lease);
    }

    #[test]
    fn job_lease_outlives_the_slowest_enabled_provider_call_budget() {
        // Default presets leave request_timeout_seconds unset → 180s. The
        // enabled OpenAI preset can make the one-shot Auto schema fallback,
        // so its high-level call budget is 2*180 plus the margin.
        let mut settings = RuntimeSettings::default();
        assert_eq!(
            job_lease_seconds(&settings),
            2 * i64::from(archivist_core::DEFAULT_AI_REQUEST_TIMEOUT_SECS)
                + JOB_LEASE_TIMEOUT_MARGIN_SECONDS
        );

        // The prod shape from the audit: a 600s timeout used to outlive the
        // hard-coded 300s lease mid-call. The lease must now cover the call
        // plus the inter-heartbeat margin.
        settings.ai.providers[0].tuning.request_timeout_seconds = Some(600);
        assert_eq!(
            job_lease_seconds(&settings),
            600 + JOB_LEASE_TIMEOUT_MARGIN_SECONDS
        );

        // The slowest enabled provider sizes the window (jobs are claimed
        // before stage→provider resolution).
        settings.ai.providers[1].tuning.request_timeout_seconds = Some(900);
        assert_eq!(
            job_lease_seconds(&settings),
            900 + JOB_LEASE_TIMEOUT_MARGIN_SECONDS
        );

        // Disabled providers can never serve a stage and must not stretch it.
        settings.ai.providers[1].enabled = false;
        assert_eq!(
            job_lease_seconds(&settings),
            600 + JOB_LEASE_TIMEOUT_MARGIN_SECONDS
        );

        // 0 means "inherit the default", not a zero-second timeout. The
        // enabled OpenAI provider still owns the larger 2*180 call budget.
        settings.ai.providers[0].tuning.request_timeout_seconds = Some(0);
        assert_eq!(
            job_lease_seconds(&settings),
            2 * i64::from(archivist_core::DEFAULT_AI_REQUEST_TIMEOUT_SECS)
                + JOB_LEASE_TIMEOUT_MARGIN_SECONDS
        );
        settings.ai.providers[0].tuning.request_timeout_seconds = Some(120);
        assert_eq!(
            job_lease_seconds(&settings),
            2 * i64::from(archivist_core::DEFAULT_AI_REQUEST_TIMEOUT_SECS)
                + JOB_LEASE_TIMEOUT_MARGIN_SECONDS
        );

        // No providers at all: fall back to the built-in default → baseline.
        settings.ai.providers.clear();
        assert_eq!(job_lease_seconds(&settings), BASE_JOB_LEASE_SECONDS);
    }

    #[test]
    fn minimax_m3_capacity_preset_reserves_interactive_slot_and_has_lease_margin() {
        let mut settings = RuntimeSettings::default();
        settings.ai.default_provider = archivist_core::SGLANG_MINIMAX_M3_PROVIDER_NAME.to_owned();
        for provider in &mut settings.ai.providers {
            provider.enabled = false;
        }
        let provider = settings
            .ai
            .providers
            .iter_mut()
            .find(|provider| provider.name == archivist_core::SGLANG_MINIMAX_M3_PROVIDER_NAME)
            .expect("built-in MiniMax M3 provider");
        provider.enabled = true;

        let effective = settings.effective_tuning();
        assert_eq!(effective.worker_concurrency, 1);
        assert_eq!(resolve_target_concurrency(8, &settings), 1);
        assert_eq!(effective.request_timeout_seconds, 180);
        assert_eq!(
            job_lease_seconds(&settings),
            2 * i64::from(effective.request_timeout_seconds) + JOB_LEASE_TIMEOUT_MARGIN_SECONDS
        );

        let provider = settings
            .ai
            .providers
            .iter_mut()
            .find(|provider| provider.name == archivist_core::SGLANG_MINIMAX_M3_PROVIDER_NAME)
            .expect("built-in MiniMax M3 provider");
        provider.tuning.structured_output = Some(StructuredOutputMode::Off);
        assert_eq!(job_lease_seconds(&settings), BASE_JOB_LEASE_SECONDS);
    }

    #[test]
    fn openai_auto_schema_retry_also_doubles_the_lease_request_budget() {
        let mut settings = RuntimeSettings::default();
        for provider in &mut settings.ai.providers {
            provider.enabled = provider.kind == AiProviderKind::Openai;
            if provider.enabled {
                provider.tuning.request_timeout_seconds = Some(180);
                provider.tuning.structured_output = Some(StructuredOutputMode::Auto);
            }
        }
        assert_eq!(
            job_lease_seconds(&settings),
            2 * 180 + JOB_LEASE_TIMEOUT_MARGIN_SECONDS
        );
    }
}
