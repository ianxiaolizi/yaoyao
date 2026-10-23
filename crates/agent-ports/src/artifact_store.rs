//! ArtifactStore trait — content-addressed blob storage.

use agent_protocol::permissions::ArtifactRef;

/// Port for storing tool outputs, logs, diffs, and other large results.
pub trait ArtifactStore: Send + Sync {
    /// Store bytes and return a content-addressed reference.
    fn put(&self, bytes: &[u8], mime_type: &str) -> Result<ArtifactRef, ArtifactStoreError>;

    /// Retrieve bytes by reference.
    ///
    /// `hash` is the only lookup key: it names the content itself. `size` and
    /// `mime_type` are metadata the writer recorded when it called [`put`];
    /// a reader holding only a locator cannot know them and may leave them
    /// empty/zero. Implementations must not require them to match.
    ///
    /// The consumer is `agent artifact get`, which fetches a locator the log
    /// handed to a person or a program. That is the other half of [`put`]:
    /// without it an offloaded tool result would be a locator no reader can
    /// follow.
    ///
    /// [`put`]: ArtifactStore::put
    fn get(&self, artifact: &ArtifactRef) -> Result<Vec<u8>, ArtifactStoreError>;
}

/// Whether `hash` is exactly the name a content-addressed store gives a blob:
/// 64 lowercase hex characters, the SHA-256 of its content.
///
/// It lives beside the trait because both sides of the seam need the same
/// answer. A store must refuse a reference that is not a blob name *before*
/// turning it into a path — a hash like `../escape` would otherwise take a read
/// out of the root. A test double must produce names of the shape the real
/// store produces, or it is a double of a type that does not exist. Two copies
/// of "what a blob name looks like" would eventually be two answers.
///
/// Uppercase is refused rather than folded: one content has one name, and
/// accepting both spellings would give it two.
pub fn is_blob_name(hash: &str) -> bool {
    hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Errors from an ArtifactStore implementation.
#[derive(Debug, thiserror::Error)]
pub enum ArtifactStoreError {
    #[error("artifact not found: {0}")]
    NotFound(String),
    /// The reference does not name a blob at all. A blob's name is the
    /// 64-character lowercase hex SHA-256 of its content; anything else is
    /// refused rather than turned into a path (YA-09). [`is_blob_name`] is
    /// that shape.
    #[error("not a blob name: {0}")]
    InvalidHash(String),
    /// Something is at the blob's path, but its bytes do not hash to the name
    /// — a damaged or truncated file, or one that was not written by `put`.
    /// Distinct from `InvalidHash`: there the reference is wrong, here the
    /// content is.
    #[error("artifact {expected} does not match its content, which hashes to {actual}")]
    ContentMismatch { expected: String, actual: String },
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}
