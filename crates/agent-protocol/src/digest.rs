//! Canonical digests for the values this protocol persists.
//!
//! FNV-1a, written out rather than taken from `std::hash`: `DefaultHasher` is
//! explicitly allowed to change between Rust releases, and these digests are
//! persisted and compared across builds. Every use here is drift detection —
//! "is this the same declaration it was before?" — never security, so a
//! non-cryptographic hash is the right size of hammer.
//!
//! One digest, one place: a manifest's `schema_hash`, a tool's
//! `registration_version`, and a resolved profile's `config_hash` all answer
//! the same shape of question, and a second implementation would be a second
//! answer waiting to disagree.

use serde_json::Value;

/// The FNV-1a 64-bit digest of `bytes`, as lowercase hex.
pub fn fnv1a_hex(bytes: &[u8]) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// The digest of a JSON value's canonical serialization.
///
/// `serde_json::Value` maps are ordered, so this serialization is canonical:
/// a value built in one order and a value built in another produce the same
/// bytes, and therefore the same digest.
pub fn json(value: &Value) -> String {
    // A `serde_json::Value` is plain data; serializing it cannot fail. If that
    // ever stops being true, reading the failure as empty bytes would make two
    // different declarations digest alike — the one thing this function exists
    // to prevent — so the impossible case is made loud instead of silent.
    let bytes = serde_json::to_vec(value).expect("serde_json serializes a Value without error");
    fnv1a_hex(&bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_digest_is_a_function_of_the_bytes() {
        assert_eq!(fnv1a_hex(b""), "cbf29ce484222325");
        assert_eq!(fnv1a_hex(b""), fnv1a_hex(&[]));
        assert_ne!(fnv1a_hex(b"a"), fnv1a_hex(b"b"));
        assert_eq!(fnv1a_hex(b"agent").len(), 16);
    }

    #[test]
    fn key_order_does_not_move_the_digest() {
        // The whole point: two spellings of the same object are the same
        // declaration, and a digest that disagreed would make them differ.
        let one = json!({ "b": 2, "a": 1 });
        let other = json!({ "a": 1, "b": 2 });
        assert_eq!(json(&one), json(&other));
        assert_ne!(json(&one), json(&json!({ "a": 1, "b": 3 })));
    }
}
