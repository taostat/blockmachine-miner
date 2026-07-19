//! `BlockMachine` TEE proxy-miner attestation server library.
//!
//! Serves `GET /attestation/info` with a fresh Intel TDX quote fetched
//! from the dstack guest agent, plus the latest in-enclave method-check
//! results. Runs as a small HTTP server alongside Envoy inside the
//! miner's TEE container; Envoy routes `/attestation/*` paths to it.
//!
//! The wire contract is `blockmachine_playground/docs/tee-proxy-miners.md`
//! — the registry's attestation checker verifies the quote, the
//! `report_data` binding, the `(compose_hash, os_image_hash)` allowlist,
//! and the attested `method_check` capability claims.
//!
//! This crate is a structural port of gm-miner's `attestd`, adapted for
//! the JSON-RPC proxy-miner trust model: the upstream is a hardcoded
//! per-(chain, provider) RPC provider rather than an LLM API, and the
//! new method-checker proves the provider actually serves the required
//! methods (archive depth, proofs, traces).

pub mod info;
pub mod keypair;
pub mod method_check;
pub mod provider;
pub mod ratls;
pub mod redact;
pub mod report_data;
pub mod secret;
pub mod signed_claims;
pub mod upstream;

pub use info::{attestation_info, AppState, AttestationInfoQuery};
pub use keypair::SigningKeypair;
pub use method_check::{
    manifest_hash, CapabilitiesWire, CapabilityFlag, Manifest, MethodCheckReport,
    MethodCheckResult, MethodChecker, ProbeKind, ETH_MANIFEST_JSON,
};
pub use provider::{
    AttestationError, AttestationInfo, AttestationProvider, DstackAttestationProvider, TcbStatus,
};
pub use ratls::{
    cert_der_sha256_hex, provision as provision_ratls, read_cert_der_sha256, CertError, RatlsError,
    RatlsPaths, DEFAULT_RATLS_CERT_PATH, DEFAULT_RATLS_KEY_PATH,
};
pub use redact::{reject_marker_api_key, Redactor, REDACTION_MARKER};
pub use report_data::compute_report_data;
pub use secret::{validate_node_secret, validate_optional_node_secret, NodeSecretError};
pub use signed_claims::{
    canonical_json_bytes, method_check_digest, signed_claims_bytes, SignedClaims,
};
pub use upstream::{encode_api_key, resolve_upstream, ProviderUpstream};
