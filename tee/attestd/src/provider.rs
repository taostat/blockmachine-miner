//! Attestation evidence provider.
//!
//! [`AttestationProvider`] is the seam between the `GET /attestation/info`
//! request handler and the dstack guest agent. The production
//! implementation, [`DstackAttestationProvider`], calls
//! `DstackClient::info()` once at startup to cache the static per-CVM
//! fields (`app_id`, `compose_hash`, `os_image_hash`, `instance_id`),
//! then asks the guest agent for a fresh quote bound to a caller nonce
//! on every request, and attaches the latest completed method-check
//! report.
//!
//! The wire struct [`AttestationInfo`] follows the `BlockMachine` TEE
//! proxy-miner contract (`blockmachine_playground/docs/tee-proxy-miners.md`):
//! the pubkey field is named `node_pubkey`, and the response carries the
//! `chain` / `provider` slugs (rendered compose literals) plus the new
//! `method_check` section.

use async_trait::async_trait;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine;
use chrono::{DateTime, Utc};
use dstack_sdk::dstack_client::{DstackClient, EventLog};
use thiserror::Error;

use crate::keypair::SigningKeypair;
use crate::method_check::{MethodCheckReport, SharedReport};
use crate::report_data::compute_report_data;
use crate::signed_claims::{method_check_digest, signed_claims_bytes, SignedClaims};

/// Wire-shaped attestation evidence. Serialized as the
/// `GET /attestation/info` response body. Field shape matches the
/// contract doc's "Wire contract" section.
#[derive(Debug, Clone, serde::Serialize)]
pub struct AttestationInfo {
    pub schema_version: u32,
    /// The node attestation keypair's public key, base64. The registry
    /// reads this exact field name; it is the key bound into the
    /// quote's `report_data`.
    pub node_pubkey: String,
    pub tee_platform: String,
    pub app_id: String,
    pub compose_hash: String,
    pub os_image_hash: String,
    pub instance_id: String,
    /// Chain slug (`eth`), a rendered literal in the measured compose.
    pub chain: String,
    /// Provider slug (`drpc`), a rendered literal in the
    /// measured compose.
    pub provider: String,
    pub tcb_info: TcbInfoWire,
    pub quote: String,
    /// The dstack event log accompanying this quote. The registry folds
    /// the IMR-3 entries into the RTMR3 hash-chain, requires the result
    /// to equal the quote's `rt_mr3`, then derives `compose_hash` from
    /// the now-trusted log — so the measurement is bound to the quote,
    /// never taken from the JSON `compose_hash` field (which is
    /// informational and must merely match the derived value).
    pub event_log: Vec<EventLog>,
    pub report_data: String,
    pub nonce: String,
    pub issued_at: DateTime<Utc>,
    /// Hex SHA-256 of the DER encoding of the RA-TLS certificate Envoy
    /// serves on the data-plane listener. Computed once at startup from
    /// the exact PEM `bm-tee-ratls` provisioned; the registry captures
    /// the served leaf cert and requires its DER SHA-256 to equal the
    /// signed `signed_claims.ratls_cert_sha256` — the serving-channel
    /// binding.
    pub ratls_cert_sha256: String,
    /// The signed claim set covering `compose_hash`, `chain`, `provider`,
    /// the serving-cert fingerprint, a digest of `method_check`,
    /// `issued_at`, and `nonce`. Signed by `node_pubkey`
    /// (`claims_signature`), so all of it is cryptographically bound to
    /// the TDX quote.
    pub signed_claims: SignedClaims,
    /// Base64 ed25519 signature by `node_pubkey` over the canonical bytes
    /// of `signed_claims`.
    pub claims_signature: String,
    /// Latest completed in-enclave method-check run. `signed_claims`
    /// carries the SHA-256 of this exact object.
    pub method_check: MethodCheckReport,
}

/// Wire shape of the `tcb_info` object inside [`AttestationInfo`].
#[derive(Debug, Clone, serde::Serialize)]
pub struct TcbInfoWire {
    pub status: TcbStatus,
    pub tcb_date: DateTime<Utc>,
    pub advisory_ids: Vec<String>,
}

