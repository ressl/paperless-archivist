//! Local login, Paperless login bridge, logout, profile, password and session endpoints.

use crate::*;

#[derive(Debug, Deserialize)]
pub(crate) struct LoginRequest {
    pub(crate) username: String,
    pub(crate) password: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct MeResponse {
    pub(crate) username: String,
    pub(crate) roles: Vec<Role>,
    /// Permission flags derived from `roles`, exposed so the frontend can gate
    /// fetches/actions on the same matrix the server enforces rather than on a
    /// hardcoded role name (see #98).
    pub(crate) permissions: PermissionFlags,
    pub(crate) csrf_token: Option<String>,
}

#[derive(Debug, Default, Serialize)]
pub(crate) struct PermissionFlags {
    pub(crate) read_dashboard: bool,
    pub(crate) read_runs: bool,
    pub(crate) write_runs: bool,
    pub(crate) read_inventory: bool,
    pub(crate) write_batches: bool,
    pub(crate) use_chat: bool,
    pub(crate) read_reviews: bool,
    pub(crate) write_reviews: bool,
    pub(crate) read_settings: bool,
    pub(crate) write_settings: bool,
    pub(crate) manage_users: bool,
    pub(crate) read_audit: bool,
}

impl PermissionFlags {
    pub(crate) fn from_roles(roles: &[Role]) -> Self {
        Self {
            read_dashboard: roles_have_permission(roles, Permission::ReadDashboard),
            read_runs: roles_have_permission(roles, Permission::ReadRuns),
            write_runs: roles_have_permission(roles, Permission::WriteRuns),
            read_inventory: roles_have_permission(roles, Permission::ReadInventory),
            write_batches: roles_have_permission(roles, Permission::WriteBatches),
            use_chat: roles_have_permission(roles, Permission::UseChat),
            read_reviews: roles_have_permission(roles, Permission::ReadReviews),
            write_reviews: roles_have_permission(roles, Permission::WriteReviews),
            read_settings: roles_have_permission(roles, Permission::ReadSettings),
            write_settings: roles_have_permission(roles, Permission::WriteSettings),
            manage_users: roles_have_permission(roles, Permission::ManageUsers),
            read_audit: roles_have_permission(roles, Permission::ReadAudit),
        }
    }
}

pub(crate) async fn login(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(request): Json<LoginRequest>,
) -> ApiResult<Response> {
    let source_ip = request_source_ip(&state, &headers, Some(peer));
    let user_agent = request_user_agent(&headers);
    let user = find_user_for_login(&state.pool, &request.username).await?;
    let Some(user) = user else {
        // Spend the same Argon2id time as a real account so an attacker can't
        // distinguish existing from non-existing usernames by response latency.
        verify_dummy_password(&request.password);
        record_login_failure(
            &state.pool,
            None,
            &request.username,
            source_ip.as_deref(),
            user_agent.as_deref(),
        )
        .await?;
        return Err(ApiError::unauthorized("invalid credentials"));
    };
    // Always run the password verification first (even for locked/disabled
    // accounts) so none of the rejection paths can be told apart from a wrong
    // password by response latency. #291
    let password_ok = verify_password(&user, &request.password)?;
    if user
        .locked_until
        .is_some_and(|locked_until| locked_until > Utc::now())
    {
        return Err(ApiError::unauthorized("invalid credentials"));
    }
    if !user.enabled || !password_ok {
        record_login_failure(
            &state.pool,
            Some(user.id),
            &request.username,
            source_ip.as_deref(),
            user_agent.as_deref(),
        )
        .await?;
        return Err(ApiError::unauthorized("invalid credentials"));
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
            event_type: "auth.login_success".to_owned(),
            actor_type: "user".to_owned(),
            actor_id: Some(user.id.to_string()),
            run_id: None,
            job_id: None,
            paperless_document_id: None,
            before: None,
            after: None,
            metadata: Some(json!({ "username": user.username })),
            outcome: "success".to_owned(),
            error_message: None,
            source_ip: source_ip.clone(),
            user_agent: user_agent.clone(),
        },
    )
    .await?;

    let (session_token, csrf_token) = issue_session(&state, user.id).await?;

    let permissions = PermissionFlags::from_roles(&user.roles);
    let body = Json(MeResponse {
        username: user.username,
        roles: user.roles,
        permissions,
        csrf_token: Some(csrf_token.clone()),
    });
    let mut response = body.into_response();
    set_session_cookies(
        response.headers_mut(),
        &state.config,
        &session_token,
        &csrf_token,
    )?;
    Ok(response)
}

