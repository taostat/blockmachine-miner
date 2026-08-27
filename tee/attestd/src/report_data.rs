//! Compute the 64-byte `report_data` value bound into the TDX quote.
//!
//! Per `blockmachine_playground/docs/tee-proxy-miners.md`:
//! `report_data = SHA-512(pubkey || caller_nonce)`.
//!
//! TDX `report_data` is exactly 64 bytes; SHA-512 fits naturally. The
//! registry's verifier recomputes the same hash and verifies it against
//! the value embedded in the quote. The scheme is byte-for-byte
//! identical to gm-miner's — this module is a direct port.

use sha2::{Digest, Sha512};

/// Compute `report_data = SHA-512(pubkey_bytes || nonce_bytes)`. Both
/// sides are decoded from base64 by callers; this helper takes raw
/// bytes so the encoding dance is not re-implemented per call site.
#[must_use]
pub fn compute_report_data(pubkey_bytes: &[u8], nonce_bytes: &[u8]) -> [u8; 64] {
    let mut hasher = Sha512::new();
    hasher.update(pubkey_bytes);
    hasher.update(nonce_bytes);
    hasher.finalize().into()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
    use base64::Engine;

    #[test]
    fn report_data_is_64_bytes() {
        let out = compute_report_data(&[1u8; 32], &[2u8; 32]);
        assert_eq!(out.len(), 64);
    }

    #[test]
    fn report_data_changes_with_nonce() {
        let pk = [7u8; 32];
        let a = compute_report_data(&pk, b"nonce-a");
        let b = compute_report_data(&pk, b"nonce-b");
        assert_ne!(a, b);
    }

    #[test]
    fn report_data_changes_with_pubkey() {
        let nonce = [11u8; 32];
        let a = compute_report_data(&[1u8; 32], &nonce);
        let b = compute_report_data(&[2u8; 32], &nonce);
        assert_ne!(a, b);
    }

    #[test]
    fn report_data_matches_fixed_test_vector() {
        // Pin the binding algorithm against an externally computed
        // vector (Python: hashlib.sha512(bytes([1]*32)+bytes([2]*16)))
        // so a swapped hasher, truncation, or reordered update is
        // caught even if the crate-internal recomputation drifts too.
        let got = compute_report_data(&[0x01u8; 32], &[0x02u8; 16]);
        let expected = "a43add4790d579ce34f458b422bbb7a8cdd8e97a3e0774f4ce461807b968e67c\
                        932be2a9fbc95b757d5c80aee21f363c730e9227a089cfb866ea5ea92a6ae147";
        assert_eq!(hex::encode(got), expected);
    }

    #[test]
    fn report_data_matches_independent_sha512() {
        // Pin the binding algorithm by recomputing it independently with
        // the same crate. Catches drift if the helper ever changes
        // (e.g. someone swaps the hasher update order).
        let pubkey = [0x42u8; 32];
        let nonce = [0xAAu8; 32];
        let got = compute_report_data(&pubkey, &nonce);
        let mut h = Sha512::new();
        h.update(pubkey);
        h.update(nonce);
        let expected: [u8; 64] = h.finalize().into();
        assert_eq!(got, expected);
    }

    #[test]
    fn report_data_base64_roundtrip() {
        // Pin the wire encoding: base64 STANDARD (not base64url). The
        // 64-byte output encodes to exactly 88 base64 chars.
        let out = compute_report_data(&[5u8; 32], &[6u8; 32]);
        let encoded = BASE64_STANDARD.encode(out);
        assert_eq!(encoded.len(), 88);
        let decoded = BASE64_STANDARD.decode(&encoded).unwrap();
        assert_eq!(decoded.as_slice(), &out);
    }
}
