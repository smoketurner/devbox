//! Allowlisting HTTPS CONNECT forward proxy.
//!
//! A devbox sets `HTTPS_PROXY` at this listener; for each `CONNECT host:443` the
//! proxy checks `host` against an allowlist, resolves it, refuses any non-public IP
//! (SSRF / IMDS / RFC1918 protection), then relays the encrypted TLS bytes. It runs
//! as a second listener in the server binary, independent of the API router.
//!
//! The proxy tunnels ciphertext — it never sees inside TLS, so it cannot inject
//! credentials. That is the reverse proxy's job ([`crate::github::git_proxy`]); this
//! is the host-allowlist gate for everything else.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::CancellationToken;

use devbox_common::env_non_empty;

const PROXY_ADDR_ENV: &str = "EGRESS_PROXY_ADDR";
const ALLOWLIST_ENV: &str = "EGRESS_ALLOWLIST";

/// Only HTTPS is proxied; a CONNECT to any other port is refused.
const ALLOWED_PORT: u16 = 443;

/// Cap on the CONNECT request head read before giving up.
const MAX_HEAD: usize = 8 * 1024;

/// Read chunk for the CONNECT head.
const READ_CHUNK: usize = 1024;

/// Bound on reading the CONNECT head, so a slow client can't hold a connection open.
const HEAD_TIMEOUT: Duration = Duration::from_secs(15);

/// Bound on the upstream TCP connect, so an unreachable/black-holed host fails fast
/// instead of hanging the client indefinitely.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Set of hostnames a devbox may reach through the proxy. An entry matches the host
/// exactly or as a parent domain (`crates.io` matches `static.crates.io`).
#[derive(Clone, Debug, Default)]
pub struct Allowlist {
    entries: Vec<String>,
}

impl Allowlist {
    /// Parse a comma-separated allowlist, lowercasing and trimming each entry.
    fn from_csv(raw: &str) -> Self {
        let entries = raw
            .split(',')
            .map(|e| e.trim().trim_matches('.').to_ascii_lowercase())
            .filter(|e| !e.is_empty())
            .collect();
        Self { entries }
    }

    /// Whether `host` is allowed: equal to an entry or a subdomain of one.
    fn allows(&self, host: &str) -> bool {
        let host = host.trim_matches('.').to_ascii_lowercase();
        self.entries
            .iter()
            .any(|entry| host == *entry || host.ends_with(&format!(".{entry}")))
    }

    fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Configuration for the egress proxy, or `None` when `EGRESS_PROXY_ADDR` is unset
/// (the proxy is disabled).
pub struct EgressConfig {
    pub addr: SocketAddr,
    pub allowlist: Allowlist,
}

impl EgressConfig {
    /// Read the proxy config from the environment. `Ok(None)` disables the proxy.
    ///
    /// # Errors
    ///
    /// Returns an error when `EGRESS_PROXY_ADDR` is set but not a valid socket address.
    pub fn from_env() -> Result<Option<Self>> {
        let Some(addr_raw) = env_non_empty(PROXY_ADDR_ENV) else {
            return Ok(None);
        };
        let addr: SocketAddr = addr_raw
            .parse()
            .with_context(|| format!("invalid {PROXY_ADDR_ENV}: {addr_raw}"))?;
        let allowlist = Allowlist::from_csv(&env_non_empty(ALLOWLIST_ENV).unwrap_or_default());
        if allowlist.is_empty() {
            tracing::warn!("egress proxy enabled with an empty allowlist; all egress is denied");
        }
        Ok(Some(Self { addr, allowlist }))
    }
}

/// Accept CONNECT tunnels until `cancel` fires. Each connection is handled on its
/// own task; a failed connection is logged and dropped.
pub async fn serve(listener: TcpListener, allowlist: Arc<Allowlist>, cancel: CancellationToken) {
    loop {
        tokio::select! {
            () = cancel.cancelled() => {
                tracing::info!("egress proxy shutting down");
                return;
            }
            accepted = listener.accept() => match accepted {
                Ok((stream, peer)) => {
                    let allowlist = Arc::clone(&allowlist);
                    tokio::spawn(async move {
                        if let Err(e) = handle_connection(stream, &allowlist).await {
                            tracing::debug!(%peer, error = %format!("{e:#}"), "egress connection failed");
                        }
                    });
                }
                Err(e) => tracing::warn!(error = %e, "egress accept failed"),
            },
        }
    }
}

/// Handle one client connection: parse CONNECT, authorize, then tunnel.
async fn handle_connection(mut client: TcpStream, allowlist: &Allowlist) -> Result<()> {
    let (host, port, leftover) =
        match tokio::time::timeout(HEAD_TIMEOUT, read_connect_head(&mut client)).await {
            Ok(Ok(Some(head))) => head,
            Ok(Ok(None)) => return refuse(&mut client, "400 Bad Request").await,
            Ok(Err(e)) => return Err(e),
            Err(_) => return refuse(&mut client, "408 Request Timeout").await,
        };

    if port != ALLOWED_PORT {
        tracing::info!(host, port, "egress denied (port not allowed)");
        return refuse(&mut client, "403 Forbidden").await;
    }
    if !allowlist.allows(&host) {
        tracing::info!(host, "egress denied (host not in allowlist)");
        return refuse(&mut client, "403 Forbidden").await;
    }

    let Some(target) = resolve_public(&host, port).await? else {
        tracing::warn!(
            host,
            "egress denied (resolves only to non-public addresses)"
        );
        return refuse(&mut client, "403 Forbidden").await;
    };

    let mut upstream = match tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(target)).await
    {
        Ok(Ok(stream)) => stream,
        Ok(Err(e)) => {
            tracing::debug!(host, %target, error = %e, "egress upstream connect failed");
            return refuse(&mut client, "502 Bad Gateway").await;
        }
        Err(_) => {
            tracing::warn!(host, %target, "egress upstream connect timed out");
            return refuse(&mut client, "504 Gateway Timeout").await;
        }
    };
    client
        .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
        .await?;
    if !leftover.is_empty() {
        upstream.write_all(&leftover).await?;
    }
    tokio::io::copy_bidirectional(&mut client, &mut upstream).await?;
    Ok(())
}

