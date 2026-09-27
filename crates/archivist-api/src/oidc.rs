//! OpenID Connect login flow, discovery, token verification and role mapping.

use crate::*;

#[derive(Debug, Serialize)]
pub(crate) struct OidcConfigResponse {
    pub(crate) enabled: bool,
    pub(crate) login_url: Option<String>,
    pub(crate) provider: Option<String>,
    pub(crate) paperless_login_enabled: bool,
}

#[derive(Debug, Deserialize)]
pub(crate) struct OidcLoginQuery {
    pub(crate) return_to: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct OidcCallbackQuery {
    pub(crate) code: Option<String>,
    pub(crate) state: Option<String>,
    pub(crate) error: Option<String>,
    pub(crate) error_description: Option<String>,
}

pub(crate) struct OidcValues<'a> {
    pub(crate) issuer_url: &'a str,
    pub(crate) client_id: &'a str,
    pub(crate) client_secret: &'a SecretString,
    pub(crate) redirect_uri: &'a str,
}

#[derive(Debug, Deserialize)]
pub(crate) struct OidcProviderMetadata {
    pub(crate) issuer: String,
    pub(crate) authorization_endpoint: String,
    pub(crate) token_endpoint: String,
    pub(crate) jwks_uri: String,
    /// Optional per the discovery spec. Many IdPs (ZITADEL by default) return
    /// profile/email/roles claims here rather than inlining them in the ID
    /// token, so the callback fetches it to populate roles and the username/
    /// email allowlist. #299.
    #[serde(default)]
    pub(crate) userinfo_endpoint: Option<String>,
    #[serde(default)]
    pub(crate) id_token_signing_alg_values_supported: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct OidcTokenResponse {
    pub(crate) access_token: String,
    pub(crate) id_token: String,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct OidcIdClaims {
    pub(crate) iss: String,
    pub(crate) sub: String,
    pub(crate) aud: Value,
    pub(crate) exp: i64,
    #[serde(default)]
    pub(crate) nonce: Option<String>,
    #[serde(default)]
    pub(crate) email: Option<String>,
    #[serde(default)]
    pub(crate) email_verified: Option<bool>,
    #[serde(default)]
    pub(crate) preferred_username: Option<String>,
    #[serde(default)]
    pub(crate) at_hash: Option<String>,
    /// Every claim not captured by a named field above — including the IdP's
    /// roles claim, whose name is operator-configurable and not a fixed Rust
    /// field (ZITADEL uses URN-style claim names). Read via [`oidc_idp_roles`].
    #[serde(flatten)]
    pub(crate) additional: serde_json::Map<String, Value>,
}

impl OidcIdClaims {
    /// Merge claims fetched from the IdP userinfo endpoint. The signed ID token
    /// wins for identity fields it already carries; userinfo only FILLS gaps and
    /// contributes claims the ID token lacked (notably the roles claim, which
    /// ZITADEL returns from userinfo rather than the ID token by default). The
    /// caller must have verified the userinfo `sub` equals the ID token `sub`.
    pub(crate) fn merge_userinfo(&mut self, userinfo: serde_json::Map<String, Value>) {
        if self.preferred_username.is_none()
            && let Some(value) = userinfo.get("preferred_username").and_then(Value::as_str)
        {
            self.preferred_username = Some(value.to_owned());
        }
        if self.email.is_none()
            && let Some(value) = userinfo.get("email").and_then(Value::as_str)
        {
            self.email = Some(value.to_owned());
        }
        if self.email_verified.is_none()
            && let Some(value) = userinfo.get("email_verified").and_then(Value::as_bool)
        {
            self.email_verified = Some(value);
        }
        // Contribute any claim the ID token did not already carry (roles, etc.);
        // never overwrite a signed ID-token claim.
        for (key, value) in userinfo {
            self.additional.entry(key).or_insert(value);
        }
    }
}

pub(crate) async fn oidc_config(State(state): State<AppState>) -> Json<OidcConfigResponse> {
    let paperless_login_enabled = get_runtime_settings(&state.pool)
        .await
        .map(|settings| settings.paperless.login_bridge_enabled)
        .unwrap_or(false);
    Json(OidcConfigResponse {
        enabled: state.config.oidc_enabled,
        login_url: state
            .config
            .oidc_enabled
            .then(|| "/api/auth/oidc/login".to_owned()),
        provider: state.config.oidc_enabled.then(|| "ZITADEL".to_owned()),
        paperless_login_enabled,
    })
}

pub(crate) async fn oidc_login(
    State(state): State<AppState>,
    Query(query): Query<OidcLoginQuery>,
) -> ApiResult<Response> {
    let values = oidc_values(&state.config)?;
    let http_client = oidc_http_client()?;
    let provider_metadata = oidc_discover(&http_client, values.issuer_url).await?;
    let csrf_state = random_token();
    let nonce = random_token();
    let pkce_verifier = random_token();
    let auth_url = oidc_authorization_url(
        &provider_metadata,
        &values,
        &oidc_scopes(&state.config),
        &csrf_state,
        &nonce,
        &pkce_challenge(&pkce_verifier),
    )?;
    let return_to = safe_return_to(query.return_to.as_deref());
    create_oidc_login_state(
        &state.pool,
        &hash_token(&csrf_state),
        &nonce,
        &pkce_verifier,
        return_to.as_deref(),
        Utc::now() + Duration::minutes(OIDC_STATE_TTL_MINUTES),
    )
    .await?;

    let mut response = Redirect::temporary(&auth_url).into_response();
    // Bind the login attempt to this browser: the callback only accepts a
    // `state` that matches this cookie, so an attacker cannot hand a victim
    // a callback URL carrying the attacker's own code/state (login CSRF). #387
    response.headers_mut().append(
        header::SET_COOKIE,
        header_value(oidc_state_cookie(&csrf_state, state.config.cookie_secure))?,
    );
    Ok(response)
}

pub(crate) const OIDC_STATE_COOKIE: &str = "pa_oidc_state";
pub(crate) const OIDC_STATE_TTL_MINUTES: i64 = 10;

/// Short-lived, HttpOnly browser binding for an in-progress OIDC login.
/// `SameSite=Lax` still sends it on the IdP's top-level GET redirect back to
/// the callback. Path `/` keeps it working behind prefix-rewriting proxies. #387
pub(crate) fn oidc_state_cookie(value: &str, secure: bool) -> Cookie<'static> {
    Cookie::build((OIDC_STATE_COOKIE, value.to_owned()))
        .path("/")
        .same_site(SameSite::Lax)
        .http_only(true)
        .secure(secure)
        .max_age(cookie::time::Duration::minutes(OIDC_STATE_TTL_MINUTES))
        .build()
}

/// Reject a callback whose `state` was not issued to this browser. Constant
/// time so the comparison leaks nothing about the stored value. #387
pub(crate) fn verify_oidc_state_binding(headers: &HeaderMap, state_value: &str) -> ApiResult<()> {
    let Some(bound) = cookie_value(headers, OIDC_STATE_COOKIE).filter(|value| !value.is_empty())
    else {
        return Err(ApiError::unauthorized(
            "OIDC login was not started in this browser",
        ));
    };
    if bound.len() != state_value.len()
        || !bool::from(bound.as_bytes().ct_eq(state_value.as_bytes()))
    {
        return Err(ApiError::unauthorized(
            "OIDC state does not match this browser",
        ));
    }
    Ok(())
}

pub(crate) async fn oidc_callback(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Query(query): Query<OidcCallbackQuery>,
) -> Response {
    let secure = state.config.cookie_secure;
    let mut response = oidc_callback_inner(state, peer, headers, query)
        .await
        .unwrap_or_else(IntoResponse::into_response);
    // The browser binding is single-use: clear it on every outcome so a
    // stale state cookie can never pair with a later callback. #387
    if let Ok(value) = header_value(expire_cookie(OIDC_STATE_COOKIE, true, secure)) {
        response.headers_mut().append(header::SET_COOKIE, value);
    }
    response
}

pub(crate) async fn oidc_callback_inner(
    state: AppState,
    peer: SocketAddr,
    headers: HeaderMap,
    query: OidcCallbackQuery,
) -> ApiResult<Response> {
    let source_ip = request_source_ip(&state, &headers, Some(peer));
    let user_agent = request_user_agent(&headers);
    if let Some(error) = query.error {
        let description = query.error_description.unwrap_or_default();
        return Err(ApiError::unauthorized(format!(
            "OIDC login failed: {error} {description}"
        )));
    }
    let code = query
        .code
        .ok_or_else(|| ApiError::bad_request("missing OIDC code"))?;
    let state_value = query
        .state
        .ok_or_else(|| ApiError::bad_request("missing OIDC state"))?;
    // Check the browser binding before consuming the server-side state so a
    // forged callback cannot even burn the attacker-initiated login. #387
    verify_oidc_state_binding(&headers, &state_value)?;
    let login_state = consume_oidc_login_state(&state.pool, &hash_token(&state_value))
        .await?
        .ok_or_else(|| ApiError::unauthorized("invalid or expired OIDC state"))?;

    let values = oidc_values(&state.config)?;
    let http_client = oidc_http_client()?;
    let provider_metadata = oidc_discover(&http_client, values.issuer_url).await?;
    let token_response = oidc_exchange_code(
        &http_client,
        &provider_metadata,
        &values,
        &code,
        &login_state.pkce_verifier,
    )
    .await?;
    let mut claims = oidc_verify_id_token(
        &http_client,
        &provider_metadata,
        &values,
        &token_response.id_token,
        &login_state.nonce,
    )
    .await?;
    if let Some(expected_hash) = claims.at_hash.as_deref() {
        let header = decode_header(&token_response.id_token).map_err(|error| {
            ApiError::unauthorized(format!("OIDC ID token header error: {error}"))
        })?;
        if !oidc_access_token_hash_matches(header.alg, &token_response.access_token, expected_hash)
        {
            return Err(ApiError::unauthorized("OIDC access token hash mismatch"));
        }
    }

    // ZITADEL (and many IdPs) return profile/email/roles from the userinfo
    // endpoint rather than inlining them in the ID token — a minimal ID token
    // then carries only `sub`, which the username/email allowlist and the roles
    // claim cannot match. Fetch userinfo (sub-verified, best-effort) and merge
    // it so role-based admin and the allowlist work regardless of whether the
    // IdP inlines user info into the ID token. #299.
    if let Some(userinfo_endpoint) = provider_metadata.userinfo_endpoint.as_deref() {
        let id_sub = claims.sub.clone();
        match oidc_fetch_userinfo(
            &http_client,
            userinfo_endpoint,
            &token_response.access_token,
            &id_sub,
        )
        .await
        {
            Some(userinfo) => claims.merge_userinfo(userinfo),
            None => warn!(
                "OIDC userinfo fetch returned no usable claims; proceeding on the ID token alone"
            ),
        }
    }

    let subject = claims.sub.as_str();
    // OIDC Core §5.7: the email claim is only trustworthy when the IdP
    // asserts email_verified=true. An unverified email must not influence
    // admin-role mapping, account linking, or the derived username —
    // otherwise an attacker who can set a free-form email at the IdP could
    // escalate to the allowlisted admin or take over a local account.
    let email = oidc_verified_email(&claims);
    let claims_degraded = oidc_claims_degraded(&claims);
    let username = oidc_username(
        claims
            .preferred_username
            .as_deref()
            .or(email)
            .unwrap_or(subject),
    );
    let resolution = oidc_roles(&state.config, &claims, subject, &username, email)?;
    let roles = resolution.roles;
    // Degraded ID token (#299): without preferred_username and a verified
    // email the *identity-derived* roles (allowlist by username/email) can't be
    // matched, so a returning user must keep their existing roles instead of
    // being silently demoted. This guard only applies when the roles are NOT
    // authoritative — if the IdP asserted a roles claim (or the subject is
    // allowlisted), those roles win, including a deliberate demotion. #289/#299.
    let preserve_existing_roles =
        !resolution.authoritative && claims_degraded && !roles.contains(&Role::Admin);
    if claims_degraded && !resolution.authoritative {
        warn!(
            subject_hash = %hash_token(subject),
            email_claim_present = claims.email.is_some(),
            "OIDC ID token carries neither preferred_username nor a verified email and no IdP \
             roles claim; existing roles are preserved unless the subject is allowlisted \
             (configure the IdP to include profile/email or roles claims in the ID token)"
        );
    }
    if !resolution.idp_claim_present {
        // Self-diagnosis for the common misconfiguration where the IdP is not
        // asserting roles into the ID token (so role-based admin can't work).
        // Log only the claim *names* present (never values) so an operator can
        // see whether the roles claim is there and under what name. #299.
        let available_claims: Vec<&str> = claims.additional.keys().map(String::as_str).collect();
        warn!(
            configured_roles_claim = %state.config.oidc_roles_claim,
            available_claims = ?available_claims,
            "OIDC ID token carried no recognizable roles claim; roles fall back to the admin \
             allowlist/defaults. Enable role assertion into the ID token at the IdP (ZITADEL: \
             'Assert Roles on Authentication'), or set ARCHIVIST_OIDC_ROLES_CLAIM to one of the \
             claim names listed here."
        );
    }
    let allow_username_link = roles.contains(&Role::Admin);
    let disabled_password_hash = hash_password(&random_token())?;
    let user = upsert_oidc_user(
        &state.pool,
        OidcUserInput {
            provider: "zitadel",
            subject,
            username: &username,
            email,
            disabled_password_hash: &disabled_password_hash,
            roles: &roles,
            allow_username_link,
            allow_email_link: state.config.oidc_allow_email_link,
            preserve_existing_roles,
        },
    )
    .await?;
    if !user.enabled {
        return Err(ApiError::unauthorized("user is disabled"));
    }

    record_login_success(
        &state.pool,
        user.id,
        source_ip.as_deref(),
        user_agent.as_deref(),
    )
    .await?;
    append_audit(
        &state.pool,
        AuditEventInput {
            event_type: "auth.oidc_login_success".to_owned(),
            actor_type: "user".to_owned(),
            actor_id: Some(user.id.to_string()),
            run_id: None,
            job_id: None,
            paperless_document_id: None,
            before: None,
            after: None,
            metadata: Some(json!({
                "username": user.username,
                "issuer": values.issuer_url,
                "subject_hash": hash_token(subject)
            })),
            outcome: "success".to_owned(),
            error_message: None,
            source_ip: source_ip.clone(),
            user_agent: user_agent.clone(),
        },
    )
    .await?;

    let (session_token, csrf_token) = issue_session(&state, user.id).await?;
    let mut response =
        Redirect::to(login_state.return_to.as_deref().unwrap_or("/")).into_response();
    set_session_cookies(
        response.headers_mut(),
        &state.config,
        &session_token,
        &csrf_token,
    )?;
    Ok(response)
}

pub(crate) fn oidc_values(config: &AppConfig) -> ApiResult<OidcValues<'_>> {
    if !config.oidc_enabled {
        return Err(ApiError::bad_request("OIDC is not enabled"));
    }
    Ok(OidcValues {
        issuer_url: config
            .oidc_issuer_url
            .as_deref()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| ApiError::internal("OIDC issuer URL is not configured"))?,
        client_id: config
            .oidc_client_id
            .as_deref()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| ApiError::internal("OIDC client ID is not configured"))?,
        client_secret: config
            .oidc_client_secret
            .as_ref()
            .filter(|value| !value.expose_secret().is_empty())
            .ok_or_else(|| ApiError::internal("OIDC client secret is not configured"))?,
        redirect_uri: config
            .oidc_redirect_uri
            .as_deref()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| ApiError::internal("OIDC redirect URI is not configured"))?,
    })
}

