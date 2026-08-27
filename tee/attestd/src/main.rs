//! `BlockMachine` TEE proxy-miner attestation server binary.
//!
//! Bootstraps a TEE-bound ed25519 keypair from the dstack guest agent,
//! runs the boot method check against the hardcoded provider upstream
//! (fail-closed), then serves `GET /attestation/info` on a loopback
//! address. Envoy (the miner's data plane) routes `/attestation/*`
//! here; the registry probes it through Envoy's public `:8080` port.
//! `/attestation/*` is **public** — Envoy strips any inbound
//! `Authorization` header before the bearer check, so the attestation
//! surface carries no node secret; only the proxied `/rpc` and `/ws`
//! routes require the `NODE_SECRET` bearer token.
//!
//! Configuration (environment):
//!
//! * `CHAIN` — chain slug (`eth`). Rendered literal in the measured
//!   compose. Required.
//! * `PROVIDER` — provider slug (`drpc`). Rendered literal in
//!   the measured compose. Required.
//! * `PROVIDER_API_KEY` — the operator's upstream API key. Required.
//! * `METHOD_CHECK_INTERVAL_SECS` — periodic method re-check interval.
//!   Default 3600.
//! * `BM_ATTESTD_BIND_ADDR` — bind address. Default `127.0.0.1:8081`.
//! * `DSTACK_SOCKET` — dstack guest agent socket override. Default
//!   `/var/run/dstack.sock` (handled by the SDK).
//! * `BM_RATLS_CERT_PATH` — PEM path of the RA-TLS cert Envoy serves
//!   (read fail-closed at startup to compute `ratls_cert_sha256`).
//!   Default `/tmp/bm-ratls/cert.pem` — the `bm-tee-ratls` output path.
//!
//! A failure to reach dstack at startup, an unknown (chain, provider)
//! combination, or a failed required method check is fatal: the process
//! exits non-zero, the container restarts, and — because `start.sh`
//! only launches Envoy after this server starts listening — Envoy never
//! serves a data plane whose required checks did not pass.

#![forbid(unsafe_code)]

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use axum::routing::get;
use axum::Router;
use bm_tee_attestd::info::AppState;
use bm_tee_attestd::method_check::SharedReport;
use bm_tee_attestd::{
    attestation_info, resolve_upstream, validate_node_secret, validate_optional_node_secret,
    DstackAttestationProvider, MethodChecker, SigningKeypair, DEFAULT_RATLS_CERT_PATH,
    ETH_MANIFEST_JSON,
};
use tokio::sync::RwLock;

/// Default bind address. Loopback only — Envoy reaches it in-container;
/// nothing external should hit the attestation server directly.
const DEFAULT_BIND_ADDR: &str = "127.0.0.1:8081";
/// Default periodic method re-check interval (seconds). Baked default
/// per the contract doc; `METHOD_CHECK_INTERVAL_SECS` overrides.
const DEFAULT_METHOD_CHECK_INTERVAL_SECS: u64 = 3600;
/// Number of CONSECUTIVE periodic method-check runs that must fail a
/// required probe before attestd self-demotes (exits non-zero). A single
/// transient provider blip must NOT take the data plane down — that would
/// flap the node — so the threshold tolerates a couple of failed runs;
/// but a SUSTAINED failure must stop the data plane rather than relying
/// solely on the external registry demotion. On the Kth consecutive
/// failure attestd exits, the container restarts, and the fail-closed
/// boot gate re-runs — which refuses to serve if the provider is still
/// bad. A single success resets the counter.
const MAX_CONSECUTIVE_PERIODIC_FAILURES: u32 = 3;

/// Environment-derived configuration, resolved once at startup.
struct EnvConfig {
    chain: String,
    provider_slug: String,
    api_key: String,
    bind_addr: String,
    dstack_socket: Option<String>,
    method_check_interval: u64,
    /// PEM path of the RA-TLS cert Envoy serves — read fail-closed at
    /// startup to compute `ratls_cert_sha256`. Defaults to the shared
    /// `bm-tee-ratls` output path; overridable for tests/alt layouts.
    ratls_cert_path: String,
}

