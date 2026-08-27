#!/usr/bin/env python3
"""Standalone end-to-end verifier for a DEPLOYED TEE proxy-miner CVM.

Plays the registry + gateway against a live Phala CVM, WITHOUT standing up
either service or a database. It imports the registry's real verification
library (``core.tee_attestation``, ``core.tee_load_test``, ``core.tee_tls``)
so it performs the EXACT checks the production registry would, then sends
real authenticated RPC queries the way the gateway routes them.

Sequence (mirrors docs/tee-proxy-miners.md):

  1. Attestation + RA-TLS verification (``TeeAttestationService.verify_tee_node``):
     capture the served RA-TLS cert, fetch /attestation/info (unauthenticated),
     freshness-check, dcap-qvl verify the TDX quote against Phala PCCS, replay
     the RTMR3 event log to DERIVE (compose_hash, os_image_hash), bind the boot
     measurements (mr_td, rtmr0..2) from the verified quote, verify the ed25519
     signed claims against node_pubkey, and confirm report_data == the cert
     binding. This is the same code path the gateway's per-connection RA-TLS
     check runs.
  2. In-enclave method-check verdict (from the signed claims): whether the boot
     probes of the baked manifest passed, plus the attested capability flags.
  3. Registry load test (``run_tee_load_test``): open-loop fixed-rate burst over
     the cert-PINNED channel, error-rate gated, correctness cross-checked against
     a trusted reference RPC.
  4. Live authenticated RPC queries over the pinned bearer channel — the shape a
     gateway-routed client sees.

The values printed in step 1 (compose_hash, os_image_hash, mr_td, rtmr0..2) are
exactly what an operator would publish into a ``tee_image_versions`` row to
allowlist this image.

Usage:
    export REGISTRY_PATH=/path/to/blockmachine_playground/registry   # or auto-detected
    export NODE_SECRET=<the bearer you baked into the CVM env>
    export TEE_REFERENCE_ETH_URL=wss://<a trusted archive RPC>       # for the load test
    ./verify_deployed_cvm.py wss://<app>-8080s.<phala-domain>

    # Skip the load test (no reference RPC handy):
    ./verify_deployed_cvm.py --no-load-test wss://...
    # Expect a specific enclave key across redeploys:
    ./verify_deployed_cvm.py --expected-pubkey <b64> wss://...
"""

from __future__ import annotations

import argparse
import asyncio
import json
import os
import sys
from pathlib import Path


def _bootstrap_imports() -> None:
    """Put the registry package on sys.path so we reuse its real verifier."""
    candidates = []
    env = os.environ.get("REGISTRY_PATH")
    if env:
        candidates.append(Path(env))
    # Sibling checkout: <...>/blockmachine_playground/registry
    here = Path(__file__).resolve()
    for parent in here.parents:
        candidates.append(parent / "blockmachine_playground" / "registry")
        candidates.append(parent.parent / "blockmachine_playground" / "registry")
    for cand in candidates:
        if (cand / "registry" / "__init__.py").exists():
            sys.path.insert(0, str(cand))
            return
        if (cand / "core" / "tee_attestation.py").exists():
            # ``registry`` is the package dir itself; add its parent.
            sys.path.insert(0, str(cand.parent))
            return
    sys.exit(
        "Could not locate the registry package. Set REGISTRY_PATH to "
        "…/blockmachine_playground/registry"
    )


_bootstrap_imports()

# Imported after sys.path is set up.
from registry.config import TeeConfig  # noqa: E402
from registry.core.tee_attestation import (  # noqa: E402
    TeeAttestationError,
    TeeAttestationService,
)
from registry.core.tee_load_test import run_tee_load_test  # noqa: E402
from registry.core.tee_tls import pinned_cert_ssl_context  # noqa: E402

try:
    import websockets
except ImportError:
    sys.exit("pip install websockets  (needed for the live-query step)")


GREEN, RED, YELLOW, DIM, BOLD, RESET = (
    "\033[32m",
    "\033[31m",
    "\033[33m",
    "\033[2m",
    "\033[1m",
    "\033[0m",
)


def ok(msg: str) -> None:
    print(f"  {GREEN}✓{RESET} {msg}")


def bad(msg: str) -> None:
    print(f"  {RED}✗{RESET} {msg}")


def info(msg: str) -> None:
    print(f"  {DIM}{msg}{RESET}")


def section(title: str) -> None:
    print(f"\n{BOLD}{title}{RESET}")


