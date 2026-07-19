# BlockMachine TEE Proxy Miner (in-enclave runtime)

Run a BlockMachine miner as a measured proxy inside a Phala dstack CVM
(Intel TDX) that forwards JSON-RPC to an approved upstream provider,
instead of operating a blockchain node directly. Phase 1: **ETH via
dRPC** only.

Contract: `blockmachine_playground/docs/tee-proxy-miners.md` (wire
contract, env vars, trust model). Architecture is a direct adaptation
of gm-miner's attestation stack.

## Layout

- `attestd/` — Rust crate with two binaries:
  - `bm-tee-attestd` — serves `GET /attestation/info?nonce=` (TDX quote,
    dstack measurements, in-enclave method-check results) and runs the
    method checker at boot (fail-closed) and every
    `METHOD_CHECK_INTERVAL_SECS` (default 3600).
  - `bm-tee-ratls` — one-shot RA-TLS cert provisioner (phase-2 ready).
  - `attestd/manifests/eth.json` — the baked method-check manifest;
    its sha256 is the `manifest_hash` in the attestation response.
- `image/` — container image: digest-pinned multi-stage Dockerfile,
  `envoy.yaml` template, `start.sh` entrypoint. The provider upstream
  hosts are hardcoded in `start.sh` and `attestd` (inside the measured
  compose hash).
- `dstack/` — the compose template submitted to Phala Cloud, plus the
  Phala pre-launch script.
- `scripts/derive_compose_hash.py` — offline `compose_hash`
  derivation (canonical app_compose JSON → sha256), used to publish
  approved hashes to the registry before release.

## Operator deploy flow

1. **Get the release image** (or build it reproducibly yourself):

   ```bash
   cd tee/
   docker buildx build -f image/Dockerfile -t <registry>/bm-tee-miner:<ver> --push .
   # Resolve the digest-pinned ref:
   docker buildx imagetools inspect <registry>/bm-tee-miner:<ver>
   IMAGE_REF="<registry>/bm-tee-miner@sha256:<digest>"
   ```

2. **Render the compose** for your chain/provider:

   ```bash
   ./scripts/derive_compose_hash.py --image-ref "$IMAGE_REF" \
       --chain eth --provider drpc --render > docker-compose.rendered.yaml
   ```

3. **Write the env file** (encrypted client-side to the CVM key by
   `phala deploy`; never measured):

   ```bash
   cat > .env <<EOF
   PROVIDER_API_KEY=<your dRPC API key>
   NODE_SECRET=$(openssl rand -hex 32)
   NODE_SECRET_NEXT=
   EOF
   ```

   All **three** keys must be present, including `NODE_SECRET_NEXT=`
   with an empty value on a first deploy. Phala derives the measured
   `allowed_envs` from the keys declared in this `.env`, and
   `derive_compose_hash.py` assumes all three
   (`PROVIDER_API_KEY`, `NODE_SECRET`, `NODE_SECRET_NEXT`) — so omitting
   `NODE_SECRET_NEXT` here would drop it from `allowed_envs` and produce
   a **different** `compose_hash` than the tool computes (and than the
   registry allowlist expects). Listing it with an empty value keeps the
   key present without enabling rotation: the hash covers env-var
   *names*, not values, and attestd treats an absent or empty
   `NODE_SECRET_NEXT` as "no rotation secret", so this default deploy
   both reproduces the approved hash and boots. For secret rotation
   later, set `NODE_SECRET_NEXT` to a real value, roll the
   registry/gateway over, then promote it to `NODE_SECRET`.

4. **Deploy to Phala Cloud** (requires the `phala` CLI and a Phala
   Cloud API key). Use the **pinned** CLI version — `derive_compose_hash.py`
   mirrors `PHALA_CLI_VERSION` (currently `0.1.15`); a different CLI can
   reorder/add `app_compose` fields and move the hash. You MUST also pin
   the OS image with `--image dstack-0.5.3` (the `OS_IMAGE_NAME` the tool
   pins): the boot measurement set (`mr_td`, `rtmr0..2`) is bound to the
   exact base image, so omitting the flag lets Phala pick a default image
   and drifts those measurements off the registry allowlist:

   ```bash
   phala deploy \
     --name <app-name> \
     --image dstack-0.5.3 \
     --compose docker-compose.rendered.yaml \
     --env-file .env \
     --pre-launch-script dstack/prelaunch.sh
   ```

   `./scripts/derive_compose_hash.py` prints the exact `--image` value to
   pin (from `OS_IMAGE_NAME`) alongside the computed hash, so the deploy
   and the approved registry row stay on the same base image.

   The container fails closed: it refuses to start on an unknown
   (CHAIN, PROVIDER) combo, a missing key/secret, or a failed required
   method check, and Envoy never serves until the boot check passes.