/// Read and validate the process environment.
fn load_env_config() -> Result<EnvConfig> {
    let chain = std::env::var("CHAIN").context("CHAIN must be set (compose rendered literal)")?;
    let provider_slug =
        std::env::var("PROVIDER").context("PROVIDER must be set (compose rendered literal)")?;
    let api_key =
        std::env::var("PROVIDER_API_KEY").context("PROVIDER_API_KEY must be set (operator env)")?;
    // Reject a key equal to the redaction marker fail-closed: such a value
    // would defeat error redaction (a no-op replacement) and let a
    // key-echoing upstream error survive verbatim into a stored/logged
    // error string. It is never a real provider key.
    bm_tee_attestd::reject_marker_api_key(&api_key).map_err(|e| anyhow::anyhow!(e))?;
    let bind_addr =
        std::env::var("BM_ATTESTD_BIND_ADDR").unwrap_or_else(|_| DEFAULT_BIND_ADDR.to_owned());
    let dstack_socket = std::env::var("DSTACK_SOCKET").ok();
    let method_check_interval =
        parse_method_check_interval(std::env::var("METHOD_CHECK_INTERVAL_SECS").ok())?;
    let ratls_cert_path =
        std::env::var("BM_RATLS_CERT_PATH").unwrap_or_else(|_| DEFAULT_RATLS_CERT_PATH.to_owned());

    // The node secret(s) are enforced by Envoy as opaque bearer data.
    // Validate their charset here — in measured code, before Envoy is
    // gated open — so the enclave and the registry agree on what a valid
    // secret is and no malformed value can slip past the boot gate.
    let node_secret =
        std::env::var("NODE_SECRET").context("NODE_SECRET must be set (operator env)")?;
    validate_node_secret("NODE_SECRET", &node_secret).map_err(|e| anyhow::anyhow!(e))?;
    validate_optional_node_secret(
        "NODE_SECRET_NEXT",
        std::env::var("NODE_SECRET_NEXT").ok().as_deref(),
    )
    .map_err(|e| anyhow::anyhow!(e))?;

    Ok(EnvConfig {
        chain,
        provider_slug,
        api_key,
        bind_addr,
        dstack_socket,
        method_check_interval,
        ratls_cert_path,
    })
}

/// Parse and validate `METHOD_CHECK_INTERVAL_SECS`. Absent → the baked
/// default; a zero value is rejected fail-closed because
/// `tokio::time::interval` panics on a zero period, which would crash
/// the periodic checker after the boot gate has already passed.
fn parse_method_check_interval(raw: Option<String>) -> Result<u64> {
    let secs = match raw {
        Some(v) => v
            .parse::<u64>()
            .with_context(|| format!("METHOD_CHECK_INTERVAL_SECS must be an integer: {v:?}"))?,
        None => DEFAULT_METHOD_CHECK_INTERVAL_SECS,
    };
    anyhow::ensure!(
        secs > 0,
        "METHOD_CHECK_INTERVAL_SECS must be greater than 0 (got 0)",
    );
    Ok(secs)
}

/// Log every probe result of a method-check run, one line per probe,
/// so `phala cvms logs` names the exact method that failed a gate.
fn log_method_check_results(report: &bm_tee_attestd::MethodCheckReport) {
    for result in &report.results {
        tracing::info!(
            method = %result.method,
            probe = ?result.probe,
            required = result.required,
            passed = result.passed,
            latency_ms = result.latency_ms,
            error = result.error.as_deref().unwrap_or(""),
            "method check result",
        );
    }
}

/// Whether a run of `consecutive_failures` failed periodic method checks
/// should self-demote the node (exit non-zero so the container restarts
/// and the fail-closed boot gate re-runs). Pure and unit-tested: returns
/// true only once the count reaches [`MAX_CONSECUTIVE_PERIODIC_FAILURES`],
/// so a single transient failure (a provider blip) never takes the node
/// down, but a sustained failure does.
fn should_self_demote(consecutive_failures: u32) -> bool {
    consecutive_failures >= MAX_CONSECUTIVE_PERIODIC_FAILURES
}

