//! Shared application state and Paperless client construction.

use crate::*;

#[derive(Clone)]
pub(crate) struct AppState {
    pub(crate) pool: DbPool,
    pub(crate) config: Arc<AppConfig>,
    pub(crate) auth_rate_limiter: Arc<AuthRateLimiter>,
}

pub(crate) async fn paperless_client_from_settings(
    pool: &DbPool,
    config: &AppConfig,
    settings: &RuntimeSettings,
) -> Result<PaperlessClient> {
    // The global token is only inherited by a same-origin profile. #396
    let (base_url, secret_id) = settings.paperless.active_connection();
    let secret_id = secret_id.ok_or_else(|| {
        not_configured("Paperless token is not configured for the active archive profile")
    })?;
    let token = resolve_secret(pool, &config.secret_key, secret_id)
        .await?
        .ok_or_else(|| not_configured("Paperless token secret reference does not exist"))?;
    PaperlessClient::new(base_url, token, settings.paperless.timeout_seconds)
}
