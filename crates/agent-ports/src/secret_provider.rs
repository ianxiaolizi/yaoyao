//! SecretProvider trait — resolves named secret references to plaintext.

use agent_protocol::permissions::SecretRef;

/// Port for resolving secrets by reference. The resolved plaintext is injected
/// at execution-lease creation time and never persisted.
pub trait SecretProvider: Send + Sync {
    /// Whether this reference could be resolved, without producing its value.
    ///
    /// Asked before a person is shown the call: a reference this run cannot
    /// resolve is a call that could never run, and nobody should be asked about
    /// one. The answer must not be obtained by resolving — a value that exists
    /// before the approval exists is a value that can leak before the decision
    /// was made.
    ///
    /// The default answer is `Ok(())`: "ask the person, and find out at
    /// dispatch". A provider that cannot tell without producing the value takes
    /// that default rather than resolving early; the cost is an approval spent
    /// on a call that then fails closed, and the alternative is the secret in
    /// memory before the decision.
    fn has(&self, _secret: &SecretRef) -> Result<(), SecretProviderError> {
        Ok(())
    }

    /// The value behind a reference.
    ///
    /// Called only after the decision to run exists, and once per occurrence:
    /// the caller substitutes it into the arguments, scrubs it out of whatever
    /// comes back, and drops it with the call.
    fn resolve(&self, secret: &SecretRef) -> Result<String, SecretProviderError>;
}

/// Errors from a SecretProvider implementation.
#[derive(Debug, thiserror::Error)]
pub enum SecretProviderError {
    #[error("secret not found: {provider}/{name}")]
    NotFound { provider: String, name: String },
    /// A provider-side failure. The message reaches a caller and can reach a
    /// log, so an implementation must never put the secret's value in it.
    #[error("secret provider error: {0}")]
    Failed(String),
}