/// TCB status enum. Informational on this response — the registry's
/// `dcap-qvl` verification decides the real TCB status from the quote's
/// collateral.
#[derive(Debug, Clone, Copy, serde::Serialize)]
pub enum TcbStatus {
    UpToDate,
    #[serde(rename = "SWHardeningNeeded")]
    SwHardeningNeeded,
    ConfigurationNeeded,
    #[serde(rename = "ConfigurationAndSWHardeningNeeded")]
    ConfigurationAndSwHardeningNeeded,
    OutOfDate,
    OutOfDateConfigurationNeeded,
    Revoked,
}

/// Errors emitted by the attestation path.
#[derive(Debug, Error)]
pub enum AttestationError {
    #[error("dstack get_quote failed: {0}")]
    DstackQuote(String),
    #[error("dstack get_key failed: {0}")]
    DstackKey(String),
    #[error("dstack info failed: {0}")]
    DstackInfo(String),
    #[error("read RA-TLS serving cert: {0}")]
    RatlsCert(String),
    #[error("canonicalize signed claims: {0}")]
    Canonicalize(String),
    #[error("quote generation at capacity — shedding load")]
    QuoteBusy,
}

/// The attestation provider trait. Implementations are `Send + Sync`
/// because the axum app state holds an `Arc<dyn AttestationProvider>`
/// shared across concurrent requests.
#[async_trait]
pub trait AttestationProvider: Send + Sync {
    /// Build the full [`AttestationInfo`] bound to `nonce`:
    ///
    /// 1. Compute `report_data = SHA-512(pubkey || nonce)`.
    /// 2. Ask dstack for a fresh quote with that `report_data`.
    /// 3. Attach the latest completed method-check report.
    /// 4. Stamp `issued_at = Utc::now()`.
    ///
    /// `nonce` is the raw bytes — the handler base64-decodes the wire
    /// `?nonce=...` first. The returned `nonce` field is the base64 echo.
    ///
    /// # Errors
    ///
    /// Returns an [`AttestationError`] when the dstack guest agent
    /// cannot produce a quote (unreachable, error response, or a quote
    /// that does not decode as hex).
    async fn build_info(&self, nonce: &[u8]) -> Result<AttestationInfo, AttestationError>;
}

/// Production provider: calls the dstack guest agent for each quote.
pub struct DstackAttestationProvider {
    chain: String,
    provider: String,
    keypair: SigningKeypair,
    /// Cached per-CVM static fields, fetched once at startup via
    /// `DstackClient::info()`.
    static_info: StaticAttestationFields,
    /// Latest completed method-check report, shared with the periodic
    /// re-check task.
    method_report: SharedReport,
    /// Hex SHA-256 of the DER RA-TLS serving cert. Computed once at
    /// bootstrap — the cert is stable for the life of the process, so
    /// this need not be recomputed per request (only the nonce,
    /// `issued_at`, and the method-check change per response).
    ratls_cert_sha256: String,
    /// Path to the dstack socket. `None` means the default
    /// `/var/run/dstack.sock`; tests inject a simulator endpoint.
    dstack_socket: Option<String>,
    /// Bounds concurrent in-flight dstack `get_quote` operations.
    /// `/attestation/info` is unauthenticated and each request triggers
    /// an expensive quote; HTTP/2 multiplexing can defeat Envoy's
    /// connection ceiling, so without this a flood could spawn unbounded
    /// concurrent quote ops and starve attestd / the guest agent. A
    /// request that finds the small permit pool exhausted is shed fast
    /// (`QuoteBusy` → 429), never queued.
    quote_semaphore: tokio::sync::Semaphore,
}

// The dstack guest agent can hang; bound quote requests in-process instead of relying on a proxy timeout.
const QUOTE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Maximum concurrent in-flight `get_quote` operations. Small by design:
/// a legitimate registry sweep issues one probe at a time, so a handful
/// of permits covers real concurrency while a flood is shed immediately
/// rather than fanning out unbounded quote work onto the guest agent.
const QUOTE_CONCURRENCY_LIMIT: usize = 4;

/// Try to take a quote permit without waiting. Returns [`AttestationError::QuoteBusy`]
/// the instant the pool is exhausted, so a flood is shed fast instead of
/// queueing unbounded — the permit is released when the returned guard drops.
fn try_acquire_quote_permit(
    semaphore: &tokio::sync::Semaphore,
) -> Result<tokio::sync::SemaphorePermit<'_>, AttestationError> {
    semaphore
        .try_acquire()
        .map_err(|_| AttestationError::QuoteBusy)
}

