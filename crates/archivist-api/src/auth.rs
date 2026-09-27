//! Authentication: rate limiting, sessions, cookies, API tokens, CSRF and the auth middleware.

use crate::*;

pub(crate) const SESSION_COOKIE: &str = "pa_session";
pub(crate) const CSRF_COOKIE: &str = "pa_csrf";
/// Hand-rolled per-IP token-bucket limiter used for `/api/auth/*`. We keep
/// it in-process (no external dependency) and stick with a single-instance
/// deploy assumption that matches the rest of the API.
///
/// Each IP gets `capacity` tokens that refill linearly across `window`
/// seconds. Consuming a token while empty rejects the request.
pub(crate) struct AuthRateLimiter {
    pub(crate) capacity: u32,
    pub(crate) window: std::time::Duration,
    pub(crate) buckets: Mutex<HashMap<IpAddr, Bucket>>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Bucket {
    pub(crate) tokens: f64,
    pub(crate) last_refill: Instant,
}

impl AuthRateLimiter {
    pub(crate) fn new(capacity: u32, window_seconds: u64) -> Self {
        let window_seconds = window_seconds.max(1);
        Self {
            capacity,
            window: std::time::Duration::from_secs(window_seconds),
            buckets: Mutex::new(HashMap::new()),
        }
    }

    /// Returns Ok(remaining_tokens) when a request is allowed,
    /// Err(retry_after_seconds) when it is denied.
    pub(crate) fn check(&self, ip: IpAddr, now: Instant) -> Result<f64, u64> {
        if self.capacity == 0 {
            return Ok(0.0);
        }
        let capacity_f = f64::from(self.capacity);
        let refill_per_second = capacity_f / self.window.as_secs_f64();
        let mut buckets = self
            .buckets
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());

        // Opportunistic cleanup: drop stale buckets so the map does not grow
        // without bound for short-lived attackers. Cap the work per call.
        if buckets.len() > 4096 {
            buckets.retain(|_, bucket| {
                now.saturating_duration_since(bucket.last_refill) < self.window * 4
            });
        }

        let bucket = buckets.entry(ip).or_insert(Bucket {
            tokens: capacity_f,
            last_refill: now,
        });
        let elapsed = now
            .saturating_duration_since(bucket.last_refill)
            .as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * refill_per_second).min(capacity_f);
        bucket.last_refill = now;
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            Ok(bucket.tokens)
        } else {
            let missing = 1.0 - bucket.tokens;
            let retry_after = (missing / refill_per_second).ceil() as u64;
            Err(retry_after.max(1))
        }
    }
}

pub(crate) async fn auth_rate_limit_middleware(
    State(state): State<AppState>,
    req: Request,
    next: Next,
) -> Result<Response, ApiError> {
    // No path check here: this layer is only attached to the `auth_public`
    // sub-router. That router is nested at `/api/auth`, so axum strips the
    // prefix and this middleware would only ever see `/login`. A
    // `starts_with("/api/auth/")` guard therefore disabled the limiter. #385
    let ip = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|info| info.0.ip());
    let trusted_proxy = state.config.trust_proxy;
    let header_ip = if trusted_proxy {
        forwarded_for_nearest_hop(req.headers())
    } else {
        None
    };
    let client_ip = header_ip.or(ip);
    let Some(client_ip) = client_ip else {
        // No address available (unit-test transport, etc.) — let it through.
        return Ok(next.run(req).await);
    };
    if let Err(retry_after) = state.auth_rate_limiter.check(client_ip, Instant::now()) {
        let mut response = (
            StatusCode::TOO_MANY_REQUESTS,
            Json(json!({ "error": "too many authentication attempts" })),
        )
            .into_response();
        if let Ok(value) = HeaderValue::from_str(&retry_after.to_string()) {
            response.headers_mut().insert(header::RETRY_AFTER, value);
        }
        return Ok(response);
    }
    Ok(next.run(req).await)
}

