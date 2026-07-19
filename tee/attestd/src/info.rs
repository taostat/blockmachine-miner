//! `GET /attestation/info` handler.
//!
//! Wire contract: `blockmachine_playground/docs/tee-proxy-miners.md`.
//! The caller (the BM registry's verification sequence) supplies a
//! fresh random nonce as a base64 (STANDARD) query parameter; the
//! server echoes it back in the response and binds it into the quote's
//! `report_data`.
//!
//! This endpoint is UNAUTHENTICATED per the contract: it returns only
//! public attestation data (a TDX quote, event log, and signed public
//! claims), so no `Authorization: Bearer` is required — requiring one
//! would force the registry to send the node secret over a
//! not-yet-verified TLS channel. Envoy's inbound auth filter (see
//! `image/envoy.yaml`) returns `/attestation/*` before its bearer check
//! and enforces the bearer only on the RPC routes; attestd itself stays
//! auth-agnostic either way.

use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine;
use serde::Deserialize;
use serde_json::json;

use crate::provider::{AttestationError, AttestationProvider};

/// Maximum decoded nonce length. 32 bytes is the recommended size per
/// the contract; 64 bytes is the ceiling (the SHA-512 input bound).
const MAX_NONCE_BYTES: usize = 64;
/// Minimum decoded nonce length. The contract specifies a freshly
/// random 32-byte value; anything shorter weakens replay protection.
const MIN_NONCE_BYTES: usize = 16;

/// Axum router state — the attestation provider behind the endpoint.
pub type AppState = Arc<dyn AttestationProvider>;

/// Query string shape for `GET /attestation/info?nonce=...`.
#[derive(Debug, Deserialize)]
pub struct AttestationInfoQuery {
    /// Base64 STANDARD encoded nonce bytes. Required — the handler
    /// returns 400 if missing.
    pub nonce: String,
}

/// `GET /attestation/info` handler.
pub async fn attestation_info(
    State(provider): State<AppState>,
    Query(query): Query<AttestationInfoQuery>,
) -> Response {
    let nonce_bytes = match decode_nonce(&query.nonce) {
        Ok(b) => b,
        Err(e) => return error_response(StatusCode::BAD_REQUEST, e),
    };
    match provider.build_info(&nonce_bytes).await {
        Ok(info) => Json(info).into_response(),
        Err(e) => error_response_from(&e),
    }
}

/// Decode and validate the caller-supplied nonce.
fn decode_nonce(b64: &str) -> Result<Vec<u8>, &'static str> {
    if b64.is_empty() {
        return Err("nonce_required");
    }
    let bytes = BASE64_STANDARD
        .decode(b64)
        .map_err(|_| "nonce_not_base64")?;
    if bytes.len() < MIN_NONCE_BYTES {
        return Err("nonce_too_short");
    }
    if bytes.len() > MAX_NONCE_BYTES {
        return Err("nonce_too_long");
    }
    Ok(bytes)
}

fn error_response(status: StatusCode, code: &str) -> Response {
    let body = json!({ "error": { "code": code } });
    (status, Json(body)).into_response()
}