pub(crate) fn oidc_http_client() -> ApiResult<HttpClient> {
    HttpClient::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|error| ApiError::internal(format!("build OIDC HTTP client: {error}")))
}

pub(crate) async fn oidc_discover(
    http_client: &HttpClient,
    issuer_url: &str,
) -> ApiResult<OidcProviderMetadata> {
    let issuer = Url::parse(issuer_url)
        .map_err(|error| ApiError::internal(format!("invalid OIDC issuer URL: {error}")))?;
    if issuer.scheme() != "https" && issuer.host_str() != Some("localhost") {
        return Err(ApiError::internal("OIDC issuer URL must use HTTPS"));
    }
    let discovery_url = format!(
        "{}/.well-known/openid-configuration",
        issuer_url.trim_end_matches('/')
    );
    // Log upstream detail (issuer URL, connection info) server-side but return
    // a generic message — these endpoints are publicly reachable. #291
    let metadata = http_client
        .get(&discovery_url)
        .send()
        .await
        .map_err(|error| {
            tracing::error!(%error, "OIDC discovery request failed");
            ApiError::internal("OIDC discovery failed")
        })?
        .error_for_status()
        .map_err(|error| {
            tracing::error!(%error, "OIDC discovery returned an error status");
            ApiError::internal("OIDC discovery failed")
        })?
        .json::<OidcProviderMetadata>()
        .await
        .map_err(|error| {
            tracing::error!(%error, "OIDC discovery parse failed");
            ApiError::internal("OIDC discovery failed")
        })?;
    if metadata.issuer.trim_end_matches('/') != issuer_url.trim_end_matches('/') {
        return Err(ApiError::unauthorized("OIDC issuer mismatch"));
    }
    Ok(metadata)
}

