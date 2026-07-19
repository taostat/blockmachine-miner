//! Signed attestation claims (schema v3).
//!
//! Wire contract: `blockmachine_playground/docs/tee-proxy-miners.md`
//! (the "Claim binding" section). In addition to binding `node_pubkey`
//! into the TDX quote's `report_data`, the enclave signs a small
//! `signed_claims` object with that same key (`claims_signature`). The
//! object covers the fields the registry must trust *without* a full
//! dstack-attest in Python:
//!
//! * `compose_hash` — the measured compose (also derived from the event
//!   log; the signature is defence-in-depth binding it to the key).
//! * `chain` / `provider` — the rendered compose literals.
//! * `ratls_cert_sha256` — the DER SHA-256 of the RA-TLS certificate the
//!   enclave actually serves on the data plane. This binds the serving
//!   channel: the registry captures the served leaf cert, fingerprints
//!   it, and requires equality, so a relay proxy cannot substitute its
//!   own cert behind a good quote.
//! * `method_check_digest` — SHA-256 of the canonical `method_check`
//!   object returned in the same response, so the registry knows the
//!   signed capability claim matches the body it received.
//! * `issued_at` / `nonce` — freshness + replay binding.
//!
//! ## Canonicalization (MUST match the registry byte-for-byte)
//!
//! Both `signed_claims` and the `method_check` object are canonicalized
//! as JSON with **lexicographically sorted keys**, **compact separators**
//! (`,` and `:` with no surrounding whitespace), UTF-8, no trailing
//! whitespace. [`canonical_json_bytes`] produces exactly those bytes for
//! any [`serde_json::Value`]; it is used for both the signature input and
//! the `method_check` digest so the two never drift.

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::method_check::MethodCheckReport;

/// The exact key set of `signed_claims`, in wire (struct) order. Field
/// order here is irrelevant to the signature: [`canonical_json_bytes`]
/// re-sorts keys. Serialized into the response as the `signed_claims`
/// object and, canonicalized, as the `claims_signature` input.
#[derive(Debug, Clone, Serialize)]
pub struct SignedClaims {
    pub compose_hash: String,
    pub chain: String,
    pub provider: String,
    pub ratls_cert_sha256: String,
    pub method_check_digest: String,
    pub issued_at: DateTime<Utc>,
    pub nonce: String,
}

/// Recursively rebuild `value` with every object's keys sorted
/// lexicographically. Arrays keep their order; scalars are copied. This
/// is what makes the serialization order deterministic regardless of
/// whether `serde_json` was built with the `preserve_order` feature.
fn canonicalize(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort_unstable();
            let mut out = Map::new();
            for key in keys {
                // Present because `key` came from `map`'s own keys.
                if let Some(child) = map.get(key) {
                    out.insert(key.clone(), canonicalize(child));
                }
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(canonicalize).collect()),
        other => other.clone(),
    }
}

/// Canonical JSON bytes of `value`: lexicographically sorted keys,
/// compact separators, UTF-8, no trailing whitespace. `serde_json`'s
/// default (compact) formatter supplies the separators and correct
/// string escaping; [`canonicalize`] supplies the key ordering.
///
/// # Errors
///
/// Returns the `serde_json` error if serialization fails. For a
/// [`Value`] (string keys, no custom `Serialize`) this cannot happen in
/// practice, but the error is propagated rather than swallowed so a
/// future non-`Value` caller cannot silently sign empty bytes.
pub fn canonical_json_bytes(value: &Value) -> Result<Vec<u8>, serde_json::Error> {
    serde_json::to_vec(&canonicalize(value))
}

/// SHA-256 (lowercase hex) of the canonical `method_check` object — the
/// `signed_claims.method_check_digest` field. The registry recomputes
/// SHA-256 over the canonical `method_check` it receives and requires
/// equality, so this MUST be computed from the exact same object the
/// response serializes.
///
/// # Errors
///
/// Propagates a `serde_json` serialization error (see
/// [`canonical_json_bytes`]).
pub fn method_check_digest(report: &MethodCheckReport) -> Result<String, serde_json::Error> {
    let value = serde_json::to_value(report)?;
    let canonical = canonical_json_bytes(&value)?;
    Ok(hex::encode(Sha256::digest(&canonical)))
}

