//! API error type, JSON error bodies and error conversions.

use crate::*;

pub(crate) type ApiResult<T> = Result<T, ApiError>;

#[derive(Debug)]
pub(crate) struct ApiError {
    pub(crate) status: StatusCode,
    pub(crate) message: String,
}

impl ApiError {
    pub(crate) fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
        }
    }

    pub(crate) fn unauthorized(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            message: message.into(),
        }
    }

    pub(crate) fn forbidden(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::FORBIDDEN,
            message: message.into(),
        }
    }

    pub(crate) fn not_found(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message: message.into(),
        }
    }

    pub(crate) fn conflict(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::CONFLICT,
            message: message.into(),
        }
    }

    pub(crate) fn too_many_requests(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::TOO_MANY_REQUESTS,
            message: message.into(),
        }
    }

    pub(crate) fn internal(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: message.into(),
        }
    }

    pub(crate) fn service_unavailable(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::SERVICE_UNAVAILABLE,
            message: message.into(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = Json(json!({ "error": self.message }));
        (self.status, body).into_response()
    }
}

/// JSON 404 for unknown `/api/*` paths. #399
pub(crate) async fn api_not_found() -> ApiError {
    ApiError::not_found("not found")
}

/// Rewrite axum's plain-text extractor rejections (malformed JSON, missing
/// content type, body too large, bad path/query parameters) into the
/// `{"error": ...}` shape every other API error uses. Only client errors
/// with a `text/plain` body are touched, so handler responses pass through
/// unchanged. #399
pub(crate) async fn json_error_body(response: Response) -> Response {
    const MAX_REJECTION_BODY: usize = 16 * 1024;
    let status = response.status();
    let is_plain_text = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("text/plain"));
    if !status.is_client_error() || !is_plain_text {
        return response;
    }
    let (parts, body) = response.into_parts();
    let message = match axum::body::to_bytes(body, MAX_REJECTION_BODY).await {
        Ok(bytes) if !bytes.is_empty() => String::from_utf8_lossy(&bytes).trim().to_owned(),
        _ => parts
            .status
            .canonical_reason()
            .unwrap_or("request rejected")
            .to_owned(),
    };
    let mut rewritten = ApiError {
        status: parts.status,
        message,
    }
    .into_response();
    for (name, value) in &parts.headers {
        if name != header::CONTENT_TYPE && name != header::CONTENT_LENGTH {
            rewritten.headers_mut().append(name.clone(), value.clone());
        }
    }
    rewritten
}

/// A feature the request depends on is not configured (Paperless token, AI
/// provider). Operator state rather than a server fault: 409 without an
/// ERROR log. #441
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub(crate) struct NotConfiguredError(pub(crate) String);

pub(crate) fn not_configured(message: impl Into<String>) -> anyhow::Error {
    NotConfiguredError(message.into()).into()
}

impl From<anyhow::Error> for ApiError {
    fn from(error: anyhow::Error) -> Self {
        // Expected domain conditions are client-visible states, not server
        // faults: 4xx and no ERROR log. #441
        if let Some(not_found) = error.downcast_ref::<NotFoundError>() {
            return Self::not_found(not_found.to_string());
        }
        if let Some(not_configured) = error.downcast_ref::<NotConfiguredError>() {
            warn!(reason = %not_configured, "request rejected: dependency not configured");
            return Self::conflict(not_configured.to_string());
        }
        if error.downcast_ref::<LastEnabledAdminError>().is_some() {
            warn!("user mutation rejected to preserve the enabled administrator invariant");
            return Self {
                status: StatusCode::CONFLICT,
                message: error.to_string(),
            };
        }
        if error.downcast_ref::<UserIdentityConflictError>().is_some() {
            warn!("user mutation rejected due to a normalized identity conflict");
            return Self {
                status: StatusCode::CONFLICT,
                message: "username or email is already assigned".to_owned(),
            };
        }
        if error.downcast_ref::<InvalidUserIdentityError>().is_some() {
            return Self::bad_request(error.to_string());
        }
        if error
            .downcast_ref::<AmbiguousUserIdentityLinkError>()
            .is_some()
        {
            warn!("OIDC linking rejected because claims identify multiple local accounts");
            return Self {
                status: StatusCode::CONFLICT,
                message: "OIDC identity matches multiple local accounts".to_owned(),
            };
        }
        // Expected review races (double click, concurrent reviewers) are
        // client-visible states, not server faults: no ERROR log. #391
        if let Some(decision) = error.downcast_ref::<ReviewDecisionError>() {
            return match decision {
                ReviewDecisionError::NotFound => Self::not_found(decision.to_string()),
                ReviewDecisionError::NotPending => Self::conflict(decision.to_string()),
            };
        }
        if let Some(conflict) = error.downcast_ref::<ReviewApplyConflict>() {
            warn!(
                fields = ?conflict.fields(),
                "review apply rejected due to newer Paperless changes"
            );
            return Self {
                status: StatusCode::CONFLICT,
                message: conflict.to_string(),
            };
        }
        // Log the full cause chain server-side, but never return internal
        // error text (SQL fragments, column/constraint names, pool/reqwest
        // URLs) to the client — some 5xx paths are unauthenticated.
        tracing::error!(error = format!("{error:#}"), "internal server error");
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: "internal server error".to_owned(),
        }
    }
}

impl From<sqlx::Error> for ApiError {
    fn from(error: sqlx::Error) -> Self {
        tracing::error!(error = format!("{error:#}"), "database error");
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: "internal server error".to_owned(),
        }
    }
}

impl From<serde_json::Error> for ApiError {
    fn from(error: serde_json::Error) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: error.to_string(),
        }
    }
}