pub(crate) fn oidc_authorization_url(
    metadata: &OidcProviderMetadata,
    values: &OidcValues<'_>,
    scopes: &[String],
    csrf_state: &str,
    nonce: &str,
    code_challenge: &str,
) -> ApiResult<String> {
    let mut url = Url::parse(&metadata.authorization_endpoint).map_err(|error| {
        ApiError::internal(format!("invalid OIDC authorization endpoint: {error}"))
    })?;
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", values.client_id)
        .append_pair("redirect_uri", values.redirect_uri)
        .append_pair("scope", &scopes.join(" "))
        .append_pair("state", csrf_state)
        .append_pair("nonce", nonce)
        .append_pair("code_challenge", code_challenge)
        .append_pair("code_challenge_method", "S256");
    Ok(url.to_string())
}

pub(crate) fn pkce_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

pub(crate) async fn oidc_exchange_code(
    http_client: &HttpClient,
    metadata: &OidcProviderMetadata,
    values: &OidcValues<'_>,
    code: &str,
    pkce_verifier: &str,
) -> ApiResult<OidcTokenResponse> {
    let client_secret = values.client_secret.expose_secret();
    http_client
        .post(&metadata.token_endpoint)
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", values.redirect_uri),
            ("client_id", values.client_id),
            ("client_secret", client_secret),
            ("code_verifier", pkce_verifier),
        ])
        .send()
        .await
        .map_err(|error| {
            tracing::error!(%error, "OIDC code exchange request failed");
            ApiError::unauthorized("OIDC code exchange failed")
        })?
        .error_for_status()
        .map_err(|error| {
            tracing::error!(%error, "OIDC code exchange returned an error status");
            ApiError::unauthorized("OIDC code exchange failed")
        })?
        .json::<OidcTokenResponse>()
        .await
        .map_err(|error| {
            tracing::error!(%error, "OIDC token response parse failed");
            ApiError::unauthorized("OIDC code exchange failed")
        })
}

