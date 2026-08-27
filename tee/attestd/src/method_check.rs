//! In-enclave method checker.
//!
//! Proves — from inside the measured image — that the configured
//! upstream provider actually serves the methods the chain's
//! capability policy requires (archive-depth state, logs, proofs,
//! traces). The per-chain manifest is baked into the binary via
//! `include_str!`, so its content is covered by the image measurement;
//! `manifest_hash` (sha256 of the manifest file bytes) is published in
//! `GET /attestation/info` so the registry can pin the exact probe set.
//!
//! The probe targets mirror the registry's direct compatibility policy
//! (`registry/data/compatibility_policy.json`, ETH entry): the archive
//! probe block (`archive_probe_block`), the archive tx
//! (`archive_probe_tx_hash`), the archive log block/address
//! (`archive_probe_log_block` / `archive_probe_log_address`), the
//! `eth_getProof` depth (`serves_proofs_probe_depth_from_head`), and the
//! expected chain id (`required_chain_id`). Final capabilities are the
//! logical AND of these attested flags and the registry's own direct
//! probe.
//!
//! Probe acceptance is **shape- and content-checked**, never "any
//! non-null": an archive probe must return the specific historical
//! object it asked for (block number / tx hash echoed, log address
//! matched), a proof must carry a non-empty `accountProof` of
//! even-length RLP-node hex strings plus the account fields in hex shape
//! (20-byte address, 32-byte hashes), and a trace must be a non-empty
//! array of trace frames (or a single frame) each carrying REAL call
//! content — a parity `trace_block` shape (`action` + `result`/`error`)
//! or a geth `callTracer` shape (`from`/`to`/`gas`). `false`, `{}`,
//! `[]`, `[{}]`, `[null]`, `{"type":"CALL"}`, and `accountProof:["0x0"]`
//! all fail.
//!
//! The checker runs at boot — fail-closed: `main` exits non-zero when a
//! required check fails, so the container restarts and Envoy never
//! serves — and again every `METHOD_CHECK_INTERVAL_SECS` (default
//! 3600), storing the latest completed report for the attestation
//! endpoint. It probes **both** the HTTP `/rpc` endpoint and the WS
//! `/ws` endpoint (a handshake plus a trivial `eth_subscribe`), since
//! the registered data plane is WSS.
//!
//! The checker NEVER load-tests: it issues single sequential JSON-RPC
//! probes with a modest per-probe timeout. Rate-limit testing is
//! registry-owned by contract (it must not burn the operator's
//! provider quota).
//!
//! No secret material ever reaches a stored `error` or a log line: the
//! provider API key lives in the upstream URL, so every error derived
//! from an upstream request is passed through a [`Redactor`] before it
//! is recorded.

use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::sync::RwLock;
use tokio_tungstenite::tungstenite::Message;

use crate::redact::Redactor;

/// The baked ETH method-check manifest. `include_str!` makes the
/// manifest bytes part of the compiled binary, hence of the measured
/// image.
pub const ETH_MANIFEST_JSON: &str = include_str!("../manifests/eth.json");

/// Per-probe timeout. Modest by design — a probe that cannot answer in
/// this window fails; the checker never retries in a tight loop.
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// Placeholder substituted with `hex(head - proof_depth_from_head)`
/// before the proof probe runs.
const PROOF_BLOCK_PLACEHOLDER: &str = "__PROOF_BLOCK__";

/// The kind of probe a manifest entry runs. Decides how the JSON-RPC
/// result is validated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProbeKind {
    /// The method answers at head: a hex quantity (`eth_blockNumber`) or
    /// a block object carrying a well-formed `hash` (`eth_getBlockByNumber`
    /// at `latest`).
    Liveness,
    /// A historic block object at the policy's archive probe block; its
    /// `number` must echo the requested block and it must carry a
    /// well-formed `hash`.
    BlockByNumber,
    /// Historic state at the policy's archive probe block: a well-formed
    /// hex quantity (a pruned node errors instead of answering).
    ArchiveState,
    /// A historic transaction: its `hash` must echo the request and its
    /// `blockNumber` must be non-null (mined and retained).
    ArchiveTx,
    /// Historic logs at the policy's archive log block: a non-empty
    /// array whose every entry carries the expected `address`.
    ArchiveLogs,
    /// Trace surface: a non-empty array of trace frames (`trace_block`)
    /// or a single trace frame (`debug_traceTransaction`), where a frame
    /// carries REAL call content — a parity `trace_block` shape
    /// (`action` + `result`/`error` + a known action `type`) or a geth
    /// `callTracer` shape (`from`/`to`/`gas`). `{"type":"CALL"}` alone,
    /// `[{}]`, and arbitrary objects fail.
    Trace,
    /// `eth_getProof` at the policy depth: an object with a non-empty
    /// `accountProof` of even-length RLP-node hex strings plus the
    /// account fields — `address` a 20-byte hex, `balance`/`nonce` hex
    /// quantities, `storageHash`/`codeHash` 32-byte hashes, `storageProof`
    /// a well-shaped array. `accountProof:["0x0"]`, `[null]`, and dummy
    /// stubs fail.
    Proof,
    /// `eth_chainId` equals the manifest's expected chain id.
    ChainId,
    /// WebSocket `eth_subscribe` reply: a non-empty `0x…` subscription
    /// id. Only produced by the WS probe, never parsed from a manifest.
    WsSubscribe,
}

/// The capability flag a probe group feeds. Every entry mapped to a
/// flag must pass for the flag to derive true.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityFlag {
    IsArchive,
    ServesProofs,
    AuditCompatible,
}

/// Content expectations pinned by a manifest entry, checked against the
/// JSON-RPC result in addition to the probe-kind shape rule. Every field
/// is optional; the manifest fills in only the ones a given probe needs.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Expect {
    /// The result object's `number` must equal this (hex, case-insensitive).
    #[serde(default)]
    pub number: Option<String>,
    /// The result object's `hash` must equal this (hex, case-insensitive) —
    /// an echo of the requested block/tx identifier.
    #[serde(default)]
    pub hash: Option<String>,
    /// The result object's `blockNumber` must be present and non-null
    /// (the transaction is mined and the node retained it).
    #[serde(default)]
    pub block_number_present: bool,
    /// Every entry in the result array must carry this `address`
    /// (hex, case-insensitive).
    #[serde(default)]
    pub log_address: Option<String>,
}

/// One manifest entry: a method probe and how it feeds the derived
/// capability flags.
#[derive(Debug, Clone, Deserialize)]
pub struct ManifestEntry {
    /// JSON-RPC method name.
    pub method: String,
    /// How the result is validated.
    pub probe: ProbeKind,
    /// JSON-RPC params array. String values may contain
    /// `__PROOF_BLOCK__`, substituted with the hex block number at the
    /// manifest's proof depth from head.
    pub params: Value,
    /// Required entries feed `all_required_passed` (and the boot
    /// fail-closed gate).
    pub required: bool,
    /// The capability flag this probe feeds, if any.
    pub capability: Option<CapabilityFlag>,
    /// Content expectations checked against the result.
    #[serde(default)]
    pub expect: Expect,
}

/// The per-chain method-check manifest, baked into the binary.
#[derive(Debug, Clone, Deserialize)]
pub struct Manifest {
    /// Chain slug the manifest applies to (canonical lower-case).
    pub chain: String,
    /// Expected `eth_chainId` result, hex (e.g. `0x1`).
    pub expected_chain_id: String,
    /// Depth from head for the `eth_getProof` probe — mirrors the
    /// registry policy's `serves_proofs_probe_depth_from_head`.
    pub proof_depth_from_head: u64,
    /// The probe entries, run sequentially in order.
    pub methods: Vec<ManifestEntry>,
}

impl Manifest {
    /// Parse a manifest from its JSON bytes.
    ///
    /// # Errors
    ///
    /// Returns a parse error message when the JSON does not match the
    /// manifest shape — a build bug, fatal at boot.
    pub fn parse(manifest_json: &str) -> Result<Self, String> {
        serde_json::from_str(manifest_json).map_err(|e| format!("parse method manifest: {e}"))
    }
}

/// sha256 lowercase hex of the manifest file bytes — the
/// `method_check.manifest_hash` wire field.
#[must_use]
pub fn manifest_hash(manifest_json: &str) -> String {
    hex::encode(Sha256::digest(manifest_json.as_bytes()))
}

/// Wire shape of one probe result inside `method_check.results`.
#[derive(Debug, Clone, Serialize)]
pub struct MethodCheckResult {
    pub method: String,
    pub probe: ProbeKind,
    pub required: bool,
    pub passed: bool,
    pub latency_ms: u64,
    pub error: Option<String>,
}

/// Wire shape of `method_check.capabilities` — the attested capability
/// claim, derived per the manifest's flag mapping.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct CapabilitiesWire {
    pub is_archive: bool,
    pub serves_proofs: bool,
    pub audit_compatible: bool,
}

/// Wire shape of the `method_check` section of the attestation
/// response — the latest completed run.
#[derive(Debug, Clone, Serialize)]
pub struct MethodCheckReport {
    pub manifest_hash: String,
    pub checked_at: DateTime<Utc>,
    pub all_required_passed: bool,
    pub capabilities: CapabilitiesWire,
    pub results: Vec<MethodCheckResult>,
}

/// Shared latest-report slot: the boot check writes it, the periodic
/// re-check replaces it, and the attestation handler reads it.
pub type SharedReport = Arc<RwLock<MethodCheckReport>>;

/// The method checker: a parsed manifest bound to the provider's HTTP
/// JSON-RPC endpoint and WS endpoint.
pub struct MethodChecker {
    manifest: Manifest,
    manifest_hash: String,
    http_url: String,
    ws_url: String,
    client: reqwest::Client,
    /// Scrubs the API key out of any error string before it is stored or
    /// logged (the key is embedded in `http_url` / `ws_url`).
    redactor: Redactor,
}

