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
    #[error("'{0}' is not in the configured network allowlist")]
    NotAllowed(String),
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

/// One entry of a configured allowlist: a site, not a page.
///
/// A path in an entry would read as if one page were authorized and the rest
/// forbidden, and nothing here can enforce that — a redirect is allowed to
/// change the path freely, and a pattern language for paths is a language to
/// get wrong. The origin is what both HTTP and this guard can compare exactly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Origin {
    scheme: &'static str,
    host: String,
    port: u16,
}

impl Origin {
    /// The canonical spelling: lowercased host, and the port carried only
    /// when the scheme's default would not say the same thing.
    pub fn as_str(&self) -> String {
        let default = match self.scheme {
            "http" => 80,
            _ => 443,
        };
        if self.port == default {
            format!("{}://{}", self.scheme, self.host)
        } else {
            format!("{}://{}:{}", self.scheme, self.host, self.port)
        }
    }

    /// Whether `url` is this origin: same scheme, same host, same port.
    fn matches(&self, url: &Url) -> bool {
        url.scheme() == self.scheme
            && url.host_str() == Some(self.host.as_str())
            && url.port_or_known_default() == Some(self.port)
    }
}

/// Why a configured entry is not an origin this build will reach.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OriginError {
    #[error("'{0}' is not an absolute URL; write the origin, e.g. https://example.com")]
    NotAUrl(String),
    #[error("'{raw}' uses scheme '{scheme}'; only http and https are reachable")]
    Scheme { raw: String, scheme: String },
    #[error("'{0}' carries credentials; an allowlist entry names a site, not a user")]
    Credentials(String),
    #[error("'{0}' is not an origin; write scheme, host and port only, e.g. https://example.com")]
    NotAnOrigin(String),
    #[error("'{0}' contains a wildcard; name each origin explicitly")]
    Wildcard(String),
    #[error("'{0}' names no host")]
    NoHost(String),
    #[error("'{0}' is an address on this machine or a private network, never a remote site")]
    NotPublic(String),
}

fn origin(raw: &str, allow_private: bool) -> Result<Origin, OriginError> {
    // Checked before parsing so that a typo'd wildcard is named as one rather
    // than as an unparseable URL.
    if raw.contains('*') {
        return Err(OriginError::Wildcard(raw.to_string()));
    }
    let url = Url::parse(raw).map_err(|_| OriginError::NotAUrl(raw.to_string()))?;
    let scheme = match url.scheme() {
        "http" => "http",
        "https" => "https",
        other => {
            return Err(OriginError::Scheme { raw: raw.to_string(), scheme: other.to_string() });
        }
    };
    if !url.username().is_empty() || url.password().is_some() {
        return Err(OriginError::Credentials(raw.to_string()));
    }
    if url.path() != "/" || url.query().is_some() || url.fragment().is_some() {
        return Err(OriginError::NotAnOrigin(raw.to_string()));
    }
    let host = url.host_str().ok_or_else(|| OriginError::NoHost(raw.to_string()))?.to_string();
    let port = url.port_or_known_default().ok_or_else(|| OriginError::NoHost(raw.to_string()))?;
    // Defense in depth: the address guard would refuse these at request time,
    // but a configuration that names one is a mistake worth reporting when the
    // run starts, not when the first call fails.
    if !allow_private && literal_address(&host).is_some_and(|address| !is_public(address)) {
        return Err(OriginError::NotPublic(raw.to_string()));
    }
    Ok(Origin { scheme, host, port })
}

/// The address a host names when it is written as one, IPv6 brackets included.
fn literal_address(host: &str) -> Option<IpAddr> {
    let bare = host.strip_prefix('[').and_then(|host| host.strip_suffix(']')).unwrap_or(host);
    bare.parse().ok()
}

/// The remote origins a run may reach.
///
/// Empty is the shipped posture: no site is reachable until a person names it
/// in the user layer. An allowlist is checked before DNS and again on every
/// redirect hop, so a page cannot send the request somewhere the list does not
/// name.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NetworkAllowlist {
    origins: Vec<Origin>,
    /// Fixture switch: a test server's origin is not known until it binds, and
    /// a loopback address is one an assembled run must never name. Named so a
    /// reader of the wiring can see which one a run was given.
    any_origin: bool,
}

impl NetworkAllowlist {
    /// No reachable origin. The default, and what an absent section means.
    pub fn deny_all() -> Self {
        Self::default()
    }

    /// Read the user layer's entries.
    pub fn from_origins(entries: &[String]) -> Result<Self, OriginError> {
        Self::build(entries, false)
    }

    /// The fixture variant: loopback origins are accepted so a test server can
    /// be named. Never used by the assembled binary.
    pub fn from_origins_allowing_loopback(entries: &[String]) -> Result<Self, OriginError> {
        Self::build(entries, true)
    }