pub(crate) async fn oidc_verify_id_token(
    http_client: &HttpClient,
    metadata: &OidcProviderMetadata,
    values: &OidcValues<'_>,
    id_token: &str,
    expected_nonce: &str,
) -> ApiResult<OidcIdClaims> {
    let header = decode_header(id_token)
        .map_err(|error| ApiError::unauthorized(format!("OIDC ID token header error: {error}")))?;
    if matches!(
        header.alg,
        Algorithm::HS256 | Algorithm::HS384 | Algorithm::HS512
    ) {
        return Err(ApiError::unauthorized(
            "OIDC ID token uses unsupported symmetric signing",
        ));
    }
    if !metadata.id_token_signing_alg_values_supported.is_empty()
        && !metadata
            .id_token_signing_alg_values_supported
            .iter()
            .any(|algorithm| algorithm == oidc_algorithm_name(header.alg))
    {
        return Err(ApiError::unauthorized(
            "OIDC ID token algorithm is not supported by issuer metadata",
        ));
    }

    let kid = header
        .kid
        .as_deref()
        .ok_or_else(|| ApiError::unauthorized("OIDC ID token has no key id"))?;
    let jwks = http_client
        .get(&metadata.jwks_uri)
        .send()
        .await
        .map_err(|error| ApiError::unauthorized(format!("OIDC JWKS request failed: {error}")))?
        .error_for_status()
        .map_err(|error| ApiError::unauthorized(format!("OIDC JWKS request failed: {error}")))?
        .json::<JwkSet>()
        .await
        .map_err(|error| ApiError::unauthorized(format!("OIDC JWKS parse failed: {error}")))?;
    let jwk = jwks
        .find(kid)
        .ok_or_else(|| ApiError::unauthorized("OIDC signing key not found"))?;
    let decoding_key = DecodingKey::from_jwk(jwk)
        .map_err(|error| ApiError::unauthorized(format!("OIDC signing key rejected: {error}")))?;
    let mut validation = Validation::new(header.alg);
    validation.set_audience(&[values.client_id]);
    validation.set_issuer(&[metadata.issuer.as_str()]);
    validation.set_required_spec_claims(&["exp", "iss", "sub", "aud"]);

    let claims = decode::<OidcIdClaims>(id_token, &decoding_key, &validation)
        .map_err(|error| {
            ApiError::unauthorized(format!("OIDC ID token validation failed: {error}"))
        })?
        .claims;
    if claims.nonce.as_deref() != Some(expected_nonce) {
        return Err(ApiError::unauthorized("OIDC nonce mismatch"));
    }
    Ok(claims)
}

