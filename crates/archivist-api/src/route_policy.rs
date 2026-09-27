//! Declarative per-route authorization. #442
//!
//! Every Axum route is listed exactly once in [`ROUTE_POLICIES`] with the
//! permission it needs and the kinds of principal (browser session, API
//! token) that may call it. `auth_middleware` enforces the table for every
//! authenticated route, so handlers no longer call `require(...)` /
//! `require_user_session(...)` themselves; a handler that needs the acting
//! user of a session-only route uses [`Authenticated::session_user_id`].
//!
//! Adding a route is one line here next to the `.route(...)` in `router()`.
//! A route that is missing from the table is rejected at runtime (fail
//! closed, 403) and by `scripts/verify/router_openapi_contract.mjs`, which
//! also checks the table against the OpenAPI `security` and
//! `x-archivist-permission` of every operation.

use archivist_core::{Permission, Role, roles_have_permission};
use axum::extract::MatchedPath;
use axum::http::Method;
use uuid::Uuid;

use super::{ApiError, AuthContext, Authenticated};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Verb {
    Get,
    Post,
    Put,
    Patch,
    Delete,
}

impl Verb {
    fn matches(self, method: &Method) -> bool {
        match self {
            Self::Get => *method == Method::GET || *method == Method::HEAD,
            Self::Post => *method == Method::POST,
            Self::Put => *method == Method::PUT,
            Self::Patch => *method == Method::PATCH,
            Self::Delete => *method == Method::DELETE,
        }
    }
}

/// What a principal must be allowed to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Access {
    /// No application principal; the handler checks its own credential
    /// (login form, OIDC state, webhook secret, metrics token) or none.
    Public,
    /// Any principal admitted by the route's [`AuthKinds`].
    Authenticated,
    /// Role permission (sessions) or the matching token scope (tokens).
    Require(Permission),
}

/// Which principal kinds may call a route.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AuthKinds {
    /// Outside `auth_middleware`; only valid together with [`Access::Public`].
    Unauthenticated,
    /// Browser session (cookie + CSRF) or `Authorization: Bearer` API token.
    SessionOrToken,
    /// Interactive browser session only; tokens get 403 with this message
    /// regardless of their scopes (the action is attributed to a person).
    SessionOnly(&'static str),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RoutePolicy {
    pub verb: Verb,
    pub path: &'static str,
    pub access: Access,
    pub auth: AuthKinds,
}

const fn route(verb: Verb, path: &'static str, access: Access, auth: AuthKinds) -> RoutePolicy {
    RoutePolicy {
        verb,
        path,
        access,
        auth,
    }
}

use Access::{Authenticated as AnyPrincipal, Public, Require};
use AuthKinds::{SessionOnly, SessionOrToken, Unauthenticated};
use Permission::{
    ManageUsers, ReadAudit, ReadDashboard, ReadInventory, ReadReviews, ReadRuns, ReadSettings,
    UseChat, WriteBatches, WriteReviews, WriteRuns, WriteSettings,
};
use Verb::{Delete, Get, Patch, Post, Put};

const REVIEW_SESSION: AuthKinds = SessionOnly("review decisions require a user session");
const CHAT_SESSION: AuthKinds = SessionOnly("document chat requires a user session");
const USER_SESSION: AuthKinds = SessionOnly("user management requires a user session");
const PROMPT_SESSION: AuthKinds = SessionOnly("prompt management requires a user session");
const RECOVERY_SESSION: AuthKinds = SessionOnly("recovery requires a user session");

