//! Single shared SSRF / IMDS / RFC1918 IP-classification guard.
//!
//! This module is the **one** source of truth for deciding whether an IP address
//! may be reached on behalf of a box. It is used at every outbound connect point
//! the control plane owns:
//!
//! - the CONNECT forward proxy ([`crate::egress`]), which resolves the requested
//!   host and connects to the exact resolved [`SocketAddr`] (so a name cannot
//!   rebind to a forbidden IP between the check and the connect); and
//! - the credential-injecting reverse-proxy transport ([`crate::proxy`]), whose
//!   custom reqwest DNS resolver hands reqwest only the validated public
//!   addresses for a name (see [`crate::proxy`]'s `GuardedResolver`).
//!
//! Both call points share [`is_forbidden_ip`] and the resolve-and-filter helpers
//! below, so there is exactly one definition of "forbidden" and no forked copy
//! can drift. The classification covers loopback, private (RFC1918), link-local
//! (including the `169.254.169.254` cloud IMDS endpoint), CGNAT, benchmarking,
//! documentation, reserved, and IPv4-embedded-in-IPv6 ranges — the last so a
//! value like `::ffff:127.0.0.1` or `::7f00:1` cannot smuggle a private host
//! through an IPv6 literal.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use anyhow::{Context, Result};

/// Whether an IP must not be reached: loopback, private, link-local (incl. the
/// IMDS endpoint), and other non-globally-routable ranges. This is the single
/// shared SSRF/IMDS/RFC1918 verdict used by both the CONNECT proxy and the
/// reverse-proxy transport.
pub(crate) fn is_forbidden_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_forbidden_v4(v4),
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(mapped) => is_forbidden_v4(mapped),
            None => is_forbidden_v6(v6),
        },
    }
}

fn is_forbidden_v4(ip: Ipv4Addr) -> bool {
    ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_broadcast()
        || ip.is_documentation()
        || ip.is_multicast()
        || is_shared_v4(ip)
        || is_benchmarking_v4(ip)
        || is_ietf_v4(ip)
        || is_reserved_v4(ip)
}

/// 100.64.0.0/10 — CGNAT shared address space.
fn is_shared_v4(ip: Ipv4Addr) -> bool {
    let [a, b, ..] = ip.octets();
    a == 100 && (64..=127).contains(&b)
}

/// 198.18.0.0/15 — benchmarking.
fn is_benchmarking_v4(ip: Ipv4Addr) -> bool {
    let [a, b, ..] = ip.octets();
    a == 198 && (b == 18 || b == 19)
}

/// 192.0.0.0/24 — IETF protocol assignments.
fn is_ietf_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    a == 192 && b == 0 && c == 0
}

/// 240.0.0.0/4 — reserved.
fn is_reserved_v4(ip: Ipv4Addr) -> bool {
    let [a, ..] = ip.octets();
    a >= 240
}

fn is_forbidden_v6(ip: Ipv6Addr) -> bool {
    ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_multicast()
        || is_unique_local_v6(ip)
        || is_link_local_v6(ip)
        || is_documentation_v6(ip)
        || is_embedded_v4_compat(ip)
}

/// fc00::/7 — unique local addresses.
fn is_unique_local_v6(ip: Ipv6Addr) -> bool {
    let [a, ..] = ip.segments();
    (a & 0xfe00) == 0xfc00
}

/// fe80::/10 — link-local unicast.
fn is_link_local_v6(ip: Ipv6Addr) -> bool {
    let [a, ..] = ip.segments();
    (a & 0xffc0) == 0xfe80
}

/// 2001:db8::/32 — documentation.
fn is_documentation_v6(ip: Ipv6Addr) -> bool {
    let [a, b, ..] = ip.segments();
    a == 0x2001 && b == 0x0db8
}

/// ::a.b.c.d — deprecated IPv4-compatible addresses embed an IPv4 host; refuse them
/// so `::7f00:1` (`::127.0.0.1`) can't tunnel to loopback. `::` and `::1` are already
/// refused as unspecified/loopback.
fn is_embedded_v4_compat(ip: Ipv6Addr) -> bool {
    let [a, b, c, d, e, f, ..] = ip.segments();
    a == 0 && b == 0 && c == 0 && d == 0 && e == 0 && f == 0
}

/// Drop every forbidden address from `addrs`, keeping only publicly routable
/// ones. This is the pure filter both the CONNECT proxy and the reverse-proxy
/// DNS resolver delegate to, so the exact same verdict is applied wherever a
/// resolved address set is screened.
pub(crate) fn retain_public(addrs: impl Iterator<Item = SocketAddr>) -> Vec<SocketAddr> {
    addrs.filter(|addr| !is_forbidden_ip(addr.ip())).collect()
}

/// The first publicly routable address in `addrs`, or `None` when every address
/// is forbidden. Pure counterpart to [`resolve_public`] used by the CONNECT path.
pub(crate) fn first_public(mut addrs: impl Iterator<Item = SocketAddr>) -> Option<SocketAddr> {
    addrs.find(|addr| !is_forbidden_ip(addr.ip()))
}

