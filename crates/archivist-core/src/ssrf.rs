//! Shared SSRF hardening for outbound HTTP clients.
//!
//! [`is_ssrf_dangerous_ip`] is the address policy — the set of IPs that have no
//! legitimate operator target and that an attacker who can influence an
//! outbound URL could abuse (loopback, link-local incl. cloud-metadata IMDS,
//! unspecified, broadcast, multicast). It is intentionally permissive about
//! RFC1918 / RFC6598 / RFC4193 private ranges, because Paperless-ngx and Ollama
//! routinely live on private addresses in the deployments this app targets.
//!
//! It is applied up front by `validate_outbound_url` (in the API crate), which
//! resolves the configured host and rejects dangerous targets both when
//! settings are persisted (`update_settings`, including archive profiles and
//! the notification webhook) and on every outbound tester endpoint. The
//! worker deliberately does not re-validate on its hot path: the persisted
//! values are already guarded, and a per-request DNS lookup would put DNS
//! flakiness in front of every job.
//!
//! There is deliberately **no** connection-time IP-pinning DNS resolver. A
//! `reqwest` `Resolve` implementation was trialled (#183) to close the
//! DNS-rebinding TOCTOU but it replaced reqwest's happy-eyeballs behaviour and
//! caused a worker-only connectivity regression against a dual-stack (A+AAAA)
//! host, so it was reverted (v1.8.1). The residual TOCTOU is accepted because
//! every outbound target is operator-configured (Paperless, AI providers,
//! notification webhook), not user-supplied per request. See
//! `docs/SECURITY_DESIGN.md` §4.3.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// Decide whether an IP must be hard-rejected for outbound requests to
/// operator- or attacker-influenceable URLs (Paperless, Ollama / AI providers,
/// notification webhooks).
///
/// The threat model is narrow on purpose:
///
/// - Paperless Archivist is routinely deployed inside Kubernetes / Docker
///   Compose / on-prem networks where Paperless-ngx and Ollama live on private
///   addresses (10/8, 172.16/12, 192.168/16, RFC6598 100.64/10, RFC4193
///   fc00::/7). Rejecting those would make the integration unusable in every
///   realistic deployment, so they are allowed.
///
/// What we DO reject — the addresses that have no legitimate target and that an
/// attacker who can influence a URL (or rebind DNS) could abuse:
///
/// - Loopback (127.0.0.0/8, ::1)
/// - Link-local incl. cloud metadata IMDS (169.254.0.0/16, fe80::/10)
/// - Unspecified (0.0.0.0, ::)
/// - Broadcast (255.255.255.255)
/// - Multicast
///
/// See `docs/SECURITY_DESIGN.md` section 4.3 for the full rationale.
pub fn is_ssrf_dangerous_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_ssrf_dangerous_ipv4(v4),
        IpAddr::V6(v6) => is_ssrf_dangerous_ipv6(v6),
    }
}

fn is_ssrf_dangerous_ipv4(ip: Ipv4Addr) -> bool {
    // Hard reject — no legitimate operator target.
    if ip.is_loopback() || ip.is_unspecified() || ip.is_broadcast() || ip.is_multicast() {
        return true;
    }
    // Link-local 169.254/16 includes the cloud-metadata IMDS endpoint
    // (169.254.169.254). Keep rejecting — leaking cloud IAM creds via a
    // ghost request would be catastrophic, and no real integration target
    // lives there.
    if ip.is_link_local() {
        return true;
    }
    false
}

fn is_ssrf_dangerous_ipv6(ip: Ipv6Addr) -> bool {
    if ip.is_loopback() || ip.is_unspecified() || ip.is_multicast() {
        return true;
    }
    // Mapped IPv4: re-evaluate the embedded v4 so an attacker can't smuggle
    // 127.0.0.1 as ::ffff:127.0.0.1 / ::127.0.0.1 past the loopback check.
    if let Some(v4) = ip.to_ipv4_mapped()
        && is_ssrf_dangerous_ipv4(v4)
    {
        return true;
    }
    if let Some(v4) = ip.to_ipv4()
        && is_ssrf_dangerous_ipv4(v4)
    {
        return true;
    }
    let segments = ip.segments();
    // Link-local fe80::/10 — same metadata/IMDS reasoning as v4.
    if (segments[0] & 0xffc0) == 0xfe80 {
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_loopback_and_metadata() {
        assert!(is_ssrf_dangerous_ip("127.0.0.1".parse().unwrap()));
        assert!(is_ssrf_dangerous_ip("169.254.169.254".parse().unwrap()));
        assert!(is_ssrf_dangerous_ip("0.0.0.0".parse().unwrap()));
        assert!(is_ssrf_dangerous_ip("::1".parse().unwrap()));
        assert!(is_ssrf_dangerous_ip("fe80::1".parse().unwrap()));
        assert!(is_ssrf_dangerous_ip("::ffff:127.0.0.1".parse().unwrap()));
    }

    #[test]
    fn allows_public_and_private_targets() {
        assert!(!is_ssrf_dangerous_ip("8.8.8.8".parse().unwrap()));
        assert!(!is_ssrf_dangerous_ip("10.0.0.5".parse().unwrap()));
        assert!(!is_ssrf_dangerous_ip("192.168.1.10".parse().unwrap()));
        assert!(!is_ssrf_dangerous_ip("100.64.0.5".parse().unwrap()));
        assert!(!is_ssrf_dangerous_ip("fd00::1".parse().unwrap()));
    }
}
