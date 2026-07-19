## BlockMachine TEE proxy-miner image — multi-stage, digest-pinned,
## reproducible build. Mirrors gm-miner's image/Dockerfile.
##
## Build context is the tee/ directory:
##   docker buildx build -f image/Dockerfile .
##
## Reproducibility status:
##   - Base images are pinned by SHA256 digest: fully reproducible given
##     the same Rust toolchain and dependency lock.
##   - Rust binaries are built with RUSTFLAGS="-C codegen-units=1
##     -C debuginfo=0" which eliminates the most common sources of
##     non-determinism (parallel codegen, DWARF timestamps).
##   - SOURCE_DATE_EPOCH is set to a fixed epoch so embedded build
##     timestamps are constant.
##   - Remaining non-determinism: Docker layer timestamps (not part of
##     the attestation measurement; the compose_hash covers only the
##     app_compose wrapper around dstack/docker-compose.yaml) and any
##     platform-dependent entropy in LLVM's code layout.

## ── Stage 1: Rust builder ──────────────────────────────────────────────
## Pinned to the rust:1.96-slim-bookworm multi-arch index digest (same
## pin as gm-miner). The locked dependency tree (dstack-sdk -> alloy)
## needs rustc 1.88+.
## Update the pin via: docker buildx imagetools inspect rust:1.96-slim-bookworm
FROM rust:1.96-slim-bookworm@sha256:b5f842fac1e3b4ff718a652a8e0173b62d9403ec826ef4998880b9347db30684 AS builder

## Reproducibility: fixed build timestamp, single codegen unit.
ENV SOURCE_DATE_EPOCH=1700000000
ENV RUSTFLAGS="-C codegen-units=1 -C debuginfo=0"
ENV CARGO_INCREMENTAL=0

WORKDIR /build

## Copy workspace manifests first for layer caching.
COPY Cargo.toml Cargo.lock ./
COPY attestd/Cargo.toml ./attestd/

## Copy the method-check manifests required by include_str! at compile
## time.
COPY attestd/manifests/ ./attestd/manifests/

## Pre-build a dummy workspace to cache dependencies. The attestd crate
## ships a lib and two bins (bm-tee-attestd, bm-tee-ratls), so the
## dummy tree needs an entry file for each declared target.
RUN mkdir -p attestd/src/bin \
  && echo "fn main() {}" > attestd/src/main.rs \
  && echo "fn main() {}" > attestd/src/bin/ratls.rs \
  && echo "" > attestd/src/lib.rs \
  && cargo build --release --workspace 2>/dev/null || true \
  && rm -rf attestd/src

## Copy real sources.
COPY attestd/ ./attestd/

## Build the workspace. Touch the entry points to invalidate the dummy
## build cache so the real sources compile.
RUN touch attestd/src/main.rs attestd/src/lib.rs attestd/src/bin/ratls.rs \
  && cargo build --release --workspace \
  && strip target/release/bm-tee-attestd target/release/bm-tee-ratls

## ── Stage 2: Runtime image ────────────────────────────────────────────
## Envoy pinned by digest (same pin as gm-miner).
## Update the pin via: docker manifest inspect envoyproxy/envoy:v1.38-latest
FROM envoyproxy/envoy:v1.38-latest@sha256:4e3c734cbe9892a3513c97cf3974ff73cfef5d784f5cceed9ea758a2b80e0c31

## ca-certificates is already present in the envoy base image; nothing
## else needs to be installed — the attestation server talks to the
## dstack guest agent over a unix socket and is reached by envoy over
## loopback, so no extra runtime packages are required.

## Image version — set at build time by the build pipeline.
ARG BM_IMAGE_VERSION=dev
ENV BM_IMAGE_VERSION=${BM_IMAGE_VERSION}

## Copy the attestation server. start.sh launches it alongside envoy;
## envoy routes /attestation/* to it on loopback.
COPY --from=builder /build/target/release/bm-tee-attestd /usr/local/bin/bm-tee-attestd

## Copy the RA-TLS provisioner. start.sh runs it once before envoy to
## mint the data-plane TLS cert via the dstack guest agent.
COPY --from=builder /build/target/release/bm-tee-ratls /usr/local/bin/bm-tee-ratls

## Copy Envoy config template.
COPY image/envoy.yaml /etc/envoy/envoy.yaml

## Copy startup script.
COPY image/start.sh /usr/local/bin/start.sh
RUN chmod +x /usr/local/bin/start.sh

## Envoy data plane: node-secret-gated JSON-RPC/WebSocket proxying to
## the hardcoded provider upstream, plus the registry's
## GET /attestation/info probes (routed to the loopback attestation
## server). The listener terminates TLS in-enclave with the dstack-minted
## RA-TLS certificate (served on the Phala `-8080s` passthrough endpoint).
EXPOSE 8080
## Envoy admin port — loopback-only via the admin block in envoy.yaml;
## the EXPOSE is informational and the port is never published.
EXPOSE 9901

## Drop to non-root. The envoy base image has the envoy user (uid 100).
## The RA-TLS provisioner and the attestation server run as the same
## user; they need read access to the bind-mounted /var/run/dstack.sock
## and write access to /tmp (world-writable) for the RA-TLS PEM files
## and the rendered envoy config.
USER envoy

## start.sh renders the envoy config, provisions the RA-TLS cert,
## launches the attestation server (waiting for its fail-closed boot
## method check), then runs envoy and supervises both.
ENTRYPOINT ["/usr/local/bin/start.sh"]
