//! ed25519 keypair bootstrap, TEE-bound in production.
//!
//! The miner's attestation server holds one ed25519 keypair per
//! container instance. The pubkey is published in `GET /attestation/info`
//! (the `node_pubkey` field) and bound into the TDX quote's
//! `report_data`; the registry verifies that binding and, on
//! re-attestation, requires the pubkey to equal the stored `tee_pubkey`.
//!
//! In production the secret is derived from a dstack-KMS sealed key via
//! the guest agent's `get_key` endpoint (`/var/run/dstack.sock`). The
//! dstack-KMS releases the key only inside an attested CVM and only for
//! this `app_id` + `compose_hash`, so a container replacement (crash,
//! redeploy of the same image) regenerates the same key bytes.
//!
//! The signing key is held inside an `Arc<SigningKey>` and never
//! exposed as raw bytes through the public API. This mirrors gm-miner's
//! `attestd/src/keypair.rs`.

use std::sync::Arc;

use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine;
use dstack_sdk::dstack_client::DstackClient;
use ed25519_dalek::{Signer, SigningKey};
use sha2::{Digest, Sha256};

use crate::provider::AttestationError;

/// dstack `get_key` namespace for the BM TEE attestation keypair.
const KEY_PATH: &str = "bm-tee-attestation";

/// ed25519 keypair bootstrapped at attestation-server startup. Cheaply
/// cloneable — the secret is wrapped in an `Arc` so the bootstrap and
/// the request handler share a single allocation.
#[derive(Clone)]
pub struct SigningKeypair {
    inner: Arc<SigningKey>,
}

impl std::fmt::Debug for SigningKeypair {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print the private key. The pubkey is safe.
        f.debug_struct("SigningKeypair")
            .field("pubkey_b64", &self.public_b64())
            .finish()
    }
}

impl SigningKeypair {
    /// Bootstrap the keypair from the dstack guest agent.
    ///
    /// Opens a connection to the dstack guest agent socket (default
    /// `/var/run/dstack.sock`, overridable via the `dstack_socket`
    /// parameter or the `DSTACK_SIMULATOR_ENDPOINT` env var). `get_key`
    /// returns a hex-encoded secret bound to `(app_id, compose_hash,
    /// path, purpose)`; the miner uses the fixed `bm-tee-attestation`
    /// path with the chain slug as the purpose.
    ///
    /// # Errors
    ///
    /// Returns [`AttestationError::DstackKey`] when the guest agent is
    /// unreachable, returns an error, hands back a key that is not
    /// valid hex, or returns fewer than 32 secret bytes. The caller
    /// fails fast — the container exits and the runtime restarts it.
    pub async fn bootstrap(
        chain: &str,
        dstack_socket: Option<&str>,
    ) -> Result<Self, AttestationError> {
        let client = DstackClient::new(dstack_socket);
        let response = client
            .get_key(Some(KEY_PATH.to_owned()), Some(chain.to_owned()))
            .await
            .map_err(|e| AttestationError::DstackKey(e.to_string()))?;
        let key_bytes = response
            .decode_key()
            .map_err(|e| AttestationError::DstackKey(format!("decode hex: {e}")))?;
        if key_bytes.len() < 32 {
            return Err(AttestationError::DstackKey(format!(
                "expected at least 32 secret bytes from dstack, got {}",
                key_bytes.len()
            )));
        }
        let secret: [u8; 32] = key_bytes[..32]
            .try_into()
            .map_err(|_| AttestationError::DstackKey("slice into 32 bytes".to_owned()))?;
        Ok(Self::from_secret_bytes(secret))
    }

    /// Construct a deterministic keypair from `seed_label` without a
    /// dstack call. Used only by tests and by `cargo build` paths that
    /// have no TDX socket; never reached on a deployed miner.
    #[must_use]
    pub fn deterministic(seed_label: &str) -> Self {
        let mut h = Sha256::new();
        h.update(b"bm-tee-attestd-test");
        h.update(seed_label.as_bytes());
        let secret: [u8; 32] = h.finalize().into();
        Self::from_secret_bytes(secret)
    }

    /// Construct from explicit 32 secret bytes.
    #[must_use]
    pub fn from_secret_bytes(secret: [u8; 32]) -> Self {
        Self {
            inner: Arc::new(SigningKey::from_bytes(&secret)),
        }
    }

    /// Raw 32-byte ed25519 public key.
    #[must_use]
    pub fn public_bytes(&self) -> [u8; 32] {
        self.inner.verifying_key().to_bytes()
    }

    /// Base64 (STANDARD, not URL-safe) encoded public key. This is the
    /// `node_pubkey` field on the wire — the registry reads that key
    /// name verbatim and binds it into `report_data` verification.
    #[must_use]
    pub fn public_b64(&self) -> String {
        BASE64_STANDARD.encode(self.public_bytes())
    }

    /// ed25519 signature over `message`, raw 64 bytes. The `node_pubkey`
    /// bound into the quote's `report_data` is the verifying key, so a
    /// signature by this keypair is cryptographically bound to the TDX
    /// quote — this is what the registry uses to trust `signed_claims`.
    #[must_use]
    pub fn sign_bytes(&self, message: &[u8]) -> [u8; 64] {
        self.inner.sign(message).to_bytes()
    }

    /// Base64 (STANDARD) ed25519 signature over `message`. The wire
    /// `claims_signature` field — the registry verifies it against the
    /// quote-bound `node_pubkey` over the canonical `signed_claims`.
    #[must_use]
    pub fn sign_b64(&self, message: &[u8]) -> String {
        BASE64_STANDARD.encode(self.sign_bytes(message))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn deterministic_keypair_is_stable_per_label() {
        let a = SigningKeypair::deterministic("eth");
        let b = SigningKeypair::deterministic("eth");
        assert_eq!(a.public_b64(), b.public_b64());
        assert_eq!(a.public_bytes(), b.public_bytes());
    }

    #[test]
    fn deterministic_keypair_distinct_per_label() {
        let a = SigningKeypair::deterministic("eth");
        let b = SigningKeypair::deterministic("bsc");
        assert_ne!(a.public_b64(), b.public_b64());
    }

    #[test]
    fn pubkey_is_32_bytes_b64() {
        let kp = SigningKeypair::deterministic("eth");
        let decoded = BASE64_STANDARD.decode(kp.public_b64()).unwrap();
        assert_eq!(decoded.len(), 32);
    }

    #[test]
    fn signature_verifies_with_pubkey_and_fails_on_tamper() {
        use ed25519_dalek::{Signature, Verifier, VerifyingKey};
        let kp = SigningKeypair::deterministic("eth");
        let msg = b"canonical-signed-claims-bytes";
        let sig_bytes = kp.sign_bytes(msg);
        assert_eq!(sig_bytes.len(), 64);
        // b64 form decodes back to the raw signature.
        let decoded = BASE64_STANDARD.decode(kp.sign_b64(msg)).unwrap();
        assert_eq!(decoded.as_slice(), &sig_bytes);
        // The published node_pubkey verifies it.
        let vk = VerifyingKey::from_bytes(&kp.public_bytes()).unwrap();
        let sig = Signature::from_bytes(&sig_bytes);
        assert!(vk.verify(msg, &sig).is_ok());
        // A single tampered byte in the message fails verification.
        let mut tampered = msg.to_vec();
        tampered[0] ^= 0x01;
        assert!(vk.verify(&tampered, &sig).is_err());
    }
}