/// Extract the client IP from `X-Forwarded-For` using the RIGHTMOST entry.
///
/// `X-Forwarded-For` is built left-to-right: each proxy *appends* the address
/// of the peer it received the request from. The leftmost token is therefore
/// fully attacker-controlled (a client can send any value, and proxies append
/// to the right), so trusting it would allow rate-limit bypass and audit-log
/// IP spoofing. The rightmost entry is the one written by the single trusted
/// reverse proxy sitting directly in front of us, so we trust exactly that hop.
pub(crate) fn forwarded_for_nearest_hop(headers: &HeaderMap) -> Option<IpAddr> {
    let value = headers.get("x-forwarded-for")?.to_str().ok()?;
    let nearest = value.split(',').next_back()?.trim();
    nearest.parse::<IpAddr>().ok()
}

/// Resolve the client IP for audit/logging purposes. When `trust_proxy` is
/// enabled and `X-Forwarded-For` is present, use the nearest (rightmost) hop
/// written by the trusted proxy; otherwise fall back to the TCP peer recorded
/// by axum.
pub(crate) fn request_source_ip(
    state: &AppState,
    headers: &HeaderMap,
    peer: Option<SocketAddr>,
) -> Option<String> {
    let forwarded = if state.config.trust_proxy {
        forwarded_for_nearest_hop(headers)
    } else {
        None
    };
    forwarded
        .or_else(|| peer.map(|addr| addr.ip()))
        .map(|ip| ip.to_string())
}

pub(crate) fn request_user_agent(headers: &HeaderMap) -> Option<String> {
    let raw = headers.get(header::USER_AGENT)?.to_str().ok()?.trim();
    if raw.is_empty() {
        return None;
    }
    // Cap the length we store so an attacker can't bloat audit rows.
    const MAX: usize = 255;
    Some(if raw.len() > MAX {
        raw.chars().take(MAX).collect()
    } else {
        raw.to_owned()
    })
}

#[derive(Debug, Clone)]
pub(crate) struct AuthContext {
    pub(crate) actor_type: String,
    pub(crate) actor_id: Option<String>,
    pub(crate) user_id: Option<Uuid>,
    pub(crate) username: Option<String>,
    pub(crate) roles: Vec<Role>,
    pub(crate) scopes: Vec<String>,
    pub(crate) session_id: Option<Uuid>,
    pub(crate) csrf_secret_hash: Option<String>,
    pub(crate) cookie_auth: bool,
}

pub(crate) async fn issue_session(state: &AppState, user_id: Uuid) -> ApiResult<(String, String)> {
    let session_token = random_token();
    let csrf_token = random_token();
    let session_hash = hash_token(&session_token);
    let csrf_hash = hash_token(&csrf_token);
    let expires_at = Utc::now() + Duration::hours(state.config.session_ttl_hours);
    create_session(&state.pool, user_id, &session_hash, &csrf_hash, expires_at).await?;
    Ok((session_token, csrf_token))
}

pub(crate) fn set_session_cookies(
    headers: &mut HeaderMap,
    config: &AppConfig,
    session_token: &str,
    csrf_token: &str,
) -> Result<(), ApiError> {
    let session_cookie = build_cookie(
        SESSION_COOKIE,
        session_token,
        true,
        config.cookie_secure,
        config.session_ttl_hours,
    );
    let csrf_cookie = build_cookie(
        CSRF_COOKIE,
        csrf_token,
        false,
        config.cookie_secure,
        config.session_ttl_hours,
    );
    headers.append(header::SET_COOKIE, header_value(session_cookie)?);
    headers.append(header::SET_COOKIE, header_value(csrf_cookie)?);
    Ok(())
}

/// Scope the request with its audit context (source IP honoring
/// `trust_proxy`, capped User-Agent) so `archivist_db` fills both fields of
/// every audit event the request writes, including events written deep in
/// DB helpers. Explicit values on an event still win. #441
pub(crate) async fn audit_context_middleware(
    State(state): State<AppState>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let peer = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(peer)| *peer);
    let context = AuditRequestContext {
        source_ip: request_source_ip(&state, request.headers(), peer),
        user_agent: request_user_agent(request.headers()),
    };
    with_audit_request_context(context, next.run(request)).await
}