/// Spawn the periodic method re-check task. Each failed re-check is logged
/// and stored (the registry's re-attestation sweep also reads
/// `all_required_passed` and demotes the node externally). A single
/// transient failure does NOT kill the process — the counter tolerates a
/// provider blip to avoid flapping the node. But after
/// [`MAX_CONSECUTIVE_PERIODIC_FAILURES`] consecutive failed runs the task
/// self-demotes by RETURNING: `serve_supervised` treats the checker
/// exiting as fatal and `main` exits non-zero, so the container restarts
/// and the fail-closed boot gate re-runs (which refuses to serve if the
/// provider is still bad) rather than serving `/rpc` and `/ws` on a
/// sustained-broken provider until the external registry demotes it. A
/// single successful run resets the counter.
fn spawn_periodic_recheck(
    checker: MethodChecker,
    method_report: SharedReport,
    interval: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // The first tick fires immediately; the boot check already
        // covered it.
        ticker.tick().await;
        let mut consecutive_failures: u32 = 0;
        loop {
            ticker.tick().await;
            tracing::info!("running periodic method check");
            let report = checker.run().await;
            if report.all_required_passed {
                consecutive_failures = 0;
            } else {
                consecutive_failures = consecutive_failures.saturating_add(1);
                log_method_check_results(&report);
                tracing::warn!(
                    consecutive_failures,
                    threshold = MAX_CONSECUTIVE_PERIODIC_FAILURES,
                    "periodic method check failed a required probe — \
                     the next attestation reports all_required_passed=false",
                );
            }
            *method_report.write().await = report;
            if should_self_demote(consecutive_failures) {
                tracing::error!(
                    consecutive_failures,
                    threshold = MAX_CONSECUTIVE_PERIODIC_FAILURES,
                    "periodic method check failed a required probe on \
                     {consecutive_failures} consecutive runs — self-demoting: \
                     exiting so the container restarts and the fail-closed boot \
                     gate re-runs instead of serving a broken provider",
                );
                return;
            }
        }
    })
}

#[tokio::main]
async fn main() -> Result<()> {
    // Log to stderr, not stdout: the container entrypoint (start.sh)
    // also logs to stderr, so a single stream keeps attestd's and the
    // entrypoint's lines correctly interleaved in `phala cvms logs`,
    // and an anyhow fatal-error printout (also stderr) lands in order.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let EnvConfig {
        chain,
        provider_slug,
        api_key,
        bind_addr,
        dstack_socket,
        method_check_interval,
        ratls_cert_path,
    } = load_env_config()?;

    // Refuse to start on unknown (chain, provider) combos — the
    // upstream allowlist is hardcoded per release.
    let upstream = resolve_upstream(&chain, &provider_slug, &api_key)
        .map_err(|e| anyhow::anyhow!(e.to_string()))?;
    tracing::info!(
        chain = %chain,
        provider = %provider_slug,
        upstream_host = %upstream.host,
        bind_addr = %bind_addr,
        method_check_interval_secs = method_check_interval,
        dstack_socket = dstack_socket.as_deref().unwrap_or("<sdk default>"),
        "attestd starting",
    );

    // TEE-bound signing keypair. Derived from the dstack-KMS sealed key;
    // a redeploy of the same image yields the same pubkey. Each dstack
    // step is logged before it runs so a hang or a kill mid-bootstrap
    // is visible in the container log.
    tracing::info!("bootstrapping attestation keypair from dstack get_key");
    let keypair = SigningKeypair::bootstrap(&chain, dstack_socket.as_deref())
        .await
        .context("bootstrap node attestation keypair from dstack")?;
    tracing::info!(
        node_pubkey = %keypair.public_b64(),
        "attestation keypair bootstrapped",
    );

    // Boot method check — fail-closed. Single sequential probes only;
    // rate/load testing is registry-owned by contract. Both the HTTP
    // and WS upstreams are probed. The raw `api_key` seeds the error
    // redactor; the URLs already carry its percent-encoded form.
    let checker = MethodChecker::from_manifest_json(
        ETH_MANIFEST_JSON,
        upstream.http_url.clone(),
        upstream.ws_url.clone(),
        &api_key,
    )
    .map_err(|e| anyhow::anyhow!(e))?;
    anyhow::ensure!(
        checker.manifest().chain == chain,
        "baked method manifest is for chain {:?} but CHAIN is {:?}",
        checker.manifest().chain,
        chain,
    );
    tracing::info!("running boot method check against provider upstream");
    let boot_report = checker.run().await;
    log_method_check_results(&boot_report);
    anyhow::ensure!(
        boot_report.all_required_passed,
        "boot method check failed: a required probe did not pass — refusing to serve \
         (the container will restart and retry)",
    );
    tracing::info!(
        is_archive = boot_report.capabilities.is_archive,
        serves_proofs = boot_report.capabilities.serves_proofs,
        audit_compatible = boot_report.capabilities.audit_compatible,
        "boot method check passed",
    );
    let method_report: SharedReport = Arc::new(RwLock::new(boot_report));

    // Fetch the static CVM attestation fields once at startup, and read
    // the served RA-TLS cert fingerprint (fail-closed: a missing cert
    // aborts startup, so attestd never serves an unbindable attestation).
    tracing::info!(
        ratls_cert_path = %ratls_cert_path,
        "fetching static CVM fields from dstack info and reading served RA-TLS cert",
    );
    let provider: AppState = Arc::new(
        DstackAttestationProvider::bootstrap(
            chain.clone(),
            provider_slug.clone(),
            keypair,
            Arc::clone(&method_report),
            std::path::Path::new(&ratls_cert_path),
            dstack_socket,
        )
        .await
        .context("bootstrap dstack attestation provider")?,
    );

    let recheck_task = spawn_periodic_recheck(
        checker,
        Arc::clone(&method_report),
        Duration::from_secs(method_check_interval),
    );

    let app = Router::new()
        .route("/attestation/info", get(attestation_info))
        .with_state(provider);

    let listener = tokio::net::TcpListener::bind(&bind_addr)
        .await
        .with_context(|| format!("bind attestation server to {bind_addr}"))?;
    tracing::info!(bind_addr = %bind_addr, "attestation server listening");
    serve_supervised(listener, app, recheck_task).await
}