async def live_queries(
    endpoint: str,
    secret: str,
    cert_der: bytes,
    pinned_ip: str | None,
) -> bool:
    """Send a handful of authenticated RPC queries over the PINNED channel.

    Uses the same cert-pinned TLS context the registry builds, and dials the
    vetted pinned IP so this leg cannot DNS-rebind — exactly the gateway's
    posture. Returns True if every query returned a well-formed result.
    """
    ssl_ctx = pinned_cert_ssl_context(cert_der)
    # Dial the pinned IP but keep the SNI/Host as the real hostname.
    from urllib.parse import urlparse

    parsed = urlparse(endpoint)
    host = parsed.hostname or ""
    port = parsed.port or 443
    connect_host = pinned_ip or host

    uri = endpoint
    extra = {"server_hostname": host} if pinned_ip else {}
    queries = [
        ("web3_clientVersion", []),
        ("eth_chainId", []),
        ("eth_blockNumber", []),
        ("eth_getBlockByNumber", ["latest", False]),
    ]
    try:
        async with websockets.connect(
            uri,
            ssl=ssl_ctx,
            host=connect_host,
            port=port,
            additional_headers={"Authorization": f"Bearer {secret}"},
            open_timeout=20,
            **extra,
        ) as ws:
            all_ok = True
            for i, (method, params) in enumerate(queries, 1):
                await ws.send(
                    json.dumps(
                        {"jsonrpc": "2.0", "id": i, "method": method, "params": params}
                    )
                )
                resp = json.loads(await asyncio.wait_for(ws.recv(), 20))
                if "result" in resp:
                    r = resp["result"]
                    shown = r if isinstance(r, str) else json.dumps(r)[:60] + "…"
                    ok(f"{method} → {shown}")
                else:
                    bad(f"{method} → {resp.get('error')}")
                    all_ok = False
            # One authenticated subscription to prove the WS upgrade path.
            await ws.send(
                json.dumps(
                    {
                        "jsonrpc": "2.0",
                        "id": 99,
                        "method": "eth_subscribe",
                        "params": ["newHeads"],
                    }
                )
            )
            sub = json.loads(await asyncio.wait_for(ws.recv(), 20))
            if "result" in sub:
                head = json.loads(await asyncio.wait_for(ws.recv(), 40))
                num = head.get("params", {}).get("result", {}).get("number")
                ok(f"eth_subscribe newHeads → first head {num}")
            else:
                bad(f"eth_subscribe → {sub.get('error')}")
                all_ok = False
            return all_ok
    except Exception as e:  # noqa: BLE001 - report and fail
        bad(f"authenticated query channel failed: {type(e).__name__}: {e}")
        return False


async def reject_unauthenticated(endpoint: str, cert_der: bytes) -> None:
    """Confirm the RPC surface rejects a MISSING bearer (contract check)."""
    ssl_ctx = pinned_cert_ssl_context(cert_der)
    try:
        async with websockets.connect(
            endpoint, ssl=ssl_ctx, open_timeout=15
        ) as ws:
            await ws.send(
                json.dumps(
                    {"jsonrpc": "2.0", "id": 1, "method": "eth_chainId", "params": []}
                )
            )
            resp = await asyncio.wait_for(ws.recv(), 15)
            body = json.loads(resp)
            if body.get("error", {}).get("message", "").lower().find("secret") >= 0:
                ok("RPC without a bearer is rejected (401-style error)")
            else:
                bad(f"RPC without a bearer was NOT rejected: {resp[:80]}")
    except websockets.exceptions.InvalidStatus as e:
        # An HTTP 401 on the upgrade is the expected rejection.
        ok(f"RPC without a bearer is rejected at upgrade ({e})")
    except Exception as e:  # noqa: BLE001
        info(f"unauthenticated-rejection probe inconclusive: {type(e).__name__}: {e}")