/// Canonical bytes of a [`SignedClaims`] object — the message signed by
/// `node_pubkey` to produce `claims_signature`.
///
/// # Errors
///
/// Propagates a `serde_json` serialization error (see
/// [`canonical_json_bytes`]).
pub fn signed_claims_bytes(claims: &SignedClaims) -> Result<Vec<u8>, serde_json::Error> {
    let value = serde_json::to_value(claims)?;
    canonical_json_bytes(&value)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::keypair::SigningKeypair;
    use chrono::TimeZone;
    use ed25519_dalek::{Signature, Verifier, VerifyingKey};

    fn fixed_claims() -> SignedClaims {
        SignedClaims {
            compose_hash: "aa".repeat(32),
            chain: "eth".to_owned(),
            provider: "drpc".to_owned(),
            ratls_cert_sha256: "bb".repeat(32),
            method_check_digest: "cc".repeat(32),
            // A fixed instant so the pinned-bytes vector is stable.
            issued_at: Utc.with_ymd_and_hms(2026, 7, 18, 12, 34, 56).unwrap(),
            nonce: "AAAAAAAAAAAAAAAAAAAAAA==".to_owned(),
        }
    }

    #[test]
    fn canonical_signed_claims_bytes_are_pinned() {
        // Pin the EXACT canonical bytes for a known input so any drift in
        // key ordering, separators, or field naming is caught. The
        // registry verifies against these bytes byte-for-byte.
        let bytes = signed_claims_bytes(&fixed_claims()).unwrap();
        let expected = concat!(
            "{",
            "\"chain\":\"eth\",",
            "\"compose_hash\":\"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\",",
            "\"issued_at\":\"2026-07-18T12:34:56Z\",",
            "\"method_check_digest\":\"cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc\",",
            "\"nonce\":\"AAAAAAAAAAAAAAAAAAAAAA==\",",
            "\"provider\":\"drpc\",",
            "\"ratls_cert_sha256\":\"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\"",
            "}"
        );
        assert_eq!(String::from_utf8(bytes).unwrap(), expected);
    }

    #[test]
    fn canonical_bytes_have_no_whitespace_and_sorted_keys() {
        let value = serde_json::json!({ "b": 1, "a": { "d": 2, "c": 3 } });
        let bytes = canonical_json_bytes(&value).unwrap();
        assert_eq!(
            String::from_utf8(bytes).unwrap(),
            "{\"a\":{\"c\":3,\"d\":2},\"b\":1}"
        );
    }

    #[test]
    fn signature_verifies_and_fails_on_tampered_claim() {
        let kp = SigningKeypair::deterministic("eth");
        let claims = fixed_claims();
        let bytes = signed_claims_bytes(&claims).unwrap();
        let sig_bytes = kp.sign_bytes(&bytes);
        let vk = VerifyingKey::from_bytes(&kp.public_bytes()).unwrap();
        assert!(vk
            .verify(&bytes, &Signature::from_bytes(&sig_bytes))
            .is_ok());
        // Tamper a single claim field: the signature no longer verifies.
        let mut tampered = claims;
        tampered.provider = "not-drpc".to_owned();
        let tampered_bytes = signed_claims_bytes(&tampered).unwrap();
        assert!(vk
            .verify(&tampered_bytes, &Signature::from_bytes(&sig_bytes))
            .is_err());
    }

    #[test]
    fn method_check_digest_matches_manual_canonical_sha256() {
        let report = crate::provider::testing::synthetic_method_report();
        let digest = method_check_digest(&report).unwrap();
        // Recompute independently: to_value -> canonicalize -> sha256.
        let value = serde_json::to_value(&report).unwrap();
        let canonical = canonical_json_bytes(&value).unwrap();
        let expected = hex::encode(Sha256::digest(&canonical));
        assert_eq!(digest, expected);
        assert_eq!(digest.len(), 64);
    }
}