/// The route table. Paths are the full request paths (as `MatchedPath`
/// reports them and OpenAPI documents them). Session-only messages are the
/// 403 texts the handlers returned before #442.
#[rustfmt::skip]
pub(crate) const ROUTE_POLICIES: &[RoutePolicy] = &[
    // Probes and metrics (own bearer token for /metrics).
    route(Get, "/healthz", Public, Unauthenticated),
    route(Get, "/readyz", Public, Unauthenticated),
    route(Get, "/metrics", Public, Unauthenticated),
    // Rate-limited public auth endpoints.
    route(Post, "/api/auth/login", Public, Unauthenticated),
    route(Post, "/api/auth/paperless-login", Public, Unauthenticated),
    route(Get, "/api/auth/oidc/config", Public, Unauthenticated),
    route(Get, "/api/auth/oidc/login", Public, Unauthenticated),
    route(Get, "/api/auth/oidc/callback", Public, Unauthenticated),
    // Webhooks authenticate with their shared secret.
    route(Post, "/api/webhooks/paperless/document-consumed", Public, Unauthenticated),
    // Own account.
    route(Get, "/api/auth/me", AnyPrincipal, SessionOrToken),
    route(Post, "/api/auth/logout", AnyPrincipal, SessionOrToken),
    route(Post, "/api/auth/change-password", AnyPrincipal, SessionOnly("password changes require a user session")),
    route(Get, "/api/auth/sessions", AnyPrincipal, SessionOnly("session listing requires a user session")),
    route(Post, "/api/auth/sessions/{id}/revoke", Require(ManageUsers), SessionOnly("session revocation requires a user session")),
    // Settings, providers, prompts.
    route(Get, "/api/settings", Require(ReadSettings), SessionOrToken),
    route(Put, "/api/settings", Require(WriteSettings), SessionOnly("settings updates require a user session")),
    route(Post, "/api/settings/test-paperless", Require(ReadSettings), SessionOrToken),
    route(Post, "/api/notifications/test", Require(WriteSettings), SessionOnly("notification tests require a user session")),
    route(Post, "/api/model-providers/test", Require(WriteSettings), SessionOnly("provider tests require a user session")),
    route(Post, "/api/model-providers/{name}/models", Require(ReadSettings), SessionOnly("model discovery requires a user session")),
    route(Get, "/api/ai/runtime-hints", Require(ReadSettings), SessionOrToken),
    route(Get, "/api/secret-references", Require(ReadSettings), SessionOrToken),
    route(Get, "/api/prompts", Require(ReadSettings), SessionOrToken),
    route(Post, "/api/prompts", Require(WriteSettings), PROMPT_SESSION),
    route(Get, "/api/prompts/usage", Require(ReadSettings), SessionOrToken),
    route(Get, "/api/prompts/experiments", Require(ReadSettings), SessionOrToken),
    route(Post, "/api/prompts/test", Require(WriteSettings), SessionOnly("prompt tests require a user session")),
    route(Post, "/api/prompts/{id}/activate", Require(WriteSettings), PROMPT_SESSION),
    route(Put, "/api/workflow/mode", Require(WriteSettings), SessionOnly("workflow mode updates require a user session")),
    route(Patch, "/api/workflow/controls", Require(WriteSettings), SessionOnly("workflow control updates require a user session")),
    // Paperless sync and consistency.
    route(Post, "/api/paperless/sync-metadata", Require(WriteBatches), SessionOrToken),
    route(Get, "/api/paperless/consistency", Require(ReadInventory), SessionOrToken),
    // #420: synced correspondent / document type names for the review edit select.
    route(Get, "/api/paperless/correspondents", Require(ReadInventory), SessionOrToken),
    route(Get, "/api/paperless/document-types", Require(ReadInventory), SessionOrToken),
    route(Post, "/api/paperless/completion-tags/reconcile", Require(WriteBatches), SessionOrToken),
    // Dashboard, statistics, inventory.
    route(Get, "/api/dashboard", Require(ReadDashboard), SessionOrToken),
    route(Get, "/api/dashboard/live", Require(ReadDashboard), SessionOrToken),
    route(Get, "/api/statistics", Require(ReadDashboard), SessionOrToken),
    route(Get, "/api/inventory", Require(ReadInventory), SessionOrToken),
    route(Get, "/api/inventory/duplicates", Require(ReadInventory), SessionOrToken),
    route(Get, "/api/inventory/{document_id}/metadata-trace", Require(ReadInventory), SessionOrToken),
    // Document chat is always per person.
    route(Get, "/api/chat/sessions", Require(UseChat), CHAT_SESSION),
    route(Post, "/api/chat/sessions", Require(UseChat), CHAT_SESSION),
    route(Get, "/api/chat/sessions/{id}", Require(UseChat), CHAT_SESSION),
    route(Post, "/api/chat/sessions/{id}/messages", Require(UseChat), CHAT_SESSION),
    // Runs and batches.
    route(Post, "/api/documents/{paperless_document_id}/trigger", Require(WriteRuns), SessionOrToken),
    route(Post, "/api/batches/ocr", Require(WriteBatches), SessionOrToken),
    route(Post, "/api/batches/full", Require(WriteBatches), SessionOrToken),
    route(Post, "/api/batches/rerun", Require(WriteBatches), SessionOrToken),
    route(Post, "/api/batches/rerun-failed", Require(WriteBatches), SessionOrToken),
    // Reviews: decisions are attributed to a person (#393).
    route(Get, "/api/reviews", Require(ReadReviews), SessionOrToken),
    route(Post, "/api/reviews/batch", Require(WriteReviews), REVIEW_SESSION),
    route(Post, "/api/reviews/auto-fix-preview", Require(WriteReviews), SessionOrToken),
    route(Post, "/api/reviews/auto-fix", Require(WriteReviews), REVIEW_SESSION),
    route(Post, "/api/reviews/{id}/approve", Require(WriteReviews), REVIEW_SESSION),
    route(Post, "/api/reviews/{id}/reject", Require(WriteReviews), REVIEW_SESSION),
    route(Post, "/api/reviews/{id}/edit", Require(WriteReviews), REVIEW_SESSION),
    route(Post, "/api/reviews/{id}/auto-fix", Require(WriteReviews), REVIEW_SESSION),
    // #445: retry with provider/model/prompt, document preview proxy.
    route(Get, "/api/reviews/retry-options", Require(WriteReviews), SessionOrToken),
    route(Post, "/api/reviews/{id}/retry", Require(WriteReviews), REVIEW_SESSION),
    route(Get, "/api/reviews/{id}/thumbnail", Require(ReadReviews), SessionOrToken),
    route(Get, "/api/reviews/{id}/preview", Require(ReadReviews), SessionOrToken),
    // Operations.
    route(Get, "/api/operations/recovery", Require(ReadRuns), SessionOrToken),
    route(Post, "/api/operations/recovery/stale-leases", Require(WriteRuns), RECOVERY_SESSION),
    route(Post, "/api/operations/recovery/stuck-runs", Require(WriteRuns), RECOVERY_SESSION),
    route(Post, "/api/operations/unblock-jobs", Require(WriteRuns), SessionOnly("unblock requires a user session")),
    route(Get, "/api/operations/provider-cooldowns", Require(ReadDashboard), SessionOrToken),
    route(Post, "/api/operations/provider-cooldowns/clear", Require(WriteRuns), SessionOnly("clearing cooldowns requires a user session")),
    route(Post, "/api/operations/release-scheduled-retries", Require(WriteRuns), SessionOnly("releasing scheduled retries requires a user session")),
    // Audit.
    route(Get, "/api/audit", Require(ReadAudit), SessionOrToken),
    route(Get, "/api/audit/export.csv", Require(ReadAudit), SessionOrToken),
    route(Get, "/api/audit/integrity", Require(ReadAudit), SessionOrToken),
    route(Post, "/api/audit/retention/apply", Require(WriteSettings), SessionOnly("audit retention requires a user session")),
    // Users and API tokens.
    route(Get, "/api/users", Require(ManageUsers), USER_SESSION),
    route(Post, "/api/users", Require(ManageUsers), USER_SESSION),
    route(Post, "/api/users/{id}/enable", Require(ManageUsers), USER_SESSION),
    route(Post, "/api/users/{id}/disable", Require(ManageUsers), USER_SESSION),
    route(Post, "/api/users/{id}/roles", Require(ManageUsers), USER_SESSION),
    route(Post, "/api/users/{id}/reset-password", Require(ManageUsers), SessionOnly("password reset requires a user session")),
    route(Get, "/api/api-tokens", Require(ManageUsers), SessionOnly("API token management requires a user session")),
    route(Post, "/api/api-tokens", Require(ManageUsers), SessionOnly("API token creation requires a user session")),
    route(Post, "/api/api-tokens/{id}/rotate", Require(ManageUsers), SessionOnly("API token rotation requires a user session")),
    route(Delete, "/api/api-tokens/{id}", Require(ManageUsers), SessionOnly("API token revocation requires a user session")),
];

