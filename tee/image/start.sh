#!/usr/bin/env bash
# BlockMachine TEE proxy-miner container entrypoint.
#
# Startup runs the required one-shot gates, then launches two co-located
# long-running processes:
#
#   1. bm-tee-ratls (one-shot) — mints the data-plane RA-TLS
#      certificate via dstack's GetTlsKey RPC and writes the key/cert
#      PEM files to /tmp/bm-ratls/. Envoy's :8080 ingress listener
#      terminates TLS with these files (its DownstreamTlsContext), so
#      the node serves its enclave-bound cert on the Phala `-8080s`
#      TLS-passthrough endpoint. This step must finish before envoy
#      starts; a dstack failure aborts the container.
#   2. bm-tee-attestd — bootstraps the TEE-bound keypair, runs the boot
#      method check against the provider upstream (fail-closed: it
#      exits non-zero when a required probe fails), then serves
#      GET /attestation/info on loopback :8081. This script waits for
#      attestd to start listening BEFORE launching envoy, so the data
#      plane never serves while required checks are failing.
#   3. envoy — the data plane on :8080. Enforces the node-secret bearer
#      token on the RPC routes, forwards /attestation/* to attestd
#      UNAUTHENTICATED (public attestation data only), and proxies
#      everything else (HTTP JSON-RPC and WebSocket) to the hardcoded
#      provider upstream.
#
# Provider upstream: hardcoded per (chain, provider) below, NOT taken
# from a runtime env var. CHAIN and PROVIDER are rendered literals in
# dstack/docker-compose.yaml — part of the attestation-measured
# compose_hash — so a miner cannot point the proxy at a different
# upstream without moving the hash, which the registry's
# tee_image_versions allowlist rejects. Only the API key and the node
# secrets come from operator env (encrypted client-side to the CVM key
# by `phala deploy`; unmeasured).
#
# Process supervision: attestd and envoy both run in the background;
# this script stays PID 1 and `wait -n`s on both. When either exits the
# whole container exits non-zero so the runtime's `restart:
# unless-stopped` policy recreates the stack — a miner missing either
# process cannot serve the registry, so crashing fast and recovering is
# the correct behaviour. The exit log names which process died and its
# status, so a genuine crash is diagnosable from `phala cvms logs`.

set -euo pipefail

log() { printf '[start] %s\n' "$*" >&2; }

# ── Required configuration ────────────────────────────────────────────
: "${CHAIN:?CHAIN must be set (rendered into dstack/docker-compose.yaml)}"
: "${PROVIDER:?PROVIDER must be set (rendered into dstack/docker-compose.yaml)}"

if [[ -z "${PROVIDER_API_KEY:-}" ]]; then
  log "error: PROVIDER_API_KEY must be set (operator env, encrypted to the CVM)"
  exit 1
fi
if [[ -z "${NODE_SECRET:-}" ]]; then
  # The BM contract enforces the bearer token on every RPC route (only
  # /attestation/* is unauthenticated public attestation data), so an
  # unset secret would leave the data plane's RPC surface open — a
  # configuration error, fail closed.
  log "error: NODE_SECRET must be set (operator env, encrypted to the CVM)"
  exit 1
fi

# ── Validate the node-secret charset ──────────────────────────────────
# Envoy enforces the node secret as opaque bearer DATA: its Lua filter
# reads NODE_SECRET / NODE_SECRET_NEXT from the process environment with
# os.getenv and NEVER interpolates them into config or script text, so a
# secret value cannot alter the auth logic whatever it contains. As a
# second, agreeing constraint (the registry applies the same rule and
# attestd re-checks it in measured code), restrict the secret to a narrow
# alphabet with no shell/YAML/JSON/URL-special characters and fail closed
# on any violation.
validate_node_secret() {
  local name="$1" value="$2"
  if [[ "${value}" == *[!A-Za-z0-9._-]* ]]; then
    log "error: ${name} contains a character outside the allowed alphabet [A-Za-z0-9._-]"
    exit 1
  fi
}
validate_node_secret "NODE_SECRET" "${NODE_SECRET}"
if [[ -n "${NODE_SECRET_NEXT:-}" ]]; then
  validate_node_secret "NODE_SECRET_NEXT" "${NODE_SECRET_NEXT}"
