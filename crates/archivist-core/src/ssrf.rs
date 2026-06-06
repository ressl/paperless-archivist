//! Shared SSRF hardening for outbound HTTP clients.
//!
//! Two layers cooperate:
//!
//! * [`is_ssrf_dangerous_ip`] is the address policy — the set of IPs that have
//!   no legitimate operator target and that an attacker who can influence an
//!   outbound URL could abuse (loopback, link-local incl. cloud-metadata IMDS,
//!   unspecified, broadcast, multicast). It is intentionally permissive about
//!   RFC1918 / RFC6598 / RFC4193 private ranges, because Paperless-ngx and
//!   Ollama routinely live on private addresses in the deployments this app
//!   targets.
//!
//! * [`SsrfGuardResolver`] is a `reqwest` DNS resolver that applies that policy
//!   at *connection time*, on the exact addresses reqwest is about to dial.
//!   Installing it on every client that talks to an operator- or
//!   attacker-influenceable URL (webhook, Paperless, AI providers) closes the
//!   DNS-rebinding TOCTOU: a hostname cannot be re-pointed to an internal
//!   address in the gap between an up-front `validate_outbound_url` check and
//!   the actual connect, because the resolver re-checks the resolved IPs at the
//!   moment they are used and the connection can only use the addresses it
//!   returns. This runs on both the UI "Test" handlers and the worker data
//!   path (download / AI calls), not just the test endpoints.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;

use reqwest::dns::{Addrs, Name, Resolve, Resolving};

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

/// `reqwest` DNS resolver that resolves a host and drops every address that
/// [`is_ssrf_dangerous_ip`] rejects before reqwest connects.
///
/// Because the filtering happens at connection time on the exact addresses that
/// will be dialed, a hostname cannot be rebound to an internal address between
/// an up-front validation and the actual connect (DNS-rebinding TOCTOU).
///
/// Note: hyper does not route IP-literal hosts through the DNS resolver, so a
/// URL whose host is already an IP literal is connected to directly and bypasses
/// this resolver. IP literals carry no rebinding risk (the address is fixed);
/// the UI test handlers additionally screen them up front via
/// `validate_outbound_url`.
#[derive(Debug, Clone, Copy, Default)]
pub struct SsrfGuardResolver;

impl SsrfGuardResolver {
    /// Construct the resolver wrapped in an `Arc`, ready for
    /// `reqwest::ClientBuilder::dns_resolver`.
    pub fn arc() -> Arc<Self> {
        Arc::new(Self)
    }
}

impl Resolve for SsrfGuardResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_owned();
        Box::pin(async move {
            // Port 0: reqwest overrides it with the URL's explicit port, or the
            // scheme default, after this resolver returns.
            let resolved = tokio::net::lookup_host((host.as_str(), 0u16)).await?;
            let mut safe: Vec<SocketAddr> = Vec::new();
            for addr in resolved {
                if !is_ssrf_dangerous_ip(addr.ip()) {
                    safe.push(addr);
                }
            }
            if safe.is_empty() {
                let err: Box<dyn std::error::Error + Send + Sync> = format!(
                    "SSRF guard blocked host '{host}': resolved only to loopback / \
                     link-local / unspecified / multicast addresses"
                )
                .into();
                return Err(err);
            }
            Ok(Box::new(safe.into_iter()) as Addrs)
        })
    }
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
