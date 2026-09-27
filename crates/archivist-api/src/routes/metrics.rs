//! Health, readiness and Prometheus metrics endpoints.

use crate::*;

pub(crate) async fn healthz() -> &'static str {
    "ok"
}

pub(crate) async fn readyz(State(state): State<AppState>) -> ApiResult<&'static str> {
    sqlx::query("select 1").execute(&state.pool).await?;
    Ok("ready")
}

pub(crate) fn authorize_metrics_request(
    expected: Option<&SecretString>,
    headers: &HeaderMap,
) -> ApiResult<()> {
    let Some(expected) = expected else {
        return Err(ApiError::service_unavailable(
            "metrics disabled: set ARCHIVIST_METRICS_TOKEN",
        ));
    };
    let provided = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .unwrap_or_default();
    let expected = expected.expose_secret();
    if expected.len() != provided.len()
        || !bool::from(expected.as_bytes().ct_eq(provided.as_bytes()))
    {
        return Err(ApiError::unauthorized("invalid metrics token"));
    }
    Ok(())
}

pub(crate) async fn metrics(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    // /metrics sits outside the session-auth middleware so Prometheus can
    // scrape it, but it discloses operational internals (queue depth, failure
    // counts, latencies) and every hit runs aggregate queries. Require the
    // dedicated scrape token; disabled (503) when unconfigured — same
    // contract as the inbound webhook.
    authorize_metrics_request(state.config.metrics_token.as_ref(), &headers)?;
    let snapshot = db_metrics_snapshot(&state.pool).await?;
    // Migrated series now live in `metrics_counters` (real monotone counters that
    // survive audit retention). Read them here and serve them as `# TYPE counter`
    // below; anything not migrated keeps its `audit_events`-derived gauge value
    // from `snapshot`. Missing keys default to 0 (the migration seeds them, so in
    // practice they are always present once `migrate()` has run at startup).
    let counters = read_metric_counters(&state.pool).await?;
    let counter = |name: &str| counters.get(name).copied().unwrap_or(0);
    let body = format!(
        concat!(
            "# HELP paperless_archivist_jobs_queued Queued jobs\n",
            "# TYPE paperless_archivist_jobs_queued gauge\n",
            "paperless_archivist_jobs_queued {}\n",
            "# HELP paperless_archivist_jobs_running Running jobs\n",
            "# TYPE paperless_archivist_jobs_running gauge\n",
            "paperless_archivist_jobs_running {}\n",
            "# HELP paperless_archivist_jobs_failed Failed jobs\n",
            "# TYPE paperless_archivist_jobs_failed gauge\n",
            "paperless_archivist_jobs_failed {}\n",
            "# HELP paperless_archivist_jobs_succeeded Succeeded jobs\n",
            "# TYPE paperless_archivist_jobs_succeeded counter\n",
            "paperless_archivist_jobs_succeeded {}\n",
            "# HELP paperless_archivist_reviews_pending Pending review items\n",
            "# TYPE paperless_archivist_reviews_pending gauge\n",
            "paperless_archivist_reviews_pending {}\n",
            "# HELP paperless_archivist_runs_active Active pipeline runs\n",
            "# TYPE paperless_archivist_runs_active gauge\n",
            "paperless_archivist_runs_active {}\n",
            // Approximate row count from planner statistics (reltuples), not a
            // live COUNT — audit_events is unbounded and this runs on every
            // scrape. Retention prunes the table, so the value can decrease:
            // it stays a gauge (a counter would make Prometheus misread
            // retention pruning as a counter reset and corrupt rate()).
            "# HELP paperless_archivist_audit_events Audit events currently retained (approximate)\n",
            "# TYPE paperless_archivist_audit_events gauge\n",
            "paperless_archivist_audit_events {}\n",
            // selector_* and job_retries_* are now real monotone counters backed
            // by `metrics_counters`, so they survive audit retention pruning and
            // are safe for rate(). See migration 0031.
            "# HELP paperless_archivist_selector_runs_total Automatic selector runs (monotone counter)\n",
            "# TYPE paperless_archivist_selector_runs_total counter\n",
            "paperless_archivist_selector_runs_total {}\n",
            "# HELP paperless_archivist_selector_documents_queued_total Documents queued by automatic selector (monotone counter)\n",
            "# TYPE paperless_archivist_selector_documents_queued_total counter\n",
            "paperless_archivist_selector_documents_queued_total {}\n",
            "# HELP paperless_archivist_job_retries_scheduled_total Job retries scheduled after transient failures (monotone counter)\n",
            "# TYPE paperless_archivist_job_retries_scheduled_total counter\n",
            "paperless_archivist_job_retries_scheduled_total {}\n",
            "# HELP paperless_archivist_job_failures_total Jobs that reached a permanent failed state (monotone counter)\n",
            "# TYPE paperless_archivist_job_failures_total counter\n",
            "paperless_archivist_job_failures_total {}\n",
            // Provider quota-exhausted events: the rate of this counter is the
            // signal the #311 quota alert targets. Incremented once per job that
            // a provider rejects with a usage-cap signal (before the cooldown is
            // recorded), so a sustained rate means a provider is capped.
            "# HELP paperless_archivist_provider_quota_total Provider quota-exhausted events (monotone counter)\n",
            "# TYPE paperless_archivist_provider_quota_total counter\n",
            "paperless_archivist_provider_quota_total {}\n",
            // model_errors_total is a live COUNT over the (non-prunable) `jobs`
            // table; it can decrease as rows are reprocessed, so it stays a gauge.
            "# HELP paperless_archivist_model_errors_total Jobs with model-stage error messages\n",
            "# TYPE paperless_archivist_model_errors_total gauge\n",
            "paperless_archivist_model_errors_total {}\n",
            // apply_* totals are now real monotone counters backed by
            // `metrics_counters`, incremented once at each apply event.
            "# HELP paperless_archivist_apply_success_total Successful Paperless apply operations (monotone counter)\n",
            "# TYPE paperless_archivist_apply_success_total counter\n",
            "paperless_archivist_apply_success_total {}\n",
            "# HELP paperless_archivist_apply_failure_total Failed Paperless apply operations (monotone counter)\n",
            "# TYPE paperless_archivist_apply_failure_total counter\n",
            "paperless_archivist_apply_failure_total {}\n",
            "# HELP paperless_archivist_apply_latency_ms_sum Sum of observed Paperless apply latency in milliseconds (retained in audit log)\n",
            "# TYPE paperless_archivist_apply_latency_ms_sum gauge\n",
            "paperless_archivist_apply_latency_ms_sum {}\n",
            "# HELP paperless_archivist_apply_latency_ms_count Count of observed Paperless apply latency samples (retained in audit log)\n",
            "# TYPE paperless_archivist_apply_latency_ms_count gauge\n",
            "paperless_archivist_apply_latency_ms_count {}\n",
            "# HELP paperless_archivist_apply_latency_ms_p95 Lifetime p95 of observed Paperless apply latency in milliseconds (over retained audit events)\n",
            "# TYPE paperless_archivist_apply_latency_ms_p95 gauge\n",
            "paperless_archivist_apply_latency_ms_p95 {}\n",
            // Per-stage latency gauges sourced from ai_artifacts.duration_ms over
            // a recent 24h window (see metrics_snapshot). They can decrease as the
            // window rolls forward, so they are gauges, not counters.
            "# HELP paperless_archivist_ocr_latency_ms_count Count of OCR-stage latency samples observed in the last 24h\n",
            "# TYPE paperless_archivist_ocr_latency_ms_count gauge\n",
            "paperless_archivist_ocr_latency_ms_count {}\n",
            "# HELP paperless_archivist_ocr_latency_ms_p95 p95 of OCR-stage latency in milliseconds over the last 24h\n",
            "# TYPE paperless_archivist_ocr_latency_ms_p95 gauge\n",
            "paperless_archivist_ocr_latency_ms_p95 {}\n",
            "# HELP paperless_archivist_metadata_latency_ms_count Count of metadata-stage latency samples observed in the last 24h\n",
            "# TYPE paperless_archivist_metadata_latency_ms_count gauge\n",
            "paperless_archivist_metadata_latency_ms_count {}\n",
            "# HELP paperless_archivist_metadata_latency_ms_p95 p95 of metadata-stage latency in milliseconds over the last 24h\n",
            "# TYPE paperless_archivist_metadata_latency_ms_p95 gauge\n",
            "paperless_archivist_metadata_latency_ms_p95 {}\n",
            "# HELP paperless_archivist_oldest_queued_age_seconds Age in seconds of the oldest queued job (now() - min(run_after) over status='queued')\n",
            "# TYPE paperless_archivist_oldest_queued_age_seconds gauge\n",
            "paperless_archivist_oldest_queued_age_seconds {}\n"
        ),
        snapshot.jobs_queued,
        snapshot.jobs_running,
        snapshot.jobs_failed,
        snapshot.jobs_succeeded,
        snapshot.reviews_pending,
        snapshot.runs_active,
        snapshot.audit_events,
        counter("selector_runs_total"),
        counter("selector_documents_queued_total"),
        counter("job_retries_scheduled_total"),
        counter("job_failures_total"),
        counter("provider_quota_total"),
        snapshot.model_errors_total,
        counter("apply_success_total"),
        counter("apply_failure_total"),
        snapshot.apply_latency_ms_sum,
        snapshot.apply_latency_ms_count,
        snapshot.apply_latency_ms_p95,
        snapshot.ocr_latency_ms_count,
        snapshot.ocr_latency_ms_p95,
        snapshot.metadata_latency_ms_count,
        snapshot.metadata_latency_ms_p95,
        snapshot.oldest_queued_age_seconds
    );
    let mut response = body.into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; version=0.0.4"),
    );
    Ok(response)
}
