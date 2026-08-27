//! RA-TLS data-plane certificate provisioning.
//!
//! One-shot container-start step: calls dstack's native RA-TLS facility
//! — the guest agent's `GetTlsKey` RPC — and writes the resulting
//! key/cert PEM files to disk. `image/start.sh` runs `bm-tee-ratls`
//! before Envoy; a dstack failure aborts the container (fail-fast, the
//! same posture attestd uses for its own dstack calls).
//!
//! # What dstack provides
//!
//! `DstackClient::get_tls_key(TlsKeyConfig { usage_ra_tls: true, .. })`
//! makes the guest agent, inside the CVM:
//!
//! 1. Generate a fresh P-256 key pair (random per call — not the
//!    KMS-sealed app key; an ephemeral data-plane key).
//! 2. Take a fresh Intel TDX quote whose `report_data` commits to that
//!    key: `report_data = SHA-512("ratls-cert:" || pubkey_der)`.
//! 3. Issue an X.509 leaf certificate carrying the quote (and the CVM
//!    event log) in the dstack RA-TLS extension, OID
//!    `1.3.6.1.4.1.62397.1.8` (`PHALA_RATLS_ATTESTATION`).
//!
//! # How the cert is served
//!
//! Envoy's `:8080` ingress listener terminates TLS with these exact PEM
//! paths (`image/envoy.yaml`'s `DownstreamTlsContext`), so the node
//! serves its enclave-bound cert on the Phala `-8080s` TLS-passthrough
//! endpoint it registers. The dstack gateway forwards the raw TLS stream,
//! so the caller (registry at registration, gateway per connection)
//! receives this cert and verifies the embedded quote binds it to the
//! enclave key — a relay proxy cannot reproduce that binding. This
//! provisioning step also fail-fast-validates the dstack guest agent
//! before the data plane starts.

use std::path::{Path, PathBuf};

use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine;
use dstack_sdk::dstack_client::{DstackClient, TlsKeyConfig};
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::fs;

/// `0o600` — owner read/write only. The RA-TLS private key is a
/// credential; the cert is public but kept beside the key with the
/// same restrictive mode for simplicity.
const KEY_FILE_MODE: u32 = 0o600;

/// Default PEM certificate-chain path. A build-time contract shared by
/// three places that MUST agree: `bm-tee-ratls` writes the cert here,
/// Envoy's ingress `DownstreamTlsContext` serves it, and `attestd` reads
/// it to compute `ratls_cert_sha256`. Keeping the path in one place
/// keeps the serving cert and the fingerprinted cert identical.
pub const DEFAULT_RATLS_CERT_PATH: &str = "/tmp/bm-ratls/cert.pem";
/// Default PEM private-key path. Same build-time contract with
/// `image/envoy.yaml` (its ingress `DownstreamTlsContext`).
pub const DEFAULT_RATLS_KEY_PATH: &str = "/tmp/bm-ratls/key.pem";

/// PEM label bounding a certificate block.
const CERT_PEM_BEGIN: &str = "-----BEGIN CERTIFICATE-----";
const CERT_PEM_END: &str = "-----END CERTIFICATE-----";

/// Certificate subject Common Name prefix. Informational only — a
/// verifier trusts the cert via the embedded quote, not via a CA or a
/// hostname match, so the CN carries the node identity for
/// human-readable `openssl x509` inspection rather than verification.
const CERT_SUBJECT_PREFIX: &str = "bm-tee-miner-ratls";

/// Errors from RA-TLS certificate provisioning.
#[derive(Debug, Error)]
pub enum RatlsError {
    /// The dstack guest agent's `GetTlsKey` RPC failed or was
    /// unreachable.
    #[error("dstack get_tls_key failed: {0}")]
    DstackTlsKey(String),
    /// The guest agent returned an empty certificate chain — it cannot
    /// be used as a TLS leaf.
    #[error("dstack get_tls_key returned an empty certificate chain")]
    EmptyCertChain,
    /// Writing a PEM artifact to disk failed.
    #[error("write {path}: {source}")]
    Write {
        /// The artifact path the write targeted.
        path: PathBuf,
        /// The underlying I/O error.
        source: std::io::Error,
    },
}

