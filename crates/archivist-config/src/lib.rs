use clap::Parser;
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Parser, Clone)]
#[command(name = "paperless-archivist")]
pub struct AppConfig {
    #[arg(long, env = "ARCHIVIST_HTTP_ADDR", default_value = "0.0.0.0:8080")]
    pub http_addr: String,

    #[arg(long, env = "DATABASE_URL")]
    pub database_url: SecretString,

    #[arg(long, env = "ARCHIVIST_WORKER_CONCURRENCY", default_value_t = 2)]
    pub worker_concurrency: usize,

    #[arg(long, env = "ARCHIVIST_LOG_LEVEL", default_value = "info")]
    pub log_level: String,

    #[arg(long, env = "ARCHIVIST_COOKIE_SECURE", default_value_t = false)]
    pub cookie_secure: bool,

    #[arg(long, env = "ARCHIVIST_SESSION_TTL_HOURS", default_value_t = 12)]
    pub session_ttl_hours: i64,

    #[arg(long, env = "ARCHIVIST_ADMIN_USERNAME", default_value = "admin")]
    pub bootstrap_admin_username: String,

    #[arg(long, env = "ARCHIVIST_ADMIN_PASSWORD")]
    pub bootstrap_admin_password: Option<SecretString>,

    #[arg(long, env = "ARCHIVIST_OIDC_ENABLED", default_value_t = false)]
    pub oidc_enabled: bool,

    #[arg(long, env = "ARCHIVIST_OIDC_ISSUER_URL")]
    pub oidc_issuer_url: Option<String>,

    #[arg(long, env = "ARCHIVIST_OIDC_CLIENT_ID")]
    pub oidc_client_id: Option<String>,

    #[arg(long, env = "ARCHIVIST_OIDC_CLIENT_SECRET")]
    pub oidc_client_secret: Option<SecretString>,

    #[arg(long, env = "ARCHIVIST_OIDC_REDIRECT_URI")]
    pub oidc_redirect_uri: Option<String>,

    #[arg(
        long,
        env = "ARCHIVIST_OIDC_SCOPES",
        default_value = "openid profile email"
    )]
    pub oidc_scopes: String,

    #[arg(long, env = "ARCHIVIST_OIDC_ADMIN_USERS", default_value = "")]
    pub oidc_admin_users: String,

    #[arg(long, env = "ARCHIVIST_OIDC_DEFAULT_ROLES", default_value = "viewer")]
    pub oidc_default_roles: String,

    #[arg(long, env = "ARCHIVIST_SECRET_KEY")]
    pub secret_key: SecretString,

    #[arg(long, env = "ARCHIVIST_STATIC_DIR", default_value = "frontend/dist")]
    pub static_dir: String,

    /// Trust X-Forwarded-For when extracting the client IP. Only enable when
    /// a reverse proxy that strips/normalizes the header sits in front of
    /// the API.
    #[arg(long, env = "ARCHIVIST_TRUST_PROXY", default_value_t = false)]
    pub trust_proxy: bool,

    /// Maximum number of /api/auth/* requests permitted per source IP within
    /// `auth_rate_limit_window_seconds`. Set to zero to disable the limiter.
    #[arg(long, env = "ARCHIVIST_AUTH_RATE_LIMIT", default_value_t = 10)]
    pub auth_rate_limit: u32,

    /// Sliding window for the auth rate limiter, in seconds.
    #[arg(
        long,
        env = "ARCHIVIST_AUTH_RATE_LIMIT_WINDOW_SECONDS",
        default_value_t = 60
    )]
    pub auth_rate_limit_window_seconds: u64,
}

impl AppConfig {
    pub fn from_env() -> Self {
        Self::parse()
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.worker_concurrency == 0 {
            return Err(ConfigError::Invalid(
                "worker_concurrency must be greater than zero",
            ));
        }
        if self.session_ttl_hours <= 0 {
            return Err(ConfigError::Invalid(
                "session ttl must be greater than zero",
            ));
        }
        if self.secret_key.expose_secret().len() < 32 {
            return Err(ConfigError::Invalid(
                "ARCHIVIST_SECRET_KEY must be at least 32 bytes",
            ));
        }
        if self.oidc_enabled {
            if self.oidc_issuer_url.as_deref().is_none_or(str::is_empty) {
                return Err(ConfigError::Invalid(
                    "ARCHIVIST_OIDC_ISSUER_URL is required when OIDC is enabled",
                ));
            }
            if self.oidc_client_id.as_deref().is_none_or(str::is_empty) {
                return Err(ConfigError::Invalid(
                    "ARCHIVIST_OIDC_CLIENT_ID is required when OIDC is enabled",
                ));
            }
            if self
                .oidc_client_secret
                .as_ref()
                .is_none_or(|secret| secret.expose_secret().is_empty())
            {
                return Err(ConfigError::Invalid(
                    "ARCHIVIST_OIDC_CLIENT_SECRET is required when OIDC is enabled",
                ));
            }
            if self.oidc_redirect_uri.as_deref().is_none_or(str::is_empty) {
                return Err(ConfigError::Invalid(
                    "ARCHIVIST_OIDC_REDIRECT_URI is required when OIDC is enabled",
                ));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PublicBootstrapConfig {
    pub cookie_secure: bool,
    pub session_ttl_hours: i64,
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("{0}")]
    Invalid(&'static str),
}