#[derive(Debug, Deserialize)]
pub(crate) struct PaperlessTokenResponse {
    pub(crate) token: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct PaperlessUiSettingsResponse {
    pub(crate) user: PaperlessUserIdentity,
}

#[derive(Debug, Deserialize)]
pub(crate) struct PaperlessUserIdentity {
    pub(crate) id: i64,
    pub(crate) username: String,
}

pub(crate) struct PaperlessBridgeIdentity {
    pub(crate) subject: String,
    pub(crate) username: String,
}

pub(crate) async fn paperless_login(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(request): Json<LoginRequest>,
) -> ApiResult<Response> {
    let source_ip = request_source_ip(&state, &headers, Some(peer));
    let user_agent = request_user_agent(&headers);
    let settings = get_runtime_settings(&state.pool).await?;
    if !settings.paperless.login_bridge_enabled {
        return Err(ApiError::forbidden("Paperless login bridge is disabled"));
    }
    let paperless_username = request.username.trim();
    if paperless_username.is_empty() || request.password.is_empty() {
        return Err(ApiError::unauthorized("invalid credentials"));
    }

    let bridge_identity = match verify_paperless_credentials(
        &settings,
        paperless_username,
        &request.password,
    )
    .await
    {
        Ok(identity) => identity,
        Err(_) => {
            record_login_failure(
                &state.pool,
                None,
                &paperless_bridge_username(paperless_username),
                source_ip.as_deref(),
                user_agent.as_deref(),
            )
            .await?;
            return Err(ApiError::unauthorized("invalid credentials"));
        }
    };

    let username = paperless_bridge_username(&bridge_identity.username);
    let user = match find_paperless_bridge_user(&state.pool, &bridge_identity.subject).await? {
        Some(user) => user,
        None => {
            let disabled_password_hash = hash_password(&random_token())?;
            find_or_create_paperless_bridge_user(
                &state.pool,
                &username,
                &bridge_identity.subject,
                &disabled_password_hash,
            )
            .await?
        }
    };
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
            event_type: "auth.paperless_login_success".to_owned(),
            actor_type: "user".to_owned(),
            actor_id: Some(user.id.to_string()),
            run_id: None,
            job_id: None,
            paperless_document_id: None,
            before: None,
            after: None,
            metadata: Some(json!({
                "username": user.username,
                "paperless_username": bridge_identity.username
            })),
            outcome: "success".to_owned(),
            error_message: None,
            source_ip: source_ip.clone(),
            user_agent: user_agent.clone(),
        },
    )
    .await?;

    let (session_token, csrf_token) = issue_session(&state, user.id).await?;
    let permissions = PermissionFlags::from_roles(&user.roles);
    let body = Json(MeResponse {
        username: user.username,
        roles: user.roles,
        permissions,
        csrf_token: Some(csrf_token.clone()),
    });
    let mut response = body.into_response();
    set_session_cookies(
        response.headers_mut(),
        &state.config,
        &session_token,
        &csrf_token,
    )?;
    Ok(response)
}

pub(crate) async fn verify_paperless_credentials(
    settings: &RuntimeSettings,
    username: &str,
    password: &str,
) -> Result<PaperlessBridgeIdentity> {
    // Same up-front SSRF validation as the other outbound tester paths —
    // this endpoint forwards user credentials to the configured URL.
    // Authenticate against the *active* archive profile, the same instance
    // every other Paperless call uses; the bridge subject below is derived
    // from it too. #396
    let (active_base_url, _) = settings.paperless.active_connection();
    let base_url = validate_outbound_url(active_base_url.trim())
        .await
        .map_err(|error| anyhow!("Paperless base URL rejected: {}", error.message))?;
    let api_root = base_url.join("api/").context("build Paperless API root")?;
    let token_url = api_root
        .join("token/")
        .context("build Paperless token URL")?;
    let client = HttpClient::builder()
        .timeout(std::time::Duration::from_secs(
            settings.paperless.timeout_seconds.clamp(1, 120),
        ))
        // Refuse redirects so a 3xx response can't steer this credentialed
        // request to an internal address after the SSRF guard validated only
        // the originally supplied URL.
        .redirect(reqwest::redirect::Policy::none())
        // No connect-time IP-pinning: the DNS-rebinding TOCTOU is an accepted
        // residual risk for this operator-configured Paperless host (the
        // pinning resolver was reverted, see #183).
        .build()
        .context("build Paperless login HTTP client")?;
    let response = client
        .post(token_url)
        .json(&json!({ "username": username, "password": password }))
        .send()
        .await
        .context("connect to Paperless token endpoint")?;
    let status = response.status();
    if !status.is_success() {
        return Err(anyhow!("Paperless returned {status}"));
    }
    let token: PaperlessTokenResponse = response
        .json()
        .await
        .context("decode Paperless token response")?;
    let token = token.token.trim();
    if token.is_empty() {
        return Err(anyhow!("Paperless returned an empty token"));
    }
    let identity_url = api_root
        .join("ui_settings/")
        .context("build Paperless identity URL")?;
    let identity_response = client
        .get(identity_url)
        .header(reqwest::header::AUTHORIZATION, format!("Token {token}"))
        .send()
        .await
        .context("read authenticated Paperless identity")?;
    let identity_status = identity_response.status();
    if !identity_status.is_success() {
        return Err(anyhow!(
            "Paperless identity endpoint returned {identity_status}"
        ));
    }
    let identity: PaperlessUiSettingsResponse = identity_response
        .json()
        .await
        .context("decode authenticated Paperless identity")?;
    let identity_username = identity.user.username.trim();
    if identity.user.id <= 0 || identity_username.is_empty() {
        return Err(anyhow!("Paperless returned an invalid user identity"));
    }
    Ok(PaperlessBridgeIdentity {
        subject: paperless_user_subject(&api_root, identity.user.id),
        username: identity_username.to_owned(),
    })
}