    /// The fixture variant that accepts any origin, for a test whose subject is
    /// an address or a redirect rather than the list itself.
    pub fn any() -> Self {
        Self { origins: Vec::new(), any_origin: true }
    }

    fn build(entries: &[String], allow_private: bool) -> Result<Self, OriginError> {
        let mut origins = Vec::with_capacity(entries.len());
        let mut seen = std::collections::BTreeSet::new();
        for entry in entries {
            let origin = origin(entry, allow_private)?;
            // Two spellings of one origin are one entry, so an exact-match
            // list can never grow a second answer for the same site.
            if seen.insert(format!("{}://{}:{}", origin.scheme, origin.host, origin.port)) {
                origins.push(origin);
            }
        }
        Ok(Self { origins, any_origin: false })
    }

    /// Whether a request to `url` is one this run authorized.
    pub fn allows(&self, url: &Url) -> bool {
        self.any_origin || self.origins.iter().any(|origin| origin.matches(url))
    }

    /// The entries, canonically spelled, for a report or a doctor line.
    pub fn origins(&self) -> Vec<String> {
        if self.any_origin {
            return vec!["*".to_string()];
        }
        self.origins.iter().map(Origin::as_str).collect()
    }
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
    fn an_allowlist_entry_is_an_origin_and_nothing_more() {
        let accepted = NetworkAllowlist::from_origins(&[
            "https://api.github.com".to_string(),
            "https://docs.rs/".to_string(),
            "http://example.com:8080".to_string(),
            "https://example.com:443".to_string(),
            "HTTPS://API.GitHub.com".to_string(),
        ])
        .expect("origins");
        assert_eq!(
            accepted.origins(),
            vec![
                "https://api.github.com".to_string(),
                "https://docs.rs".to_string(),
                "http://example.com:8080".to_string(),
                "https://example.com".to_string(),
            ],
            "the spelling is canonical and two spellings of one origin are one entry"
        );
    }

    #[test]
    fn an_entry_that_names_more_than_an_origin_is_refused() {
        let refused = |entry: &str| {
            NetworkAllowlist::from_origins(&[entry.to_string()])
                .expect_err("an entry this guard cannot compare exactly must not parse")
                .to_string()
        };
        assert!(refused("api.github.com").contains("not an absolute URL"));
        assert!(refused("ftp://example.com").contains("only http and https"));
        assert!(refused("https://user:pw@example.com").contains("credentials"));
        assert!(refused("https://example.com/repos").contains("not an origin"));
        assert!(refused("https://example.com/?q=1").contains("not an origin"));
        assert!(refused("https://*.example.com").contains("wildcard"));
    }

    #[test]
    fn a_private_or_loopback_address_is_never_allowed_even_if_named() {
        // Naming one is a configuration mistake, not a way to reach it: the
        // address guard would refuse the request anyway, and a run that
        // started with a target it can never use is a run that lied.
        for entry in ["http://127.0.0.1:8080", "http://[::1]:8080", "http://169.254.169.254"] {
            let error = NetworkAllowlist::from_origins(&[entry.to_string()])
                .expect_err("a private origin is never a remote site");
            assert!(error.to_string().contains("private network"), "{entry}: {error}");
        }

        // The fixture constructor is the one way to name a loopback origin,
        // and it is named for that.
        let fixture =
            NetworkAllowlist::from_origins_allowing_loopback(
                &["http://127.0.0.1:8080".to_string()],
            )
            .expect("a fixture may name its test server");
        assert!(fixture.allows(&Url::parse("http://127.0.0.1:8080/ok").unwrap()));
    }

    #[test]
    fn an_allowlist_matches_scheme_host_and_port_exactly() {
        let allowlist = NetworkAllowlist::from_origins(&["https://docs.rs".to_string()]).unwrap();
        let allows = |url: &str| allowlist.allows(&Url::parse(url).unwrap());

        assert!(allows("https://docs.rs/serde/latest"), "the list is per site, not per page");
        assert!(!allows("http://docs.rs/"), "a downgrade to cleartext is another origin");
        assert!(!allows("https://docs.rs:8443/"), "a different port is another origin");
        assert!(!allows("https://mirror.docs.rs/"), "a subdomain is another origin");
        assert!(!allowlist.allows(&Url::parse("https://elsewhere.example/").unwrap()));
    }

    #[test]
    fn an_absent_allowlist_reaches_nothing() {
        let empty = NetworkAllowlist::deny_all();
        assert!(!empty.allows(&Url::parse("https://docs.rs/").unwrap()));
        assert!(empty.origins().is_empty());
    }

    #[test]
    fn the_permissive_policy_still_parses_urls() {
        assert!(matches!(parse("not a url"), Err(GuardError::Scheme(_))));
        assert!(Addresses::any().allows(ip("127.0.0.1")));
        assert!(!Addresses::public_only().allows(ip("127.0.0.1")));
    }
}
