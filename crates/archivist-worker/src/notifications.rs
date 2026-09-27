//! Operational notification webhooks sent from the worker tick.

use std::time::Duration;

use anyhow::{Result, anyhow};
use archivist_config::AppConfig;
use archivist_core::ProcessingMode;
use archivist_db::{
    DbPool, claim_notification_delivery, get_backlog_counts, get_dashboard_live_status,
    get_runtime_settings, resolve_secret,
};
use reqwest::Client as HttpClient;
use secrecy::{ExposeSecret, SecretString};
use serde_json::json;

pub(crate) async fn send_operational_notifications(
    pool: &DbPool,
    config: &AppConfig,
) -> Result<()> {
    let settings = get_runtime_settings(pool).await?;
    if !settings.notifications.enabled {
        return Ok(());
    }
    let Some(webhook_secret_id) = settings.notifications.webhook_url_secret_id else {
        return Ok(());
    };
    let Some(webhook_url) = resolve_secret(pool, &config.secret_key, webhook_secret_id).await?
    else {
        return Ok(());
    };
    let cooldown = settings.notifications.cooldown_minutes as i32;
    let counts = get_backlog_counts(pool).await?;
    if counts.waiting_review >= settings.notifications.review_queue_threshold
        && claim_notification_delivery(pool, "review_queue_backlog", cooldown).await?
    {
        send_notification_webhook(
            &webhook_url,
            json!({
                "app": "paperless-archivist",
                "event": "review_queue_backlog",
                "severity": "warning",
                "title": "Review queue needs attention",
                "description": "Paperless Archivist has documents waiting for human review.",
                "metadata": {
                    "waiting_review": counts.waiting_review,
                    "threshold": settings.notifications.review_queue_threshold
                }
            }),
        )
        .await?;
    }

    let live = get_dashboard_live_status(pool, &settings).await?;
    let hard_failures = live
        .recent_failures
        .iter()
        .filter(|failure| failure.status == "failed" || failure.failure_kind == "failed")
        .count() as i64;
    if hard_failures >= settings.notifications.repeated_failure_threshold
        && claim_notification_delivery(pool, "repeated_processing_failures", cooldown).await?
    {
        send_notification_webhook(
            &webhook_url,
            json!({
                "app": "paperless-archivist",
                "event": "repeated_processing_failures",
                "severity": "error",
                "title": "Repeated processing failures",
                "description": "Recent Paperless Archivist jobs are failing. Check the dashboard live status and worker logs.",
                "metadata": {
                    "recent_failure_count": hard_failures,
                    "threshold": settings.notifications.repeated_failure_threshold
                }
            }),
        )
        .await?;
    }

    if settings.workflow.mode == ProcessingMode::FullAuto
        && settings.workflow.paused
        && claim_notification_delivery(pool, "paused_full_auto", cooldown).await?
    {
        send_notification_webhook(
            &webhook_url,
            json!({
                "app": "paperless-archivist",
                "event": "paused_full_auto",
                "severity": "warning",
                "title": "Full autopilot is paused",
                "description": "Full autopilot is configured but processing is paused.",
                "metadata": {
                    "workflow_mode": "full_auto",
                    "paused": true
                }
            }),
        )
        .await?;
    }
    Ok(())
}

async fn send_notification_webhook(
    webhook_url: &SecretString,
    payload: serde_json::Value,
) -> Result<()> {
    let response = HttpClient::builder()
        .timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        // No connect-time IP-pinning: the DNS-rebinding TOCTOU is an accepted
        // residual risk for this operator-configured webhook host (see #183).
        .build()?
        .post(webhook_url.expose_secret())
        .json(&payload)
        .send()
        .await
        .map_err(|error| {
            anyhow!(
                "notification webhook request failed: {}",
                error.without_url()
            )
        })?;
    let status = response.status();
    if !status.is_success() {
        return Err(anyhow!("notification webhook returned {status}"));
    }
    Ok(())
}