/// Await `fut` under `timeout`, flattening both the timeout and the inner
/// error into a single message string. A fired timeout reports the bound so
/// the operator sees the request was cut off in-process, not by the agent.
async fn with_quote_timeout<T, E: std::fmt::Display>(
    timeout: std::time::Duration,
    fut: impl std::future::Future<Output = Result<T, E>>,
) -> Result<T, String> {
    match tokio::time::timeout(timeout, fut).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(e)) => Err(e.to_string()),
        Err(_elapsed) => Err(format!("get_quote timed out after {}s", timeout.as_secs())),
    }
}

#[derive(Debug, Clone)]
struct StaticAttestationFields {
    app_id: String,
    compose_hash: String,
    os_image_hash: String,
    instance_id: String,
}

impl DstackAttestationProvider {
    /// Boot the provider by fetching the dstack `info()` snapshot once.
    /// Subsequent `build_info` calls reuse this snapshot and only hit
    /// the quote endpoint per request.
    ///
    /// `ratls_cert_path` is the PEM path Envoy serves on the data-plane
    /// listener (`bm-tee-ratls`'s output). Its leaf DER SHA-256 is read
    /// here **fail-closed**: a missing/unreadable/undecodable cert aborts
    /// bootstrap (attestd exits), because without the exact served cert
    /// the `ratls_cert_sha256` binding cannot be produced.
    ///
    /// # Errors
    ///
    /// Returns [`AttestationError::DstackInfo`] when the dstack guest
    /// agent is unreachable or its `info()` call fails, and
    /// [`AttestationError::RatlsCert`] when the served cert cannot be
    /// read or fingerprinted.
    pub async fn bootstrap(
        chain: String,
        provider: String,
        keypair: SigningKeypair,
        method_report: SharedReport,
        ratls_cert_path: &std::path::Path,
        dstack_socket: Option<String>,
    ) -> Result<Self, AttestationError> {
        // Fail closed on the serving cert before anything else: the whole
        // point of the signed claim is to bind THIS served cert.
        let ratls_cert_sha256 = crate::ratls::read_cert_der_sha256(ratls_cert_path)
            .await
            .map_err(|e| AttestationError::RatlsCert(e.to_string()))?;
        let client = DstackClient::new(dstack_socket.as_deref());
        let info = client
            .info()
            .await
            .map_err(|e| AttestationError::DstackInfo(e.to_string()))?;
        // os_image_hash is empty when the OS image is not measured by
        // the KMS; the InfoResponse carries it on the top level and the
        // tcb_info both — prefer the top-level field, fall back to
        // tcb_info so a non-empty value is always used when present.
        let os_image_hash = if info.os_image_hash.is_empty() {
            info.tcb_info.os_image_hash.clone()
        } else {
            info.os_image_hash.clone()
        };
        let static_info = StaticAttestationFields {
            app_id: info.app_id,
            compose_hash: info.compose_hash,
            os_image_hash,
            instance_id: info.instance_id,
        };
        Ok(Self {
            chain,
            provider,
            keypair,
            static_info,
            method_report,
            ratls_cert_sha256,
            dstack_socket,
            quote_semaphore: tokio::sync::Semaphore::new(QUOTE_CONCURRENCY_LIMIT),
        })
    }
}

/// The per-response inputs to `signed_claims`. Grouped so the assembly
/// helper stays a two-argument call.
struct ClaimInputs<'a> {
    compose_hash: String,
    chain: String,
    provider: String,
    ratls_cert_sha256: String,
    method_check: &'a MethodCheckReport,
    issued_at: DateTime<Utc>,
    nonce_b64: String,
}