/// Serve the attestation endpoint while supervising the periodic
/// checker. The checker must run for the life of the process; if it
/// panics or returns, attestd exits non-zero so the container restarts
/// and re-runs the fail-closed boot gate — it never keeps serving a
/// stale-green method-check report while the checker is dead. A return
/// of the HTTP server is likewise treated as fatal.
async fn serve_supervised(
    listener: tokio::net::TcpListener,
    app: Router,
    recheck_task: tokio::task::JoinHandle<()>,
) -> Result<()> {
    tokio::select! {
        result = axum::serve(listener, app) => {
            result.context("attestation server terminated")?;
            anyhow::bail!("attestation HTTP server exited unexpectedly");
        }
        joined = recheck_task => match joined {
            Ok(()) => anyhow::bail!(
                "periodic method-check task exited — refusing to serve stale-green results",
            ),
            Err(join_err) => anyhow::bail!(
                "periodic method-check task panicked ({join_err}) — refusing to serve stale-green results",
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn interval_defaults_when_absent() {
        assert_eq!(
            parse_method_check_interval(None).unwrap(),
            DEFAULT_METHOD_CHECK_INTERVAL_SECS
        );
    }

    #[test]
    fn interval_zero_rejected() {
        // A zero period would panic tokio::time::interval; reject it at
        // startup so the boot gate fails closed instead.
        let err = parse_method_check_interval(Some("0".to_owned()))
            .expect_err("zero interval must be rejected");
        assert!(err.to_string().contains("greater than 0"), "{err}");
    }

    #[test]
    fn interval_valid_parsed() {
        assert_eq!(
            parse_method_check_interval(Some("60".to_owned())).unwrap(),
            60
        );
    }

    #[test]
    fn interval_non_integer_rejected() {
        assert!(parse_method_check_interval(Some("abc".to_owned())).is_err());
    }

    #[test]
    fn single_transient_periodic_failure_does_not_self_demote() {
        // A single failed run (and any count below the threshold) must NOT
        // take the node down — that would flap the data plane on a
        // transient provider blip.
        assert!(!should_self_demote(0));
        assert!(!should_self_demote(1));
        assert!(!should_self_demote(MAX_CONSECUTIVE_PERIODIC_FAILURES - 1));
    }

    #[test]
    fn sustained_periodic_failures_self_demote_at_threshold() {
        // At and beyond the threshold, a SUSTAINED failure self-demotes.
        assert!(should_self_demote(MAX_CONSECUTIVE_PERIODIC_FAILURES));
        assert!(should_self_demote(MAX_CONSECUTIVE_PERIODIC_FAILURES + 1));
        assert!(should_self_demote(u32::MAX));
        // The threshold must tolerate at least one transient failure.
        const { assert!(MAX_CONSECUTIVE_PERIODIC_FAILURES >= 2) };
    }
}