/// Errors reading/fingerprinting the served RA-TLS certificate.
///
/// These are fatal at attestd startup: without the exact cert Envoy
/// serves, `ratls_cert_sha256` cannot bind the serving channel to the
/// quote, so attestd fails closed (exits) rather than serve an
/// unbindable attestation.
#[derive(Debug, Error)]
pub enum CertError {
    /// The certificate file could not be read (absent, unreadable).
    #[error("read RA-TLS cert {path}: {source}")]
    Read {
        /// The cert path that could not be read.
        path: PathBuf,
        /// The underlying I/O error.
        source: std::io::Error,
    },
    /// The file did not contain a PEM `CERTIFICATE` block.
    #[error("RA-TLS cert {path} contains no PEM CERTIFICATE block")]
    NoPemBlock {
        /// The cert path lacking a PEM block.
        path: PathBuf,
    },
    /// The PEM block's base64 body did not decode to DER bytes.
    #[error("RA-TLS cert {path} PEM body is not valid base64: {reason}")]
    BadBase64 {
        /// The cert path with the malformed base64.
        path: PathBuf,
        /// The base64 decode error.
        reason: String,
    },
}

/// SHA-256 (lowercase hex) of the DER encoding of the first PEM
/// `CERTIFICATE` block in `pem` — the RA-TLS leaf. The registry captures
/// the served leaf cert and compares its DER SHA-256 to the signed
/// `ratls_cert_sha256`, so this must fingerprint the same leaf Envoy
/// serves (leaf-first in the chain PEM).
///
/// # Errors
///
/// Returns [`CertError::NoPemBlock`] when no `CERTIFICATE` block is
/// present, and [`CertError::BadBase64`] when the block body does not
/// base64-decode.
pub fn cert_der_sha256_hex(pem: &str, path: &Path) -> Result<String, CertError> {
    let der = first_cert_der(pem, path)?;
    Ok(hex::encode(Sha256::digest(&der)))
}

/// Read the served RA-TLS cert PEM from `path` and return its leaf DER
/// SHA-256 (lowercase hex). Called once at attestd startup; the cert is
/// stable for the life of the process (minted once by `bm-tee-ratls`).
///
/// # Errors
///
/// Returns [`CertError::Read`] when the file cannot be read, plus the
/// decode errors of [`cert_der_sha256_hex`].
pub async fn read_cert_der_sha256(path: &Path) -> Result<String, CertError> {
    let pem = fs::read_to_string(path)
        .await
        .map_err(|source| CertError::Read {
            path: path.to_path_buf(),
            source,
        })?;
    cert_der_sha256_hex(&pem, path)
}

/// Extract the DER bytes of the first PEM `CERTIFICATE` block. The PEM
/// body is base64 (STANDARD) with embedded newlines; all ASCII
/// whitespace is stripped before decoding.
fn first_cert_der(pem: &str, path: &Path) -> Result<Vec<u8>, CertError> {
    let after_begin = pem
        .find(CERT_PEM_BEGIN)
        .map(|i| i + CERT_PEM_BEGIN.len())
        .ok_or_else(|| CertError::NoPemBlock {
            path: path.to_path_buf(),
        })?;
    let rest = &pem[after_begin..];
    let end = rest
        .find(CERT_PEM_END)
        .ok_or_else(|| CertError::NoPemBlock {
            path: path.to_path_buf(),
        })?;
    let body: String = rest[..end].chars().filter(|c| !c.is_whitespace()).collect();
    BASE64_STANDARD
        .decode(body.as_bytes())
        .map_err(|e| CertError::BadBase64 {
            path: path.to_path_buf(),
            reason: e.to_string(),
        })
}

/// Where the provisioned RA-TLS PEM artifacts are written.
#[derive(Debug, Clone)]
pub struct RatlsPaths {
    /// PEM private key (PKCS#8) path.
    pub key: PathBuf,
    /// PEM certificate chain (leaf first) path.
    pub cert: PathBuf,
}

