//! agent-store-secret: environment-variable secret backend.
//!
//! Secrets are exposed by reference only. The default backend resolves a name
//! only if it is on an explicit allowlist, so the host environment is never
//! passed through wholesale. The lookup is injectable for tests, since mutating
//! process env vars is `unsafe` in the 2024 edition.

#![deny(unsafe_code)]

use std::collections::HashSet;

use agent_ports::secret_provider::{SecretProvider, SecretProviderError};
use agent_protocol::permissions::SecretRef;

/// A name -> value lookup, injectable so tests avoid mutating process env vars.
type Lookup = dyn Fn(&str) -> Option<String> + Send + Sync;

/// A secret backend that resolves allowlisted environment variables.
pub struct EnvSecretProvider {
    allowlist: HashSet<String>,
    lookup: Box<Lookup>,
}

impl EnvSecretProvider {
    /// Resolve only the named environment variables, via `std::env::var`.
    pub fn new(allowlist: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self::with_lookup(allowlist, |name| std::env::var(name).ok())
    }

    /// As [`EnvSecretProvider::new`], but with an injectable lookup function.
    pub fn with_lookup<F>(allowlist: impl IntoIterator<Item = impl Into<String>>, lookup: F) -> Self
    where
        F: Fn(&str) -> Option<String> + Send + Sync + 'static,
    {
        Self {
            allowlist: allowlist.into_iter().map(Into::into).collect(),
            lookup: Box::new(lookup),
        }
    }
}

impl SecretProvider for EnvSecretProvider {
    fn resolve(&self, secret: &SecretRef) -> Result<String, SecretProviderError> {
        if secret.source != "env" || !self.allowlist.contains(&secret.name) {
            return Err(SecretProviderError::NotFound {
                provider: secret.source.clone(),
                name: secret.name.clone(),
            });
        }
        (self.lookup)(&secret.name).ok_or_else(|| SecretProviderError::NotFound {
            provider: secret.source.clone(),
            name: secret.name.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider() -> EnvSecretProvider {
        EnvSecretProvider::with_lookup(["ALLOWED_SECRET"], |name| {
            if name == "ALLOWED_SECRET" { Some("s3cret-value".to_string()) } else { None }
        })
    }

    fn secret(name: &str) -> SecretRef {
        SecretRef { source: "env".to_string(), name: name.to_string() }
    }

    #[test]
    fn allowlisted_secret_resolves() {
        assert_eq!(provider().resolve(&secret("ALLOWED_SECRET")).unwrap(), "s3cret-value");
    }

    #[test]
    fn non_allowlisted_secret_is_not_found() {
        assert!(matches!(
            provider().resolve(&secret("NOT_ALLOWED")),
            Err(SecretProviderError::NotFound { .. })
        ));
    }

    #[test]
    fn non_env_source_is_not_found() {
        let s = SecretRef { source: "vault".to_string(), name: "ALLOWED_SECRET".to_string() };
        assert!(matches!(provider().resolve(&s), Err(SecretProviderError::NotFound { .. })));
    }

    #[test]
    fn allowlisted_but_missing_is_not_found() {
        // "OTHER_SECRET" is not in the lookup's map, but that's still a clean
        // NotFound rather than a panic.
        let provider = EnvSecretProvider::with_lookup(["OTHER_SECRET"], |_| None);
        assert!(matches!(
            provider.resolve(&secret("OTHER_SECRET")),
            Err(SecretProviderError::NotFound { .. })
        ));
    }
}