pub(crate) fn policy_for(method: &Method, path: &str) -> Option<&'static RoutePolicy> {
    ROUTE_POLICIES
        .iter()
        .find(|policy| policy.path == path && policy.verb.matches(method))
}

/// Enforce the declared policy for an authenticated request. Runs inside
/// `auth_middleware` after authentication and CSRF, before any extractor.
/// Order matches the former handler code: permission first ("insufficient
/// permissions"), then the session-only check with the route's message.
pub(crate) fn authorize_route(
    auth: &AuthContext,
    method: &Method,
    matched_path: Option<&MatchedPath>,
) -> Result<(), ApiError> {
    let Some(policy) = matched_path.and_then(|path| policy_for(method, path.as_str())) else {
        // Fail closed: a route without a declaration is never reachable.
        tracing::error!(
            %method,
            path = matched_path.map(MatchedPath::as_str),
            "route has no declared permission policy"
        );
        return Err(ApiError::forbidden("insufficient permissions"));
    };
    authorize(auth, policy)
}

pub(crate) fn authorize(auth: &AuthContext, policy: &RoutePolicy) -> Result<(), ApiError> {
    match policy.access {
        // A public route behind auth_middleware is a table error.
        Access::Public => return Err(ApiError::forbidden("insufficient permissions")),
        Access::Authenticated => {}
        Access::Require(permission) => require(auth, permission)?,
    }
    match policy.auth {
        AuthKinds::Unauthenticated => Err(ApiError::forbidden("insufficient permissions")),
        AuthKinds::SessionOrToken => Ok(()),
        AuthKinds::SessionOnly(message) => require_user_session(auth, message).map(|_| ()),
    }
}