/// Assemble `signed_claims` + `claims_signature` for a response.
///
/// The `method_check_digest` is computed from the exact `method_check`
/// object the response serializes, so the registry's recomputed digest
/// matches. Signing the canonical bytes with `keypair` (the `node_pubkey`
/// bound into the quote) binds every claim to the TDX quote.
///
/// # Errors
///
/// Returns [`AttestationError::Canonicalize`] when the method-check or
/// the claims fail to serialize (see `signed_claims` canonicalization).
fn sign_claims(
    keypair: &SigningKeypair,
    inputs: ClaimInputs<'_>,
) -> Result<(SignedClaims, String), AttestationError> {
    let method_check_digest = method_check_digest(inputs.method_check)
        .map_err(|e| AttestationError::Canonicalize(e.to_string()))?;
    let claims = SignedClaims {
        compose_hash: inputs.compose_hash,
        chain: inputs.chain,
        provider: inputs.provider,
        ratls_cert_sha256: inputs.ratls_cert_sha256,
        method_check_digest,
        issued_at: inputs.issued_at,
        nonce: inputs.nonce_b64,
    };
    let bytes =
        signed_claims_bytes(&claims).map_err(|e| AttestationError::Canonicalize(e.to_string()))?;
    let signature = keypair.sign_b64(&bytes);
    Ok((claims, signature))
}

#[async_trait]
impl AttestationProvider for DstackAttestationProvider {
    async fn build_info(&self, nonce: &[u8]) -> Result<AttestationInfo, AttestationError> {
        let pubkey_bytes = self.keypair.public_bytes();
        let report_data = compute_report_data(&pubkey_bytes, nonce);
        // Shed fast if too many quotes are already in flight — held until
        // the quote is decoded, so the bound reflects real concurrency.
        let _quote_permit = try_acquire_quote_permit(&self.quote_semaphore)?;
        let client = DstackClient::new(self.dstack_socket.as_deref());
        let quote = with_quote_timeout(QUOTE_TIMEOUT, client.get_quote(report_data.to_vec()))
            .await
            .map_err(AttestationError::DstackQuote)?;
        let quote_bytes = quote
            .decode_quote()
            .map_err(|e| AttestationError::DstackQuote(format!("decode hex quote: {e}")))?;
        // The event log travels alongside the quote so the registry can
        // replay RTMR3 and derive compose_hash from the quote itself.
        let event_log = quote
            .decode_event_log()
            .map_err(|e| AttestationError::DstackQuote(format!("decode event log: {e}")))?;
        let method_check = self.method_report.read().await.clone();
        // One `issued_at` and one `nonce` echo, used for both the
        // top-level fields and the signed claim so they never disagree.
        let issued_at = Utc::now();
        let nonce_b64 = BASE64_STANDARD.encode(nonce);
        let (signed_claims, claims_signature) = sign_claims(
            &self.keypair,
            ClaimInputs {
                compose_hash: self.static_info.compose_hash.clone(),
                chain: self.chain.clone(),
                provider: self.provider.clone(),
                ratls_cert_sha256: self.ratls_cert_sha256.clone(),
                method_check: &method_check,
                issued_at,
                nonce_b64: nonce_b64.clone(),
            },
        )?;
        Ok(AttestationInfo {
            schema_version: 3,
            node_pubkey: self.keypair.public_b64(),
            tee_platform: "intel-tdx".to_owned(),
            app_id: self.static_info.app_id.clone(),
            compose_hash: self.static_info.compose_hash.clone(),
            os_image_hash: self.static_info.os_image_hash.clone(),
            instance_id: self.static_info.instance_id.clone(),
            chain: self.chain.clone(),
            provider: self.provider.clone(),
            // The registry decides the real TCB status from the quote's
            // collateral via dcap-qvl; this field is an informational
            // echo. Report `UpToDate` — a stale host surfaces as a
            // verification failure on the registry side regardless.
            tcb_info: TcbInfoWire {
                status: TcbStatus::UpToDate,
                tcb_date: Utc::now(),
                advisory_ids: Vec::new(),
            },
            quote: BASE64_STANDARD.encode(&quote_bytes),
            event_log,
            report_data: BASE64_STANDARD.encode(report_data),
            nonce: nonce_b64,
            issued_at,
            ratls_cert_sha256: self.ratls_cert_sha256.clone(),
            signed_claims,
            claims_signature,
            method_check,
        })
    }
}

#[cfg(test)]
pub(crate) mod testing {
    //! A deterministic provider for handler tests. Returns a synthetic
    //! quote so the wire shape and the `report_data` binding can be
    //! exercised without a TDX socket. Never used on a deployed miner.
    use super::{
        async_trait, AttestationError, AttestationInfo, AttestationProvider, EventLog, TcbInfoWire,
        TcbStatus, Utc, BASE64_STANDARD,
    };
    use crate::keypair::SigningKeypair;
    use crate::method_check::{CapabilitiesWire, MethodCheckReport, MethodCheckResult, ProbeKind};
    use crate::report_data::compute_report_data;
    use base64::Engine;

