#!/usr/bin/env bash
# Deploy the BlockMachine TEE proxy-miner image to Phala Cloud, exactly the
# way a real miner would. Renders the measured compose, derives the approved
# compose_hash offline, writes the operator .env, and drives `phala deploy`.
#
# This does NOT need the registry: it is the miner-side deploy flow. Verify the
# deployed CVM afterwards with scripts/verify_deployed_cvm.py, which plays the
# registry + gateway against the live endpoint.
#
# Prerequisites (one-time):
#   - Docker image pushed to a registry Phala can pull, PINNED BY DIGEST:
#       docker buildx build --platform linux/amd64 -f image/Dockerfile \
#         -t ghcr.io/<you>/bm-tee-miner:<tag> --push .
#       IMAGE_REF=ghcr.io/<you>/bm-tee-miner@sha256:<digest of that push>
#   - Phala CLI installed + authenticated:
#       npm install -g phala && phala auth login <your Phala Cloud API key>
#
# Required env:
#   IMAGE_REF         digest-pinned image ref (ghcr.io/...@sha256:...)
#   PROVIDER_API_KEY  your dRPC ETH API key
#   NODE_SECRET       the bearer clients present (also used by the verifier)
# Optional env:
#   CHAIN (default eth)  PROVIDER (default drpc)
#   NODE_SECRET_NEXT     second bearer for rotation
#   CVM_NAME (default bm-tee-miner-test)
#   VCPU (default 2)  MEMORY_MB (default 2048)  DISK_GB (default 20)

set -euo pipefail
cd "$(dirname "$0")/.."   # tee/

log() { printf '\033[1m[deploy]\033[0m %s\n' "$*" >&2; }
die() { printf '\033[31m[deploy] error:\033[0m %s\n' "$*" >&2; exit 1; }

: "${IMAGE_REF:?IMAGE_REF must be set (ghcr.io/...@sha256:<digest>)}"
: "${PROVIDER_API_KEY:?PROVIDER_API_KEY must be set (your dRPC key)}"
: "${NODE_SECRET:?NODE_SECRET must be set (the client bearer)}"
CHAIN="${CHAIN:-eth}"
PROVIDER="${PROVIDER:-drpc}"
CVM_NAME="${CVM_NAME:-bm-tee-miner-test}"
VCPU="${VCPU:-2}"
MEMORY_MB="${MEMORY_MB:-2048}"
DISK_GB="${DISK_GB:-20}"

case "${IMAGE_REF}" in
  *@sha256:*) : ;;
  *) die "IMAGE_REF must be digest-pinned (…@sha256:<64 hex>), not a mutable tag" ;;
esac

OUT_DIR="${OUT_DIR:-./.deploy}"
mkdir -p "${OUT_DIR}"
COMPOSE_OUT="${OUT_DIR}/docker-compose.rendered.yaml"
ENV_OUT="${OUT_DIR}/.env"

# ── 1. Render the measured compose (image ref + chain/provider literals) ──
log "rendering compose for (${CHAIN}, ${PROVIDER}) @ ${IMAGE_REF}"
python3 scripts/derive_compose_hash.py \
  --image-ref "${IMAGE_REF}" --chain "${CHAIN}" --provider "${PROVIDER}" \
  --render > "${COMPOSE_OUT}"

# ── 2. Derive the approved compose_hash offline (publish to the registry) ──
COMPOSE_HASH="$(python3 scripts/derive_compose_hash.py \
  --image-ref "${IMAGE_REF}" --chain "${CHAIN}" --provider "${PROVIDER}" 2>/dev/null)"
# The dstack OS image the boot measurements (mr_td/rtmr0..2) are bound to. The
# deploy MUST pin it — a default image boots with different measurements that
# fall off the registry allowlist.
OS_IMAGE="$(python3 - <<'PY'
import sys; sys.path.insert(0, "scripts")
from derive_compose_hash import OS_IMAGE_NAME
print(OS_IMAGE_NAME)
PY
)"
log "approved compose_hash: ${COMPOSE_HASH}"
log "pinned dstack OS image: ${OS_IMAGE}  (boot measurements are bound to it)"
log "  (publish the hash + the CVM's boot measurements to tee_image_versions)"

# ── 3. Write the operator env (encrypted client-side to the CVM by Phala) ──
umask 077
{
  echo "PROVIDER_API_KEY=${PROVIDER_API_KEY}"
  echo "NODE_SECRET=${NODE_SECRET}"
  echo "NODE_SECRET_NEXT=${NODE_SECRET_NEXT:-}"
} > "${ENV_OUT}"
log "wrote operator env: ${ENV_OUT} (0600)"

# ── 4. Deploy to Phala Cloud ──────────────────────────────────────────────
# The exact `phala` sub-command/flags vary by CLI version, so by default we
# only PRINT the command for you to review + run. Set DEPLOY=1 to execute it.
# Recent `phala` (npm) uses `phala cvms create`; confirm with `phala cvms create --help`.
DEPLOY_CMD=(phala cvms create
  --name "${CVM_NAME}"
  --compose "${COMPOSE_OUT}"
  --env-file "${ENV_OUT}"
  --image "${OS_IMAGE}"
  --vcpu "${VCPU}"
  --memory "${MEMORY_MB}"
  --disk-size "${DISK_GB}")

if [[ "${DEPLOY:-0}" == "1" ]]; then
  command -v phala >/dev/null || die "phala CLI not found — npm install -g phala && phala auth login <key>"
  log "deploying CVM '${CVM_NAME}' (${VCPU} vCPU, ${MEMORY_MB}MB, ${DISK_GB}GB)…"
  set -x
  "${DEPLOY_CMD[@]}"
  set +x
else
  log "DRY RUN — rendered compose + env are ready; review, then run the deploy:"
  printf '    %q ' "${DEPLOY_CMD[@]}" >&2; echo >&2
  log "re-run with DEPLOY=1 to execute (after confirming the flags for your phala version)"
fi

cat >&2 <<EOF

Next steps:
  1. Find the app id + endpoint:   phala cvms list
     Use the TLS-passthrough endpoint form:
       wss://<app-id>-8080s.<node>.phala.network
     (the -8080s form — NOT -8080 — so the enclave RA-TLS cert reaches clients)
  2. Verify it end-to-end (plays registry + gateway):
       NODE_SECRET='<your NODE_SECRET>' \\
       TEE_REFERENCE_ETH_URL=wss://<a trusted archive RPC> \\
       ./scripts/verify_deployed_cvm.py wss://<app-id>-8080s.<node>.phala.network
  3. Compare the verifier's printed compose_hash against:
       ${COMPOSE_HASH}
     They MUST match — that proves the deployed image is the measured one.
EOF