/// Resolve `host:port` and return every address whose IP is publicly routable
/// (forbidden addresses dropped). The returned vector may be empty when the host
/// resolves only to forbidden IPs — callers must treat an empty result as
/// fail-closed.
///
/// # Errors
///
/// Returns an error when name resolution itself fails.
pub(crate) async fn resolve_public_addrs(host: &str, port: u16) -> Result<Vec<SocketAddr>> {
    let addrs = tokio::net::lookup_host((host, port))
        .await
        .with_context(|| format!("resolve {host}"))?;
    Ok(retain_public(addrs))
}

/// Resolve `host:port` and return the first address whose IP is publicly
/// routable, or `None` when every resolved address is forbidden
/// (private/loopback/link-local/etc.).
///
/// A caller that connects to this exact resolved address (not a re-resolution)
/// closes the DNS-rebind window: a name cannot rebind to a forbidden IP between
/// this check and the connect.
///
/// # Errors
///
/// Returns an error when name resolution itself fails.
pub(crate) async fn resolve_public(host: &str, port: u16) -> Result<Option<SocketAddr>> {
    let addrs = tokio::net::lookup_host((host, port))
        .await
        .with_context(|| format!("resolve {host}"))?;
    Ok(first_public(addrs.into_iter()))
}

#[cfg(test)]
#[expect(
    clippy::unwrap_used,
    reason = "test code: panic on assertion failure is acceptable"
)]
mod tests {
    use super::*;

    fn forbidden(ip: &str) -> bool {
        is_forbidden_ip(ip.parse().unwrap())
    }

    fn sock(ip: &str) -> SocketAddr {
        SocketAddr::new(ip.parse().unwrap(), 0)
    }

    #[test]
    fn public_addresses_are_allowed() {
        // GitHub, Cloudflare DNS, a public IPv6.
        assert!(!forbidden("140.82.121.3"));
        assert!(!forbidden("1.1.1.1"));
        assert!(!forbidden("2606:4700:4700::1111"));
    }

    #[test]
    fn imds_and_private_v4_are_forbidden() {
        assert!(forbidden("169.254.169.254")); // IMDS — the one that matters most
        assert!(forbidden("10.0.0.5"));
        assert!(forbidden("172.16.0.1"));
        assert!(forbidden("192.168.1.1"));
        assert!(forbidden("127.0.0.1"));
        assert!(forbidden("100.64.0.1")); // CGNAT
        assert!(forbidden("0.0.0.0"));
        assert!(forbidden("255.255.255.255"));
        assert!(forbidden("240.0.0.1")); // reserved
    }

    #[test]
    fn loopback_and_local_v6_are_forbidden() {
        assert!(forbidden("::1"));
        assert!(forbidden("::"));
        assert!(forbidden("fe80::1")); // link-local
        assert!(forbidden("fc00::1")); // unique-local
        assert!(forbidden("2001:db8::1")); // documentation
    }

    #[test]
    fn v6_embedded_v4_cannot_smuggle_a_private_host() {
        // IPv4-mapped and IPv4-compatible forms of loopback must both be refused.
        assert!(forbidden("::ffff:127.0.0.1"));
        assert!(forbidden("::ffff:169.254.169.254"));
        assert!(forbidden("::7f00:1")); // ::127.0.0.1 (compatible form)
    }

    #[test]
    fn ip_guard_is_port_agnostic() {
        // is_forbidden_ip classifies the IP only, independent of the port, so the
        // SAME SSRF/IMDS/RFC1918 verdict covers a port-80 CONNECT exactly as it
        // covers 443 (the port gate is a separate concern in egress.rs).
        assert!(forbidden("169.254.169.254")); // IMDS link-local
        assert!(forbidden("10.0.0.5")); // RFC1918 private
        assert!(forbidden("127.0.0.1")); // loopback
        assert!(forbidden("100.64.0.1")); // CGNAT shared
        assert!(forbidden("::1")); // IPv6 loopback
        assert!(forbidden("fe80::1")); // IPv6 link-local
        assert!(forbidden("::ffff:169.254.169.254")); // IPv4-mapped IMDS
    }

    #[test]
    fn retain_public_drops_every_forbidden_candidate() {
        // The shared filter that both proxies delegate to: forbidden candidates
        // (IMDS, RFC1918, loopback, and IPv6-embedded loopback in mapped and
        // compatible forms) are dropped while a public addr is kept.
        let candidates = [
            sock("169.254.169.254"),
            sock("10.0.0.5"),
            sock("127.0.0.1"),
            sock("::ffff:127.0.0.1"),
            sock("::7f00:1"),
            sock("140.82.121.3"),
        ];
        let kept = retain_public(candidates.into_iter());
        assert_eq!(kept, vec![sock("140.82.121.3")]);
    }

    #[test]
    fn first_public_skips_forbidden_and_fails_closed_when_all_forbidden() {
        // first_public returns the first non-forbidden addr, skipping forbidden
        // ones ahead of it...
        let mixed = [sock("10.0.0.5"), sock("140.82.121.3"), sock("1.1.1.1")];
        assert_eq!(first_public(mixed.into_iter()), Some(sock("140.82.121.3")));
        // ...and yields None (fail-closed) when every candidate is forbidden.
        let all_forbidden = [sock("169.254.169.254"), sock("127.0.0.1"), sock("::7f00:1")];
        assert_eq!(first_public(all_forbidden.into_iter()), None);
    }
}
