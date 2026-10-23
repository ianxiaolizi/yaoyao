//! Fidelity tests for the artifact-store double.
//!
//! `fakes.rs` covers the event store's bookkeeping and the doctor check's
//! results. This file is about the other thing a double can get wrong: the name
//! it gives a blob. The port says that name is the 64-character lowercase hex
//! SHA-256 of the content, so the fake is held to the same sentence the
//! filesystem store is — and a name that is not that is what the model is shown
//! when the kernel offloads tool output through a fake.

use agent_ports::artifact_store::{ArtifactStore, ArtifactStoreError, is_blob_name};
use agent_protocol::permissions::ArtifactRef;
use agent_testkit::InMemoryArtifactStore;

fn reference(hash: &str) -> ArtifactRef {
    ArtifactRef { hash: hash.to_string(), mime_type: "text/plain".to_string(), size: 0 }
}

/// The known answer is the only assertion here that can say *SHA-256*.
///
/// Round-tripping, same-content-same-name, different-content-differs and the
/// 64-hex shape all hold for any hex function of the bytes: a 64-bit hash
/// expanded to 64 characters would pass every other test in this file. The same
/// constant is asserted in `agent-store-artifact`'s own suite, deliberately —
/// it is a published property of the algorithm, not a fact about either
/// implementation.
#[test]
fn the_fake_names_a_blob_the_way_the_port_says() {
    let store = InMemoryArtifactStore::default();
    let artifact = store.put(b"hello world", "text/plain").unwrap();

    assert_eq!(artifact.hash, "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9");
    assert!(is_blob_name(&artifact.hash), "the port's own shape check has to accept it");
    assert_eq!(artifact.locator(), format!("artifact://sha256/{}", artifact.hash));
}

#[test]
fn the_fake_is_content_addressed() {
    let store = InMemoryArtifactStore::default();
    assert_eq!(
        store.put(b"same", "text/plain").unwrap().hash,
        store.put(b"same", "text/plain").unwrap().hash
    );
    assert_ne!(
        store.put(b"one", "text/plain").unwrap().hash,
        store.put(b"two", "text/plain").unwrap().hash
    );
}

#[test]
fn the_fake_round_trips_what_it_stored() {
    let store = InMemoryArtifactStore::default();
    let artifact = store.put(b"the bytes", "text/plain").unwrap();

    assert_eq!(artifact.size, 9);
    assert_eq!(artifact.mime_type, "text/plain");
    assert_eq!(store.get(&artifact).unwrap(), b"the bytes");
}

/// A reference that is not a blob name is refused *before* it is looked up,
/// which is what the real store does before turning one into a path.
#[test]
fn the_fake_refuses_a_reference_that_is_not_a_blob_name() {
    let store = InMemoryArtifactStore::default();

    // Uppercase is a blob name in shape only: `put` writes lowercase, so the
    // same content would otherwise have two names.
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

/// Bytes that are there but are not that content are not the artifact the name
/// claims. The real store meets this case through a torn write or a hand-edited
/// file; `damage` is how the in-memory map is handed one.
#[test]
fn the_fake_refuses_bytes_that_do_not_hash_to_the_name() {
    let store = InMemoryArtifactStore::default();
    let artifact = store.put(b"the original bytes", "text/plain").unwrap();
    store.damage(&artifact.hash, b"tampered");

    match store.get(&artifact) {
        Err(ArtifactStoreError::ContentMismatch { expected, actual }) => {
            assert_eq!(expected, artifact.hash);
            assert_ne!(actual, artifact.hash);
        }
        other => panic!("expected a content mismatch, got {other:?}"),
    }
}

/// `put` has no way to repair a blob it already has, and does not try: the real
/// store writes only when the path is not there yet, so a damaged blob stays
/// damaged until something notices. Minting a fresh name here would be a
/// difference a test built on the fake could not see.
#[test]
fn the_fake_does_not_rewrite_a_blob_it_already_has() {
    let store = InMemoryArtifactStore::default();
    let artifact = store.put(b"the original bytes", "text/plain").unwrap();
    store.damage(&artifact.hash, b"tampered");

    store.put(b"the original bytes", "text/plain").unwrap();

    assert!(matches!(store.get(&artifact), Err(ArtifactStoreError::ContentMismatch { .. })));
}