    /// A synthetic single-entry IMR-3 event log, shaped like dstack's.
    pub(crate) fn synthetic_event_log() -> Vec<EventLog> {
        vec![EventLog {
            imr: 3,
            event_type: 134_217_729,
            digest: "aa".repeat(48),
            event: "compose-hash".to_owned(),
            event_payload: "11".repeat(32),
        }]
    }

    pub(crate) struct DeterministicProvider {
        keypair: SigningKeypair,
        pub(crate) fail_quote: bool,
    }

    impl DeterministicProvider {
        pub(crate) fn new() -> Self {
            Self {
                keypair: SigningKeypair::deterministic("eth"),
                fail_quote: false,
            }
        }
    }

    pub(crate) fn synthetic_method_report() -> MethodCheckReport {
        MethodCheckReport {
            manifest_hash: "3".repeat(64),
            checked_at: Utc::now(),
            all_required_passed: true,
            capabilities: CapabilitiesWire {
                is_archive: true,
                serves_proofs: true,
                audit_compatible: false,
            },
            results: vec![MethodCheckResult {
                method: "eth_getBalance".to_owned(),
                probe: ProbeKind::ArchiveState,
                required: true,
                passed: true,
                latency_ms: 42,
                error: None,
            }],
        }
    }

    /// The DER SHA-256 of the test RA-TLS cert fixture, mirroring the
    /// value the real provider computes from the served cert PEM.
    pub(crate) const TEST_RATLS_CERT_SHA256: &str = crate::ratls::TEST_CERT_DER_SHA256;

