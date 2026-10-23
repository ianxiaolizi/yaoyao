//! agent-store-artifact: a content-addressed blob store on the filesystem.
//!
//! Blobs are keyed by their SHA-256 hash and written atomically (temp file then
//! rename), so the same content always resolves to the same path and a partial
//! write never leaves a torn blob behind.
//!
//! The temp file's name is unique per call, not derived from the hash: two
//! writers of the same content would otherwise meet on one file, and opening
//! it to write would truncate what the first writer was still writing.
//!
//! Both ends of the name are checked. A reference must be a blob name before
//! it becomes a path — otherwise it is a way out of the root — and the bytes
//! found at that path must hash back to the name — otherwise the name is a
//! claim rather than a check.

#![deny(unsafe_code)]

use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use agent_ports::artifact_store::{ArtifactStore, ArtifactStoreError, is_blob_name};
use agent_protocol::permissions::ArtifactRef;
use sha2::{Digest, Sha256};

/// A content-addressed blob store under a single root directory.
pub struct FsArtifactStore {
    root: PathBuf,
}

impl FsArtifactStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn path_for(&self, hash: &str) -> PathBuf {
        self.root.join(format!("{hash}.blob"))
    }

    /// The name this `put` writes under before renaming the blob into place.
    ///
    /// Unique per call. The hash alone would not be: two writers of the same
    /// content would then share one temp file, `File::create` would truncate
    /// what the first writer was still writing, and the rename that followed
    /// would carry a file the other writer still held open. The name carries
    /// the process id and a per-process counter — not a clock and not a random
    /// source, neither of which this crate has a port for.
    fn temp_path(&self, hash: &str) -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let seq = NEXT.fetch_add(1, Ordering::Relaxed);
        self.root.join(format!(".{hash}.{}.{seq}.tmp", std::process::id()))
    }
}

