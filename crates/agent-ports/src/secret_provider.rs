//! SecretProvider trait — resolves named secret references to plaintext.

use agent_protocol::permissions::SecretRef;

/// Port for resolving secrets by reference. The resolved plaintext is injected
/// at execution-lease creation time and never persisted.
pub trait SecretProvider: Send + Sync {
    fn resolve(&self, secret: &SecretRef) -> Result<String, SecretProviderError>;
}

/// Errors from a SecretProvider implementation.
#[derive(Debug, thiserror::Error)]
pub enum SecretProviderError {
    #[error("secret not found: {provider}/{name}")]
    NotFound { provider: String, name: String },
    #[error("secret provider error: {0}")]
    Failed(String),
}