fn require(auth: &AuthContext, permission: Permission) -> Result<(), ApiError> {
    let token_scope = token_scope_for_permission(permission);
    if roles_have_permission(&auth.roles, permission)
        || token_scope.is_some_and(|scope| auth.scopes.iter().any(|granted| granted == scope))
    {
        Ok(())
    } else {
        Err(ApiError::forbidden("insufficient permissions"))
    }
}

/// The interactive user behind a request; tokens (which carry their
/// creator's `user_id`) are rejected so nothing is attributed to the creator.
pub(crate) fn require_user_session(
    auth: &AuthContext,
    message: &'static str,
) -> Result<Uuid, ApiError> {
    if !auth.cookie_auth {
        return Err(ApiError::forbidden(message));
    }
    auth.user_id.ok_or_else(|| ApiError::forbidden(message))
}

impl Authenticated {
    /// Acting user of a session-only route. The route table has already
    /// rejected tokens; this re-checks it so a table/handler mismatch can
    /// never attribute an action to a token's creator.
    pub(crate) fn session_user_id(&self) -> Result<Uuid, ApiError> {
        require_user_session(&self.0, "this endpoint requires a user session")
    }
}

pub(crate) const ALL_PERMISSIONS: [Permission; 12] = [
    Permission::ReadDashboard,
    Permission::ReadRuns,
    Permission::WriteRuns,
    Permission::ReadInventory,
    Permission::WriteBatches,
    Permission::UseChat,
    Permission::ReadReviews,
    Permission::WriteReviews,
    Permission::ReadSettings,
    Permission::WriteSettings,
    Permission::ManageUsers,
    Permission::ReadAudit,
];

