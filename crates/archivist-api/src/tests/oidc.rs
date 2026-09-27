//! OIDC claim, role mapping and username normalisation tests.

use crate::test_support::*;
use crate::*;

#[test]
fn oidc_email_is_only_used_when_verified() {
    let mut claims = OidcIdClaims {
        iss: "https://issuer.example.com".to_owned(),
        sub: "subject-1".to_owned(),
        aud: serde_json::Value::String("client".to_owned()),
        exp: 0,
        nonce: None,
        email: Some("admin@example.com".to_owned()),
        email_verified: Some(true),
        preferred_username: None,
        at_hash: None,
        additional: serde_json::Map::new(),
    };
    assert_eq!(oidc_verified_email(&claims), Some("admin@example.com"));

    claims.email_verified = Some(false);
    assert_eq!(oidc_verified_email(&claims), None);

    // Absent email_verified must be treated as unverified.
    claims.email_verified = None;
    assert_eq!(oidc_verified_email(&claims), None);
}

#[test]
fn normalizes_oidc_usernames_for_local_accounts() {
    assert_eq!(oidc_username(" Rressl@example.com "), "rressl@example.com");
    assert_eq!(oidc_username("René Ressl"), "ren-ressl");
    assert_eq!(oidc_username("!!"), "oidc-user");
}

fn oidc_test_claims() -> OidcIdClaims {
    OidcIdClaims {
        iss: "https://issuer.example.com".to_owned(),
        sub: "subject-1".to_owned(),
        aud: serde_json::Value::String("client".to_owned()),
        exp: 0,
        nonce: None,
        email: None,
        email_verified: None,
        preferred_username: None,
        at_hash: None,
        additional: serde_json::Map::new(),
    }
}

/// Claims carrying a roles claim `value` under claim name `claim`.
fn oidc_test_claims_with_roles(claim: &str, value: serde_json::Value) -> OidcIdClaims {
    let mut claims = oidc_test_claims();
    claims.additional.insert(claim.to_owned(), value);
    claims
}

#[test]
fn oidc_admin_allowlist_gets_admin_roles() {
    let mut config = test_config();
    config.oidc_admin_users = "oidc-admin, admin@example.com".to_owned();

    let roles = oidc_roles(
        &config,
        &oidc_test_claims(),
        "subject-1",
        "oidc-admin",
        None,
    )
    .expect("roles parse")
    .roles;
    assert!(roles.contains(&Role::Admin));
    assert!(roles.contains(&Role::Auditor));

    let email_roles = oidc_roles(
        &config,
        &oidc_test_claims(),
        "subject-2",
        "someone",
        Some("admin@example.com"),
    )
    .expect("roles parse")
    .roles;
    assert!(email_roles.contains(&Role::Admin));
}

#[test]
fn oidc_admin_allowlist_matches_immutable_subject() {
    let mut config = test_config();
    config.oidc_admin_users = "327680913418715137".to_owned();

    // Degraded claims: username fell back to the raw subject, no email.
    // The allowlisted subject must still grant admin (#299).
    let roles = oidc_roles(
        &config,
        &oidc_test_claims(),
        "327680913418715137",
        "327680913418715137",
        None,
    )
    .expect("roles parse")
    .roles;
    assert!(roles.contains(&Role::Admin));

    // Subjects are matched verbatim — a different subject stays default.
    let other = oidc_roles(
        &config,
        &oidc_test_claims(),
        "999999999999999999",
        "999999999999999999",
        None,
    )
    .expect("roles parse")
    .roles;
    assert!(!other.contains(&Role::Admin));
}