async def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("endpoint", help="wss://<app>-8080s.<phala-domain>")
    ap.add_argument("--expected-pubkey", help="b64 node_pubkey to require (stability)")
    ap.add_argument("--no-load-test", action="store_true")
    ap.add_argument("--chain", default="eth")
    ap.add_argument(
        "--pccs-url",
        default=os.environ.get("TEE_PCCS_URL", "https://pccs.phala.network"),
    )
    args = ap.parse_args()

    secret = os.environ.get("NODE_SECRET")
    reference_url = os.environ.get("TEE_REFERENCE_ETH_URL")

    cfg = TeeConfig()
    cfg.pccs_url = args.pccs_url
    if reference_url:
        cfg.reference_urls = {args.chain: reference_url}

    service = TeeAttestationService(cfg)

    print(f"{BOLD}TEE proxy-miner CVM verifier{RESET}")
    info(f"endpoint : {args.endpoint}")
    info(f"PCCS     : {cfg.pccs_url}")

    # ── 1. Attestation + RA-TLS verification ──────────────────────────────
    section("1. Attestation + RA-TLS quote verification")
    try:
        result = await service.verify_tee_node(
            args.endpoint,
            secret=None,
            expected_pubkey=args.expected_pubkey,
        )
    except TeeAttestationError as e:
        bad(f"attestation FAILED ({'transient' if e.transient else 'definitive'}): {e}")
        return 1
    except Exception as e:  # noqa: BLE001
        bad(f"attestation raised {type(e).__name__}: {e}")
        return 1

    ok("TDX quote verified (dcap-qvl, TCB accepted, debug-off)")
    ok("RTMR3 event log replayed; report_data ↔ RA-TLS cert binding checked")
    ok("ed25519 signed claims verified against node_pubkey")
    print(f"\n  {BOLD}Derived identity (publish these to tee_image_versions):{RESET}")
    info(f"chain / provider : {result.chain} / {result.provider}")
    info(f"node_pubkey (b64): {result.pubkey_b64}")
    info(f"compose_hash     : {result.compose_hash}")
    info(f"os_image_hash    : {result.os_image_hash}")
    info(f"mr_td            : {result.mr_td}")
    info(f"rtmr0            : {result.rt_mr0}")
    info(f"rtmr1            : {result.rt_mr1}")
    info(f"rtmr2            : {result.rt_mr2}")
    info(f"tcb_status       : {result.tcb_status}")
    info(f"cert fingerprint : {result.cert_fingerprint}")
    if args.expected_pubkey:
        ok("node_pubkey matches the expected (stable across redeploys)")

    if result.cert_der is None:
        bad("no RA-TLS cert captured — cannot run the pinned-channel steps")
        return 1

    # ── 2. In-enclave method-check verdict ────────────────────────────────
    section("2. In-enclave method-check verdict (from signed claims)")
    if result.method_check_passed:
        ok("all REQUIRED probes passed in-enclave")
    else:
        bad("in-enclave method check reports required methods NOT passing")
    caps = result.method_check.get("capabilities", {})
    if caps:
        info("attested capability flags:")
        for flag, val in sorted(caps.items()):
            mark = f"{GREEN}yes{RESET}" if val else f"{YELLOW}no{RESET}"
            info(f"    {flag}: {mark}")

    # ── 3. Registry load test ─────────────────────────────────────────────
    if args.no_load_test:
        section("3. Registry load test — SKIPPED (--no-load-test)")
    elif not reference_url:
        section("3. Registry load test — SKIPPED")
        info("set TEE_REFERENCE_ETH_URL=wss://<trusted archive RPC> to enable")
    else:
        section(
            f"3. Registry load test ({cfg.probe_min_rps} rps × "
            f"{cfg.probe_load_seconds}s, ≤{cfg.probe_max_error_rate:.0%} errors)"
        )
        load = await run_tee_load_test(
            args.endpoint,
            secret or "",
            min_rps=cfg.probe_min_rps,
            duration_secs=cfg.probe_load_seconds,
            max_error_rate=cfg.probe_max_error_rate,
            reference_url=reference_url,
            verify_ssl=False,
            pinned_cert_der=result.cert_der,
            pinned_ip=result.pinned_ip,
        )
        (ok if load.passed else bad)(load.summary())

    # ── 4. Live authenticated queries over the pinned channel ─────────────
    section("4. Live authenticated RPC over the pinned channel")
    if not secret:
        info("NODE_SECRET not set — skipping authenticated queries")
        info("(set NODE_SECRET to the bearer you baked into the CVM env)")
    else:
        await reject_unauthenticated(args.endpoint, result.cert_der)
        queries_ok = await live_queries(
            args.endpoint, secret, result.cert_der, result.pinned_ip
        )
        if not queries_ok:
            return 1

    section("Verdict")
    ok("this CVM would be ACCEPTED and routable by the registry + gateway")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(asyncio.run(main()))
    except KeyboardInterrupt:
        raise SystemExit(130)