/// Token scope granting a permission, or `None` when every route needing the
/// permission is session-only. `chat:write`, `settings:write` and
/// `users:manage` were accepted before #442 but could never authorize a
/// request, so they are no longer issued and are ignored on old tokens.
pub(crate) fn token_scope_for_permission(permission: Permission) -> Option<&'static str> {
    match permission {
        Permission::ReadDashboard | Permission::ReadRuns => Some("runs:read"),
        Permission::WriteRuns => Some("runs:write"),
        Permission::ReadInventory => Some("inventory:read"),
        Permission::WriteBatches => Some("batches:write"),
        Permission::ReadReviews => Some("reviews:read"),
        Permission::WriteReviews => Some("reviews:write"),
        Permission::ReadSettings => Some("settings:read"),
        Permission::ReadAudit => Some("audit:read"),
        Permission::UseChat | Permission::WriteSettings | Permission::ManageUsers => None,
    }
}

/// Scopes accepted when creating an API token. #442
pub(crate) const TOKEN_SCOPES: &[&str] = &[
    "runs:read",
    "runs:write",
    "inventory:read",
    "batches:write",
    "reviews:read",
    "reviews:write",
    "settings:read",
    "audit:read",
];

/// A token can never do more than its creator currently may: keep only the
/// scopes backed by a permission the creator's *current* roles still grant,
/// so demoting a user (OIDC role replace #289, `set_user_roles`) immediately
/// narrows every token they created. #392
pub(crate) fn effective_token_scopes(scopes: &[String], creator_roles: &[Role]) -> Vec<String> {
    scopes
        .iter()
        .filter(|scope| {
            ALL_PERMISSIONS.iter().any(|permission| {
                token_scope_for_permission(*permission) == Some(scope.as_str())
                    && roles_have_permission(creator_roles, *permission)
            })
        })
        .cloned()
        .collect()
}

pub(crate) fn validate_api_token_scopes(scopes: &[String]) -> Result<(), ApiError> {
    if scopes.is_empty() {
        return Err(ApiError::bad_request(
            "API token requires at least one scope",
        ));
    }
    for scope in scopes {
        if !TOKEN_SCOPES.contains(&scope.as_str()) {
            return Err(ApiError::bad_request(format!(
                "unsupported API token scope: {scope}"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn every_route_is_declared_once() {
        let mut seen = HashSet::new();
        for policy in ROUTE_POLICIES {
            assert!(
                seen.insert((policy.verb, policy.path)),
                "duplicate route policy: {:?} {}",
                policy.verb,
                policy.path
            );
            assert!(policy.path.starts_with('/'), "{}", policy.path);
        }
    }

    #[test]
    fn public_routes_are_exactly_the_unauthenticated_ones() {
        for policy in ROUTE_POLICIES {
            assert_eq!(
                policy.access == Access::Public,
                policy.auth == AuthKinds::Unauthenticated,
                "{:?} {}",
                policy.verb,
                policy.path
            );
        }
    }

    #[test]
    fn token_routes_have_an_issuable_scope() {
        // A SessionOrToken route whose permission has no token scope would be
        // silently session-only; declare it SessionOnly instead.
        for policy in ROUTE_POLICIES {
            if let (Access::Require(permission), AuthKinds::SessionOrToken) =
                (policy.access, policy.auth)
            {
                assert!(
                    token_scope_for_permission(permission).is_some(),
                    "{:?} {} needs a token scope for {permission:?}",
                    policy.verb,
                    policy.path
                );
            }
        }
    }

    #[test]
    fn every_issuable_scope_authorizes_at_least_one_route() {
        // #442: a scope that no route accepts must not be issued.
        for scope in TOKEN_SCOPES {
            let usable = ROUTE_POLICIES.iter().any(|policy| match policy.access {
                Access::Require(permission) => {
                    policy.auth == AuthKinds::SessionOrToken
                        && token_scope_for_permission(permission) == Some(*scope)
                }
                _ => false,
            });
            assert!(usable, "token scope {scope} authorizes no route");
        }
        for permission in ALL_PERMISSIONS {
            if let Some(scope) = token_scope_for_permission(permission) {
                assert!(TOKEN_SCOPES.contains(&scope), "{scope}");
            }
        }
    }
}
