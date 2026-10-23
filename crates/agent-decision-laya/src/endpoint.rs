//! The endpoint a sidecar is reached at, and the one rule that bounds it.
//!
//! Laya's server binds `0.0.0.0` out of the box, which means the documented way
//! to start it and the safe way to reach it are different things. The design
//! says a fast-decision sidecar is local. So this is not a preference
//! the caller can express: an endpoint that is not loopback has no constructor,
//! and a configuration naming one fails when the participant is built rather
//! than when a turn is already running.
//!
//! The check is here, in the thing that will open the socket, rather than in
//! the configuration parser. Both would refuse; only one of them guarantees the
//! invariant, because only one of them owns the connection.

use url::{Host, Url};

/// The path one question is asked at, on a sidecar named as an origin.
pub const SYSTEMONE_PATH: &str = "/v1/systemone";

/// Why an endpoint was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EndpointError {
    #[error("'{0}' is not a URL")]
    NotAUrl(String),
    #[error("'{0}' has scheme '{1}', which is neither http nor https")]
    UnsupportedScheme(String, String),
    #[error(
        "sidecar endpoint '{0}' is not loopback; a decision sidecar must be reached over the \
         loopback interface (start `laya-serve` with `LAYA_HOST=127.0.0.1`)"
    )]
    NotLoopback(String),
    #[error("sidecar endpoint '{0}' names no host")]
    NoHost(String),
}

/// A validated, loopback-only endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayaEndpoint {
    url: Url,
}

impl LayaEndpoint {
    /// Parse and validate an endpoint.
    pub fn parse(raw: &str) -> Result<Self, EndpointError> {
        let url = Url::parse(raw).map_err(|_| EndpointError::NotAUrl(raw.to_string()))?;

        match url.scheme() {
            "http" | "https" => {}
            other => {
                return Err(EndpointError::UnsupportedScheme(raw.to_string(), other.to_string()));
            }
        }

        match url.host() {
            Some(Host::Ipv4(address)) if address.is_loopback() => {}
            Some(Host::Ipv6(address)) if address.is_loopback() => {}
            // `localhost` is the name of the loopback interface. Anything else
            // — including a name that resolves to it today — is refused: what a
            // name resolves to is not a property of this configuration.
            Some(Host::Domain(name)) if name.eq_ignore_ascii_case("localhost") => {}
            Some(_) => return Err(EndpointError::NotLoopback(raw.to_string())),
            None => return Err(EndpointError::NoHost(raw.to_string())),
        }

        Ok(Self { url })
    }

    /// The endpoint as it was configured. What a message about this sidecar
    /// should name.
    pub fn url(&self) -> &str {
        self.url.as_str()
    }

    /// The URL one question is posted to.
    ///
    /// A sidecar named as an origin (`http://127.0.0.1:8231`) gets the API path
    /// appended. That is how the server is documented and how a person writes
    /// it, and the first live run of this adapter is the reason the rule is
    /// spelled out here: with the path taken straight off a bare origin the
    /// request went to `/`, where a real `laya-serve` answers 404 while the
    /// fixture — which ignored the path — answered happily.
    ///
    /// A URL that names a path is used as written, so a sidecar behind a prefix
    /// and a caller who spelled the API path themselves both work. Any query is
    /// kept, which is what tests use to select the fixture's behaviour.
    pub fn request_url(&self) -> String {
        let mut url = self.url.clone();
        if url.path() == "/" {
            url.set_path(SYSTEMONE_PATH);
        }
        url.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_loopback_interface_is_accepted() {
        for raw in [
            "http://127.0.0.1:8000",
            "http://127.0.0.1:8000/v1/systemone",
            "http://127.1.2.3:9000", // the whole 127/8 is loopback
            "http://localhost:8000",
            "http://LocalHost:8000",
            "http://[::1]:8000",
            "https://127.0.0.1:8443",
        ] {
            assert!(LayaEndpoint::parse(raw).is_ok(), "{raw} should be accepted");
        }
    }

    #[test]
    fn anything_else_is_refused() {
        // The default `laya-serve` bind address is the first of these.
        for raw in [
            "http://0.0.0.0:8000",
            "http://192.168.1.10:8000",
            "http://10.0.0.5:8000",
            "http://laya.internal:8000",
            "http://notlocalhost:8000",
            "http://[2001:db8::1]:8000",
        ] {
            assert!(
                matches!(LayaEndpoint::parse(raw), Err(EndpointError::NotLoopback(_))),
                "{raw} should be refused as non-loopback"
            );
        }
    }

    #[test]
    fn a_url_that_is_not_one_says_so() {
        assert_eq!(
            LayaEndpoint::parse("127.0.0.1:8000"),
            Err(EndpointError::NotAUrl("127.0.0.1:8000".to_string())),
            "a bare host:port is not a URL"
        );
        assert!(matches!(
            LayaEndpoint::parse("ftp://127.0.0.1"),
            Err(EndpointError::UnsupportedScheme(_, _))
        ));
    }

    #[test]
    fn the_endpoint_keeps_its_path_and_port() {
        let endpoint = LayaEndpoint::parse("http://127.0.0.1:8231/v1/systemone").unwrap();
        assert_eq!(endpoint.url(), "http://127.0.0.1:8231/v1/systemone");
    }

    #[test]
    fn an_origin_is_asked_at_the_api_path() {
        // What a person writes in a configuration file, and what the server
        // actually serves — the two differ by exactly this path.
        let origin = LayaEndpoint::parse("http://127.0.0.1:8231").unwrap();
        assert_eq!(origin.url(), "http://127.0.0.1:8231/");
        assert_eq!(origin.request_url(), "http://127.0.0.1:8231/v1/systemone");

        // A path, once named, is the caller's: this is how a sidecar behind a
        // prefix, or the fully spelled URL, stays reachable.
        let spelled = LayaEndpoint::parse("http://127.0.0.1:8231/v1/systemone").unwrap();
        assert_eq!(spelled.request_url(), "http://127.0.0.1:8231/v1/systemone");

        let prefixed = LayaEndpoint::parse("http://127.0.0.1:8231/laya/v1/systemone").unwrap();
        assert_eq!(prefixed.request_url(), "http://127.0.0.1:8231/laya/v1/systemone");

        // The query survives the rewrite, which is what the fixture's behaviour
        // switch rides on.
        let switched = LayaEndpoint::parse("http://127.0.0.1:8231/?mode=slow").unwrap();
        assert_eq!(switched.request_url(), "http://127.0.0.1:8231/v1/systemone?mode=slow");
    }
}
