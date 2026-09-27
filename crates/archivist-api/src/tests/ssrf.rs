//! Outbound URL validation (SSRF guard) tests.

use crate::*;

#[tokio::test]
async fn validate_outbound_url_accepts_public_host() {
    // 8.8.8.8 is a public unicast address; no DNS needed.
    let ok = validate_outbound_url("https://8.8.8.8/healthz").await;
    assert!(ok.is_ok(), "expected public IP to be accepted: {ok:?}");
}

#[tokio::test]
async fn validate_outbound_url_rejects_loopback() {
    let err = validate_outbound_url("http://127.0.0.1:8080/")
        .await
        .expect_err("loopback must be rejected");
    assert_eq!(err.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn validate_outbound_url_accepts_rfc1918() {
    // RFC1918 is the common case for K8s service IPs, Docker bridge
    // networks, and on-prem service meshes. Operator-trusted internal
    // targets must be allowed for the in-UI "Test" buttons to work.
    for url in [
        "http://10.0.0.5/api",
        "http://172.16.5.5/api",
        "http://192.168.1.10/api",
    ] {
        let ok = validate_outbound_url(url).await;
        assert!(ok.is_ok(), "{url} must be accepted: {ok:?}");
    }
}

#[tokio::test]
async fn validate_outbound_url_accepts_rfc6598() {
    // RFC6598 shared-address space (100.64.0.0/10) is used by ISP CGN
    // and some homelab/router setups.
    let ok = validate_outbound_url("http://100.64.0.5/api").await;
    assert!(ok.is_ok(), "RFC6598 must be accepted: {ok:?}");
}

#[tokio::test]
async fn validate_outbound_url_accepts_rfc4193() {
    // RFC4193 unique-local IPv6 (fc00::/7). K8s dual-stack clusters
    // and on-prem v6 deployments live here. The previous validator
    // would have rejected this with "private, loopback, or link-local";
    // the new policy must let it through.
    //
    // Use an explicit port so getaddrinfo treats the host as a literal
    // and doesn't actually try DNS (which fails for `fd00::1` in CI).
    let ok = validate_outbound_url("http://[fd00::1]:8080/api").await;
    assert!(ok.is_ok(), "RFC4193 must be accepted: {ok:?}");
}

#[tokio::test]
async fn validate_outbound_url_rejects_non_http_scheme() {
    let err = validate_outbound_url("file:///etc/passwd")
        .await
        .expect_err("non-http scheme rejected");
    assert_eq!(err.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn validate_outbound_url_rejects_userinfo() {
    let err = validate_outbound_url("http://user:pass@8.8.8.8/")
        .await
        .expect_err("userinfo rejected");
    assert_eq!(err.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn validate_outbound_url_rejects_ipv6_loopback() {
    let err = validate_outbound_url("http://[::1]/")
        .await
        .expect_err("IPv6 loopback rejected");
    assert_eq!(err.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn validate_outbound_url_rejects_link_local() {
    // Cloud-metadata IMDS (AWS/Azure/GCP) is at 169.254.169.254.
    let err = validate_outbound_url("http://169.254.169.254/latest/meta-data/")
        .await
        .expect_err("link-local rejected");
    assert_eq!(err.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn validate_outbound_url_rejects_ipv6_link_local() {
    let err = validate_outbound_url("http://[fe80::1]/")
        .await
        .expect_err("IPv6 link-local rejected");
    assert_eq!(err.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn validate_outbound_url_rejects_v4_mapped_loopback() {
    // Make sure an attacker can't smuggle 127.0.0.1 past the v4 check
    // by encoding it as ::ffff:127.0.0.1.
    let err = validate_outbound_url("http://[::ffff:127.0.0.1]/")
        .await
        .expect_err("v4-mapped loopback must be rejected");
    assert_eq!(err.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn validate_outbound_url_rejects_unspecified() {
    let err = validate_outbound_url("http://0.0.0.0/")
        .await
        .expect_err("0.0.0.0 must be rejected");
    assert_eq!(err.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn validate_outbound_url_rejects_multicast() {
    let err = validate_outbound_url("http://224.0.0.1/")
        .await
        .expect_err("multicast must be rejected");
    assert_eq!(err.status, StatusCode::BAD_REQUEST);
}