/// Fetch the IdP userinfo endpoint with the access token and return its claims.
///
/// Verifies the userinfo `sub` equals the ID token `sub` (OIDC Core §5.3.2) so a
/// swapped or forged response cannot inject another user's identity or roles.
/// Best-effort: any transport/parse error or sub mismatch yields `None`, and the
/// caller proceeds on the signed ID token alone.
pub(crate) async fn oidc_fetch_userinfo(
    http_client: &HttpClient,
    userinfo_endpoint: &str,
    access_token: &str,
    expected_sub: &str,
) -> Option<serde_json::Map<String, Value>> {
    let response = http_client
        .get(userinfo_endpoint)
        .bearer_auth(access_token)
        .send()
        .await
        .ok()?
        .error_for_status()
        .ok()?;
    let claims = response
        .json::<serde_json::Map<String, Value>>()
        .await
        .ok()?;
    if claims.get("sub").and_then(Value::as_str) != Some(expected_sub) {
        warn!("OIDC userinfo sub did not match the ID token sub; ignoring userinfo");
        return None;
    }
    Some(claims)
}

pub(crate) fn oidc_access_token_hash_matches(
    alg: Algorithm,
    access_token: &str,
    expected_hash: &str,
) -> bool {
    let digest = match alg {
        Algorithm::RS256 | Algorithm::PS256 | Algorithm::ES256 => {
            Sha256::digest(access_token.as_bytes()).to_vec()
        }
        Algorithm::RS384 | Algorithm::PS384 | Algorithm::ES384 => {
            Sha384::digest(access_token.as_bytes()).to_vec()
        }
        Algorithm::RS512 | Algorithm::PS512 | Algorithm::EdDSA => {
            Sha512::digest(access_token.as_bytes()).to_vec()
        }
        Algorithm::HS256 | Algorithm::HS384 | Algorithm::HS512 => return false,
    };
    URL_SAFE_NO_PAD.encode(&digest[..digest.len() / 2]) == expected_hash
}