pub(crate) fn paperless_user_subject(api_root: &Url, user_id: i64) -> String {
    let instance_hash = hex::encode(Sha256::digest(api_root.as_str().as_bytes()));
    format!("instance-sha256:{instance_hash}:user-id:{user_id}")
}

pub(crate) fn paperless_bridge_username(username: &str) -> String {
    let normalized = oidc_username(username);
    format!("paperless-{normalized}")
}

pub(crate) async fn logout(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    auth: Authenticated,
) -> ApiResult<impl IntoResponse> {
    let source_ip = request_source_ip(&state, &headers, Some(peer));
    let user_agent = request_user_agent(&headers);
    if let (Some(session_id), Some(user_id)) = (auth.0.session_id, auth.0.user_id) {
        archivist_db::revoke_session(
            &state.pool,
            session_id,
            user_id,
            source_ip.as_deref(),
            user_agent.as_deref(),
        )
        .await?;
    }
    let mut response = Json(json!({ "ok": true })).into_response();
    response.headers_mut().append(
        header::SET_COOKIE,
        header_value(expire_cookie(
            SESSION_COOKIE,
            true,
            state.config.cookie_secure,
        ))?,
    );
    response.headers_mut().append(
        header::SET_COOKIE,
        header_value(expire_cookie(
            CSRF_COOKIE,
            false,
            state.config.cookie_secure,
        ))?,
    );
    Ok(response)
}

pub(crate) async fn me(auth: Authenticated) -> ApiResult<Json<MeResponse>> {
    let permissions = PermissionFlags::from_roles(&auth.0.roles);
    Ok(Json(MeResponse {
        username: auth.0.username.unwrap_or_else(|| "api-token".to_owned()),
        roles: auth.0.roles,
        permissions,
        csrf_token: None,
    }))
}

#[derive(Debug, Deserialize)]
pub(crate) struct ChangePasswordRequest {
    pub(crate) current_password: String,
    pub(crate) new_password: String,
}

pub(crate) async fn change_password(
    State(state): State<AppState>,
    auth: Authenticated,
    Json(request): Json<ChangePasswordRequest>,
) -> ApiResult<Response> {
    let user_id = auth
        .0
        .user_id
        .ok_or_else(|| ApiError::forbidden("password changes require a user session"))?;
    let username = auth
        .0
        .username
        .as_deref()
        .ok_or_else(|| ApiError::forbidden("password changes require a user session"))?;
    let user = find_user_for_login(&state.pool, username)
        .await?
        .ok_or_else(|| ApiError::unauthorized("invalid user"))?;
    if !verify_password(&user, &request.current_password)? {
        return Err(ApiError::unauthorized("invalid current password"));
    }
    validate_password_strength(&request.new_password).map_err(ApiError::bad_request)?;
    let password_hash = hash_password(&request.new_password)?;
    update_user_password_hash(
        &state.pool,
        user_id,
        &password_hash,
        user_id,
        "auth.password_changed",
    )
    .await?;
    let mut response = Json(json!({ "ok": true })).into_response();
    response.headers_mut().append(
        header::SET_COOKIE,
        header_value(expire_cookie(
            SESSION_COOKIE,
            true,
            state.config.cookie_secure,
        ))?,
    );
    response.headers_mut().append(
        header::SET_COOKIE,
        header_value(expire_cookie(
            CSRF_COOKIE,
            false,
            state.config.cookie_secure,
        ))?,
    );
    Ok(response)
}

pub(crate) async fn sessions(
    State(state): State<AppState>,
    auth: Authenticated,
) -> ApiResult<Json<Value>> {
    let user_id = session_listing_user_filter(&auth.0)?;
    Ok(Json(
        json!({ "items": list_sessions(&state.pool, user_id).await? }),
    ))
}

pub(crate) fn session_listing_user_filter(auth: &AuthContext) -> Result<Option<Uuid>, ApiError> {
    let user_id = require_user_session(auth, "session listing requires a user session")?;
    Ok(
        if roles_have_permission(&auth.roles, Permission::ManageUsers) {
            None
        } else {
            Some(user_id)
        },
    )
}

pub(crate) async fn revoke_session_endpoint(
    State(state): State<AppState>,
    auth: Authenticated,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Value>> {
    let actor_id = auth.session_user_id()?;
    revoke_session_by_admin(&state.pool, id, actor_id).await?;
    Ok(Json(json!({ "ok": true })))
}