/// Mint the node's data-plane RA-TLS certificate via dstack and write
/// the key/cert PEM files.
///
/// Calls `DstackClient::get_tls_key` with `usage_ra_tls = true` and
/// `usage_server_auth = true` (the data plane is a TLS *server*),
/// then writes the returned private key and the joined certificate
/// chain to `paths`.
///
/// `identity` is folded into the certificate subject CN so a manual
/// `openssl x509 -in cert.pem -noout -subject` names the node; it does
/// not affect the attestation binding.
///
/// # Errors
///
/// Returns [`RatlsError::DstackTlsKey`] when the guest agent is
/// unreachable or the RPC fails, [`RatlsError::EmptyCertChain`] when
/// the chain comes back empty, and [`RatlsError::Write`] when a PEM
/// artifact cannot be written. The caller fails fast — the container
/// exits and the runtime restarts it.
pub async fn provision(
    identity: &str,
    dstack_socket: Option<&str>,
    paths: &RatlsPaths,
) -> Result<(), RatlsError> {
    let client = DstackClient::new(dstack_socket);
    let config = TlsKeyConfig::builder()
        .subject(format!("{CERT_SUBJECT_PREFIX}/{identity}"))
        .usage_ra_tls(true)
        .usage_server_auth(true)
        .usage_client_auth(false)
        .build();
    let response = client
        .get_tls_key(config)
        .await
        .map_err(|e| RatlsError::DstackTlsKey(e.to_string()))?;

    if response.certificate_chain.is_empty() {
        return Err(RatlsError::EmptyCertChain);
    }
    // dstack returns each PEM block already newline-terminated; joining
    // with an empty separator yields a valid concatenated PEM bundle
    // (leaf first, then any intermediates).
    let cert_pem = response.certificate_chain.join("");

    write_artifact(&paths.key, response.key.as_bytes()).await?;
    write_artifact(&paths.cert, cert_pem.as_bytes()).await?;
    Ok(())
}

/// Write a PEM artifact with `0o600` permissions, creating the parent
/// directory if needed.
async fn write_artifact(path: &Path, contents: &[u8]) -> Result<(), RatlsError> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)
                .await
                .map_err(|source| RatlsError::Write {
                    path: parent.to_path_buf(),
                    source,
                })?;
        }
    }
    fs::write(path, contents)
        .await
        .map_err(|source| RatlsError::Write {
            path: path.to_path_buf(),
            source,
        })?;
    set_mode(path, KEY_FILE_MODE).await
}

/// Restrict a written artifact to `mode`.
#[cfg(unix)]
async fn set_mode(path: &Path, mode: u32) -> Result<(), RatlsError> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .await
        .map_err(|source| RatlsError::Write {
            path: path.to_path_buf(),
            source,
        })
}

/// Non-Unix builds (developer machines) skip the `chmod`; the miner
/// only ever runs on the Linux CVM image.
#[cfg(not(unix))]
async fn set_mode(_path: &Path, _mode: u32) -> Result<(), RatlsError> {
    Ok(())
}

/// A self-signed P-256 test certificate (leaf), and the SHA-256 of its
/// DER encoding. Shared by unit tests here and by the attestation
/// provider tests. Generated with:
///   openssl ecparam -name prime256v1 -genkey -noout -out k.pem
///   openssl req -x509 -new -key k.pem -subj /CN=bm-tee-miner-ratls ...
/// and the DER SHA-256 computed with `openssl x509 -outform DER | sha256`.
#[cfg(test)]
pub(crate) const TEST_CERT_PEM: &str = "\
-----BEGIN CERTIFICATE-----
MIIBKDCB0AIJAJ6+YsRLvxBxMAoGCCqGSM49BAMCMB0xGzAZBgNVBAMMEmJtLXRl
ZS1taW5lci1yYXRsczAeFw0yNjA3MTcyMjI3MjBaFw0zNjA3MTQyMjI3MjBaMB0x
GzAZBgNVBAMMEmJtLXRlZS1taW5lci1yYXRsczBZMBMGByqGSM49AgEGCCqGSM49
AwEHA0IABHxsOk3Ft/BtldierLhtw93WVJKprw3ueUY4DQ5NstygBth0Kpzr+BdJ
ScA1k+zvvPzS4U50Zq/XrelZbHnVHPQwCgYIKoZIzj0EAwIDRwAwRAIgSE/KZOus
aVPMG8lbtZgQCO3CAeg7kjWr2UmHrv42obsCIBTns37gOg8E7mOSUUz0GzKtPHYz
oAhw+PxLk7IuS6EK
-----END CERTIFICATE-----
";