pub(crate) fn oidc_algorithm_name(alg: Algorithm) -> &'static str {
    match alg {
        Algorithm::HS256 => "HS256",
        Algorithm::HS384 => "HS384",
        Algorithm::HS512 => "HS512",
        Algorithm::ES256 => "ES256",
        Algorithm::ES384 => "ES384",
        Algorithm::RS256 => "RS256",
        Algorithm::RS384 => "RS384",
        Algorithm::RS512 => "RS512",
        Algorithm::PS256 => "PS256",
        Algorithm::PS384 => "PS384",
        Algorithm::PS512 => "PS512",
        Algorithm::EdDSA => "EdDSA",
    }
}

pub(crate) fn oidc_scopes(config: &AppConfig) -> Vec<String> {
    let mut scopes = config
        .oidc_scopes
        .split([',', ' ', '\n', '\t'])
        .map(str::trim)
        .filter(|scope| !scope.is_empty())
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    if !scopes.iter().any(|scope| scope == "openid") {
        scopes.insert(0, "openid".to_owned());
    }
    scopes
}

pub(crate) fn safe_return_to(value: Option<&str>) -> Option<String> {
    let value = value.unwrap_or("/");
    // Reject backslashes too: browsers normalize `\` to `/` per the WHATWG URL
    // spec, so `/\evil.com` becomes the protocol-relative `//evil.com` and
    // redirects off-origin (CWE-601). #271
    if value.starts_with('/')
        && !value.starts_with("//")
        && !value.contains('\\')
        && !value.contains('\r')
        && !value.contains('\n')
    {
        Some(value.to_owned())
    } else {
        Some("/".to_owned())
    }
}

/// The email claim, but only when the IdP asserted `email_verified=true` —
/// the only condition under which OIDC permits using it for authorization.
pub(crate) fn oidc_verified_email(claims: &OidcIdClaims) -> Option<&str> {
    claims
        .email
        .as_deref()
        .filter(|_| claims.email_verified == Some(true))
}