fn error_response_from(err: &AttestationError) -> Response {
    // Most dstack-side failures are transient infrastructure faults —
    // the guest agent is unreachable or returned an error — so 503 lets
    // the registry's verification sequence treat them as a retryable
    // probe failure. `QuoteBusy` is instead a deliberate load-shed: the
    // in-process concurrency bound around get_quote is saturated, so
    // return 429 fast (a flood is turned away without fanning out quote
    // work onto the guest agent).
    let (status, code) = match err {
        AttestationError::QuoteBusy => (StatusCode::TOO_MANY_REQUESTS, "quote_capacity"),
        AttestationError::DstackQuote(_) => {
            (StatusCode::SERVICE_UNAVAILABLE, "dstack_quote_failed")
        }
        AttestationError::DstackKey(_) => (StatusCode::SERVICE_UNAVAILABLE, "dstack_key_failed"),
        AttestationError::DstackInfo(_) => (StatusCode::SERVICE_UNAVAILABLE, "dstack_info_failed"),
        AttestationError::RatlsCert(_) => (StatusCode::SERVICE_UNAVAILABLE, "ratls_cert_failed"),
        AttestationError::Canonicalize(_) => {
            (StatusCode::SERVICE_UNAVAILABLE, "sign_claims_failed")
        }
    };
    if status == StatusCode::TOO_MANY_REQUESTS {
        // A flood can trip this repeatedly; keep it off the warn stream.
        tracing::debug!(error = %err, "attestation quote at capacity — shedding load");
    } else {
        tracing::warn!(error = %err, "attestation build failed");
    }
    error_response(status, code)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::provider::testing::DeterministicProvider;
    use axum::body::Body;
    use axum::http::Request;
    use axum::routing::get;
    use axum::Router;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    #[test]
    fn missing_nonce_rejected() {
        assert_eq!(decode_nonce(""), Err("nonce_required"));
    }

    #[test]
    fn non_base64_nonce_rejected() {
        assert_eq!(decode_nonce("!not-base64!"), Err("nonce_not_base64"));
    }

    #[test]
    fn short_nonce_rejected() {
        let b64 = BASE64_STANDARD.encode([0u8; 8]);
        assert_eq!(decode_nonce(&b64), Err("nonce_too_short"));
    }

    #[test]
    fn long_nonce_rejected() {
        let b64 = BASE64_STANDARD.encode([0u8; 128]);
        assert_eq!(decode_nonce(&b64), Err("nonce_too_long"));
    }

    #[test]
    fn valid_nonce_accepted() {
        let nonce = [7u8; 32];
        let b64 = BASE64_STANDARD.encode(nonce);
        assert_eq!(decode_nonce(&b64).unwrap().as_slice(), &nonce);
    }

    fn test_app(provider: DeterministicProvider) -> Router {
        let state: AppState = Arc::new(provider);
        Router::new()
            .route("/attestation/info", get(attestation_info))
            .with_state(state)
    }

    async fn get_json(app: Router, uri: &str) -> (StatusCode, serde_json::Value) {
        let response = app
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        (status, value)
    }

    #[tokio::test]
    async fn handler_serves_contract_shape() {
        let nonce = [9u8; 32];
        let nonce_b64 = BASE64_STANDARD.encode(nonce);
        // Query-encode the nonce: '+' and '=' are reserved in a query
        // string. urlencoding by hand keeps the test dependency-free.
        let encoded = nonce_b64.replace('+', "%2B").replace('=', "%3D");
        let (status, body) = get_json(
            test_app(DeterministicProvider::new()),
            &format!("/attestation/info?nonce={encoded}"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["schema_version"], 3);
        assert_eq!(body["tee_platform"], "intel-tdx");
        assert_eq!(body["chain"], "eth");
        assert_eq!(body["provider"], "drpc");
        assert_eq!(body["nonce"], nonce_b64);
        // Every contract field is present.
        for field in [
            "node_pubkey",
            "app_id",
            "compose_hash",
            "os_image_hash",
            "instance_id",
            "tcb_info",
            "quote",
            "event_log",
            "report_data",
            "issued_at",
            "ratls_cert_sha256",
            "signed_claims",
            "claims_signature",
            "method_check",
        ] {
            assert!(body.get(field).is_some(), "missing contract field {field}");
        }
        // signed_claims carries exactly the contract key set.
        let sc = &body["signed_claims"];
        for key in [
            "compose_hash",
            "chain",
            "provider",
            "ratls_cert_sha256",
            "method_check_digest",
            "issued_at",
            "nonce",
        ] {
            assert!(sc.get(key).is_some(), "missing signed_claims key {key}");
        }
        assert_eq!(
            sc.as_object().unwrap().len(),
            7,
            "signed_claims must carry exactly the 7 contract keys"
        );
        // ratls_cert_sha256 is a 64-char hex string echoed into the claim.
        assert_eq!(body["ratls_cert_sha256"].as_str().unwrap().len(), 64);
        assert_eq!(sc["ratls_cert_sha256"], body["ratls_cert_sha256"]);
        // claims_signature verifies against node_pubkey over the
        // canonical signed_claims the body carries.
        let pubkey: [u8; 32] = BASE64_STANDARD
            .decode(body["node_pubkey"].as_str().unwrap())
            .unwrap()
            .try_into()
            .unwrap();
        let sig: [u8; 64] = BASE64_STANDARD
            .decode(body["claims_signature"].as_str().unwrap())
            .unwrap()
            .try_into()
            .unwrap();
        let canonical = crate::signed_claims::canonical_json_bytes(&body["signed_claims"]).unwrap();
        let vk = ed25519_dalek::VerifyingKey::from_bytes(&pubkey).unwrap();
        ed25519_dalek::Verifier::verify(
            &vk,
            &canonical,
            &ed25519_dalek::Signature::from_bytes(&sig),
        )
        .expect("claims_signature must verify over the wire signed_claims");
        // The event log is a JSON array the registry replays for RTMR3.
        let events = body["event_log"].as_array().expect("event_log is an array");
        assert!(!events.is_empty());
        assert_eq!(events[0]["imr"], 3);
        assert!(events[0].get("digest").is_some());
        // The method_check section carries the contract sub-fields.
        let mc = &body["method_check"];
        assert_eq!(mc["all_required_passed"], true);
        assert_eq!(mc["capabilities"]["is_archive"], true);
        assert_eq!(mc["capabilities"]["serves_proofs"], true);
        assert_eq!(mc["capabilities"]["audit_compatible"], false);
        assert_eq!(mc["results"][0]["method"], "eth_getBalance");
        assert_eq!(mc["results"][0]["probe"], "archive_state");
        assert_eq!(mc["results"][0]["latency_ms"], 42);
        assert_eq!(mc["results"][0]["error"], serde_json::Value::Null);
        assert_eq!(mc["manifest_hash"].as_str().unwrap().len(), 64);
    }

    #[tokio::test]
    async fn handler_rejects_missing_nonce_with_400() {
        // axum's Query extractor rejects the missing required param
        // (plain-text rejection body, so no JSON parse here).
        let response = test_app(DeterministicProvider::new())
            .oneshot(
                Request::builder()
                    .uri("/attestation/info")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn handler_rejects_short_nonce_with_400() {
        let b64 = BASE64_STANDARD.encode([0u8; 4]);
        let (status, body) = get_json(
            test_app(DeterministicProvider::new()),
            &format!("/attestation/info?nonce={b64}"),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"]["code"], "nonce_too_short");
    }

    /// The Envoy data-plane template. Embedded at compile time so the
    /// auth-ordering assertions below travel with the crate (CI runs
    /// them with `cargo test`, no CVM/Lua interpreter needed).
    const ENVOY_TEMPLATE: &str = include_str!("../../image/envoy.yaml");

    /// The contract requires `/attestation/*` to be served WITHOUT the
    /// node-secret bearer (so the registry verifies the RA-TLS cert
    /// before sending the secret), while RPC traffic still requires it.
    /// Assert the rendered Lua enforces exactly that ordering: the
    /// `/attestation/` early return must come BEFORE the bearer check
    /// and its 401, and the secret must be read only afterwards.
    #[test]
    fn attestation_path_is_unauthenticated_before_bearer_check() {
        let src = ENVOY_TEMPLATE;
        let attn = src
            .find(r#"if bare:sub(1, 13) == "/attestation/" then"#)
            .expect("Lua must branch on the /attestation/ prefix");
        let respond_401 = src
            .find(r#"[":status"] = "401""#)
            .expect("Lua must still 401 on a missing/invalid RPC bearer");
        let reads_secret = src
            .find(r#"getenv("NODE_SECRET")"#)
            .expect("Lua must read NODE_SECRET for the RPC bearer check");
        // The attestation branch returns before the bearer is ever
        // enforced: /attestation/info is reachable with no Authorization.
        assert!(
            attn < respond_401,
            "the /attestation/ branch must precede the 401 bearer enforcement"
        );
        // The node secret is consulted only on the RPC path, after the
        // attestation early return — attestation never depends on it.
        assert!(
            attn < reads_secret,
            "attestation must be handled before NODE_SECRET is read"
        );
        // RPC bearer enforcement is intact: both secrets are accepted
        // (rotation) and a missing/invalid one is rejected.
        assert!(src.contains(r#"getenv("NODE_SECRET_NEXT")"#));
        assert!(src.contains(r#"auth == "Bearer " .. secret"#));
    }

    #[test]
    fn quote_busy_maps_to_429() {
        // The concurrency-bound shed is a distinct status from the
        // transient dstack faults: a saturated quote pool returns 429,
        // not 503, so a flood is turned away fast without implying the
        // guest agent is unhealthy.
        let response = error_response_from(&AttestationError::QuoteBusy);
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    }

    /// The unauthenticated `/attestation/*` route must carry a local
    /// rate limit (bounding the expensive quote generation), while the
    /// RPC (`/rpc`, `/ws`) route must be entirely untouched so the
    /// provider's own 429s pass through for the registry load test.
    /// Assert against the rendered template that the token bucket is
    /// scoped to the attestation route only.
    #[test]
    fn rate_limit_scoped_to_attestation_route_only() {
        let src = ENVOY_TEMPLATE;
        // Exactly one token bucket in the whole config — the per-route
        // one. The listener-level filter is declared with none (a no-op),
        // so no RPC-path rate limiting is possible.
        assert_eq!(
            src.matches("token_bucket:").count(),
            1,
            "only the /attestation/ route may carry a rate-limit token bucket"
        );
        let attn_route = src
            .find(r#"prefix: "/attestation/""#)
            .expect("attestation route present");
        let token_bucket = src
            .find("token_bucket:")
            .expect("per-route token bucket present");
        // `prefix: "/"` (closing quote right after the slash) matches
        // only the provider catch-all, never `/attestation/`.
        let provider_route = src
            .find(r#"prefix: "/""#)
            .expect("provider catch-all route present");
        // The token bucket lives inside the attestation route block,
        // ahead of the provider catch-all: it rate-limits /attestation/*.
        assert!(
            attn_route < token_bucket && token_bucket < provider_route,
            "the token bucket must be attached to the /attestation/ route, ahead of the provider route"
        );
        // The provider route carries no rate-limit override whatsoever.
        // Match the config keys (trailing colon) so prose mentions of the
        // keywords in the trailing comments don't false-positive.
        let provider_tail = &src[provider_route..];
        assert!(
            !provider_tail.contains("token_bucket:"),
            "RPC route must have no token bucket — provider 429s must pass through unmodified"
        );
        assert!(
            !provider_tail.contains("typed_per_filter_config:"),
            "RPC route must not override the local rate-limit filter"
        );
        // The listener-level filter is declared (so the per-route
        // override resolves) but is a global no-op (no token bucket).
        assert!(
            src.contains("name: envoy.filters.http.local_ratelimit"),
            "the local_ratelimit http filter must be declared"
        );
        assert!(src.contains("stat_prefix: http_local_rate_limiter"));
        assert!(src.contains("stat_prefix: attestation_rate_limiter"));
    }

    #[tokio::test]
    async fn handler_maps_provider_failure_to_503() {
        let mut provider = DeterministicProvider::new();
        provider.fail_quote = true;
        let b64 = BASE64_STANDARD.encode([7u8; 32]);
        let encoded = b64.replace('+', "%2B").replace('=', "%3D");
        let (status, body) = get_json(
            test_app(provider),
            &format!("/attestation/info?nonce={encoded}"),
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["error"]["code"], "dstack_quote_failed");
    }
}