#[test]
fn oidc_reads_zitadel_project_roles_object_and_maps_admin() {
    // The real bug: ZITADEL asserts project roles as an OBJECT keyed by
    // role name. Previously these were dropped entirely; now archivist-admin
    // maps to Admin. #299.
    let config = test_config();
    let claims = oidc_test_claims_with_roles(
        "urn:zitadel:iam:org:project:roles",
        serde_json::json!({
            "archivist-admin": {"327680000000000000": "acme.zitadel.cloud"},
            "archivist-reviewer": {"327680000000000000": "acme.zitadel.cloud"}
        }),
    );
    let resolution = oidc_roles(
        &config,
        &claims,
        "327680913418715137",
        "327680913418715137",
        None,
    )
    .expect("roles parse");
    assert!(
        resolution.roles.contains(&Role::Admin),
        "archivist-admin maps to admin"
    );
    assert!(resolution.roles.contains(&Role::Reviewer));
    assert!(
        resolution.authoritative,
        "an asserted roles claim is authoritative"
    );
    assert!(resolution.idp_claim_present);
}

#[test]
fn oidc_idp_admin_role_survives_degraded_identity() {
    // The exact production scenario: ZITADEL sends archivist-admin but the
    // token has no preferred_username and no verified email. The role claim
    // must still grant admin (and be authoritative, so it is not preserved
    // away). This is what v1.12.4 missed — it never read the roles claim.
    let config = test_config();
    let claims = oidc_test_claims_with_roles(
        "urn:zitadel:iam:org:project:roles",
        serde_json::json!({"archivist-admin": {"o": "d"}}),
    );
    assert!(
        oidc_claims_degraded(&claims),
        "no username and no verified email is degraded"
    );
    let resolution = oidc_roles(
        &config,
        &claims,
        "327680913418715137",
        "327680913418715137",
        None,
    )
    .expect("roles parse");
    assert!(resolution.roles.contains(&Role::Admin));
    assert!(resolution.authoritative);
}

#[test]
fn oidc_maps_project_scoped_claim_and_array_shape() {
    let config = test_config();
    // Project-scoped claim name (…:<projectid>:roles) + array-of-strings.
    let claims = oidc_test_claims_with_roles(
        "urn:zitadel:iam:org:project:289000000000000000:roles",
        serde_json::json!(["archivist-operator"]),
    );
    let resolution = oidc_roles(&config, &claims, "s", "u", None).expect("roles parse");
    assert_eq!(resolution.roles, vec![Role::Operator]);
    assert!(resolution.idp_claim_present);
}

#[test]
fn oidc_ignores_unmapped_idp_roles_no_escalation() {
    let config = test_config();
    let claims = oidc_test_claims_with_roles(
        "urn:zitadel:iam:org:project:roles",
        serde_json::json!({"some-unrelated-role": {}}),
    );
    let resolution = oidc_roles(&config, &claims, "s", "u", None).expect("roles parse");
    // Claim present but nothing maps → authoritative empty, falls back to
    // the default role, and crucially does NOT grant admin.
    assert!(resolution.idp_claim_present);
    assert!(resolution.authoritative);
    assert!(!resolution.roles.contains(&Role::Admin));
}

#[test]
fn oidc_no_roles_claim_is_not_authoritative() {
    let config = test_config();
    let resolution = oidc_roles(&config, &oidc_test_claims(), "s", "u", None).expect("roles parse");
    assert!(!resolution.idp_claim_present);
    assert!(
        !resolution.authoritative,
        "absent roles claim + no allowlist → fallback, must not demote a returning user"
    );
    assert_eq!(resolution.roles, vec![Role::Viewer]);
}