/// An ID token is "degraded" when it carries neither a usable
/// `preferred_username` nor a verified email (#299): the derived username
/// then falls back to the raw subject, and a recomputed role set would be
/// based on identities the operator never allowlisted. Such tokens must not
/// drive role demotions for returning users.
pub(crate) fn oidc_claims_degraded(claims: &OidcIdClaims) -> bool {
    claims
        .preferred_username
        .as_deref()
        .is_none_or(|value| value.trim().is_empty())
        && oidc_verified_email(claims).is_none()
}

pub(crate) fn oidc_username(value: &str) -> String {
    let mut username = value
        .trim()
        .to_ascii_lowercase()
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-' | '@') {
                ch
            } else {
                '-'
            }
        })
        .collect::<String>();
    while username.contains("--") {
        username = username.replace("--", "-");
    }
    let username = username
        .trim_matches(|ch| matches!(ch, '.' | '_' | '-' | '@'))
        .chars()
        .take(80)
        .collect::<String>();
    if username.is_empty() {
        "oidc-user".to_owned()
    } else {
        username
    }
}

/// Outcome of resolving an OIDC login's roles.
pub(crate) struct OidcRoleResolution {
    pub(crate) roles: Vec<Role>,
    /// True when the roles are authoritative for this login: the IdP asserted a
    /// roles claim, or the subject is on the admin allowlist. When false the
    /// roles are a fallback (the configured defaults) and a degraded token must
    /// not be allowed to demote a returning user (#299).
    pub(crate) authoritative: bool,
    /// True when the ID token carried a recognizable roles claim at all
    /// (independent of whether any role mapped). Drives the diagnostic that
    /// tells an operator their IdP is not asserting roles into the ID token.
    pub(crate) idp_claim_present: bool,
}

/// Resolve the effective roles for an OIDC login. Precedence:
///
/// 1. The admin allowlist (`ARCHIVIST_OIDC_ADMIN_USERS`) is a break-glass that
///    always grants full admin — it keeps working even when the IdP stops
///    asserting roles, and matches the immutable `sub` (#299).
/// 2. Roles the IdP asserted in the configured roles claim, mapped through
///    `ARCHIVIST_OIDC_ROLE_MAPPINGS`.
///
/// Their union is authoritative and replaces the stored roles on every login
/// (#289). Only when neither source yields a role do the configured default
/// roles apply, and that fallback is *not* authoritative.
pub(crate) fn oidc_roles(
    config: &AppConfig,
    claims: &OidcIdClaims,
    subject: &str,
    username: &str,
    email: Option<&str>,
) -> ApiResult<OidcRoleResolution> {
    let mut roles: Vec<Role> = Vec::new();

    let allowlisted = oidc_is_admin(config, subject, username, email);
    if allowlisted {
        roles.extend([Role::Admin, Role::Operator, Role::Reviewer, Role::Auditor]);
    }

    let idp_roles = oidc_idp_roles(config, claims);
    let idp_claim_present = idp_roles.is_some();
    for role in idp_roles.into_iter().flatten() {
        if !roles.contains(&role) {
            roles.push(role);
        }
    }

    let authoritative = allowlisted || idp_claim_present;
    if roles.is_empty() {
        roles = parse_oidc_roles(&config.oidc_default_roles)
            .map_err(|error| ApiError::internal(format!("invalid OIDC role config: {error}")))?;
    }

    Ok(OidcRoleResolution {
        roles,
        authoritative,
        idp_claim_present,
    })
}

/// Read and map the roles the IdP asserted in the ID token.
///
/// Returns `None` when the token carries no recognizable roles claim — the
/// caller treats that as "the IdP did not assert roles" and falls back without
/// demoting a returning user. Returns `Some(roles)` when a roles claim is
/// present; the vec may be empty if none of the asserted roles map to an app
/// role. Unmapped IdP roles are dropped, so the IdP can never grant a role the
/// operator did not explicitly map (no privilege escalation). #299.
pub(crate) fn oidc_idp_roles(config: &AppConfig, claims: &OidcIdClaims) -> Option<Vec<Role>> {
    let value = oidc_roles_claim_value(config, claims)?;
    let mappings = parse_oidc_role_mappings(&config.oidc_role_mappings);
    let mut roles = Vec::new();
    for raw in extract_role_strings(value) {
        let key = raw.trim().to_ascii_lowercase();
        if let Some(role) = mappings.get(key.as_str())
            && !roles.contains(role)
        {
            roles.push(role.clone());
        }
    }
    Some(roles)
}