impl MethodChecker {
    /// Build a checker from the baked manifest JSON and the resolved
    /// provider endpoints. `api_key` is the raw operator key; it is used
    /// only to seed the redactor (its raw and percent-encoded forms are
    /// scrubbed from every error) and is never stored in the clear.
    ///
    /// # Errors
    ///
    /// Returns an error when the manifest fails to parse or the HTTP
    /// client cannot be constructed — both fatal at boot.
    pub fn from_manifest_json(
        manifest_json: &str,
        http_url: String,
        ws_url: String,
        api_key: &str,
    ) -> Result<Self, String> {
        let manifest = Manifest::parse(manifest_json)?;
        let client = reqwest::Client::builder()
            .timeout(PROBE_TIMEOUT)
            .build()
            .map_err(|e| format!("build method-check http client: {e}"))?;
        let encoded_key = crate::upstream::encode_api_key(api_key);
        let redactor = Redactor::new(&[api_key, &encoded_key]);
        Ok(Self {
            manifest_hash: manifest_hash(manifest_json),
            manifest,
            http_url,
            ws_url,
            client,
            redactor,
        })
    }

    /// The manifest this checker runs.
    #[must_use]
    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    /// Run every manifest probe once, sequentially, then the WS probe,
    /// and build the report. Infallible by design: a probe failure is
    /// recorded in the report, not surfaced as an error — the caller
    /// decides what a failed required probe means (fatal at boot, logged
    /// on re-check).
    pub async fn run(&self) -> MethodCheckReport {
        // Resolve the proof-probe block first when any entry needs it.
        // A head-fetch failure fails those entries, not the whole run.
        let needs_head = self
            .manifest
            .methods
            .iter()
            .any(|entry| params_contain_placeholder(&entry.params));
        let proof_block = if needs_head {
            match self.fetch_head_number().await {
                Ok(head) => Ok(format!(
                    "0x{:x}",
                    head.saturating_sub(self.manifest.proof_depth_from_head)
                )),
                Err(e) => Err(format!("resolve proof block from head: {e}")),
            }
        } else {
            Ok(String::new())
        };

        let mut results = Vec::with_capacity(self.manifest.methods.len() + 1);
        for entry in &self.manifest.methods {
            let started = Instant::now();
            let params = if params_contain_placeholder(&entry.params) {
                proof_block
                    .as_ref()
                    .map(|block_hex| substitute_proof_block(&entry.params, block_hex))
                    .map_err(Clone::clone)
            } else {
                Ok(entry.params.clone())
            };
            let outcome = match params {
                Err(e) => Err(e),
                Ok(params) => match self.rpc_call(&entry.method, params).await {
                    Ok(result) => evaluate_probe(
                        entry.probe,
                        &self.manifest.expected_chain_id,
                        &entry.expect,
                        &result,
                    ),
                    Err(e) => Err(e),
                },
            };
            let latency_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
            results.push(MethodCheckResult {
                method: entry.method.clone(),
                probe: entry.probe,
                required: entry.required,
                passed: outcome.is_ok(),
                latency_ms,
                // Redact before storing: an upstream error carries the
                // API-key-bearing URL, and this string is served from
                // /attestation/info and written to world-readable logs.
                error: outcome.err().map(|e| self.redactor.redact(&e)),
            });
        }

        // WS probe: the registered data plane is WSS, so a green report
        // must also prove the provider's WebSocket surface answers.
        results.push(self.ws_probe().await);

        build_report(&self.manifest, &self.manifest_hash, results)
    }

    /// Fetch the current head block number (`eth_blockNumber`).
    async fn fetch_head_number(&self) -> Result<u64, String> {
        let result = self.rpc_call("eth_blockNumber", json!([])).await?;
        let hex_str = result
            .as_str()
            .ok_or_else(|| format!("eth_blockNumber returned non-string result: {result}"))?;
        parse_hex_quantity(hex_str)
    }

    /// One JSON-RPC call. Returns the `result` value (possibly null) or
    /// a redacted error string for transport/HTTP/JSON-RPC failures.
    async fn rpc_call(&self, method: &str, params: Value) -> Result<Value, String> {
        let body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": method,
            "params": params,
        });
        let response = self
            .client
            .post(&self.http_url)
            .json(&body)
            .send()
            .await
            // `without_url` drops the API-key-bearing request URL from
            // the error before it is ever formatted; the redactor is the
            // defence-in-depth backstop.
            .map_err(|e| {
                self.redactor
                    .redact(&format!("transport: {}", e.without_url()))
            })?;
        let status = response.status();
        if !status.is_success() {
            return Err(format!("http status {status}"));
        }
        let payload: Value = response.json().await.map_err(|e| {
            self.redactor
                .redact(&format!("decode response json: {}", e.without_url()))
        })?;
        if let Some(error) = payload.get("error") {
            let code = error.get("code").and_then(Value::as_i64).unwrap_or(0);
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            return Err(self
                .redactor
                .redact(&format!("rpc error {code}: {message}")));
        }
        Ok(payload.get("result").cloned().unwrap_or(Value::Null))
    }

    /// WS probe: open a WebSocket to the provider's `/ws` endpoint,
    /// `eth_subscribe` to `newHeads`, and require a well-formed
    /// subscription id. Bounded by `PROBE_TIMEOUT`. Errors are redacted
    /// (the WS URL carries the API key). Recorded as a required result.
    async fn ws_probe(&self) -> MethodCheckResult {
        let started = Instant::now();
        let outcome = match tokio::time::timeout(PROBE_TIMEOUT, self.ws_subscribe()).await {
            Ok(result) => result,
            Err(_elapsed) => Err(format!(
                "ws probe timed out after {}s",
                PROBE_TIMEOUT.as_secs()
            )),
        };
        let latency_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        MethodCheckResult {
            method: "eth_subscribe".to_owned(),
            probe: ProbeKind::WsSubscribe,
            required: true,
            passed: outcome.is_ok(),
            latency_ms,
            error: outcome.err().map(|e| self.redactor.redact(&e)),
        }
    }

    /// Handshake, subscribe, and validate the subscription reply.
    async fn ws_subscribe(&self) -> Result<(), String> {
        let (mut socket, _response) = tokio_tungstenite::connect_async(&self.ws_url)
            .await
            .map_err(|e| self.redactor.redact(&format!("ws connect: {e}")))?;
        let request =
            json!({"jsonrpc": "2.0", "id": 1, "method": "eth_subscribe", "params": ["newHeads"]});
        socket
            .send(Message::Text(request.to_string().into()))
            .await
            .map_err(|e| self.redactor.redact(&format!("ws send: {e}")))?;

        while let Some(message) = socket.next().await {
            let message = message.map_err(|e| self.redactor.redact(&format!("ws recv: {e}")))?;
            match message {
                Message::Text(text) => {
                    let value: Value = serde_json::from_str(text.as_str())
                        .map_err(|e| format!("ws decode: {e}"))?;
                    // Skip async subscription notifications; wait for the
                    // reply to our id=1 request.
                    if value.get("id") != Some(&json!(1)) {
                        continue;
                    }
                    if let Some(error) = value.get("error") {
                        let code = error.get("code").and_then(Value::as_i64).unwrap_or(0);
                        let msg = error
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown");
                        return Err(self.redactor.redact(&format!("ws rpc error {code}: {msg}")));
                    }
                    let result = value.get("result").cloned().unwrap_or(Value::Null);
                    let verdict = evaluate_probe(
                        ProbeKind::WsSubscribe,
                        &self.manifest.expected_chain_id,
                        &Expect::default(),
                        &result,
                    );
                    let _ = socket.close(None).await;
                    return verdict;
                }
                Message::Close(_) => {
                    return Err("ws closed before subscription reply".to_owned());
                }
                // Ping/Pong/Binary/Frame: keep waiting for the text reply.
                _ => {}
            }
        }
        Err("ws stream ended before subscription reply".to_owned())
    }
}

/// Parse a `0x…` hex quantity into a u64, for the sites that genuinely
/// need the numeric value (the proof-block head arithmetic).
///
/// Validate the SHAPE lexically first with [`is_hex_string`] — strict
/// `0x` + `[0-9a-fA-F]+` — so non-contract input like `0x+1` (which
/// `u64::from_str_radix` would otherwise accept via its leading-sign
/// handling) is rejected before parsing. A value that is well-formed but
/// exceeds 64 bits is a genuine numeric overflow here (a real chain head
/// fits in u64), reported as a parse error rather than a shape error.
/// Shape-only callers must use [`is_hex_string`] /
/// [`is_hex_quantity_string`] directly instead, so they neither accept
/// `0x+1` nor reject valid >64-bit quantities.
fn parse_hex_quantity(hex_str: &str) -> Result<u64, String> {
    if !is_hex_string(hex_str) {
        return Err(format!("malformed hex quantity: {hex_str}"));
    }
    let digits = hex_str.strip_prefix("0x").unwrap_or(hex_str);
    u64::from_str_radix(digits, 16).map_err(|e| format!("parse hex quantity {hex_str}: {e}"))
}

/// True when any string value inside `params` carries the proof-block
/// placeholder.
fn params_contain_placeholder(params: &Value) -> bool {
    match params {
        Value::String(s) => s.contains(PROOF_BLOCK_PLACEHOLDER),
        Value::Array(items) => items.iter().any(params_contain_placeholder),
        Value::Object(map) => map.values().any(params_contain_placeholder),
        _ => false,
    }
}

/// Substitute the proof-block placeholder in every string value of
/// `params` with `block_hex`.
fn substitute_proof_block(params: &Value, block_hex: &str) -> Value {
    match params {
        Value::String(s) => Value::String(s.replace(PROOF_BLOCK_PLACEHOLDER, block_hex)),
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|v| substitute_proof_block(v, block_hex))
                .collect(),
        ),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), substitute_proof_block(v, block_hex)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// A `0x…` hex string: `0x` followed by one or more ASCII hex digits and