pub(crate) async fn auth_middleware(
    State(state): State<AppState>,
    mut request: Request<Body>,
    next: Next,
) -> Result<Response, ApiError> {
    let auth = authenticate(&state.pool, request.headers()).await?;
    enforce_csrf(&auth, request.method(), request.headers())?;
    // Declarative per-route permission + auth-kind check, before any
    // extractor or handler runs. #442
    authorize_route(
        &auth,
        request.method(),
        request.extensions().get::<axum::extract::MatchedPath>(),
    )?;
    request.extensions_mut().insert(auth);
    Ok(next.run(request).await)
}

pub(crate) async fn authenticate(
    pool: &DbPool,
    headers: &HeaderMap,
) -> Result<AuthContext, ApiError> {
    if let Some(token) = bearer_token(headers) {
        let token_hash = hash_token(token);
        if let Some(principal) = find_api_token(pool, &token_hash).await? {
            let scopes = effective_token_scopes(&principal.scopes, &principal.creator_roles);
            return Ok(AuthContext {
                actor_type: "api_token".to_owned(),
                actor_id: Some(principal.name),
                user_id: principal.user_id,
                username: None,
                roles: Vec::new(),
                scopes,
                session_id: None,
                csrf_secret_hash: None,
                cookie_auth: false,
            });
        }
    }

    let Some(session_token) = cookie_value(headers, SESSION_COOKIE) else {
        return Err(ApiError::unauthorized("authentication required"));
    };
    let session_hash = hash_token(&session_token);
    let Some(session) = find_session(pool, &session_hash).await? else {
        return Err(ApiError::unauthorized("invalid or expired session"));
    };
    Ok(AuthContext {
        actor_type: "user".to_owned(),
        actor_id: Some(session.user_id.to_string()),
        user_id: Some(session.user_id),
        username: Some(session.username),
        roles: session.roles,
        scopes: Vec::new(),
        session_id: Some(session.session_id),
        csrf_secret_hash: Some(session.csrf_secret_hash),
        cookie_auth: true,
    })
}

pub(crate) fn enforce_csrf(
    auth: &AuthContext,
    method: &Method,
    headers: &HeaderMap,
) -> Result<(), ApiError> {
    if !auth.cookie_auth || matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS) {
        return Ok(());
    }
    let provided = headers
        .get("x-csrf-token")
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| ApiError::forbidden("missing CSRF token"))?;
    let provided_hash = hash_token(provided);
    let expected = auth
        .csrf_secret_hash
        .as_deref()
        .ok_or_else(|| ApiError::forbidden("invalid CSRF token"))?;
    // Constant-time compare to deny timing oracles on the hex hash.
    if expected.len() != provided_hash.len()
        || !bool::from(expected.as_bytes().ct_eq(provided_hash.as_bytes()))
    {
        return Err(ApiError::forbidden("invalid CSRF token"));
    }
    Ok(())
}

pub(crate) fn validate_api_token_name(name: &str) -> Result<(), ApiError> {
    let trimmed = name.trim();
    if trimmed.is_empty() || trimmed.len() > 80 {
        return Err(ApiError::bad_request(
            "API token name must be between 1 and 80 characters",
        ));
    }
    Ok(())
}

pub(crate) fn api_token_expiry(
    settings: &RuntimeSettings,
    requested_days: Option<i64>,
) -> Result<Option<chrono::DateTime<Utc>>, ApiError> {
    let security = settings.clone().normalized().security;
    let days = requested_days.unwrap_or(security.api_token_default_ttl_days);
    if days <= 0 {
        if security.api_token_expiry_required {
            return Err(ApiError::bad_request(
                "API token expiry is required by security policy",
            ));
        }
        return Ok(None);
    }
    if days > security.api_token_max_ttl_days {
        return Err(ApiError::bad_request(format!(
            "API token expiry exceeds maximum of {} days",
            security.api_token_max_ttl_days
        )));
    }
    Ok(Some(Utc::now() + Duration::days(days)))
}

