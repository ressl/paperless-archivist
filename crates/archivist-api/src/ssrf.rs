//! Outbound URL validation (SSRF guard) for operator-supplied endpoints.

use crate::*;

/// Parse a URL provided by an administrator and reject targets that would
/// allow Server-Side Request Forgery (SSRF) against the host network. The
/// caller is expected to use this on every outbound "tester" endpoint where
/// an admin can supply an arbitrary URL, for an early, friendly rejection.
///
/// This is an up-front DNS-time check. There is intentionally **no**
/// connection-time IP-pinning resolver: a custom `reqwest` DNS resolver was
/// trialled to close the DNS-rebinding TOCTOU but it replaced reqwest's
/// happy-eyeballs behaviour and caused a worker-only connectivity regression on
/// a dual-stack host (host resolved A+AAAA; curl succeeded, the worker got
/// spurious 404s), so it was reverted (v1.8.1) and removed. See #183.
///
/// Accepted residual risk: the DNS-rebinding TOCTOU between this check and the
/// actual request is **not** closed. It is acceptable here because every
/// outbound target is operator-configured (the Paperless base URL, the LLM
/// provider URLs, and the notification webhook) rather than user-supplied per
/// request, so exploiting the window requires an attacker who already controls
/// DNS for an admin-configured host. The remaining controls — this validation
/// plus `redirect::Policy::none()` on the outbound clients — cover the
/// practical vectors. Redirects are refused so a 3xx to an internal address
/// (e.g. IMDS / loopback) cannot bypass the validated origin.
///
/// Rejections:
///  * non-http/https schemes
///  * URLs containing `user:pass@` userinfo
///  * URLs whose host resolves (DNS) to a loopback, link-local, unspecified,
///    broadcast, or multicast address
///
/// Returns the parsed `Url` on success.
pub(crate) async fn validate_outbound_url(raw: &str) -> Result<Url, ApiError> {
    validate_outbound_url_with(raw, DnsFailure::Reject).await
}

/// Settings-save variant: scheme/userinfo/dangerous-IP rules are identical,
/// but an unresolvable hostname passes. A name that does not resolve is not
/// an SSRF target, and a transient DNS outage (or pre-configuring a host that
/// only resolves later/inside the cluster) must not block unrelated settings
/// changes. The strict variant stays on the tester endpoints, where "does it
/// resolve" is exactly the feedback the operator asked for.
pub(crate) async fn validate_outbound_url_for_save(raw: &str) -> Result<Url, ApiError> {
    validate_outbound_url_with(raw, DnsFailure::Allow).await
}

#[derive(Clone, Copy, PartialEq)]
pub(crate) enum DnsFailure {
    Reject,
    Allow,
}

pub(crate) async fn validate_outbound_url_with(
    raw: &str,
    dns_failure: DnsFailure,
) -> Result<Url, ApiError> {
    let parsed = Url::parse(raw.trim()).map_err(|_| ApiError::bad_request("invalid URL"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(ApiError::bad_request("URL scheme must be http or https"));
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(ApiError::bad_request("URL must not contain userinfo"));
    }
    let host = parsed
        .host()
        .ok_or_else(|| ApiError::bad_request("URL is missing a host"))?;
    let port = parsed.port_or_known_default().unwrap_or(0);
    let ips: Vec<IpAddr> = match host {
        // IP literals don't go through DNS — using `lookup_host` for them
        // is both wasteful and can fail on some platforms for ULA / RFC4193
        // addresses (macOS getaddrinfo with brackets in the host string).
        url::Host::Ipv4(v4) => vec![IpAddr::V4(v4)],
        url::Host::Ipv6(v6) => vec![IpAddr::V6(v6)],
        url::Host::Domain(domain) => match tokio::net::lookup_host((domain, port)).await {
            Ok(addresses) => addresses.map(|addr| addr.ip()).collect(),
            Err(error) if dns_failure == DnsFailure::Allow => {
                tracing::debug!(
                    domain,
                    %error,
                    "skipping SSRF IP check: host does not resolve from here"
                );
                Vec::new()
            }
            Err(error) => {
                return Err(ApiError::bad_request(format!(
                    "failed to resolve host: {error}"
                )));
            }
        },
    };
    if ips.is_empty() && dns_failure == DnsFailure::Reject {
        return Err(ApiError::bad_request("host did not resolve to any address"));
    }
    for ip in &ips {
        if archivist_core::ssrf::is_ssrf_dangerous_ip(*ip) {
            return Err(ApiError::bad_request(
                "URL resolves to a loopback, link-local, or otherwise unroutable address",
            ));
        }
    }
    Ok(parsed)
}