    #[async_trait]
    impl AttestationProvider for DeterministicProvider {
        async fn build_info(&self, nonce: &[u8]) -> Result<AttestationInfo, AttestationError> {
            if self.fail_quote {
                return Err(AttestationError::DstackQuote(
                    "synthetic quote failure".to_owned(),
                ));
            }
            let pubkey_bytes = self.keypair.public_bytes();
            let report_data = compute_report_data(&pubkey_bytes, nonce);
            let mut quote_bytes = Vec::with_capacity(64 + 64);
            quote_bytes.extend_from_slice(
                b"BM-TEE-ATTESTD-TEST-QUOTE--not-a-real-tdx-quote--padding64bytes!",
            );
            quote_bytes.extend_from_slice(&report_data);
            let compose_hash = "1".repeat(64);
            let method_check = synthetic_method_report();
            let issued_at = Utc::now();
            let nonce_b64 = BASE64_STANDARD.encode(nonce);
            // Sign real claims so handler tests can verify the signature
            // against node_pubkey — the deterministic keypair is the same
            // that publishes node_pubkey here.
            let (signed_claims, claims_signature) = super::sign_claims(
                &self.keypair,
                super::ClaimInputs {
                    compose_hash: compose_hash.clone(),
                    chain: "eth".to_owned(),
                    provider: "drpc".to_owned(),
                    ratls_cert_sha256: TEST_RATLS_CERT_SHA256.to_owned(),
                    method_check: &method_check,
                    issued_at,
                    nonce_b64: nonce_b64.clone(),
                },
            )?;
            Ok(AttestationInfo {
                schema_version: 3,
                node_pubkey: self.keypair.public_b64(),
                tee_platform: "intel-tdx".to_owned(),
                app_id: "0".repeat(40),
                compose_hash,
                os_image_hash: "2".repeat(64),
                instance_id: "dstack-test-instance".to_owned(),
                chain: "eth".to_owned(),
                provider: "drpc".to_owned(),
                tcb_info: TcbInfoWire {
                    status: TcbStatus::OutOfDate,
                    tcb_date: Utc::now(),
                    advisory_ids: vec!["BM-TEST-NOT-A-REAL-ATTESTATION".to_owned()],
                },
                quote: BASE64_STANDARD.encode(&quote_bytes),
                event_log: synthetic_event_log(),
                report_data: BASE64_STANDARD.encode(report_data),
                nonce: nonce_b64,
                issued_at,
                ratls_cert_sha256: TEST_RATLS_CERT_SHA256.to_owned(),
                signed_claims,
                claims_signature,
                method_check,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::testing::DeterministicProvider;
    use super::*;
    use crate::report_data::compute_report_data;

    #[tokio::test]
    async fn build_info_emits_contract_shape() {
        let provider = DeterministicProvider::new();
        let nonce = b"test-nonce-32-bytes-padded--abcd";
        assert_eq!(nonce.len(), 32);
        let info = provider.build_info(nonce).await.unwrap();
        assert_eq!(info.schema_version, 3);
        assert_eq!(info.tee_platform, "intel-tdx");
        assert_eq!(info.chain, "eth");
        assert_eq!(info.provider, "drpc");
        assert_eq!(info.compose_hash.len(), 64);
        assert_eq!(info.os_image_hash.len(), 64);
        assert!(info.app_id.len() >= 40);
        // report_data is base64 of 64 bytes -> 88 chars.
        assert_eq!(info.report_data.len(), 88);
        let rd_bytes = BASE64_STANDARD.decode(&info.report_data).unwrap();
        assert_eq!(rd_bytes.len(), 64);
        // The event log is present so the registry can replay RTMR3.
        assert!(!info.event_log.is_empty());
        assert!(info.event_log.iter().any(|e| e.imr == 3));
        // The method_check section is present with the derived flags.
        assert!(info.method_check.all_required_passed);
        assert!(info.method_check.capabilities.is_archive);
    }

    #[tokio::test]
    async fn report_data_binds_pubkey_and_nonce() {
        let provider = DeterministicProvider::new();
        let nonce = b"another-32-byte-nonce-padding-ab";
        let info = provider.build_info(nonce).await.unwrap();
        let pubkey = BASE64_STANDARD.decode(&info.node_pubkey).unwrap();
        let expected = compute_report_data(&pubkey, nonce);
        let got = BASE64_STANDARD.decode(&info.report_data).unwrap();
        assert_eq!(got.as_slice(), &expected);
    }

    #[tokio::test]
    async fn signed_claims_verify_against_node_pubkey_and_bind_fields() {
        use crate::signed_claims::signed_claims_bytes;
        use ed25519_dalek::{Signature, Verifier, VerifyingKey};

        let provider = DeterministicProvider::new();
        let nonce = b"claims-nonce-32-bytes-padding-ok";
        let info = provider.build_info(nonce).await.unwrap();

        // The signature verifies against the published node_pubkey over
        // the canonical signed_claims bytes.
        let pubkey: [u8; 32] = BASE64_STANDARD
            .decode(&info.node_pubkey)
            .unwrap()
            .try_into()
            .unwrap();
        let vk = VerifyingKey::from_bytes(&pubkey).unwrap();
        let sig_bytes: [u8; 64] = BASE64_STANDARD
            .decode(&info.claims_signature)
            .unwrap()
            .try_into()
            .unwrap();
        let msg = signed_claims_bytes(&info.signed_claims).unwrap();
        assert!(vk.verify(&msg, &Signature::from_bytes(&sig_bytes)).is_ok());

        // The signed fields echo the top-level response fields.
        assert_eq!(info.signed_claims.compose_hash, info.compose_hash);
        assert_eq!(info.signed_claims.chain, info.chain);
        assert_eq!(info.signed_claims.provider, info.provider);
        assert_eq!(info.signed_claims.nonce, info.nonce);
        assert_eq!(info.signed_claims.issued_at, info.issued_at);
        // ratls_cert_sha256 binds the serving cert.
        assert_eq!(info.signed_claims.ratls_cert_sha256, info.ratls_cert_sha256);
        assert_eq!(info.ratls_cert_sha256, testing::TEST_RATLS_CERT_SHA256);

        // method_check_digest == SHA-256 of the canonical method_check
        // actually returned.
        let expected_digest =
            crate::signed_claims::method_check_digest(&info.method_check).unwrap();
        assert_eq!(info.signed_claims.method_check_digest, expected_digest);

        // Tampering the returned method_check breaks the digest binding.
        let mut tampered = info.method_check.clone();
        tampered.all_required_passed = !tampered.all_required_passed;
        let tampered_digest = crate::signed_claims::method_check_digest(&tampered).unwrap();
        assert_ne!(info.signed_claims.method_check_digest, tampered_digest);
    }

    #[tokio::test]
    async fn tampered_claims_fail_signature_verification() {
        use crate::signed_claims::signed_claims_bytes;
        use ed25519_dalek::{Signature, Verifier, VerifyingKey};

        let provider = DeterministicProvider::new();
        let info = provider
            .build_info(b"tamper-nonce-32-bytes-padding-ok")
            .await
            .unwrap();
        let pubkey: [u8; 32] = BASE64_STANDARD
            .decode(&info.node_pubkey)
            .unwrap()
            .try_into()
            .unwrap();
        let vk = VerifyingKey::from_bytes(&pubkey).unwrap();
        let sig_bytes: [u8; 64] = BASE64_STANDARD
            .decode(&info.claims_signature)
            .unwrap()
            .try_into()
            .unwrap();
        // Flip the signed cert fingerprint: a relay substituting its own
        // serving cert cannot re-sign, so verification fails.
        let mut tampered = info.signed_claims.clone();
        tampered.ratls_cert_sha256 = "00".repeat(32);
        let msg = signed_claims_bytes(&tampered).unwrap();
        assert!(vk.verify(&msg, &Signature::from_bytes(&sig_bytes)).is_err());
    }

    #[tokio::test]
    async fn quote_timeout_fires_on_hang() {
        // A short real timeout keeps the test fast without needing the
        // tokio test-util paused clock.
        let timeout = std::time::Duration::from_millis(20);
        let hang = std::future::pending::<Result<(), String>>();
        let result = with_quote_timeout(timeout, hang).await;
        let err = result.expect_err("a hanging quote future must time out");
        assert!(
            err.starts_with("get_quote timed out"),
            "the timeout branch must report a timeout, got: {err}"
        );
    }

    #[tokio::test]
    async fn quote_timeout_passes_through_success() {
        let timeout = std::time::Duration::from_secs(10);
        let ready = std::future::ready(Ok::<_, String>(42));
        let value = with_quote_timeout(timeout, ready)
            .await
            .expect("a ready future must not time out");
        assert_eq!(value, 42);
    }

    #[tokio::test]
    async fn quote_timeout_propagates_inner_error() {
        let timeout = std::time::Duration::from_secs(10);
        let failed = std::future::ready(Err::<(), _>("dstack agent refused"));
        let err = with_quote_timeout(timeout, failed)
            .await
            .expect_err("an inner error must propagate");
        assert_eq!(err, "dstack agent refused");
    }

    #[test]
    fn quote_semaphore_bounds_concurrency_and_sheds_when_saturated() {
        // A small pool: saturating it must return QuoteBusy immediately
        // (fast shed), never block/queue. Releasing a permit frees a slot.
        let sem = tokio::sync::Semaphore::new(2);
        let p1 = try_acquire_quote_permit(&sem).expect("first permit");
        let p2 = try_acquire_quote_permit(&sem).expect("second permit");
        // Pool exhausted: the next attempt is shed rather than queued.
        let busy = try_acquire_quote_permit(&sem);
        assert!(
            matches!(busy, Err(AttestationError::QuoteBusy)),
            "a saturated pool must shed with QuoteBusy, got {busy:?}"
        );
        // Dropping one permit frees exactly one slot back.
        drop(p1);
        let p3 = try_acquire_quote_permit(&sem).expect("permit reclaimed after release");
        assert!(
            matches!(
                try_acquire_quote_permit(&sem),
                Err(AttestationError::QuoteBusy)
            ),
            "still bounded after reclaiming the single freed slot"
        );
        drop(p2);
        drop(p3);
    }

    #[tokio::test]
    async fn quote_changes_with_nonce() {
        let provider = DeterministicProvider::new();
        let a = provider
            .build_info(b"nonce-a-padding-to-16b")
            .await
            .unwrap();
        let b = provider
            .build_info(b"nonce-b-padding-to-16b")
            .await
            .unwrap();
        assert_ne!(a.quote, b.quote);
        assert_ne!(a.report_data, b.report_data);
        // Static fields stay constant across requests.
        assert_eq!(a.compose_hash, b.compose_hash);
        assert_eq!(a.app_id, b.app_id);
    }
}