/// nothing else. Rejects a bare `0x` (empty), `0xzz` (non-hex), and any
/// value carrying a non-hex character.
fn is_hex_string(s: &str) -> bool {
    s.strip_prefix("0x")
        .is_some_and(|digits| !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// A well-formed 32-byte block/tx hash: `0x` followed by exactly 64 hex
/// digits (2 + 64 = 66 chars). Rejects wrong-length and non-hex values.
fn is_block_hash(value: &Value) -> bool {
    value
        .as_str()
        .is_some_and(|s| s.len() == 66 && is_hex_string(s))
}

/// A well-formed hex quantity (block number, balance, subscription id):
/// `0x` followed by one or more hex digits, no non-hex characters.
fn is_hex_quantity_string(value: &Value) -> bool {
    value.as_str().is_some_and(is_hex_string)
}

/// A well-formed 20-byte EVM address: `0x` followed by exactly 40 hex
/// digits (2 + 40 = 42 chars). Rejects wrong-length and non-hex values —
/// so a trace/proof `from`/`to`/`address` field must be a real address,
/// not `0x0` or a truncated stub.
fn is_address(value: &Value) -> bool {
    value
        .as_str()
        .is_some_and(|s| s.len() == 42 && is_hex_string(s))
}

/// A plausible RLP trie node: a strict `0x…` hex string whose body is an
/// EVEN number of hex digits (whole bytes), at least 2 bytes long, whose
/// FIRST byte is an RLP list header (`0xc0..=0xff`), and whose header's
/// DECLARED payload length exactly matches the remaining byte count.
/// Every Merkle-Patricia trie node (branch / extension / leaf) serializes
/// as an RLP *list*, so a real `accountProof` / `storageProof` node
/// satisfies all of this. Rejects the trivial fabrications `"0x0"` (odd
/// nibble), `"0x0000"` (first byte not a list header), and `"0xc0ffee"`
/// (header declares an empty list but bytes follow) — without attempting
/// full RLP/Merkle verification (out of scope — that needs the state root
/// and is AND'd with the registry direct probe).
fn is_rlp_node(value: &Value) -> bool {
    value.as_str().is_some_and(|s| {
        is_hex_string(s) && {
            let digits = s.strip_prefix("0x").unwrap_or(s);
            digits.len() >= 4 && digits.len().is_multiple_of(2) && rlp_list_length_matches(digits)
        }
    })
}

/// True when `digits` (an even-length, all-hex string — the caller
/// guarantees both) starts with an RLP list header whose declared payload
/// length equals the actual remaining byte count, with a canonical
/// long-form length (`>= 56`, no leading zero byte).
fn rlp_list_length_matches(digits: &str) -> bool {
    let byte_at = |i: usize| u8::from_str_radix(&digits[2 * i..2 * i + 2], 16).unwrap_or(0);
    let total = digits.len() / 2;
    let first = byte_at(0);
    match first {
        // Short list: payload length is encoded in the header byte.
        0xc0..=0xf7 => usize::from(first - 0xc0) == total - 1,
        // Long list: `first - 0xf7` length bytes follow, big-endian.
        0xf8..=0xff => {
            let length_bytes = usize::from(first - 0xf7);
            if total < 1 + length_bytes || byte_at(1) == 0 {
                return false;
            }
            let mut declared: u64 = 0;
            for i in 1..=length_bytes {
                declared = (declared << 8) | u64::from(byte_at(i));
            }
            declared >= 56 && declared == (total - 1 - length_bytes) as u64
        }
        _ => false,
    }
}

/// A hex byte-string (calldata / init code / trace `input`): `0x` followed
/// by an EVEN number of hex digits, possibly zero (`0x` = empty calldata,
/// which a plain value transfer legitimately carries). Distinct from a hex
/// QUANTITY, which forbids an empty body and allows an odd nibble count.
fn is_hex_data(value: &Value) -> bool {
    value.as_str().is_some_and(|s| {
        s.strip_prefix("0x").is_some_and(|digits| {
            digits.bytes().all(|b| b.is_ascii_hexdigit()) && digits.len().is_multiple_of(2)
        })
    })
}

/// True when `s` is a `0x…` hex string whose every body digit is `0`
/// (`0x0`, `0x0000`, the all-zero 20/32-byte address/hash). Used to reject
/// trivially-fabricated all-zero addresses/hashes that pass the shape
/// checks but that a real account/call can never carry.
fn is_all_zero_hex(s: &str) -> bool {
    s.strip_prefix("0x")
        .is_some_and(|digits| !digits.is_empty() && digits.bytes().all(|b| b == b'0'))
}

/// A 20-byte address that is NOT the all-zero address. A real top-level
/// call `from`/`to` (and a proof `address`) is a funded account, never
/// `0x0000…0000`.
fn is_nonzero_address(value: &Value) -> bool {
    is_address(value) && !value.as_str().is_some_and(is_all_zero_hex)
}

/// A hex quantity that is NOT all-zero. A real top-level call frame has a
/// non-zero `gas` budget, so `gas:"0x0"` (a fabrication marker) fails.
fn is_nonzero_hex_quantity(value: &Value) -> bool {
    is_hex_quantity_string(value) && !value.as_str().is_some_and(is_all_zero_hex)
}

/// Require the result object to carry a well-formed 32-byte `hash` field.
fn require_hash_field(result: &Value) -> Result<(), String> {
    match result.get("hash") {
        Some(hash) if is_block_hash(hash) => Ok(()),
        Some(other) => Err(format!("malformed hash field: {other}")),
        None => Err("result object missing hash field".to_owned()),
    }
}

/// Validate a successful JSON-RPC `result` against the probe kind's
/// shape rule and the entry's content expectations. Pure —
/// unit-tested without a network.
fn evaluate_probe(
    probe: ProbeKind,
    expected_chain_id: &str,
    expect: &Expect,
    result: &Value,
) -> Result<(), String> {
    match probe {
        ProbeKind::ChainId => eval_chain_id(expected_chain_id, result),
        ProbeKind::Liveness => eval_liveness(result),
        ProbeKind::BlockByNumber => eval_block_by_number(expect, result),
        // Archive state (e.g. a historical balance) is a SHAPE-only
        // check: a pruned node errors instead of answering, so any
        // well-formed hex quantity passes. Validate lexically — this
        // rejects non-contract input like `0x+1` and, unlike a u64
        // parse, does not reject a valid quantity that exceeds 64 bits
        // (balances can). A numeric value is never needed here.
        ProbeKind::ArchiveState => {
            if is_hex_quantity_string(result) {
                Ok(())
            } else {
                Err(format!(
                    "archive state is not a well-formed hex quantity: {result}"
                ))
            }
        }
        ProbeKind::ArchiveTx => eval_archive_tx(expect, result),
        ProbeKind::ArchiveLogs => eval_archive_logs(expect, result),
        ProbeKind::Trace => eval_trace(result),
        ProbeKind::Proof => eval_proof(result),
        ProbeKind::WsSubscribe => {
            if is_hex_quantity_string(result) {
                Ok(())
            } else {
                Err(format!("malformed subscription id: {result}"))
            }
        }
    }
}

/// `eth_chainId` must equal the manifest's expected chain id.
fn eval_chain_id(expected_chain_id: &str, result: &Value) -> Result<(), String> {
    let got = result
        .as_str()
        .ok_or_else(|| format!("chain id result is not a string: {result}"))?;
    if got.eq_ignore_ascii_case(expected_chain_id) {
        Ok(())
    } else {
        Err(format!(
            "chain id mismatch: expected {expected_chain_id}, got {got}"
        ))
    }
}

/// Liveness: a hex quantity (`eth_blockNumber`) or a non-empty block
/// object carrying a well-formed `hash` (`eth_getBlockByNumber` latest).
fn eval_liveness(result: &Value) -> Result<(), String> {
    match result {
        // `eth_blockNumber` shape-only: any well-formed hex quantity
        // passes. Lexical validation rejects `0x+1` and accepts a valid
        // >64-bit quantity — no numeric value is needed for liveness.
        Value::String(s) => {
            if is_hex_string(s) {
                Ok(())
            } else {
                Err(format!("malformed liveness hex quantity: {s}"))
            }
        }
        Value::Object(map) if !map.is_empty() => require_hash_field(result),
        Value::Object(_) => Err("empty block object at head".to_owned()),
        Value::Null => Err("null result".to_owned()),
        other => Err(format!("unexpected liveness result: {other}")),
    }
}

/// A historic block object whose `number` echoes the request and which
/// carries a well-formed `hash`.
fn eval_block_by_number(expect: &Expect, result: &Value) -> Result<(), String> {
    if !result.is_object() {
        return Err(format!("block result is not an object: {result}"));
    }
    require_hash_field(result)?;
    if let Some(expected) = &expect.number {
        let got = result
            .get("number")
            .and_then(Value::as_str)
            .ok_or_else(|| "block result missing string number".to_owned())?;
        if !got.eq_ignore_ascii_case(expected) {
            return Err(format!(
                "archive block number mismatch: expected {expected}, got {got}"
            ));
        }
    }
    Ok(())
}

/// A historic transaction whose `hash` echoes the request and whose
/// `blockNumber` is present (mined and retained).
fn eval_archive_tx(expect: &Expect, result: &Value) -> Result<(), String> {
    if !result.is_object() {
        return Err(format!("tx result is not an object: {result}"));
    }
    if let Some(expected) = &expect.hash {
        let got = result
            .get("hash")
            .and_then(Value::as_str)
            .ok_or_else(|| "tx result missing string hash".to_owned())?;
        if !got.eq_ignore_ascii_case(expected) {
            return Err(format!(
                "tx hash echo mismatch: expected {expected}, got {got}"
            ));
        }
    }
    if expect.block_number_present
        && !result
            .get("blockNumber")
            .is_some_and(is_hex_quantity_string)
    {
        return Err("tx not mined (blockNumber absent/null) — not archived".to_owned());
    }
    Ok(())
}

/// A non-empty log array whose every entry carries the expected address.
fn eval_archive_logs(expect: &Expect, result: &Value) -> Result<(), String> {
    let items = result
        .as_array()
        .ok_or_else(|| format!("logs result is not an array: {result}"))?;
    if items.is_empty() {
        return Err("empty log array at archive probe block".to_owned());
    }
    if let Some(addr) = &expect.log_address {
        for (i, item) in items.iter().enumerate() {
            let got = item
                .get("address")
                .and_then(Value::as_str)
                .ok_or_else(|| format!("log[{i}] missing string address"))?;
            if !got.eq_ignore_ascii_case(addr) {
                return Err(format!(
                    "log[{i}] address mismatch: expected {addr}, got {got}"
                ));
            }
        }
    }
    Ok(())
}

/// A parity-style `trace_block` frame: an object with a known action
/// `type` whose `action` is a NON-EMPTY object carrying the REAL fields
/// that type requires, and — for `call`/`create` frames that did not
/// error — a non-empty `result` object carrying a `gasUsed` hex quantity.
///
/// This rejects the cited fabrications `{"type":"call","action":{}}` and
/// `{"type":"call","action":{},"result":{}}`: an empty `action`/`result`
/// object no longer passes. Per action type:
///
/// * `call`    — `from` a NON-ZERO 20-byte address, `to` a 20-byte address
///   (may be the burn address), `gas` hex qty, `input` hex bytes;
///   `result.gasUsed` hex qty (unless `error`).
/// * `create`  — `from` a NON-ZERO address, `gas` hex qty, `init` hex
///   bytes; `result.gasUsed` hex qty (unless `error`).
/// * `suicide` — `address`/`refundAddress` 20-byte addresses (no result).
/// * `reward`  — `author` 20-byte address, `value` hex qty (no result).
fn is_parity_trace_frame(obj: &serde_json::Map<String, Value>) -> bool {
    let Some(ty) = obj.get("type").and_then(Value::as_str) else {
        return false;
    };
    let ty = ty.to_ascii_lowercase();
    // `action` must be a NON-EMPTY object; an empty `{}` is a fabrication.
    let Some(action) = obj
        .get("action")
        .and_then(Value::as_object)
        .filter(|a| !a.is_empty())
    else {
        return false;
    };
    let addr = |k: &str| action.get(k).is_some_and(is_address);
    // Every real ETH call/create sender is a funded EOA or a contract —
    // never the all-zero address (a fabrication marker). `to` stays
    // zero-tolerant: burn transactions legitimately send to `0x0000…`.
    let nonzero_addr = |k: &str| action.get(k).is_some_and(is_nonzero_address);
    let qty = |k: &str| action.get(k).is_some_and(is_hex_quantity_string);
    let data = |k: &str| action.get(k).is_some_and(is_hex_data);
    let action_ok = match ty.as_str() {
        "call" => nonzero_addr("from") && addr("to") && qty("gas") && data("input"),
        "create" => nonzero_addr("from") && qty("gas") && data("init"),
        "suicide" => addr("address") && addr("refundAddress"),
        "reward" => addr("author") && qty("value"),
        _ => return false,
    };
    if !action_ok {
        return false;
    }
    // An errored frame carries `error` as a NON-EMPTY STRING (parity
    // serializes e.g. `"Reverted"` and OMITS the key on success — it never
    // emits `error:null`, so a null/non-string `error` is a fabrication,
    // not an errored frame). Otherwise a `call`/`create` frame must carry
    // a non-empty `result` object with a `gasUsed` hex quantity (an empty
    // `result:{}` is rejected). `suicide`/`reward` frames have no result
    // payload — the validated action is sufficient.
    if let Some(error) = obj.get("error") {
        return error.as_str().is_some_and(|e| !e.is_empty());
    }
    match ty.as_str() {
        "call" | "create" => obj
            .get("result")
            .and_then(Value::as_object)
            .is_some_and(|r| !r.is_empty() && r.get("gasUsed").is_some_and(is_hex_quantity_string)),
        _ => true,
    }
}

/// A geth `callTracer` frame: an object whose `from` is a NON-ZERO 20-byte
/// address, whose `gas` is a NON-ZERO hex quantity, and which carries
/// either a `to` address (zero-tolerant — burn transactions legitimately
/// send to `0x0000…`) or a create-kind `type` (contract creation has no
/// `to`). A real top-level call frame is a funded account spending a
/// non-zero gas budget, so the cited fabrication — all-zero `from`
/// together with `gas:"0x0"` — is rejected, as is a bare
/// `{"type":"CALL"}` with no `from`/`gas`.
fn is_calltracer_frame(obj: &serde_json::Map<String, Value>) -> bool {
    let from_ok = obj.get("from").is_some_and(is_nonzero_address);
    let gas_ok = obj.get("gas").is_some_and(is_nonzero_hex_quantity);
    let to_ok = obj.get("to").is_some_and(is_address);
    let create_ok = obj
        .get("type")
        .and_then(Value::as_str)
        .is_some_and(|ty| ty.eq_ignore_ascii_case("create") || ty.eq_ignore_ascii_case("create2"));
    from_ok && gas_ok && (to_ok || create_ok)
}

// INHERENT CEILING OF THE IN-ENCLAVE TRACE/PROOF CHECKS.
//
// `eval_trace` and `eval_proof` are STRUCTURAL liveness/capability
// signals, not cryptographic verifiers. They confirm the upstream returns
// realistically-shaped, non-fabricated trace/proof data (defeating an
// empty/all-zero/trivially-forged response and a provider that simply does
// not support the method and errors) — but they do NOT and CANNOT fully
// verify a trace or a Merkle proof: that needs the transaction plus the
// state root, which the enclave does not independently hold. This signal
// is therefore AND'd with the registry's independent direct probe (see the
// module header). It is sound within the miner-adversarial threat model —
// the miner runs measured code forwarding to a REAL configured provider,
// so the residual gap (a fully adversarial provider forging valid-looking
// data) is out of scope. The checks below defend against trivial
// fabrication and a non-supporting provider, nothing stronger.

/// A single trace frame carrying REAL call content in one of the two
/// supported shapes — parity `trace_block` (`action` + `result`/`error` +
/// known `type`) or geth `callTracer` (`from`/`to`/`gas`). A non-empty
/// `type` string alone (`{"type":"CALL"}`, `{"type":"x"}`), `{}`, and
/// `{"foo":"bar"}` all fail, so a deceptive provider cannot earn
/// `audit_compatible` with contentless frames.
fn is_trace_frame(value: &Value) -> bool {
    value
        .as_object()
        .is_some_and(|obj| is_parity_trace_frame(obj) || is_calltracer_frame(obj))
}

/// Content-checked trace surface: a non-empty array of trace frames
/// (`trace_block`) or a single trace frame (`debug_traceTransaction`
/// callTracer). Unlike "any non-empty object", `[{}]`, `[{"foo":1}]`,
/// `{}`, `[]`, `false`, and `null` all fail — a deceptive provider must
/// serve real trace data to earn `audit_compatible`.
fn eval_trace(result: &Value) -> Result<(), String> {
    match result {
        Value::Array(items) => {
            if items.is_empty() {
                Err("empty trace array".to_owned())
            } else if items.iter().all(is_trace_frame) {
                Ok(())
            } else {
                Err(
                    "trace array element is not a well-formed trace frame (missing string type)"
                        .to_owned(),
                )
            }
        }
        Value::Object(_) if is_trace_frame(result) => Ok(()),
        Value::Object(_) => {
            Err("trace object is not a well-formed trace frame (missing string type)".to_owned())
        }
        other => Err(format!("unexpected trace result: {other}")),
    }
}

/// A structurally-valid `eth_getProof` result. Content-checked, not "any
/// non-null" and not a substring test: the `accountProof` must be a
/// non-empty array whose every entry is an even-length hex string of
/// plausible RLP-node length (so `"0x0"`, `[null]`, and `[]` fail); the
/// account fields must be present with the right hex shape —
/// `address` a 20-byte hex, `balance`/`nonce` hex quantities,
/// `storageHash`/`codeHash` 32-byte hashes; `storageProof` an array (may
/// be empty for an account with no storage, but each present entry must
/// carry `key`/`value` hex and a `proof` array of even-length hex nodes).
///
/// This is structural validation, NOT full Merkle verification (that
/// needs the state root and is out of scope for the in-enclave liveness
/// signal — it is AND'd with the registry direct probe). Its purpose is
/// to reject trivial fabrications like `accountProof:["0x0"]` +
/// `address:"0x0"` + empty everything, which the old non-empty-hex check
/// accepted.
fn eval_proof(result: &Value) -> Result<(), String> {
    if !result.is_object() {
        return Err(format!("proof result is not an object: {result}"));
    }
    let account_proof = result
        .get("accountProof")
        .and_then(Value::as_array)
        .ok_or_else(|| "proof result missing accountProof array".to_owned())?;
    if account_proof.is_empty() {
        return Err("empty accountProof".to_owned());
    }
    for (i, node) in account_proof.iter().enumerate() {
        if !is_rlp_node(node) {
            return Err(format!(
                "accountProof[{i}] is not an even-length RLP-node hex string: {node}"
            ));
        }
    }
    // `address` must be a real (non-zero) 20-byte account, never `0x0`.
    require_proof_field(result, "address", is_nonzero_address)?;
    require_proof_field(result, "balance", is_hex_quantity_string)?;
    require_proof_field(result, "nonce", is_hex_quantity_string)?;
    require_proof_field(result, "storageHash", is_block_hash)?;
    require_proof_field(result, "codeHash", is_block_hash)?;
    // A real account never carries an all-zero storageHash AND codeHash:
    // an EOA has the well-known empty-trie storageHash
    // (0x56e81f171bcac1a7…) and empty-code codeHash
    // (0xc5d2460186f7233c…); a contract has real non-zero values. Both
    // all-zero is the cited fabrication and is rejected.
    let storage_hash_zero = result
        .get("storageHash")
        .and_then(Value::as_str)
        .is_some_and(is_all_zero_hex);
    let code_hash_zero = result
        .get("codeHash")
        .and_then(Value::as_str)
        .is_some_and(is_all_zero_hex);
    if storage_hash_zero && code_hash_zero {
        return Err(
            "proof storageHash and codeHash are both all-zero — fabricated account".to_owned(),
        );
    }
    let storage_proof = result
        .get("storageProof")
        .and_then(Value::as_array)
        .ok_or_else(|| "proof result missing storageProof array".to_owned())?;
    for (i, entry) in storage_proof.iter().enumerate() {
        validate_storage_proof_entry(i, entry)?;
    }
    Ok(())
}

/// One `storageProof` entry: an object with a hex `key`, a hex `value`,
/// and a `proof` array of even-length RLP-node hex strings.
fn validate_storage_proof_entry(i: usize, entry: &Value) -> Result<(), String> {
    let obj = entry
        .as_object()
        .ok_or_else(|| format!("storageProof[{i}] is not an object"))?;
    if !obj.get("key").is_some_and(is_hex_quantity_string) {
        return Err(format!("storageProof[{i}] missing/malformed hex key"));
    }
    if !obj.get("value").is_some_and(is_hex_quantity_string) {
        return Err(format!("storageProof[{i}] missing/malformed hex value"));
    }
    let proof = obj
        .get("proof")
        .and_then(Value::as_array)
        .ok_or_else(|| format!("storageProof[{i}] missing proof array"))?;
    for (j, node) in proof.iter().enumerate() {
        if !is_rlp_node(node) {
            return Err(format!(
                "storageProof[{i}].proof[{j}] is not an even-length RLP-node hex string"
            ));
        }
    }
    Ok(())
}

/// Require `field` to be present on `result` and satisfy `check` (one of
/// the strict hex helpers). Reused by [`eval_proof`] for the account
/// fields so a missing or malformed value fails the required probe.
fn require_proof_field(
    result: &Value,
    field: &str,
    check: fn(&Value) -> bool,
) -> Result<(), String> {
    match result.get(field) {
        Some(value) if check(value) => Ok(()),
        Some(value) => Err(format!("proof field {field} is malformed: {value}")),
        None => Err(format!("proof result missing {field} field")),
    }
}

/// Derive `all_required_passed` and the capability flags from the probe
/// results, per the manifest's flag mapping: a flag is true only when
/// at least one entry feeds it and every entry feeding it passed.
/// Pure — unit-tested without a network.
///
/// `results` may carry one trailing entry beyond the manifest (the WS
/// probe); it feeds `all_required_passed` but no capability flag (the
/// per-flag zip below stops at the manifest length).
fn build_report(
    manifest: &Manifest,
    manifest_hash_hex: &str,
    results: Vec<MethodCheckResult>,
) -> MethodCheckReport {
    let all_required_passed = results.iter().all(|r| !r.required || r.passed);
    let flag_state = |flag: CapabilityFlag| -> bool {
        let mut any = false;
        let mut all_passed = true;
        for (entry, result) in manifest.methods.iter().zip(results.iter()) {
            if entry.capability == Some(flag) {
                any = true;
                all_passed &= result.passed;
            }
        }
        any && all_passed
    };
    MethodCheckReport {
        manifest_hash: manifest_hash_hex.to_owned(),
        checked_at: Utc::now(),
        all_required_passed,
        capabilities: CapabilitiesWire {
            is_archive: flag_state(CapabilityFlag::IsArchive),
            serves_proofs: flag_state(CapabilityFlag::ServesProofs),
            audit_compatible: flag_state(CapabilityFlag::AuditCompatible),
        },
        results,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    const USDC: &str = "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48";
    const ARCHIVE_TX: &str = "0xea1093d492a1dcb1bef708f771a99a96ff05dcab81ca76c31940300177fcf49f";
    /// A well-formed 32-byte hash (`0x` + 64 hex digits) for block/tx
    /// `hash` fixtures — the strict validator rejects short values.
    const BLOCK_HASH: &str = "0x8e38b4dc8e38b4dc8e38b4dc8e38b4dc8e38b4dc8e38b4dc8e38b4dc8e38b4dc";

    fn result_for(entry: &ManifestEntry, passed: bool) -> MethodCheckResult {
        MethodCheckResult {
            method: entry.method.clone(),
            probe: entry.probe,
            required: entry.required,
            passed,
            latency_ms: 1,
            error: if passed {
                None
            } else {
                Some("boom".to_owned())
            },
        }
    }

    /// Manifest results plus a passing trailing WS probe, mirroring the
    /// real `run()` shape.
    fn results_with_ws(
        manifest: &Manifest,
        per_entry: impl Fn(&ManifestEntry) -> bool,
    ) -> Vec<MethodCheckResult> {
        let mut out: Vec<_> = manifest
            .methods
            .iter()
            .map(|e| result_for(e, per_entry(e)))
            .collect();
        out.push(MethodCheckResult {
            method: "eth_subscribe".to_owned(),
            probe: ProbeKind::WsSubscribe,
            required: true,
            passed: true,
            latency_ms: 1,
            error: None,
        });
        out
    }

    #[test]
    fn baked_eth_manifest_parses() {
        let manifest = Manifest::parse(ETH_MANIFEST_JSON).expect("baked manifest must parse");
        assert_eq!(manifest.chain, "eth");
        assert_eq!(manifest.expected_chain_id, "0x1");
        // Mirrors the registry policy's serves_proofs_probe_depth_from_head.
        assert_eq!(manifest.proof_depth_from_head, 8191);
        assert!(!manifest.methods.is_empty());
        // The archive probe targets mirror registry/data/compatibility_policy.json.
        let balance = manifest
            .methods
            .iter()
            .find(|m| m.method == "eth_getBalance")
            .expect("archive state probe present");
        assert_eq!(balance.probe, ProbeKind::ArchiveState);
        assert_eq!(balance.capability, Some(CapabilityFlag::IsArchive));
        // archive_probe_block 1000000 = 0xf4240.
        assert!(balance.params.to_string().contains("0xf4240"));
        // The archive block probe echoes the requested block number.
        let block = manifest
            .methods
            .iter()
            .find(|m| m.method == "eth_getBlockByNumber" && m.probe == ProbeKind::BlockByNumber)
            .expect("archive block probe present");
        assert_eq!(block.expect.number.as_deref(), Some("0xf4240"));
        // The archive tx probe pins archive_probe_tx_hash and mined-ness.
        let tx = manifest
            .methods
            .iter()
            .find(|m| m.method == "eth_getTransactionByHash")
            .expect("archive tx probe present");
        assert_eq!(tx.probe, ProbeKind::ArchiveTx);
        assert_eq!(tx.expect.hash.as_deref(), Some(ARCHIVE_TX));
        assert!(tx.expect.block_number_present);
        // The logs probe pins archive_probe_log_address (USDC) and
        // archive_probe_log_block 16000000 = 0xf42400.
        let logs = manifest
            .methods
            .iter()
            .find(|m| m.method == "eth_getLogs")
            .expect("archive logs probe present");
        assert_eq!(logs.expect.log_address.as_deref(), Some(USDC));
        assert!(logs.params.to_string().contains("0xf42400"));
        let proof = manifest
            .methods
            .iter()
            .find(|m| m.method == "eth_getProof")
            .expect("proof probe present");
        assert_eq!(proof.capability, Some(CapabilityFlag::ServesProofs));
        assert!(params_contain_placeholder(&proof.params));
    }

    #[test]
    fn manifest_hash_is_sha256_of_bytes() {
        let hash = manifest_hash("hello");
        // sha256("hello")
        assert_eq!(
            hash,
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
        assert_eq!(manifest_hash(ETH_MANIFEST_JSON).len(), 64);
    }

    #[test]
    fn malformed_manifest_rejected() {
        assert!(Manifest::parse("{").is_err());
        assert!(Manifest::parse(r#"{"chain":"eth"}"#).is_err());
        // Unknown probe kinds must be rejected, not silently skipped.
        let bad = r#"{
            "chain": "eth", "expected_chain_id": "0x1", "proof_depth_from_head": 1,
            "methods": [{"method": "m", "probe": "load_test", "params": [],
                         "required": true, "capability": null}]
        }"#;
        assert!(Manifest::parse(bad).is_err());
    }

    #[test]
    fn capability_derivation_follows_flag_mapping() {
        let manifest = Manifest::parse(ETH_MANIFEST_JSON).unwrap();
        // All pass -> all flags true, all_required_passed true.
        let report = build_report(&manifest, "h", results_with_ws(&manifest, |_| true));
        assert!(report.all_required_passed);
        assert!(report.capabilities.is_archive);
        assert!(report.capabilities.serves_proofs);
        assert!(report.capabilities.audit_compatible);
    }

    #[test]
    fn one_failed_archive_probe_clears_is_archive_only() {
        let manifest = Manifest::parse(ETH_MANIFEST_JSON).unwrap();
        let report = build_report(
            &manifest,
            "h",
            results_with_ws(&manifest, |e| e.method != "eth_getLogs"),
        );
        // eth_getLogs is required, so the required gate trips too.
        assert!(!report.all_required_passed);
        assert!(!report.capabilities.is_archive);
        assert!(report.capabilities.serves_proofs);
        assert!(report.capabilities.audit_compatible);
    }

    #[test]
    fn optional_trace_failure_keeps_required_gate_green() {
        let manifest = Manifest::parse(ETH_MANIFEST_JSON).unwrap();
        let report = build_report(
            &manifest,
            "h",
            results_with_ws(&manifest, |e| e.probe != ProbeKind::Trace),
        );
        assert!(
            report.all_required_passed,
            "trace probes are optional in the ETH manifest"
        );
        assert!(!report.capabilities.audit_compatible);
        assert!(report.capabilities.is_archive);
    }

    #[test]
    fn failed_ws_probe_trips_required_gate() {
        let manifest = Manifest::parse(ETH_MANIFEST_JSON).unwrap();
        let mut results = results_with_ws(&manifest, |_| true);
        // Flip the trailing WS probe to failed.
        if let Some(ws) = results.last_mut() {
            ws.passed = false;
            ws.error = Some("ws connect: refused".to_owned());
        }
        let report = build_report(&manifest, "h", results);
        assert!(
            !report.all_required_passed,
            "WS is a required data-plane probe"
        );
        // A WS failure does not clear the HTTP-derived capability flags.
        assert!(report.capabilities.is_archive);
    }

    #[test]
    fn unmapped_flag_derives_false() {
        let manifest = Manifest::parse(
            r#"{
                "chain": "eth", "expected_chain_id": "0x1", "proof_depth_from_head": 1,
                "methods": [{"method": "eth_blockNumber", "probe": "liveness",
                             "params": [], "required": true, "capability": null}]
            }"#,
        )
        .unwrap();
        let report = build_report(&manifest, "h", results_with_ws(&manifest, |_| true));
        assert!(report.all_required_passed);
        // No entry feeds any flag: a capability nothing probed must not
        // be claimed.
        assert!(!report.capabilities.is_archive);
        assert!(!report.capabilities.serves_proofs);
        assert!(!report.capabilities.audit_compatible);
    }

    #[test]
    fn proof_block_placeholder_substitution() {
        let params = json!(["0xabc", [], "__PROOF_BLOCK__"]);
        assert!(params_contain_placeholder(&params));
        let out = substitute_proof_block(&params, "0x152061e");
        assert_eq!(out, json!(["0xabc", [], "0x152061e"]));
        assert!(!params_contain_placeholder(&out));
        // Nested objects are covered too.
        let nested = json!([{ "block": "__PROOF_BLOCK__" }]);
        let out = substitute_proof_block(&nested, "0x1");
        assert_eq!(out, json!([{ "block": "0x1" }]));
    }

    #[test]
    fn evaluate_chain_id() {
        let e = Expect::default();
        assert!(evaluate_probe(ProbeKind::ChainId, "0x1", &e, &json!("0x1")).is_ok());
        assert!(evaluate_probe(ProbeKind::ChainId, "0x1", &e, &json!("0x38")).is_err());
        assert!(evaluate_probe(ProbeKind::ChainId, "0x1", &e, &json!(1)).is_err());
    }

    #[test]
    fn evaluate_liveness_shape() {
        let e = Expect::default();
        // eth_blockNumber: a hex quantity passes, a bare number fails.
        assert!(evaluate_probe(ProbeKind::Liveness, "0x1", &e, &json!("0x1234")).is_ok());
        assert!(evaluate_probe(ProbeKind::Liveness, "0x1", &e, &json!("nope")).is_err());
        // getBlockByNumber(latest): object with a well-formed hash passes.
        assert!(evaluate_probe(
            ProbeKind::Liveness,
            "0x1",
            &e,
            &json!({"number": "0x10", "hash": BLOCK_HASH})
        )
        .is_ok());
        // A short (non-32-byte) hash fails the strict validator.
        assert!(evaluate_probe(
            ProbeKind::Liveness,
            "0x1",
            &e,
            &json!({"number": "0x10", "hash": "0xabc123"})
        )
        .is_err());
        // "any non-null" no longer passes: false / {} / null all fail.
        assert!(evaluate_probe(ProbeKind::Liveness, "0x1", &e, &json!(false)).is_err());
        assert!(evaluate_probe(ProbeKind::Liveness, "0x1", &e, &json!({})).is_err());
        assert!(evaluate_probe(ProbeKind::Liveness, "0x1", &e, &Value::Null).is_err());
        // An object without a hash fails.
        assert!(evaluate_probe(ProbeKind::Liveness, "0x1", &e, &json!({"number": "0x1"})).is_err());
    }

    #[test]
    fn evaluate_block_by_number_echo() {
        let expect = Expect {
            number: Some("0xf4240".to_owned()),
            ..Expect::default()
        };
        // Correct echo + hash passes.
        assert!(evaluate_probe(
            ProbeKind::BlockByNumber,
            "0x1",
            &expect,
            &json!({"number": "0xf4240", "hash": BLOCK_HASH})
        )
        .is_ok());
        // Wrong block number (a node that answered a different block) fails.
        assert!(evaluate_probe(
            ProbeKind::BlockByNumber,
            "0x1",
            &expect,
            &json!({"number": "0x1", "hash": BLOCK_HASH})
        )
        .is_err());
        // Missing hash fails; null fails.
        assert!(evaluate_probe(
            ProbeKind::BlockByNumber,
            "0x1",
            &expect,
            &json!({"number": "0xf4240"})
        )
        .is_err());
        assert!(evaluate_probe(ProbeKind::BlockByNumber, "0x1", &expect, &Value::Null).is_err());
    }

    #[test]
    fn evaluate_archive_state_quantity() {
        let e = Expect::default();
        // Historical state answered as a hex quantity — 0x0 is valid.
        assert!(evaluate_probe(ProbeKind::ArchiveState, "0x1", &e, &json!("0x0")).is_ok());
        assert!(evaluate_probe(ProbeKind::ArchiveState, "0x1", &e, &json!("0xde0b6b")).is_ok());
        // A valid quantity exceeding 64 bits (balances can) passes the
        // SHAPE check — the old u64 parse rejected these outright.
        let over_u64 = format!("0x{}", "f".repeat(20)); // 80-bit value
        assert!(evaluate_probe(ProbeKind::ArchiveState, "0x1", &e, &json!(over_u64)).is_ok());
        // `0x+1`: the '+' is not a contract hex digit — u64::from_str_radix
        // would have accepted it, strict lexical validation rejects it.
        assert!(evaluate_probe(ProbeKind::ArchiveState, "0x1", &e, &json!("0x+1")).is_err());
        // null (pruned/no answer), bare 0x, and non-hex fail.
        assert!(evaluate_probe(ProbeKind::ArchiveState, "0x1", &e, &Value::Null).is_err());
        assert!(evaluate_probe(ProbeKind::ArchiveState, "0x1", &e, &json!("0x")).is_err());
        assert!(evaluate_probe(ProbeKind::ArchiveState, "0x1", &e, &json!("balance")).is_err());
    }

    #[test]
    fn evaluate_liveness_strict_hex_quantity() {
        let e = Expect::default();
        // A small valid quantity passes.
        assert!(evaluate_probe(ProbeKind::Liveness, "0x1", &e, &json!("0x1")).is_ok());
        // A valid >64-bit quantity passes the shape-only liveness check.
        let over_u64 = format!("0x{}", "a".repeat(18)); // 72-bit value
        assert!(evaluate_probe(ProbeKind::Liveness, "0x1", &e, &json!(over_u64)).is_ok());
        // `0x+1` (the '+' sign) is rejected; bare 0x is rejected.
        assert!(evaluate_probe(ProbeKind::Liveness, "0x1", &e, &json!("0x+1")).is_err());
        assert!(evaluate_probe(ProbeKind::Liveness, "0x1", &e, &json!("0x")).is_err());
    }

    #[test]
    fn evaluate_archive_tx_echo_and_mined() {
        let expect = Expect {
            hash: Some(ARCHIVE_TX.to_owned()),
            block_number_present: true,
            ..Expect::default()
        };
        assert!(evaluate_probe(
            ProbeKind::ArchiveTx,
            "0x1",
            &expect,
            &json!({"hash": ARCHIVE_TX, "blockNumber": "0x1e8480"})
        )
        .is_ok());
        // Unmined tx (blockNumber null) fails: a pending tx is not archive proof.
        assert!(evaluate_probe(
            ProbeKind::ArchiveTx,
            "0x1",
            &expect,
            &json!({"hash": ARCHIVE_TX, "blockNumber": Value::Null})
        )
        .is_err());
        // Wrong hash echo fails; null fails.
        assert!(evaluate_probe(
            ProbeKind::ArchiveTx,
            "0x1",
            &expect,
            &json!({"hash": "0xdead", "blockNumber": "0x1"})
        )
        .is_err());
        assert!(evaluate_probe(ProbeKind::ArchiveTx, "0x1", &expect, &Value::Null).is_err());
    }

    #[test]
    fn evaluate_logs_content() {
        let expect = Expect {
            log_address: Some(USDC.to_owned()),
            ..Expect::default()
        };
        // Non-empty array of logs at the expected address passes
        // (case-insensitive on the address).
        assert!(evaluate_probe(
            ProbeKind::ArchiveLogs,
            "0x1",
            &expect,
            &json!([{"address": USDC.to_uppercase()}, {"address": USDC}])
        )
        .is_ok());
        // Empty array fails.
        assert!(evaluate_probe(ProbeKind::ArchiveLogs, "0x1", &expect, &json!([])).is_err());
        // A log for a different address fails (content mismatch).
        assert!(evaluate_probe(
            ProbeKind::ArchiveLogs,
            "0x1",
            &expect,
            &json!([{"address": "0xdeadbeef"}])
        )
        .is_err());
        // Non-array fails.
        assert!(evaluate_probe(ProbeKind::ArchiveLogs, "0x1", &expect, &json!("nope")).is_err());
    }

    /// A well-formed parity `trace_block` frame: `action` object +
    /// `result` object + a known action `type`.
    fn valid_trace_frame() -> Value {
        json!({
            "type": "call",
            "action": {"from": USDC, "to": USDC, "gas": "0x2710", "input": "0xa9059cbb", "value": "0x0", "callType": "call"},
            "result": {"gasUsed": "0x5208", "output": "0x"},
            "traceAddress": [],
            "subtraces": 0
        })
    }

    /// A well-formed parity `reward` frame — carries an `action` with
    /// `author`/`value` and no `result` (`trace_block` returns one at the
    /// end of a block; the checker must accept it, not just call frames).
    fn valid_reward_frame() -> Value {
        json!({
            "type": "reward",
            "action": {"author": USDC, "value": "0x1bc16d674ec80000", "rewardType": "block"},
            "result": null,
            "traceAddress": [],
            "subtraces": 0
        })
    }

    /// A well-formed geth `callTracer` frame: 20-byte `from`/`to` and a
    /// `gas` hex quantity.
    fn valid_calltracer_frame() -> Value {
        json!({
            "type": "CALL",
            "from": USDC,
            "to": USDC,
            "gas": "0x2710",
            "gasUsed": "0x5208",
            "input": "0x"
        })
    }

    #[test]
    fn evaluate_trace_shape() {
        let e = Expect::default();
        // A well-formed parity frame (array) passes.
        assert!(evaluate_probe(ProbeKind::Trace, "0x1", &e, &json!([valid_trace_frame()])).is_ok());
        // A well-formed callTracer frame passes (both single and in an array).
        assert!(evaluate_probe(ProbeKind::Trace, "0x1", &e, &valid_calltracer_frame()).is_ok());
        assert!(evaluate_probe(
            ProbeKind::Trace,
            "0x1",
            &e,
            &json!([valid_calltracer_frame()])
        )
        .is_ok());
        // A contract-creation callTracer frame (no `to`, create-kind type) passes.
        assert!(evaluate_probe(
            ProbeKind::Trace,
            "0x1",
            &e,
            &json!({"type": "CREATE", "from": USDC, "gas": "0x2710"})
        )
        .is_ok());
        // A reward frame (as trace_block emits at end-of-block) passes,
        // including a mixed array of a call frame and a reward frame.
        assert!(evaluate_probe(ProbeKind::Trace, "0x1", &e, &valid_reward_frame()).is_ok());
        assert!(evaluate_probe(
            ProbeKind::Trace,
            "0x1",
            &e,
            &json!([valid_trace_frame(), valid_reward_frame()])
        )
        .is_ok());
        // `{"type":"CALL"}` alone — a non-empty `type` string with no call
        // content — must now FAIL (this was accepted before round 9).
        assert!(evaluate_probe(ProbeKind::Trace, "0x1", &e, &json!({"type": "CALL"})).is_err());
        assert!(evaluate_probe(ProbeKind::Trace, "0x1", &e, &json!([{"type": "CALL"}])).is_err());
        assert!(evaluate_probe(ProbeKind::Trace, "0x1", &e, &json!({"type": "x"})).is_err());
        // The cited parity fabrication — empty action + empty result
        // objects — must FAIL (both `{}` are contentless).
        assert!(evaluate_probe(
            ProbeKind::Trace,
            "0x1",
            &e,
            &json!({"type": "call", "action": {}, "result": {}})
        )
        .is_err());
        assert!(evaluate_probe(
            ProbeKind::Trace,
            "0x1",
            &e,
            &json!({"type": "call", "action": {}})
        )
        .is_err());
        // A parity `call` action missing `to`/`input` (or an empty result)
        // fails even with a `result` present.
        assert!(evaluate_probe(
            ProbeKind::Trace,
            "0x1",
            &e,
            &json!({"type": "call", "action": {"from": USDC, "gas": "0x2710"}, "result": {"gasUsed": "0x0"}})
        )
        .is_err());
        assert!(evaluate_probe(
            ProbeKind::Trace,
            "0x1",
            &e,
            &json!({"type": "call", "action": {"from": USDC, "to": USDC, "gas": "0x2710", "input": "0x"}, "result": {}})
        )
        .is_err());
        // A parity frame missing its `result`/`error` fails.
        assert!(evaluate_probe(
            ProbeKind::Trace,
            "0x1",
            &e,
            &json!({"type": "call", "action": {"from": USDC, "gas": "0x0"}})
        )
        .is_err());
        // The cited callTracer fabrication — all-zero from/to + gas:0x0 —
        // must FAIL (a real top-level frame has a funded from and non-zero
        // gas).
        assert!(evaluate_probe(
            ProbeKind::Trace,
            "0x1",
            &e,
            &json!({
                "type": "CALL",
                "from": "0x0000000000000000000000000000000000000000",
                "to": "0x0000000000000000000000000000000000000000",
                "gas": "0x0"
            })
        )
        .is_err());
        // A callTracer frame with a real from/to but gas:0x0 still fails
        // (zero gas budget is the fabrication marker).
        assert!(evaluate_probe(
            ProbeKind::Trace,
            "0x1",
            &e,
            &json!({"type": "CALL", "from": USDC, "to": USDC, "gas": "0x0"})
        )
        .is_err());
        // A callTracer frame missing `from`/`gas` fails.
        assert!(evaluate_probe(ProbeKind::Trace, "0x1", &e, &json!([{"to": USDC}])).is_err());
        // empty array / empty object / false / null all fail.
        assert!(evaluate_probe(ProbeKind::Trace, "0x1", &e, &json!([])).is_err());
        assert!(evaluate_probe(ProbeKind::Trace, "0x1", &e, &json!({})).is_err());
        assert!(evaluate_probe(ProbeKind::Trace, "0x1", &e, &json!(false)).is_err());
        assert!(evaluate_probe(ProbeKind::Trace, "0x1", &e, &Value::Null).is_err());
        // Array with a non-object element fails.
        assert!(evaluate_probe(ProbeKind::Trace, "0x1", &e, &json!([1, 2])).is_err());
        // `[{}]` (empty objects) must fail — a deceptive provider must not
        // earn audit_compatible with contentless traces.
        assert!(evaluate_probe(ProbeKind::Trace, "0x1", &e, &json!([{}])).is_err());
        // An arbitrary non-empty object without call content fails.
        assert!(evaluate_probe(ProbeKind::Trace, "0x1", &e, &json!([{"foo": "bar"}])).is_err());
        assert!(evaluate_probe(ProbeKind::Trace, "0x1", &e, &json!({"foo": "bar"})).is_err());
    }

    #[test]
    fn evaluate_trace_error_field_and_zero_from() {
        let e = Expect::default();
        // An errored parity frame carries `error` as a non-empty STRING —
        // that passes without a `result`.
        {
            let mut f = valid_trace_frame();
            f.as_object_mut().unwrap().remove("result");
            f["error"] = json!("Reverted");
            assert!(evaluate_probe(ProbeKind::Trace, "0x1", &e, &f).is_ok());
        }
        // `error:null` (and an empty/non-string `error`) is a fabrication,
        // not an errored frame — parity omits the key on success and never
        // emits null. It must not skip the `result` validation.
        for bad_error in [json!(null), json!(""), json!({}), json!(0)] {
            let mut f = valid_trace_frame();
            f.as_object_mut().unwrap().remove("result");
            f["error"] = bad_error;
            assert!(evaluate_probe(ProbeKind::Trace, "0x1", &e, &f).is_err());
        }
        // `error:null` alongside a fabricated empty `result` also fails.
        {
            let mut f = valid_trace_frame();
            f["result"] = json!({});
            f["error"] = json!(null);
            assert!(evaluate_probe(ProbeKind::Trace, "0x1", &e, &f).is_err());
        }
        // A parity `call`/`create` whose `from` is the all-zero address is
        // a fabrication — a real sender is a funded EOA or a contract.
        {
            let mut f = valid_trace_frame();
            f["action"]["from"] = json!("0x0000000000000000000000000000000000000000");
            assert!(evaluate_probe(ProbeKind::Trace, "0x1", &e, &f).is_err());
        }
        // But an all-zero `to` stays accepted: burn transactions
        // legitimately send to the zero address.
        {
            let mut f = valid_trace_frame();
            f["action"]["to"] = json!("0x0000000000000000000000000000000000000000");
            assert!(evaluate_probe(ProbeKind::Trace, "0x1", &e, &f).is_ok());
        }
        // The same burn tolerance applies to the geth callTracer shape: a
        // real frame with a non-zero from and gas but an all-zero `to`
        // passes; an all-zero `from` still fails.
        {
            let mut f = valid_calltracer_frame();
            f["to"] = json!("0x0000000000000000000000000000000000000000");
            assert!(evaluate_probe(ProbeKind::Trace, "0x1", &e, &f).is_ok());
            f["from"] = json!("0x0000000000000000000000000000000000000000");
            assert!(evaluate_probe(ProbeKind::Trace, "0x1", &e, &f).is_err());
        }
    }

    /// A syntactically-valid RLP list node: header declaring exactly
    /// `payload_len` payload bytes, then that many filler bytes — so the
    /// length-prefix check passes without hand-maintaining hex literals.
    fn rlp_node(payload_len: usize) -> String {
        let body = "ab".repeat(payload_len);
        if payload_len <= 55 {
            format!("0x{:02x}{body}", 0xc0 + payload_len)
        } else if payload_len <= 0xff {
            format!("0xf8{payload_len:02x}{body}")
        } else {
            format!("0xf9{payload_len:04x}{body}")
        }
    }

    /// A fully-formed `eth_getProof` result: non-empty hex `accountProof`
    /// plus the account fields in hex shape.
    fn valid_proof() -> Value {
        json!({
            "address": USDC,
            "accountProof": [rlp_node(529), rlp_node(2)],
            "balance": "0x0",
            "codeHash": BLOCK_HASH,
            "nonce": "0x1",
            "storageHash": BLOCK_HASH,
            "storageProof": []
        })
    }

    #[test]
    fn evaluate_proof() {
        let e = Expect::default();
        // A structurally-valid proof (even-length RLP-ish nodes, 20-byte
        // address, 32-byte hashes) passes.
        assert!(evaluate_probe(ProbeKind::Proof, "0x1", &e, &valid_proof()).is_ok());
        // A proof carrying a non-empty storageProof entry passes.
        {
            let mut p = valid_proof();
            p["storageProof"] = json!([{
                "key": "0x0",
                "value": "0x0",
                "proof": [rlp_node(529), rlp_node(2)]
            }]);
            assert!(evaluate_probe(ProbeKind::Proof, "0x1", &e, &p).is_ok());
        }
        // The cited fabrication — accountProof:["0x0"] (odd-length/1 nibble),
        // address:"0x0", empty everything — must FAIL. This was accepted by
        // the old non-empty-hex check.
        assert!(evaluate_probe(
            ProbeKind::Proof,
            "0x1",
            &e,
            &json!({
                "accountProof": ["0x0"],
                "address": "0x0",
                "balance": "0x0",
                "nonce": "0x0",
                "storageHash": "0x0",
                "codeHash": "0x0",
                "storageProof": []
            })
        )
        .is_err());
        // Just the odd-length accountProof node "0x0" on an otherwise-valid
        // proof fails (odd nibble count can't be an RLP node).
        {
            let mut p = valid_proof();
            p["accountProof"] = json!(["0x0"]);
            assert!(evaluate_probe(ProbeKind::Proof, "0x1", &e, &p).is_err());
        }
        // The cited fabrication "0x0000" (even length, but first byte 0x00
        // is not an RLP list header) fails — a real trie node is an RLP
        // list (first byte >= 0xc0).
        {
            let mut p = valid_proof();
            p["accountProof"] = json!(["0x0000"]);
            assert!(evaluate_probe(ProbeKind::Proof, "0x1", &e, &p).is_err());
        }
        // A list header whose DECLARED payload length doesn't match the
        // actual byte count fails: "0xc0ffee" declares an empty list but
        // carries 2 bytes; "0xf90211a0" declares 529 payload bytes but
        // carries 1.
        for bad in ["0xc0ffee", "0xf90211a0"] {
            let mut p = valid_proof();
            p["accountProof"] = json!([bad]);
            assert!(evaluate_probe(ProbeKind::Proof, "0x1", &e, &p).is_err());
        }
        // A non-canonical long-form header (declared length < 56 must use
        // the short form) fails.
        {
            let mut p = valid_proof();
            p["accountProof"] = json!(["0xf801ab"]);
            assert!(evaluate_probe(ProbeKind::Proof, "0x1", &e, &p).is_err());
        }
        // The zero address is not a real account, even with valid RLP nodes
        // and hashes.
        {
            let mut p = valid_proof();
            p["address"] = json!("0x0000000000000000000000000000000000000000");
            assert!(evaluate_probe(ProbeKind::Proof, "0x1", &e, &p).is_err());
        }
        // Both storageHash AND codeHash all-zero (32-byte shaped) is a
        // fabricated account and fails — a real EOA carries the well-known
        // non-zero empty-trie / empty-code hashes.
        {
            let mut p = valid_proof();
            p["storageHash"] = json!(format!("0x{}", "0".repeat(64)));
            p["codeHash"] = json!(format!("0x{}", "0".repeat(64)));
            assert!(evaluate_probe(ProbeKind::Proof, "0x1", &e, &p).is_err());
        }
        // `{"accountProof":[null]}` must fail — a null node is not usable
        // proof data.
        assert!(evaluate_probe(
            ProbeKind::Proof,
            "0x1",
            &e,
            &json!({"accountProof": [null]})
        )
        .is_err());
        // A non-hex / odd-length node string fails.
        {
            let mut p = valid_proof();
            p["accountProof"] = json!(["0xzz"]);
            assert!(evaluate_probe(ProbeKind::Proof, "0x1", &e, &p).is_err());
        }
        // Empty accountProof array fails.
        assert!(evaluate_probe(ProbeKind::Proof, "0x1", &e, &json!({"accountProof": []})).is_err());
        // Empty object fails.
        assert!(evaluate_probe(ProbeKind::Proof, "0x1", &e, &json!({})).is_err());
        // Missing account fields fail even with a valid accountProof.
        assert!(evaluate_probe(
            ProbeKind::Proof,
            "0x1",
            &e,
            &json!({"accountProof": [rlp_node(4)]})
        )
        .is_err());
        // A non-20-byte address fails.
        {
            let mut p = valid_proof();
            p["address"] = json!("0xabc");
            assert!(evaluate_probe(ProbeKind::Proof, "0x1", &e, &p).is_err());
        }
        // A malformed account field (storageHash not a 32-byte hash) fails.
        {
            let mut p = valid_proof();
            p["storageHash"] = json!("0xabc");
            assert!(evaluate_probe(ProbeKind::Proof, "0x1", &e, &p).is_err());
        }
        // storageProof must be an array, not an object.
        {
            let mut p = valid_proof();
            p["storageProof"] = json!({});
            assert!(evaluate_probe(ProbeKind::Proof, "0x1", &e, &p).is_err());
        }
        // A storageProof entry missing its `proof` node array fails.
        {
            let mut p = valid_proof();
            p["storageProof"] = json!([{"key": "0x0", "value": "0x0"}]);
            assert!(evaluate_probe(ProbeKind::Proof, "0x1", &e, &p).is_err());
        }
    }

    #[test]
    fn evaluate_ws_subscription_id() {
        let e = Expect::default();
        // A subscription id is a non-empty 0x string.
        assert!(evaluate_probe(
            ProbeKind::WsSubscribe,
            "0x1",
            &e,
            &json!("0x9cef478923ff08bf")
        )
        .is_ok());
        // false / {} / empty-string / null fail.
        assert!(evaluate_probe(ProbeKind::WsSubscribe, "0x1", &e, &json!(false)).is_err());
        assert!(evaluate_probe(ProbeKind::WsSubscribe, "0x1", &e, &json!("0x")).is_err());
        assert!(evaluate_probe(ProbeKind::WsSubscribe, "0x1", &e, &Value::Null).is_err());
    }

    #[test]
    fn hex_quantity_parsing() {
        assert_eq!(parse_hex_quantity("0xf4240").unwrap(), 1_000_000);
        assert!(parse_hex_quantity("f4240").is_err());
        assert!(parse_hex_quantity("0xzz").is_err());
        // `0x+1`: u64::from_str_radix would accept the leading '+' sign
        // and return 1 — the lexical pre-check rejects it (non-contract).
        assert!(parse_hex_quantity("0x+1").is_err());
        // Bare `0x` (empty body) is rejected by the lexical pre-check.
        assert!(parse_hex_quantity("0x").is_err());
        // A well-formed but >64-bit value is a genuine numeric overflow
        // at this (numeric) call site — reported as a parse error, not a
        // shape error. Shape-only callers must not use this function.
        assert!(parse_hex_quantity(&format!("0x{}", "f".repeat(20))).is_err());
    }

    #[test]
    fn strict_hex_string_validation() {
        // Well-formed: 0x + one-or-more hex digits, any case.
        assert!(is_hex_string("0x1"));
        assert!(is_hex_string("0xf4240"));
        assert!(is_hex_string("0xDEADbeef"));
        assert!(is_hex_string(BLOCK_HASH));
        // Rejected: non-hex chars, empty body, missing prefix.
        assert!(!is_hex_string("0xzz"));
        assert!(!is_hex_string("0x"));
        assert!(!is_hex_string("0x1g"));
        assert!(!is_hex_string("f4240"));
        assert!(!is_hex_string("0x 1"));
    }

    #[test]
    fn strict_block_hash_validation() {
        // Exactly 32 bytes (0x + 64 hex) passes; uppercase digits pass.
        assert!(is_block_hash(&json!(BLOCK_HASH)));
        assert!(is_block_hash(&json!(format!(
            "0x{}",
            ARCHIVE_TX[2..].to_uppercase()
        ))));
        // 0xzz, bare 0x, wrong-length, and non-string all fail.
        assert!(!is_block_hash(&json!("0xzz")));
        assert!(!is_block_hash(&json!("0x")));
        assert!(!is_block_hash(&json!("0xabc123"))); // too short
        assert!(!is_block_hash(&json!(format!("{BLOCK_HASH}ab")))); // too long
                                                                    // 64 chars but a non-hex digit fails.
        assert!(!is_block_hash(&json!(format!("0x{}", "z".repeat(64)))));
        assert!(!is_block_hash(&json!(42)));
    }

    #[test]
    fn strict_hex_quantity_string_validation() {
        // A valid quantity of any non-zero length passes; uppercase passes.
        assert!(is_hex_quantity_string(&json!("0x0")));
        assert!(is_hex_quantity_string(&json!("0x1e8480")));
        assert!(is_hex_quantity_string(&json!("0xABCDEF")));
        // 0xzz, bare 0x, and non-string fail.
        assert!(!is_hex_quantity_string(&json!("0xzz")));
        assert!(!is_hex_quantity_string(&json!("0x")));
        assert!(!is_hex_quantity_string(&json!(1)));
    }

    #[test]
    fn strict_hash_field_rejects_malformed_required_responses() {
        let expect = Expect::default();
        // Block hash: 0xzz must fail the required block-hash check.
        assert!(evaluate_probe(
            ProbeKind::Liveness,
            "0x1",
            &expect,
            &json!({"number": "0x1", "hash": "0xzz"})
        )
        .is_err());
        // Archive-tx blockNumber: 0xzz must fail the mined-ness check.
        let tx_expect = Expect {
            hash: Some(ARCHIVE_TX.to_owned()),
            block_number_present: true,
            ..Expect::default()
        };
        assert!(evaluate_probe(
            ProbeKind::ArchiveTx,
            "0x1",
            &tx_expect,
            &json!({"hash": ARCHIVE_TX, "blockNumber": "0xzz"})
        )
        .is_err());
        // WS subscription id: 0xzz and bare 0x must both fail.
        assert!(evaluate_probe(ProbeKind::WsSubscribe, "0x1", &expect, &json!("0xzz")).is_err());
        assert!(evaluate_probe(ProbeKind::WsSubscribe, "0x1", &expect, &json!("0x")).is_err());
        // A valid uppercase subscription id still passes.
        assert!(evaluate_probe(
            ProbeKind::WsSubscribe,
            "0x1",
            &expect,
            &json!("0x9CEF478923FF08BF")
        )
        .is_ok());
    }
}