#[test]
fn merge_userinfo_fills_identity_and_roles_from_userinfo() {
    // The exact production scenario: the ID token is minimal (only `sub`),
    // while ZITADEL returns the username/email/roles from userinfo. After
    // merge the token is no longer degraded and role-based admin works.
    let config = test_config();
    let mut claims = oidc_test_claims();
    assert!(
        oidc_claims_degraded(&claims),
        "bare token (no username, no verified email) starts degraded"
    );
    claims.merge_userinfo(
        serde_json::json!({
            "sub": "100000000000000001",
            "preferred_username": "rressl",
            "email": "rr@example.com",
            "email_verified": true,
            "urn:zitadel:iam:org:project:roles": {"archivist-admin": {"o": "d"}}
        })
        .as_object()
        .unwrap()
        .clone(),
    );
    assert!(
        !oidc_claims_degraded(&claims),
        "userinfo supplied a usable username"
    );
    assert_eq!(claims.preferred_username.as_deref(), Some("rressl"));
    assert_eq!(oidc_verified_email(&claims), Some("rr@example.com"));

    let resolution = oidc_roles(
        &config,
        &claims,
        "100000000000000001",
        "rressl",
        oidc_verified_email(&claims),
    )
    .expect("roles parse");
    assert!(
        resolution.roles.contains(&Role::Admin),
        "the roles claim merged from userinfo grants admin"
    );
    assert!(resolution.authoritative);
}

#[test]
fn merge_userinfo_does_not_override_signed_id_token_fields() {
    let mut claims = oidc_test_claims();
    claims.preferred_username = Some("from-id-token".to_owned());
    claims.merge_userinfo(
        serde_json::json!({"sub": "subject-1", "preferred_username": "from-userinfo"})
            .as_object()
            .unwrap()
            .clone(),
    );
    assert_eq!(
        claims.preferred_username.as_deref(),
        Some("from-id-token"),
        "the signed ID token wins; userinfo only fills gaps"
    );
}

#[test]
fn merge_userinfo_username_lets_the_allowlist_match() {
    // Minimal token + allowlist by username: once userinfo fills the
    // username, the allowlist matches even though the token sub is numeric.
    let mut config = test_config();
    config.oidc_admin_users = "rressl".to_owned();
    let mut claims = oidc_test_claims();
    claims.merge_userinfo(
        serde_json::json!({"sub": "100000000000000001", "preferred_username": "rressl"})
            .as_object()
            .unwrap()
            .clone(),
    );
    let resolution =
        oidc_roles(&config, &claims, "100000000000000001", "rressl", None).expect("roles parse");
    assert!(
        resolution.roles.contains(&Role::Admin),
        "username allowlist matches after the userinfo merge"
    );
}

#[test]
fn oidc_role_mappings_parse_case_insensitively_and_skip_junk() {
    let map = parse_oidc_role_mappings(
        "Archivist-Admin=admin, archivist-reviewer=reviewer, junk, bad=notarole",
    );
    assert_eq!(map.get("archivist-admin"), Some(&Role::Admin));
    assert_eq!(map.get("archivist-reviewer"), Some(&Role::Reviewer));
    assert!(!map.contains_key("bad"), "an unknown app role is skipped");
}

#[test]
fn oidc_degraded_claims_are_detected() {
    let mut claims = OidcIdClaims {
        iss: "https://issuer.example.com".to_owned(),
        sub: "subject-1".to_owned(),
        aud: serde_json::Value::String("client".to_owned()),
        exp: 0,
        nonce: None,
        email: Some("admin@example.com".to_owned()),
        email_verified: None,
        preferred_username: None,
        at_hash: None,
        additional: serde_json::Map::new(),
    };
    // Unverified email + no preferred_username → degraded.
    assert!(oidc_claims_degraded(&claims));

    claims.email_verified = Some(true);
    assert!(!oidc_claims_degraded(&claims));

    claims.email_verified = None;
    claims.preferred_username = Some("rressl".to_owned());
    assert!(!oidc_claims_degraded(&claims));

    // A whitespace-only preferred_username carries no identity.
    claims.preferred_username = Some("  ".to_owned());
    assert!(oidc_claims_degraded(&claims));
}

#[test]
fn oidc_default_roles_are_deduplicated() {
    let mut config = test_config();
    config.oidc_default_roles = "viewer reviewer viewer".to_owned();
    assert_eq!(
        oidc_roles(&config, &oidc_test_claims(), "subject-1", "user", None)
            .expect("roles parse")
            .roles,
        vec![Role::Viewer, Role::Reviewer]
    );
}
