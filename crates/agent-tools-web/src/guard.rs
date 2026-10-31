//! What a `web.request` is allowed to reach.
//!
//! The model chooses the URL, so the URL cannot be trusted: a request to
//! `http://169.254.169.254/latest/meta-data/` is a credential read, and one to
//! `http://127.0.0.1:8080/admin` is a call into every service on the machine.
//! The guard refuses addresses that are not public, at every hop — the first
//! URL, each redirect, and the address a client would actually connect to.
//!
//! Resolution is part of the check rather than a formality before it. A name
//! that resolves to a private address is refused by address, not by name, and
//! the address that was checked is the address the client is pinned to — so a
//! second lookup that answers differently cannot slip past the guard.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use reqwest::Url;

/// Why a URL is not one this tool may fetch.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GuardError {
    #[error("only http and https are reachable (got scheme '{0}')")]
    Scheme(String),
    #[error("the URL carries credentials; put them in headers instead")]
    Credentials,
    #[error("the URL has no host")]
    NoHost,
    #[error("'{host}' resolves to {address}, which is not a public address")]
    Address { host: String, address: IpAddr },
    #[error("'{host}' does not resolve: {reason}")]
    Unresolved { host: String, reason: String },
}

/// What this tool may connect to.
///
/// The assembled binary uses [`Addresses::public_only`]. The permissive
/// variant exists for fixtures — a test server listens on loopback — and is
/// named so a reader of the wiring can see which one a run was given.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Addresses {
    PublicOnly,
    Any,
}

impl Addresses {
    pub fn public_only() -> Self {
        Self::PublicOnly
    }

    pub fn any() -> Self {
        Self::Any
    }

    /// Whether an address may be reached.
    pub fn allows(&self, address: IpAddr) -> bool {
        match self {
            Self::Any => true,
            Self::PublicOnly => is_public(address),
        }
    }
}

/// Whether an address is one a public request may reach.
///
/// "Public" is the complement of every range that is not: loopback, private,
/// link-local, unique-local, shared (100.64/10), benchmarking (198.18/15),
/// documentation, multicast, broadcast, reserved, and the unspecified address.
/// An IPv6 address that embeds an IPv4 one (a mapped address) is judged by the
/// IPv4 it carries, which is where the loopback and private cases live.
pub fn is_public(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(v4) => is_public_v4(v4),
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_public_v4(v4);
            }
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || v6.is_unique_local()
                || v6.is_unicast_link_local()
                || is_documentation_v6(v6))
        }
    }
}

fn is_public_v4(address: Ipv4Addr) -> bool {
    let [a, b, _, _] = address.octets();
    !(address.is_loopback()
        || address.is_private()
        || address.is_link_local()
        || address.is_unspecified()
        || address.is_broadcast()
        || address.is_multicast()
        || address.is_documentation()
        // Carrier-grade NAT: not routable on the public internet.
        || (a == 100 && (64..128).contains(&b))
        // Benchmarking.
        || (a == 198 && (b == 18 || b == 19))
        // Reserved for future use, and this network.
        || a >= 240
        || a == 0)
}

fn is_documentation_v6(address: Ipv6Addr) -> bool {
    let segments = address.segments();
    // 2001:db8::/32
    segments[0] == 0x2001 && segments[1] == 0x0db8
}

/// The parts of a URL the guard reads.
pub struct Target {
    pub url: Url,
    pub host: String,
    pub port: u16,
}

/// Read a URL into what the guard needs, refusing the shapes that are never
/// what a public request means: non-http schemes and embedded credentials.
pub fn parse(raw: &str) -> Result<Target, GuardError> {
    let url = Url::parse(raw).map_err(|_| GuardError::Scheme(raw.to_string()))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(GuardError::Scheme(url.scheme().to_string()));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(GuardError::Credentials);
    }
    let host = url.host_str().ok_or(GuardError::NoHost)?.to_string();
    let port = url.port_or_known_default().ok_or(GuardError::NoHost)?;
    Ok(Target { url, host, port })
}

/// Resolve `target` and return the addresses the request may use.
///
/// Every resolved address is checked; one that is not allowed refuses the whole
/// target, because a client may pick any of them. The returned list is what the
/// client is pinned to.
pub async fn resolve(target: &Target, addresses: Addresses) -> Result<Vec<IpAddr>, GuardError> {
    let resolved: Vec<IpAddr> = tokio::net::lookup_host((target.host.as_str(), target.port))
        .await
        .map_err(|error| GuardError::Unresolved {
            host: target.host.clone(),
            reason: error.to_string(),
        })?
        .map(|socket| socket.ip())
        .collect();
    if resolved.is_empty() {
        return Err(GuardError::Unresolved {
            host: target.host.clone(),
            reason: "no addresses".to_string(),
        });
    }
    for address in &resolved {
        if !addresses.allows(*address) {
            return Err(GuardError::Address { host: target.host.clone(), address: *address });
        }
    }
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(text: &str) -> IpAddr {
        text.parse().expect("an address")
    }

    #[test]
    fn loopback_and_private_ranges_are_not_public() {
        for text in [
            "127.0.0.1",
            "127.1.2.3",
            "10.0.0.1",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "198.18.0.1",
            "0.0.0.0",
            "255.255.255.255",
            "240.0.0.1",
            "::1",
            "::",
            "fe80::1",
            "fc00::1",
            "fd12:3456::1",
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.1",
            "2001:db8::1",
        ] {
            assert!(!is_public(ip(text)), "{text} must not be reachable");
        }
    }

    #[test]
    fn public_addresses_are_public() {
        for text in ["8.8.8.8", "1.1.1.1", "172.32.0.1", "2606:4700::1111", "::ffff:8.8.8.8"] {
            assert!(is_public(ip(text)), "{text} must be reachable");
        }
    }

    #[test]
    fn only_http_and_https_without_credentials_are_targets() {
        assert!(matches!(parse("file:///etc/passwd"), Err(GuardError::Scheme(_))));
        assert!(matches!(parse("ftp://example.com/x"), Err(GuardError::Scheme(_))));
        assert!(matches!(parse("http://user:pw@example.com/"), Err(GuardError::Credentials)));
        assert!(matches!(parse("http://user@example.com/"), Err(GuardError::Credentials)));
        let target = parse("https://example.com/x").expect("a target");
        assert_eq!(target.port, 443, "the scheme's default port is the one checked");
        assert_eq!(parse("http://example.com/").expect("a target").port, 80);
    }

    #[test]
    fn the_permissive_policy_still_parses_urls() {
        assert!(matches!(parse("not a url"), Err(GuardError::Scheme(_))));
        assert!(Addresses::any().allows(ip("127.0.0.1")));
        assert!(!Addresses::public_only().allows(ip("127.0.0.1")));
    }
}
