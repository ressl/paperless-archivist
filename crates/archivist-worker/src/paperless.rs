//! Paperless client construction from runtime settings.

use anyhow::{Result, anyhow};
use archivist_config::AppConfig;
use archivist_core::RuntimeSettings;
use archivist_db::{DbPool, resolve_secret};
use archivist_paperless::PaperlessClient;

pub(crate) async fn paperless_client(
    pool: &DbPool,
    config: &AppConfig,
    settings: &RuntimeSettings,
) -> Result<PaperlessClient> {
    // The global token is only inherited by a same-origin profile. #396
    let (base_url, secret_id) = settings.paperless.active_connection();
    let secret_id = secret_id.ok_or_else(|| {
        anyhow!("Paperless token is not configured for the active archive profile")
    })?;
    let token = resolve_secret(pool, &config.secret_key, secret_id)
        .await?
        .ok_or_else(|| anyhow!("Paperless token secret reference does not exist"))?;
    PaperlessClient::new(base_url, token, settings.paperless.timeout_seconds)
}