/// DER SHA-256 (lowercase hex) of [`TEST_CERT_PEM`].
#[cfg(test)]
pub(crate) const TEST_CERT_DER_SHA256: &str =
    "ce0189fd9269a38818efb48e7c4b423a345b844353b6d54145451a3a3ba4c28f";

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn cert_der_sha256_matches_openssl_fixture() {
        // The fingerprint MUST equal `openssl x509 -outform DER | sha256`
        // of the served cert — that is exactly what the registry captures
        // from the TLS channel and compares to the signed value.
        let got = cert_der_sha256_hex(TEST_CERT_PEM, Path::new("cert.pem")).unwrap();
        assert_eq!(got, TEST_CERT_DER_SHA256);
        assert_eq!(got.len(), 64);
    }

    #[test]
    fn cert_der_sha256_uses_first_leaf_of_a_chain() {
        // A chain PEM is leaf-first; the fingerprint tracks the leaf.
        let chain = format!(
            "{TEST_CERT_PEM}{}",
            "-----BEGIN CERTIFICATE-----\nZZZZ\n-----END CERTIFICATE-----\n"
        );
        let got = cert_der_sha256_hex(&chain, Path::new("chain.pem")).unwrap();
        assert_eq!(got, TEST_CERT_DER_SHA256);
    }

    #[test]
    fn cert_without_pem_block_is_rejected() {
        let err = cert_der_sha256_hex("no pem here", Path::new("x.pem")).unwrap_err();
        assert!(matches!(err, CertError::NoPemBlock { .. }));
    }

    #[test]
    fn cert_with_bad_base64_is_rejected() {
        let bad = "-----BEGIN CERTIFICATE-----\n!!!!\n-----END CERTIFICATE-----\n";
        let err = cert_der_sha256_hex(bad, Path::new("x.pem")).unwrap_err();
        assert!(matches!(err, CertError::BadBase64 { .. }));
    }

    #[tokio::test]
    async fn read_cert_der_sha256_reads_file() {
        let dir = std::env::temp_dir().join(format!("bm-ratls-cert-{}", std::process::id()));
        fs::create_dir_all(&dir).await.unwrap();
        let path = dir.join("cert.pem");
        fs::write(&path, TEST_CERT_PEM).await.unwrap();
        let got = read_cert_der_sha256(&path).await.unwrap();
        assert_eq!(got, TEST_CERT_DER_SHA256);
        fs::remove_dir_all(&dir).await.ok();
    }

    #[tokio::test]
    async fn read_cert_missing_file_fails_closed() {
        let path = std::env::temp_dir().join("bm-ratls-does-not-exist-xyz.pem");
        let err = read_cert_der_sha256(&path).await.unwrap_err();
        assert!(matches!(err, CertError::Read { .. }));
    }

    #[tokio::test]
    async fn write_artifact_creates_parent_and_restricts_mode() {
        let dir = std::env::temp_dir().join(format!("bm-ratls-test-{}", std::process::id()));
        let path = dir.join("nested").join("key.pem");
        write_artifact(&path, b"-----BEGIN-----\n")
            .await
            .expect("write artifact");
        assert_eq!(fs::read(&path).await.unwrap(), b"-----BEGIN-----\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).await.unwrap().permissions().mode();
            assert_eq!(mode & 0o777, KEY_FILE_MODE);
        }
        fs::remove_dir_all(&dir).await.ok();
    }

    #[tokio::test]
    async fn write_artifact_overwrites_existing() {
        let dir = std::env::temp_dir().join(format!("bm-ratls-ow-{}", std::process::id()));
        let path = dir.join("cert.pem");
        write_artifact(&path, b"first").await.expect("first write");
        write_artifact(&path, b"second")
            .await
            .expect("second write");
        assert_eq!(fs::read(&path).await.unwrap(), b"second");
        fs::remove_dir_all(&dir).await.ok();
    }
}