5. **Verify the hashes** against the registry allowlist. Compute the
   expected `compose_hash` offline and compare it with the deployed
   CVM's measurement and with the registry's approved
   `tee_image_versions` row for `(eth, drpc)`:

   ```bash
   ./scripts/derive_compose_hash.py --image-ref "$IMAGE_REF" \
       --chain eth --provider drpc
   ```

   If your hash is not on the allowlist the registry will reject the
   registration — do not proceed with a mismatched image.

   > The tool pins the Phala CLI version and the OS image
   > (`OS_IMAGE_NAME` / `OS_IMAGE_HASH`; `os_image_hash` is bound by the
   > boot measurement set, not `compose_hash`). A byte-for-byte check of
   > the tool's output against a real `phala prepare` artifact is a
   > **bring-up dependency**: drop the real `app-compose.json` into
   > `scripts/testdata/` (see `GOLDEN_APP_COMPOSE`) to lock it in CI. It
   > cannot be produced offline, so the golden self-test is skipped until
   > then.

6. **Register the node** with the BM CLI, pointing at the Phala
   **`-8080s` TLS-passthrough** endpoint (NOT the CA-signed `-8080`
   form):

   ```bash
   bm miner add --kind tee \
     --endpoint wss://<app-id>-8080s.<phala-domain> \
     --secret <NODE_SECRET> ...
   ```

   The enclave terminates TLS itself with its own self-signed **RA-TLS**
   certificate — the cert's key is bound into a TDX quote
   (`report_data = SHA-512("ratls-cert:" || SPKI_DER)`). You MUST
   register the `-8080s` passthrough URL: the dstack gateway forwards the
   raw TLS stream so that enclave-bound cert reaches the caller
   unmodified. The default `-8080` form terminates TLS at a CA-signed
   edge the operator controls and hides the RA-TLS binding — registering
   it makes the node unverifiable (and the gateway un-routable), so it
   must not be used.

   The registry fetches `GET /attestation/info` itself over a client that
   does **no WebPKI/hostname validation**. That route is
   **unauthenticated**: Envoy strips any inbound `Authorization` header on
   `/attestation/*` before the bearer check, so the registry sends **no
   node secret** to capture the attestation — it verifies the served
   RA-TLS leaf cert against the quote (no CA chain is validated) before
   trusting the channel, then pins its fingerprint alongside `tee_pubkey`.
   It verifies the quote via dcap-qvl, replays the event log to derive
   `compose_hash`, checks the derived `(compose_hash, os_image_hash)`
   against the allowlist, reads the attested method-check capabilities,
   then runs its own direct probe and rate-limit load test over that
   RA-TLS-pinned channel — those `/rpc` and `/ws` probes DO carry the
   `Authorization: Bearer <NODE_SECRET>` header, the only routes that
   require it. Final capabilities are the AND of the attested and directly
   probed flags.

## Notes

- The in-enclave method checker never load-tests (single sequential
  probes only) — it will not burn your provider quota. The registry's
  load test is an open-loop rate burst over the RA-TLS-pinned channel and
  does hit your provider key, so size your dRPC plan accordingly.
  Anti-synthesis does not rest on the load pattern itself but on that
  attested channel terminating in approved, non-caching code; a trusted
  archive reference is consulted only as a recent-block **correctness**
  cross-check, not as an anti-gaming signal.
- `Authorization: Bearer <NODE_SECRET>` is required on the **RPC**
  routes (proxied `/rpc` and `/ws`). `/attestation/info` is
  **unauthenticated** — it serves only public attestation data, so the
  registry can capture and verify the served RA-TLS cert before it ever
  sends the node secret.
- Adding a chain/provider = new hardcoded upstream entry
  (`attestd/src/upstream.rs` + `image/start.sh`), a new manifest, a new
  image release, and new `tee_image_versions` rows.

## Development

```bash
cd tee/
cargo build && cargo test
cargo clippy --all-targets -- -D warnings
./scripts/derive_compose_hash.py --self-test

# Render + validate the envoy config without a CVM:
CHAIN=eth PROVIDER=drpc PROVIDER_API_KEY=k NODE_SECRET=s \
  BM_START_RENDER_ONLY=1 BM_ENVOY_TEMPLATE_PATH=image/envoy.yaml \
  BM_RENDERED_CONFIG=/tmp/envoy.rendered.yaml bash image/start.sh
```
