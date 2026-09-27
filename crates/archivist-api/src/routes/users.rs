//! User administration and API token endpoints.

use crate::*;

pub(crate) async fn users(
    State(state): State<AppState>,
    _auth: Authenticated,
) -> ApiResult<Json<Value>> {
    Ok(Json(json!({ "items": list_users(&state.pool).await? })))
}

#[derive(Debug, Deserialize)]
pub(crate) struct CreateUserRequest {
    pub(crate) username: String,
    pub(crate) email: Option<String>,
    pub(crate) password: String,
    pub(crate) roles: Vec<Role>,
}

pub(crate) async fn create_user(
    State(state): State<AppState>,
    auth: Authenticated,
    Json(request): Json<CreateUserRequest>,
) -> ApiResult<Json<Value>> {
    let actor_id = auth.session_user_id()?;
    validate_password_strength(&request.password).map_err(ApiError::bad_request)?;
    let password_hash = hash_password(&request.password)?;
    let id = create_user_with_roles(
        &state.pool,
        &request.username,
        request.email.as_deref(),
        &password_hash,
        &request.roles,
        Some(actor_id),
    )
    .await?;
    Ok(Json(json!({ "id": id })))
}

pub(crate) async fn enable_user(
    State(state): State<AppState>,
    auth: Authenticated,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Value>> {
    update_user_enabled(&state, &auth, id, true).await
}

pub(crate) async fn disable_user(
    State(state): State<AppState>,
    auth: Authenticated,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Value>> {
    update_user_enabled(&state, &auth, id, false).await
}

pub(crate) async fn update_user_enabled(
    state: &AppState,
    auth: &Authenticated,
    id: Uuid,
    enabled: bool,
) -> ApiResult<Json<Value>> {
    let actor_id = auth.session_user_id()?;
    set_user_enabled(&state.pool, id, enabled, actor_id).await?;
    Ok(Json(json!({ "ok": true })))
}

#[derive(Debug, Deserialize)]
pub(crate) struct UpdateUserRolesRequest {
    pub(crate) roles: Vec<Role>,
}

pub(crate) async fn update_user_roles_endpoint(
    State(state): State<AppState>,
    auth: Authenticated,
    Path(id): Path<Uuid>,
    Json(request): Json<UpdateUserRolesRequest>,
) -> ApiResult<Json<Value>> {
    let actor_id = auth.session_user_id()?;
    set_user_roles(&state.pool, id, &request.roles, actor_id).await?;
    Ok(Json(json!({ "ok": true })))
}

#[derive(Debug, Deserialize)]
pub(crate) struct ResetPasswordRequest {
    pub(crate) password: String,
}

pub(crate) async fn reset_user_password(
    State(state): State<AppState>,
    auth: Authenticated,
    Path(id): Path<Uuid>,
    Json(request): Json<ResetPasswordRequest>,
) -> ApiResult<Json<Value>> {
    let actor_id = auth.session_user_id()?;
    validate_password_strength(&request.password).map_err(ApiError::bad_request)?;
    let password_hash = hash_password(&request.password)?;
    update_user_password_hash(
        &state.pool,
        id,
        &password_hash,
        actor_id,
        "user.password_reset",
    )
    .await?;
    Ok(Json(json!({ "ok": true })))
}

pub(crate) async fn api_tokens(
    State(state): State<AppState>,
    _auth: Authenticated,
) -> ApiResult<Json<Value>> {
    Ok(Json(
        json!({ "items": archivist_db::list_api_tokens(&state.pool).await? }),
    ))
}

#[derive(Debug, Deserialize)]
pub(crate) struct CreateApiTokenRequest {
    pub(crate) name: String,
    pub(crate) scopes: Vec<String>,
    pub(crate) expires_in_days: Option<i64>,
}

pub(crate) async fn create_api_token(
    State(state): State<AppState>,
    auth: Authenticated,
    Json(request): Json<CreateApiTokenRequest>,
) -> ApiResult<Json<Value>> {
    let actor_id = auth.session_user_id()?;
    validate_api_token_name(&request.name)?;
    validate_api_token_scopes(&request.scopes)?;
    let settings = get_runtime_settings(&state.pool).await?;
    let expires_at = api_token_expiry(&settings, request.expires_in_days)?;
    let token = format!("pa_{}", random_token());
    let token_hash = hash_token(&token);
    let id = archivist_db::create_api_token(
        &state.pool,
        &request.name,
        &token_hash,
        &request.scopes,
        actor_id,
        expires_at,
    )
    .await?;
    Ok(Json(
        json!({ "id": id, "token": token, "expires_at": expires_at }),
    ))
}

#[derive(Debug, Deserialize)]
pub(crate) struct RotateApiTokenRequest {
    pub(crate) expires_in_days: Option<i64>,
}

pub(crate) async fn rotate_api_token_endpoint(
    State(state): State<AppState>,
    auth: Authenticated,
    Path(id): Path<Uuid>,
    Json(request): Json<RotateApiTokenRequest>,
) -> ApiResult<Json<Value>> {
    let actor_id = auth.session_user_id()?;
    let settings = get_runtime_settings(&state.pool).await?;
    let expires_at = api_token_expiry(&settings, request.expires_in_days)?;
    let token = format!("pa_{}", random_token());
    let token_hash = hash_token(&token);
    let new_id = rotate_api_token(&state.pool, id, &token_hash, actor_id, expires_at).await?;
    Ok(Json(
        json!({ "id": new_id, "token": token, "expires_at": expires_at }),
    ))
}

pub(crate) async fn revoke_api_token(
    State(state): State<AppState>,
    auth: Authenticated,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Value>> {
    let actor_id = auth.session_user_id()?;
    archivist_db::revoke_api_token(&state.pool, id, actor_id).await?;
    Ok(Json(json!({ "ok": true })))
}