#[derive(Debug, Clone)]
pub(crate) struct Authenticated(pub(crate) AuthContext);

impl<S> axum::extract::FromRequestParts<S> for Authenticated
where
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _state: &S,
    ) -> Result<Self, Self::Rejection> {
        parts
            .extensions
            .get::<AuthContext>()
            .cloned()
            .map(Self)
            .ok_or_else(|| ApiError::unauthorized("authentication required"))
    }
}

pub(crate) fn hash_password(password: &str) -> Result<String> {
    let salt = SaltString::generate(&mut OsRng);
    let params =
        Params::new(19_456, 2, 1, None).map_err(|error| anyhow!("argon2 params: {error}"))?;
    let argon2 = Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params);
    Ok(argon2
        .hash_password(password.as_bytes(), &salt)
        .map_err(|error| anyhow!("hash password: {error}"))?
        .to_string())
}

pub(crate) fn validate_password_strength(password: &str) -> std::result::Result<(), &'static str> {
    if password.chars().count() < 12 {
        return Err("password must be at least 12 characters");
    }
    if password.chars().all(char::is_whitespace) {
        return Err("password must not be blank");
    }
    Ok(())
}

pub(crate) fn verify_password(user: &AuthUser, password: &str) -> Result<bool> {
    let parsed = PasswordHash::new(&user.password_hash)
        .map_err(|error| anyhow!("parse password hash: {error}"))?;
    Ok(Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok())
}

/// A real Argon2id hash computed once at first use. Verifying a candidate
/// password against this when the supplied username does not exist makes the
/// login path spend the same ~Argon2id time it would for a real account,
/// closing the timing side channel that otherwise enumerates valid usernames.
pub(crate) static DUMMY_PASSWORD_HASH: std::sync::LazyLock<Option<String>> =
    std::sync::LazyLock::new(|| hash_password("paperless-archivist-dummy-password").ok());

/// Perform a throwaway Argon2id verification to equalize login timing for
/// non-existent users. The result is intentionally discarded.
pub(crate) fn verify_dummy_password(password: &str) {
    if let Some(hash) = DUMMY_PASSWORD_HASH.as_deref()
        && let Ok(parsed) = PasswordHash::new(hash)
    {
        let _ = Argon2::default().verify_password(password.as_bytes(), &parsed);
    }
}

pub(crate) fn random_token() -> String {
    let mut bytes = [0_u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}

pub(crate) fn build_cookie(
    name: &'static str,
    value: &str,
    http_only: bool,
    secure: bool,
    ttl_hours: i64,
) -> Cookie<'static> {
    let mut cookie = Cookie::build((name, value.to_owned()))
        .path("/")
        .same_site(SameSite::Lax)
        .http_only(http_only)
        .secure(secure)
        .max_age(cookie::time::Duration::hours(ttl_hours))
        .build();
    cookie.set_http_only(http_only);
    cookie
}

pub(crate) fn expire_cookie(name: &'static str, http_only: bool, secure: bool) -> Cookie<'static> {
    Cookie::build((name, ""))
        .path("/")
        .same_site(SameSite::Lax)
        .http_only(http_only)
        .secure(secure)
        .max_age(cookie::time::Duration::seconds(0))
        .build()
}

pub(crate) fn header_value(cookie: Cookie<'static>) -> Result<HeaderValue, ApiError> {
    HeaderValue::from_str(&cookie.to_string())
        .map_err(|_| ApiError::internal("invalid cookie header"))
}

pub(crate) fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
}

pub(crate) fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    let header = headers.get(header::COOKIE)?.to_str().ok()?;
    for cookie in header.split(';') {
        // Skip valueless segments rather than aborting the whole scan — a
        // leading junk cookie without `=` must not break session lookup.
        let Some((key, value)) = cookie.trim().split_once('=') else {
            continue;
        };
        if key == name {
            return Some(value.to_owned());
        }
    }
    None
}
