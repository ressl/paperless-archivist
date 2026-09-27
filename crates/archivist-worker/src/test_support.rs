//! Shared test helpers for the worker modules.

use archivist_config::AppConfig;
use archivist_core::{ProcessingMode, Stage};
use archivist_db::JobRecord;
use secrecy::SecretString;
use serde_json::json;
use uuid::Uuid;

pub(crate) fn vision_test_job() -> JobRecord {
    JobRecord {
        id: Uuid::now_v7(),
        run_id: Uuid::now_v7(),
        paperless_document_id: 42,
        stage: Stage::Ocr,
        mode: ProcessingMode::ManualReview,
        status: "running".to_owned(),
        attempts: 1,
        max_attempts: 3,
        payload: json!({}),
    }
}

pub(crate) fn test_app_config() -> AppConfig {
    // Minimal config shaped enough to feed `env_concurrency_cap`. The
    // other fields are not consulted; we only assert on
    // `worker_concurrency`.
    AppConfig {
        http_addr: "127.0.0.1:0".to_owned(),
        database_url: SecretString::new(String::new().into()),
        worker_concurrency: 4,
        db_max_connections: 10,
        log_level: "info".to_owned(),
        cookie_secure: false,
        session_ttl_hours: 12,
        bootstrap_admin_username: "admin".to_owned(),
        bootstrap_admin_password: None,
        oidc_enabled: false,
        oidc_issuer_url: None,
        oidc_client_id: None,
        oidc_client_secret: None,
        oidc_redirect_uri: None,
        oidc_scopes: "openid profile email".to_owned(),
        oidc_admin_users: String::new(),
        oidc_default_roles: "viewer".to_owned(),
        oidc_roles_claim: "urn:zitadel:iam:org:project:roles".to_owned(),
        oidc_role_mappings: "archivist-admin=admin".to_owned(),
        oidc_allow_email_link: false,
        secret_key: SecretString::new("0123456789abcdef0123456789abcdef".to_owned().into()),
        static_dir: "frontend/dist".to_owned(),
        trust_proxy: false,
        auth_rate_limit: 10,
        auth_rate_limit_window_seconds: 60,
        webhook_secret: None,
        metrics_token: None,
    }
}