impl ArtifactStore for FsArtifactStore {
    fn put(&self, bytes: &[u8], mime_type: &str) -> Result<ArtifactRef, ArtifactStoreError> {
        let hash = hex_encode(&Sha256::digest(bytes));
        let path = self.path_for(&hash);
        if !path.exists() {
            fs::create_dir_all(&self.root).map_err(ArtifactStoreError::Io)?;
            let tmp = self.temp_path(&hash);
            // `create_new`, not `create`: a put must never truncate a file it
            // did not just create. The unique name makes a collision
            // impossible in the first place, and if one happened anyway an
            // error is the honest answer — quietly reusing another writer's
            // temp is the defect this replaces.
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&tmp)
                .map_err(ArtifactStoreError::Io)?;
            file.write_all(bytes).map_err(ArtifactStoreError::Io)?;
            file.sync_all().map_err(ArtifactStoreError::Io)?;
            drop(file);
            if let Err(error) = fs::rename(&tmp, &path) {
                // The blob did not land. Take our temp back out rather than
                // leave a file behind, and report whichever of the two failed
                // — a failed clean-up must not hide the rename that caused it.
                return match fs::remove_file(&tmp) {
                    Ok(()) => Err(ArtifactStoreError::Io(error)),
                    Err(cleanup) => Err(ArtifactStoreError::Io(std::io::Error::other(format!(
                        "{error} (and {tmp:?} could not be removed: {cleanup})"
                    )))),
                };
            }
        }
        Ok(ArtifactRef { hash, mime_type: mime_type.to_string(), size: bytes.len() as u64 })
    }

    fn get(&self, artifact: &ArtifactRef) -> Result<Vec<u8>, ArtifactStoreError> {
        if !is_blob_name(&artifact.hash) {
            return Err(ArtifactStoreError::InvalidHash(artifact.hash.clone()));
        }
        let bytes = fs::read(self.path_for(&artifact.hash)).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                ArtifactStoreError::NotFound(artifact.hash.clone())
            } else {
                ArtifactStoreError::Io(e)
            }
        })?;

        // The name *is* the content's digest, so the bytes have to earn it.
        // Anything else at this path — a torn write, a damaged file, one put
        // there by hand — is not the artifact the reference names, and
        // handing it back would make the name a claim rather than a check.
        let actual = hex_encode(&Sha256::digest(&bytes));
        if actual != artifact.hash {
            return Err(ArtifactStoreError::ContentMismatch {
                expected: artifact.hash.clone(),
                actual,
            });
        }
        Ok(bytes)
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (FsArtifactStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        (FsArtifactStore::new(dir.path().to_path_buf()), dir)
    }

    #[test]
    fn put_and_get_round_trip() {
        let (store, _) = store();
        let artifact = store.put(b"hello world", "text/plain").unwrap();
        assert_eq!(artifact.size, 11);
        assert_eq!(artifact.mime_type, "text/plain");
        assert_eq!(store.get(&artifact).unwrap(), b"hello world");
    }

    #[test]
    fn same_content_is_content_addressed() {
        let (store, dir) = store();
        let a = store.put(b"same", "text/plain").unwrap();
        let b = store.put(b"same", "text/plain").unwrap();
        assert_eq!(a.hash, b.hash);
        // Idempotent: only one blob file is written.
        let count = std::fs::read_dir(dir.path()).unwrap().count();
        assert_eq!(count, 1);
    }

    #[test]
    fn different_content_hashes_differ() {
        let (store, _) = store();
        let a = store.put(b"one", "text/plain").unwrap();
        let b = store.put(b"two", "text/plain").unwrap();
        assert_ne!(a.hash, b.hash);
    }

    /// The one thing the rest of this suite cannot say: that the name is
    /// *SHA-256*. Round-tripping, same-content-same-name, different-content-
    /// differs and the 64-hex shape all hold for any hex function of the bytes,
    /// so without a known answer those four would rest on nothing.
    #[test]
    fn the_name_is_the_sha256_of_the_content() {
        let (store, _) = store();
        let artifact = store.put(b"hello world", "text/plain").unwrap();
        assert_eq!(
            artifact.hash,
            "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
        );
        // The locator spells the same claim out loud, so it is the same fact
        // and is asserted with it.
        assert_eq!(artifact.locator(), format!("artifact://sha256/{}", artifact.hash));
    }

    /// The name has to be *well formed* for this to be about absence: a hash
    /// that is not a blob name is refused for that reason instead (this test
    /// used `"deadbeef"`, which stopped reaching `NotFound` once the shape
    /// check landed — the assertion was right, the fixture was short).
    #[test]
    fn get_unknown_hash_is_not_found() {
        let (store, _) = store();
        let missing = ArtifactRef {
            hash: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".to_string(),
            mime_type: "text/plain".to_string(),
            size: 0,
        };
        assert!(matches!(store.get(&missing), Err(ArtifactStoreError::NotFound(_))));
    }

    /// A reference is a claim about where the bytes are, and `path_for` joins
    /// it onto the root — so a hash that is not a blob name is a way out of
    /// the root. The file planted beside the root is what the old code would
    /// have handed back.
    #[test]
    fn a_reference_that_is_not_a_blob_name_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("blobs");
        let store = FsArtifactStore::new(root);
        std::fs::write(dir.path().join("escape.blob"), b"beside the root, not in it").unwrap();

        let reference = |hash: &str| ArtifactRef {
            hash: hash.to_string(),
            mime_type: "text/plain".to_string(),
            size: 0,
        };

        // Uppercase is a blob name in shape only: `put` writes lowercase, so
        // accepting `AB…` would give one content two names.
        let uppercase = "A".repeat(64);
        let not_hex = "g".repeat(64);
        for hash in ["../escape", "", "deadbeef", uppercase.as_str(), not_hex.as_str()] {
            match store.get(&reference(hash)) {
                Err(ArtifactStoreError::InvalidHash(refused)) => assert_eq!(refused, hash),
                Err(other) => panic!("{hash:?} refused for the wrong reason: {other}"),
                Ok(bytes) => panic!("{hash:?} was looked up and read {} bytes", bytes.len()),
            }
        }
    }

    /// The other half of "the name is the content's digest": bytes that are
    /// there, but are not that content, are not the artifact.
    #[test]
    fn a_blob_whose_content_does_not_match_its_name_is_refused() {
        let (store, dir) = store();
        let artifact = store.put(b"the original bytes", "text/plain").unwrap();

        // Damage it behind the store's back, as a torn or corrupted file
        // would be.
        std::fs::write(dir.path().join(format!("{}.blob", artifact.hash)), b"tampered").unwrap();

        match store.get(&artifact) {
            Err(ArtifactStoreError::ContentMismatch { expected, actual }) => {
                assert_eq!(expected, artifact.hash);
                assert_ne!(actual, artifact.hash);
            }
            other => panic!("expected a content mismatch, got {other:?}"),
        }
    }

    /// The temp name is the whole of YA-61, so it is pinned directly rather
    /// than through a race: two writers of one content must never be handed
    /// the same file to write into.
    #[test]
    fn every_writer_gets_its_own_temporary_name() {
        let (store, _) = store();
        let first = store.temp_path("abc");
        let second = store.temp_path("abc");

        assert_ne!(first, second, "two writers of one content would share a temp file");
        assert!(first.to_string_lossy().ends_with(".tmp"));
        // And neither name is the blob itself — the blob is only ever reached
        // by the rename.
        assert_ne!(first, store.path_for("abc"));
        assert_ne!(second, store.path_for("abc"));
    }

    /// The behaviour the temp name exists to protect: writers of the same
    /// content, all at once, all succeed — one blob, no leftovers.
    #[test]
    fn concurrent_puts_of_the_same_content_all_succeed() {
        let (store, dir) = store();
        let store = std::sync::Arc::new(store);
        let payload = b"the same bytes, from several writers".to_vec();

        let writers: Vec<_> = (0..8)
            .map(|_| {
                let store = store.clone();
                let payload = payload.clone();
                std::thread::spawn(move || store.put(&payload, "text/plain"))
            })
            .collect();

        let mut hashes = std::collections::BTreeSet::new();
        for writer in writers {
            let artifact = writer
                .join()
                .unwrap()
                .expect("a writer of content another writer had already stored failed");
            hashes.insert(artifact.hash);
        }
        assert_eq!(hashes.len(), 1, "one content, one hash");

        // Exactly one file: each writer's rename moved its own temp into
        // place, so no `.tmp` is left for a reader to trip over.
        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        let hash = hashes.into_iter().next().unwrap();
        assert_eq!(entries, vec![format!("{hash}.blob")]);

        let artifact = ArtifactRef { hash, mime_type: "text/plain".to_string(), size: 0 };
        assert_eq!(store.get(&artifact).unwrap(), payload);
    }
}