/// Locate the roles claim value in the token: the operator-configured claim
/// name first, then the well-known ZITADEL project-roles claims — the generic
/// `urn:zitadel:iam:org:project:roles` and any project-scoped
/// `urn:zitadel:iam:org:project:<projectid>:roles`. This makes the default work
/// whether the deployment surfaces roles in the generic or the scoped claim.
pub(crate) fn oidc_roles_claim_value<'a>(
    config: &AppConfig,
    claims: &'a OidcIdClaims,
) -> Option<&'a Value> {
    let configured = config.oidc_roles_claim.trim();
    if !configured.is_empty()
        && let Some(value) = claims.additional.get(configured)
    {
        return Some(value);
    }
    if let Some(value) = claims.additional.get("urn:zitadel:iam:org:project:roles") {
        return Some(value);
    }
    claims
        .additional
        .iter()
        .find(|(key, _)| key.starts_with("urn:zitadel:iam:org:project:") && key.ends_with(":roles"))
        .map(|(_, value)| value)
}

/// Pull the raw role names out of a roles claim value, accepting the three
/// shapes IdPs use: a ZITADEL-style object (the KEYS are the granted roles), a
/// JSON array of strings, or a single space/comma-delimited string.
pub(crate) fn extract_role_strings(value: &Value) -> Vec<String> {
    match value {
        Value::Object(map) => map.keys().cloned().collect(),
        Value::Array(items) => items
            .iter()
            .filter_map(|item| item.as_str().map(str::to_owned))
            .collect(),
        Value::String(text) => text
            .split([',', ' ', '\n', '\t'])
            .map(str::trim)
            .filter(|token| !token.is_empty())
            .map(str::to_owned)
            .collect(),
        _ => Vec::new(),
    }
}

/// Parse `ARCHIVIST_OIDC_ROLE_MAPPINGS` (`idp-role=app-role,…`) into a lookup
/// keyed by the lowercased IdP role. Malformed entries (no `=`, empty side, or
/// an unknown app role) are skipped rather than failing the login.
pub(crate) fn parse_oidc_role_mappings(value: &str) -> HashMap<String, Role> {
    let mut map = HashMap::new();
    for entry in value
        .split([',', '\n', '\t'])
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
    {
        let Some((idp, app)) = entry.split_once('=') else {
            continue;
        };
        let idp = idp.trim().to_ascii_lowercase();
        if idp.is_empty() {
            continue;
        }
        if let Ok(role) = app.trim().to_ascii_lowercase().parse::<Role>() {
            map.insert(idp, role);
        }
    }
    map
}

// NOTE (#291): the `username` matched here is derived from the IdP's
// `preferred_username` claim. The email path is gated on `email_verified`, but
// username matching trusts the IdP to keep `preferred_username` unique and
// non-user-settable (true for ZITADEL). For an IdP that lets users choose it,
// prefer allowlisting by verified email or the immutable `sub` (#299).
pub(crate) fn oidc_is_admin(
    config: &AppConfig,
    subject: &str,
    username: &str,
    email: Option<&str>,
) -> bool {
    let username = username.to_ascii_lowercase();
    let email = email.map(str::to_ascii_lowercase);
    config
        .oidc_admin_users
        .split([',', ' ', '\n', '\t'])
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .any(|admin| {
            // The immutable subject is matched verbatim (`sub` is
            // case-sensitive per OIDC Core §2), so the allowlist keeps
            // working even when the ID token carries no preferred_username
            // and no verified email (#299).
            if admin == subject {
                return true;
            }
            let admin = admin.to_ascii_lowercase();
            admin == username
                || email
                    .as_deref()
                    .is_some_and(|email_value| admin == email_value)
        })
}

pub(crate) fn parse_oidc_roles(value: &str) -> Result<Vec<Role>> {
    let mut roles = Vec::new();
    for token in value
        .split([',', ' ', '\n', '\t'])
        .map(str::trim)
        .filter(|token| !token.is_empty())
    {
        let role = token
            .parse::<Role>()
            .map_err(|error| anyhow!("invalid role {token}: {error}"))?;
        if !roles.contains(&role) {
            roles.push(role);
        }
    }
    if roles.is_empty() {
        roles.push(Role::Viewer);
    }
    Ok(roles)
}