/// Write a minimal HTTP error response and end the connection. A write failure
/// here just means the client already went away, surfaced to the caller's debug log.
async fn refuse(client: &mut TcpStream, status: &str) -> Result<()> {
    let response = format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\n\r\n");
    client.write_all(response.as_bytes()).await?;
    Ok(())
}

/// Read the CONNECT request head, returning `(host, port, leftover)` where
/// `leftover` is any bytes the client pipelined after the head (forwarded upstream).
/// `Ok(None)` for a malformed or oversized head, or EOF before it completes.
async fn read_connect_head(client: &mut TcpStream) -> Result<Option<(String, u16, Vec<u8>)>> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; READ_CHUNK];
    loop {
        if buf.len() > MAX_HEAD {
            return Ok(None);
        }
        let n = client.read(&mut chunk).await?;
        if n == 0 {
            return Ok(None);
        }
        match chunk.get(..n) {
            Some(read) => buf.extend_from_slice(read),
            None => return Ok(None),
        }
        if let Some(end) = find_head_end(&buf) {
            let head = buf.get(..end).unwrap_or_default();
            let Some((host, port)) = parse_connect_target(head) else {
                return Ok(None);
            };
            let leftover = buf
                .get(end.saturating_add(4)..)
                .map(<[u8]>::to_vec)
                .unwrap_or_default();
            return Ok(Some((host, port, leftover)));
        }
    }
}

/// The index where the `\r\n\r\n` head terminator begins, if present.
fn find_head_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

/// Parse the `host` and `port` from a `CONNECT host:port HTTP/1.1` request head.
/// `None` for a non-CONNECT method or a malformed target.
fn parse_connect_target(head: &[u8]) -> Option<(String, u16)> {
    let line = std::str::from_utf8(head).ok()?.lines().next()?;
    let mut parts = line.split_whitespace();
    if !parts.next()?.eq_ignore_ascii_case("CONNECT") {
        return None;
    }
    let (host, port) = parts.next()?.rsplit_once(':')?;
    let host = host.trim_start_matches('[').trim_end_matches(']');
    if host.is_empty() {
        return None;
    }
    Some((host.to_string(), port.parse().ok()?))
}

/// Resolve `host:port` and return the first address whose IP is publicly routable,
/// or `None` when every resolved address is private/loopback/link-local/etc.
///
/// The proxy connects to this exact resolved address (not a re-resolution), so a
/// DNS name cannot rebind to a forbidden IP between the check and the connect.
async fn resolve_public(host: &str, port: u16) -> Result<Option<SocketAddr>> {
    let addrs = tokio::net::lookup_host((host, port))
        .await
        .with_context(|| format!("resolve {host}"))?;
    Ok(addrs.into_iter().find(|addr| !is_forbidden_ip(addr.ip())))
}

/// Whether an IP must not be reached through the proxy: loopback, private, link-local
/// (incl. the IMDS endpoint), and other non-globally-routable ranges.
fn is_forbidden_ip(ip: IpAddr) -> bool {
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

    #[test]
    fn allowlist_matches_exact_and_subdomains() {
        let list = Allowlist::from_csv("crates.io, static.crates.io , github.com");
        assert!(list.allows("crates.io"));
        assert!(list.allows("static.crates.io"));
        assert!(list.allows("index.crates.io")); // subdomain of crates.io
        assert!(list.allows("GitHub.com")); // case-insensitive
        assert!(list.allows("codeload.github.com"));
    }

    #[test]
    fn allowlist_rejects_non_members_and_lookalikes() {
        let list = Allowlist::from_csv("crates.io");
        assert!(!list.allows("evil.com"));
        // A suffix match must be on a dot boundary, not a substring.
        assert!(!list.allows("notcrates.io"));
        assert!(!list.allows("crates.io.evil.com"));
        assert!(!Allowlist::from_csv("").allows("crates.io"));
    }

    #[test]
    fn parses_connect_target() {
        assert_eq!(
            parse_connect_target(b"CONNECT github.com:443 HTTP/1.1"),
            Some(("github.com".to_string(), 443))
        );
        // Method is case-insensitive; IPv6 authority brackets are stripped.
        assert_eq!(
            parse_connect_target(b"connect [2606:4700::1111]:443 HTTP/1.1"),
            Some(("2606:4700::1111".to_string(), 443))
        );
    }

    #[test]
    fn rejects_non_connect_and_malformed_targets() {
        assert_eq!(parse_connect_target(b"GET / HTTP/1.1"), None);
        assert_eq!(parse_connect_target(b"CONNECT github.com HTTP/1.1"), None);
        assert_eq!(
            parse_connect_target(b"CONNECT github.com:https HTTP/1.1"),
            None
        );
        assert_eq!(parse_connect_target(b""), None);
    }

    #[test]
    fn finds_head_terminator_and_ignores_partial() {
        assert_eq!(
            find_head_end(b"CONNECT x:443 HTTP/1.1\r\n\r\nDATA"),
            Some(22)
        );
        assert_eq!(find_head_end(b"CONNECT x:443 HTTP/1.1\r\n"), None);
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
}
