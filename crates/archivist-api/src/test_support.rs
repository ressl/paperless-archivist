//! Shared test helpers: configs, states, mock servers and the HTTP harness.

use crate::*;

pub(crate) fn auth_context_for_session_listing(
    cookie_auth: bool,
    user_id: Uuid,
    roles: Vec<Role>,
    scopes: Vec<&str>,
) -> AuthContext {
    AuthContext {
        actor_type: if cookie_auth { "user" } else { "api_token" }.to_owned(),
        actor_id: Some(user_id.to_string()),
        user_id: Some(user_id),
        username: cookie_auth.then(|| "session-user".to_owned()),
        roles,
        scopes: scopes.into_iter().map(str::to_owned).collect(),
        session_id: cookie_auth.then(Uuid::new_v4),
        csrf_secret_hash: cookie_auth.then(|| "csrf-hash".to_owned()),
        cookie_auth,
    }
}

pub(crate) fn test_config() -> AppConfig {
    AppConfig {
        http_addr: "127.0.0.1:0".to_owned(),
        database_url: SecretString::from("postgres://localhost/archivist".to_owned()),
        worker_concurrency: 1,
        db_max_connections: 10,
        log_level: "info".to_owned(),
        cookie_secure: false,
        session_ttl_hours: 12,
        bootstrap_admin_username: "admin".to_owned(),
        bootstrap_admin_password: None,
        oidc_enabled: true,
        oidc_issuer_url: Some("https://issuer.example.com".to_owned()),
        oidc_client_id: Some("paperless-archivist".to_owned()),
        oidc_client_secret: Some(SecretString::from("test-secret".to_owned())),
        oidc_redirect_uri: Some(
            "https://archivist.example.com/api/auth/oidc/callback".to_owned(),
        ),
        oidc_scopes: "openid profile email".to_owned(),
        oidc_admin_users: String::new(),
        oidc_default_roles: "viewer".to_owned(),
        oidc_roles_claim: "urn:zitadel:iam:org:project:roles".to_owned(),
        oidc_role_mappings: "archivist-admin=admin,archivist-operator=operator,archivist-reviewer=reviewer,archivist-auditor=auditor,archivist-viewer=viewer".to_owned(),
        oidc_allow_email_link: false,
        secret_key: SecretString::from("a 32 byte local secret for tests".to_owned()),
        static_dir: "frontend/dist".to_owned(),
        trust_proxy: false,
        auth_rate_limit: 10,
        auth_rate_limit_window_seconds: 60,
        webhook_secret: None,
        metrics_token: None,
    }
}

// ----- /api/ai/runtime-hints --------------------------------------
//
// The non-Ollama branch is a pure function — assert its shape
// directly. The Ollama branch goes through a real OllamaClient, so
// we spin up a one-shot axum mini-server bound to 127.0.0.1:0 to
// mock `/api/version` and `/api/ps`.

pub(crate) fn make_api_provider(kind: AiProviderKind) -> ApiProvider {
    ApiProvider {
        name: format!("{kind:?}").to_ascii_lowercase(),
        kind,
        base_url: "http://example.invalid".to_owned(),
        model: "test-model".to_owned(),
        secret_id: None,
        tuning: RuntimeSettings::default().effective_tuning(),
    }
}

pub(crate) fn api_text_test_state() -> AppState {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .connect_lazy("postgres://archivist:archivist@127.0.0.1/archivist")
        .expect("lazy test pool");
    AppState {
        pool,
        config: Arc::new(test_config()),
        auth_rate_limiter: Arc::new(AuthRateLimiter::new(10, 60)),
    }
}

pub(crate) async fn security_db_state() -> Option<AppState> {
    let database_url = std::env::var("DATABASE_URL").ok()?;
    let pool = connect(&database_url, 4)
        .await
        .expect("connect security test database");
    migrate(&pool).await.expect("apply migrations");
    sqlx::query(
        "truncate paperless_apply_intents, review_items, jobs, pipeline_runs, audit_events restart identity cascade",
    )
    .execute(&pool)
    .await
    .expect("truncate review fixtures");
    Some(AppState {
        pool,
        config: Arc::new(test_config()),
        auth_rate_limiter: Arc::new(AuthRateLimiter::new(10, 60)),
    })
}

// ----- #440: HTTP-level harness: router() + tower oneshot ---------------

use tower::ServiceExt as _;

pub(crate) const HARNESS_PEER: &str = "198.51.100.7:40000";
pub(crate) const HARNESS_USER_AGENT: &str = "archivist-harness/1.0";

/// Drives the fully composed `router()` (nesting, layers, fallbacks, auth
/// and route-policy middleware) in-process through `oneshot`, without a
/// TCP listener. Every request carries a fixed peer address and
/// User-Agent so audit request context can be asserted. #440
pub(crate) struct TestApi {
    pub(crate) app: Router,
}

pub(crate) struct TestResponse {
    pub(crate) status: StatusCode,
    pub(crate) headers: HeaderMap,
    pub(crate) body: Value,
}

#[derive(Clone)]
pub(crate) enum Principal {
    Anonymous,
    Session { token: String, csrf: String },
    Token(String),
}

impl TestApi {
    pub(crate) fn new(state: AppState) -> Self {
        Self { app: router(state) }
    }

    pub(crate) async fn send(
        &self,
        method: Method,
        path: &str,
        principal: &Principal,
        body: Option<Value>,
    ) -> TestResponse {
        let mut builder = axum::http::Request::builder()
            .method(method)
            .uri(path)
            .header(header::USER_AGENT, HARNESS_USER_AGENT)
            .extension(ConnectInfo(
                HARNESS_PEER.parse::<SocketAddr>().expect("peer address"),
            ));
        match principal {
            Principal::Anonymous => {}
            Principal::Session { token, csrf } => {
                builder = builder
                    .header(header::COOKIE, format!("{SESSION_COOKIE}={token}"))
                    .header("x-csrf-token", csrf);
            }
            Principal::Token(token) => {
                builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
            }
        }
        let request = match body {
            Some(body) => builder
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_string())),
            None => builder.body(Body::empty()),
        }
        .expect("build request");
        let response = self
            .app
            .clone()
            .oneshot(request)
            .await
            .expect("router is infallible");
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read body");
        let body = serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()));
        TestResponse {
            status,
            headers,
            body,
        }
    }
}

pub(crate) async fn harness_user(pool: &DbPool, label: &str, roles: &[Role]) -> (Uuid, Principal) {
    let suffix = Uuid::now_v7().simple().to_string();
    let user_id = create_user_with_roles(
        pool,
        &format!("harness-{label}-{suffix}"),
        None,
        "hash",
        roles,
        None,
    )
    .await
    .expect("harness user");
    let token = random_token();
    let csrf = random_token();
    create_session(
        pool,
        user_id,
        &hash_token(&token),
        &hash_token(&csrf),
        Utc::now() + Duration::hours(1),
    )
    .await
    .expect("harness session");
    (user_id, Principal::Session { token, csrf })
}

pub(crate) async fn harness_token(pool: &DbPool, creator: Uuid, scopes: &[&str]) -> Principal {
    let token = format!("pa_{}", random_token());
    archivist_db::create_api_token(
        pool,
        &format!("harness-{}", Uuid::now_v7().simple()),
        &hash_token(&token),
        &scopes
            .iter()
            .map(|scope| (*scope).to_owned())
            .collect::<Vec<_>>(),
        creator,
        None,
    )
    .await
    .expect("harness token");
    Principal::Token(token)
}
