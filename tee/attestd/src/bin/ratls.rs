//! `BlockMachine` TEE miner RA-TLS certificate provisioner.
//!
//! A one-shot container-start step: mints the node's data-plane TLS
//! certificate via dstack's native RA-TLS facility (the guest agent's
//! `GetTlsKey` RPC) and writes the key/cert PEM files to
//! `/tmp/bm-ratls/`. `image/start.sh` runs this before launching
//! Envoy; on success it exits 0, on failure it exits non-zero and the
//! container restarts.
//!
//! Envoy's `:8080` ingress listener terminates TLS with these exact PEM
//! files (its `DownstreamTlsContext`), so the node serves its
//! enclave-bound RA-TLS cert on the Phala `-8080s` TLS-passthrough
//! endpoint. The registry (at registration) and the BM gateway (per
//! connection) verify the embedded TDX quote binds the cert to the
//! enclave key. See `bm_tee_attestd::ratls`.
//!
//! Configuration (environment):
//!
//! * `CHAIN` / `PROVIDER` — folded into the certificate subject CN.
//! * `DSTACK_SOCKET` — dstack guest agent socket override. Default is
//!   the SDK's socket-path search (`/var/run/dstack.sock` first).

#![forbid(unsafe_code)]

use std::path::PathBuf;

use anyhow::{Context, Result};
use bm_tee_attestd::{
    provision_ratls, RatlsPaths, DEFAULT_RATLS_CERT_PATH, DEFAULT_RATLS_KEY_PATH,
};

/// PEM private-key output path. A build-time contract with
/// `image/envoy.yaml`'s ingress `DownstreamTlsContext`.
const KEY_PATH: &str = DEFAULT_RATLS_KEY_PATH;
/// PEM certificate-chain output path. Must match `KEY_PATH`'s dir, and
/// is the same path `attestd` reads to compute `ratls_cert_sha256`.
const CERT_PATH: &str = DEFAULT_RATLS_CERT_PATH;

#[tokio::main]
async fn main() -> Result<()> {
    // Log to stderr so the provisioner's lines interleave correctly
    // with start.sh's `[start]` lines in `phala cvms logs`.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let chain = std::env::var("CHAIN").unwrap_or_else(|_| "unknown".to_owned());
    let provider = std::env::var("PROVIDER").unwrap_or_else(|_| "unknown".to_owned());
    let identity = format!("{chain}-{provider}");
    let paths = RatlsPaths {
        key: PathBuf::from(KEY_PATH),
        cert: PathBuf::from(CERT_PATH),
    };
    let dstack_socket = std::env::var("DSTACK_SOCKET").ok();

    tracing::info!(
        identity = %identity,
        key_path = KEY_PATH,
        cert_path = CERT_PATH,
        dstack_socket = dstack_socket.as_deref().unwrap_or("<sdk default>"),
        "minting data-plane RA-TLS cert via dstack get_tls_key",
    );
    provision_ratls(&identity, dstack_socket.as_deref(), &paths)
        .await
        .context("provision data-plane RA-TLS certificate from dstack")?;
    tracing::info!("RA-TLS certificate provisioned");
    Ok(())
}
