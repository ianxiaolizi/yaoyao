//! agent-store-secret: environment-variable secret backend.
//!
//! Secrets are exposed by reference only. The default backend resolves a name
//! only if it is on an explicit allowlist, so the host environment is never
//! passed through wholesale. The lookup is injectable for tests, since mutating
//! process env vars is `unsafe` in the 2024 edition.

#![deny(unsafe_code)]

use std::collections::HashSet;
use std::sync::Arc;

use agent_ports::secret_provider::{SecretProvider, SecretProviderError};
use agent_protocol::permissions::SecretRef;

/// A name -> value lookup, injectable so tests avoid mutating process env vars.
type Lookup = dyn Fn(&str) -> Option<String> + Send + Sync;

/// A name -> "is it set" probe: the existence question, without the value.
type Probe = dyn Fn(&str) -> bool + Send + Sync;

/// A secret backend that resolves allowlisted environment variables.
pub struct EnvSecretProvider {
    allowlist: HashSet<String>,
    lookup: Arc<Lookup>,
    probe: Box<Probe>,
}

impl EnvSecretProvider {
    /// Resolve only the named environment variables, via `std::env::var`.
    ///
    /// The existence probe reads `var_os`, which answers "is it set" without
    /// producing the value: the pre-approval check must not pull the plaintext
    /// into this process any earlier than dispatch does.
    pub fn new(allowlist: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            allowlist: allowlist.into_iter().map(Into::into).collect(),
            lookup: Arc::new(|name| std::env::var(name).ok()),
            probe: Box::new(|name| std::env::var_os(name).is_some()),
        }
    }

    /// As [`EnvSecretProvider::new`], but with an injectable lookup function.
    ///
    /// The probe goes through the same function and drops what it returns: an
    /// injected lookup has one channel, so a fixture cannot answer "is it set"
    /// any other way. The real provider does not pay that cost.
    pub fn with_lookup<F>(allowlist: impl IntoIterator<Item = impl Into<String>>, lookup: F) -> Self
    where
        F: Fn(&str) -> Option<String> + Send + Sync + 'static,
    {
        let lookup: Arc<Lookup> = Arc::new(lookup);
        let probe = Arc::clone(&lookup);
        Self {
            allowlist: allowlist.into_iter().map(Into::into).collect(),
            lookup,
            probe: Box::new(move |name| probe(name).is_some()),
        }
    }

    fn allowlisted(&self, secret: &SecretRef) -> bool {
        secret.source == "env" && self.allowlist.contains(&secret.name)
    }

    fn not_found(secret: &SecretRef) -> SecretProviderError {
        SecretProviderError::NotFound { provider: secret.source.clone(), name: secret.name.clone() }
    }
}

impl SecretProvider for EnvSecretProvider {
    fn has(&self, secret: &SecretRef) -> Result<(), SecretProviderError> {
        if !self.allowlisted(secret) || !(self.probe)(&secret.name) {
            return Err(Self::not_found(secret));
        }
        Ok(())
    }

    fn resolve(&self, secret: &SecretRef) -> Result<String, SecretProviderError> {
        if !self.allowlisted(secret) {
            return Err(Self::not_found(secret));
        }
        (self.lookup)(&secret.name).ok_or_else(|| Self::not_found(secret))
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

    #[test]
    fn a_reference_this_run_cannot_resolve_is_refused_by_the_probe() {
        // The question asked before a person is shown the call. It must answer
        // the same yes/no the resolution would, so the earlier refusal is not a
        // promise the later one breaks.
        let provider = provider();
        assert!(provider.has(&secret("ALLOWED_SECRET")).is_ok());
        for name in ["NOT_ALLOWED", "NEVER_SET"] {
            assert!(
                matches!(provider.has(&secret(name)), Err(SecretProviderError::NotFound { .. })),
                "{name} must be refused before an approval is raised"
            );
        }
        let vault = SecretRef { source: "vault".to_string(), name: "ALLOWED_SECRET".to_string() };
        assert!(matches!(provider.has(&vault), Err(SecretProviderError::NotFound { .. })));
    }

    #[test]
    fn the_injected_lookup_probe_uses_the_one_channel_it_has() {
        // The real provider probes "is it set" with `var_os` and never
        // produces the value. An injected lookup has one channel, so its probe
        // resolves and drops what came back — a documented deviation from the
        // shipped provider, pinned here so it stays written down.
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = Arc::clone(&calls);
        let provider = EnvSecretProvider::with_lookup(["COUNTED"], move |_| {
            counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Some("value".to_string())
        });
        assert!(provider.has(&secret("COUNTED")).is_ok());
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }
}