fi
# Ensure both secrets are visible to the envoy and attestd child
# processes' environments (os.getenv in the Lua filter; charset re-check
# in attestd). They arrive via the compose env passthrough; export makes
# the inheritance explicit and independent of shell settings.
export NODE_SECRET
[[ -n "${NODE_SECRET_NEXT:-}" ]] && export NODE_SECRET_NEXT

# ── Resolve the provider upstream host ────────────────────────────────
# The (chain, provider) -> upstream allowlist. Mirrors the Rust-side
# allowlist in attestd/src/upstream.rs — attestd probes
#   https://lb.drpc.live/ethereum/$PROVIDER_API_KEY
# and envoy proxies to the same host, rewriting every RPC request onto
# the /ethereum/<key> path at request time (Lua filter; dRPC uses the
# same path for HTTP and WebSocket). Unknown combinations refuse to
# start.
case "${CHAIN}:${PROVIDER}" in
  eth:drpc)
    BM_PROVIDER_HOST="lb.drpc.live"
    ;;
  *)
    log "error: unsupported (CHAIN, PROVIDER) combination '${CHAIN}:${PROVIDER}' — this image supports: eth:drpc"
    exit 1
    ;;
esac
log "provider upstream: https://${BM_PROVIDER_HOST} (chain=${CHAIN} provider=${PROVIDER})"

if [[ -n "${NODE_SECRET_NEXT:-}" ]]; then
  log "NODE_SECRET_NEXT set — envoy accepts both secrets (rotation window)"
fi

# ── Render the envoy config ───────────────────────────────────────────
# Literal token replaces (awk index/substr, not gsub) so values with
# regex- or replacement-special characters are handled verbatim. The
# rendered config goes to a writable path; the baked-in
# /etc/envoy/envoy.yaml stays untouched.
#
# ONLY the provider upstream host is rendered — resolved above from the
# hardcoded per-(chain, provider) allowlist. NO secret is ever rendered:
#   - The node secrets (NODE_SECRET / NODE_SECRET_NEXT) are read by the
#     Lua filter from the process environment at request time (os.getenv).
#     Interpolating a secret into the config text is exactly the
#     injection this avoids — a crafted secret could otherwise close the
#     Lua literal and disable auth on every route. The rendered config is
#     therefore byte-identical regardless of the secret values.
#   - The PROVIDER_API_KEY is likewise read from the environment (and
#     percent-encoded) by the Lua filter at request time.
RENDERED_CONFIG="${BM_RENDERED_CONFIG:-/tmp/envoy.rendered.yaml}"
BM_PROVIDER_HOST="${BM_PROVIDER_HOST}" \
  awk '
  function subst(line, token, value,    out, rest, pos) {
    out = ""
    rest = line
    while ((pos = index(rest, token)) > 0) {
      out = out substr(rest, 1, pos - 1) value
      rest = substr(rest, pos + length(token))
    }
    return out rest
  }
  BEGIN {
    provider_host = ENVIRON["BM_PROVIDER_HOST"]
  }
  {
    print subst($0, "__BM_PROVIDER_HOST__", provider_host)
  }
' "${BM_ENVOY_TEMPLATE_PATH:-/etc/envoy/envoy.yaml}" >"${RENDERED_CONFIG}"

BM_IMAGE_VERSION="${BM_IMAGE_VERSION:-unknown}"
log "image version: ${BM_IMAGE_VERSION}"

if [[ "${BM_START_RENDER_ONLY:-}" == "1" ]]; then
  log "render-only mode complete: ${RENDERED_CONFIG}"
  exit 0
fi

# ── Provision the data-plane RA-TLS certificate ───────────────────────
# One-shot: bm-tee-ratls calls the dstack guest agent's GetTlsKey RPC
# (over /var/run/dstack.sock) with usage_ra_tls=true and writes the PEM
# key/cert to /tmp/bm-ratls/. Envoy's :8080 ingress DownstreamTlsContext
# references those exact paths, so this step must finish before envoy
# starts — envoy fails to bind a TLS listener if the files are absent. A
# dstack failure here is fatal: bm-tee-ratls exits non-zero, `set -e`
# aborts the container, and the runtime's `restart: unless-stopped`
# policy retries the whole startup — the same fail-fast posture attestd
# uses for its own dstack calls.
log "minting data-plane RA-TLS certificate via dstack get_tls_key"
bm-tee-ratls
log "RA-TLS certificate ready"

# ── Launch the attestation server ─────────────────────────────────────
# bm-tee-attestd binds 127.0.0.1:8081 (envoy's `attestd` cluster
# target), fetches TDX quotes over /var/run/dstack.sock, and runs the
# boot method check against the provider upstream before it starts
# listening. The socket is bind-mounted by the dstack compose; without
# it attestd exits at startup and the container comes down.
ATTESTD_BIND_ADDR="127.0.0.1:8081"
export BM_ATTESTD_BIND_ADDR="${ATTESTD_BIND_ADDR}"
log "starting attestation server on ${ATTESTD_BIND_ADDR}"
bm-tee-attestd &
ATTESTD_PID=$!

# ── Gate envoy on attestd readiness ───────────────────────────────────
# attestd only binds its listener AFTER the boot method check passes
# (fail-closed). Waiting for the listener here means envoy — the public
# data plane — never serves a single request while a required method
# check is failing. The bound is generous: the boot check runs up to
# ~10 sequential probes with a 10s per-probe timeout, plus the dstack
# keypair/info round-trips.
log "waiting for attestd to pass its boot method check"
ATTESTD_READY=0
for _ in $(seq 1 300); do
  if ! kill -0 "${ATTESTD_PID}" 2>/dev/null; then
    ATTESTD_STATUS=0
    wait "${ATTESTD_PID}" 2>/dev/null || ATTESTD_STATUS=$?
    log "error: attestd exited during boot (status ${ATTESTD_STATUS}) — stopping container"
    exit 1
  fi
  if (exec 3<>"/dev/tcp/127.0.0.1/8081") 2>/dev/null; then
    exec 3>&- || true
    ATTESTD_READY=1
    break
  fi
  sleep 1
done
if [[ "${ATTESTD_READY}" -ne 1 ]]; then
  log "error: attestd did not become ready within the boot window — stopping container"
  kill -TERM "${ATTESTD_PID}" 2>/dev/null || true
  wait 2>/dev/null || true
  exit 1
fi
log "attestd ready — boot method check passed"

# ── Launch envoy ──────────────────────────────────────────────────────
# Not `exec`d: the script stays PID 1 so it can supervise both
# processes. SIGTERM from the container runtime is forwarded to both.
log "starting envoy"
envoy \
  -c "${RENDERED_CONFIG}" \
  --log-level warn \
  --drain-time-s 10 &
ENVOY_PID=$!

# shellcheck disable=SC2317,SC2329  # invoked indirectly via the trap below.
shutdown() {
  log "received signal — shutting down"
  kill -TERM "${ENVOY_PID}" "${ATTESTD_PID}" 2>/dev/null || true
}
trap shutdown TERM INT

# ── Supervise both processes ──────────────────────────────────────────
# `wait -n` blocks until *either* child exits, then returns that child's
# status. It must run in this (the main) shell: `wait` can only reap a
# shell's own children. Whichever process exits first, the container
# must come down so the runtime's `restart: unless-stopped` policy
# recreates the whole stack: a miner missing either envoy or attestd
# cannot serve the registry.
#
# `|| FIRST_EXIT_STATUS=$?` captures the exited child's status AND keeps
# `set -e` from aborting the script the instant a process exits
# non-zero — without it the diagnostic block below never runs and the
# exit cause is never logged.
FIRST_EXIT_STATUS=0
wait -n "${ATTESTD_PID}" "${ENVOY_PID}" || FIRST_EXIT_STATUS=$?

# Name the process that exited so the log states the real cause. `kill
# -0` succeeds only while a pid is still alive; the dead one is the one
# that triggered the `wait -n` return.
if ! kill -0 "${ATTESTD_PID}" 2>/dev/null; then
  log "error: attestation server exited (status ${FIRST_EXIT_STATUS}) — stopping container"
elif ! kill -0 "${ENVOY_PID}" 2>/dev/null; then
  log "error: envoy exited (status ${FIRST_EXIT_STATUS}) — stopping container"
else
  log "error: a supervised process exited (status ${FIRST_EXIT_STATUS}) — stopping container"
fi

# Stop the survivor and reap it before exiting.
kill -TERM "${ATTESTD_PID}" "${ENVOY_PID}" 2>/dev/null || true
wait 2>/dev/null || true

# Always exit non-zero so the container runtime's `restart:
# unless-stopped` policy recreates the stack. A supervised process
# exiting *at all* — even with a clean status 0 — leaves the miner
# missing one of its two required services, which is a failure.
if [[ "${FIRST_EXIT_STATUS}" -ne 0 ]]; then
  exit "${FIRST_EXIT_STATUS}"
fi
exit 1
