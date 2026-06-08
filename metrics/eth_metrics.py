"""Ethereum chain metrics — JSON-RPC probes, beacon API probes, and native scrape.

Shared across all three ETH tiers (minimal / archive / proof).
The EL client is identified via the EL_CLIENT env var (reth | erigon).
Customer-facing metric names mirror the tao set (blockmachine_node_*) so
dashboards work across chains.
"""

import json
import os

from exporter import (
    bool_value,
    parse_block_number,
    parse_prometheus_sample,
    safe_fetch_text,
    safe_rpc,
)

# Static identity values cached after the first successful probe — both change
# only when the EL restarts, so we skip them on every scrape after that.
_cached_chain_id = None
_cached_client_version = None


def add_eth_mode_metrics(metrics):
    tier = os.getenv("ETH_TIER", "minimal")
    el_client = os.getenv("EL_CLIENT", "reth")
    metrics.add(
        "blockmachine_node_mode_info",
        "Configured Ethereum node mode.",
        "gauge",
        1,
        {"chain": "eth", "tier": tier, "el_client": el_client},
    )
    metrics.add(
        "blockmachine_node_archive_mode",
        "Whether the node is configured as a full archive.",
        "gauge",
        1 if tier.startswith("archive") else 0,
    )


def add_eth_rpc_metrics(metrics):
    global _cached_chain_id, _cached_client_version

    block_hex, latency = safe_rpc("eth_blockNumber")
    rpc_up = block_hex is not None
    metrics.add(
        "blockmachine_node_rpc_up",
        "Whether the EL JSON-RPC endpoint responded to eth_blockNumber.",
        "gauge",
        bool_value(rpc_up),
    )
    metrics.add(
        "blockmachine_node_rpc_latency_seconds",
        "Latency of the eth_blockNumber JSON-RPC probe.",
        "gauge",
        latency,
    )
    if not rpc_up:
        return

    current_block = parse_block_number(block_hex)
    if current_block is not None:
        metrics.add(
            "blockmachine_node_current_block",
            "Latest block reported by eth_blockNumber.",
            "gauge",
            current_block,
        )
        metrics.add(
            "blockmachine_node_best_block",
            "Best block (eth uses single height; mirrors current_block).",
            "gauge",
            current_block,
        )

    sync_state, _ = safe_rpc("eth_syncing")
    syncing = isinstance(sync_state, dict)
    metrics.add(
        "blockmachine_node_syncing",
        "Whether the EL reports syncing (eth_syncing).",
        "gauge",
        bool_value(syncing),
    )

    # Emit highest_block and sync_lag_blocks unconditionally so dashboards keep
    # the series alive after the node catches up. When synced, eth_syncing
    # returns `false` and we treat the tip as highest = current_block, lag = 0.
    if syncing:
        starting = parse_block_number(sync_state.get("startingBlock"))
        highest = parse_block_number(sync_state.get("highestBlock"))
        sync_current = parse_block_number(sync_state.get("currentBlock"))
        metrics.add(
            "blockmachine_node_starting_block",
            "Starting block reported by eth_syncing.",
            "gauge",
            starting,
        )
        sync_lag = (
            max(highest - sync_current, 0)
            if highest is not None and sync_current is not None
            else None
        )
    else:
        highest = current_block
        sync_lag = 0

    metrics.add(
        "blockmachine_node_highest_block",
        "Highest known block (eth_syncing.highestBlock when syncing, else current_block).",
        "gauge",
        highest,
    )
    metrics.add(
        "blockmachine_node_sync_lag_blocks",
        "Difference between highest known block and current block (0 when synced).",
        "gauge",
        sync_lag,
    )

    peer_hex, _ = safe_rpc("net_peerCount")
    peers = parse_block_number(peer_hex) or 0
    metrics.add("blockmachine_node_peers", "Connected EL peer count.", "gauge", peers)

    if _cached_chain_id is None:
        _cached_chain_id = parse_block_number(safe_rpc("eth_chainId")[0])
    if _cached_client_version is None:
        version_result, _ = safe_rpc("web3_clientVersion")
        if isinstance(version_result, str):
            _cached_client_version = version_result

    metrics.add(
        "blockmachine_node_info",
        "EL node identity information.",
        "gauge",
        1,
        {
            "chain": "eth",
            "el_client": os.getenv("EL_CLIENT", "reth"),
            "chain_id": str(_cached_chain_id) if _cached_chain_id is not None else "unknown",
            "node_version": _cached_client_version or "unknown",
        },
    )

    healthy = (not syncing) and peers > 0
    metrics.add(
        "blockmachine_node_healthy",
        "Whether the EL has peers and is not syncing.",
        "gauge",
        bool_value(healthy),
    )


def add_cl_metrics(metrics):
    cl_url = os.getenv("CL_RPC_URL", "")
    if not cl_url:
        # Archive Erigon uses erigon's built-in Caplin CL; no separate beacon endpoint to probe.
        return
    endpoint = f"{cl_url}/eth/v1/node/syncing"

    text, latency = safe_fetch_text(endpoint)
    cl_up = text is not None
    metrics.add(
        "blockmachine_cl_up",
        "Whether the consensus layer beacon API responded.",
        "gauge",
        bool_value(cl_up),
    )
    metrics.add(
        "blockmachine_cl_latency_seconds",
        "Latency of the CL syncing probe.",
        "gauge",
        latency,
    )
    if not cl_up:
        return

    try:
        body = json.loads(text)
        data = body.get("data", {})
        head_slot = int(data.get("head_slot", "0"))
        sync_distance = int(data.get("sync_distance", "0"))
        is_syncing = bool(data.get("is_syncing", False))
        metrics.add("blockmachine_cl_head_slot", "Head slot from CL.", "gauge", head_slot)
        metrics.add(
            "blockmachine_cl_sync_distance_slots",
            "CL sync distance in slots.",
            "gauge",
            sync_distance,
        )
        metrics.add(
            "blockmachine_cl_syncing",
            "Whether the CL reports syncing.",
            "gauge",
            bool_value(is_syncing),
        )
    except (ValueError, KeyError, TypeError):
        pass


# Reth uses port 9101 not the default 9001, which would clash with Lighthouse's
# QUIC discovery port. Erigon serves on 6060.
_EL_PROCESS_SAMPLES = {
    "process_cpu_seconds_total": (
        "blockmachine_node_process_cpu_seconds_total",
        "Total CPU time consumed by the EL process.",
        "counter",
    ),
    "process_resident_memory_bytes": (
        "blockmachine_node_process_resident_memory_bytes",
        "Resident memory of the EL process.",
        "gauge",
    ),
    "process_virtual_memory_bytes": (
        "blockmachine_node_process_virtual_memory_bytes",
        "Virtual memory of the EL process.",
        "gauge",
    ),
    "process_open_fds": (
        "blockmachine_node_process_open_fds",
        "Open FDs of the EL process.",
        "gauge",
    ),
    "process_max_fds": (
        "blockmachine_node_process_max_fds",
        "Max FDs of the EL process.",
        "gauge",
    ),
}


def add_eth_native_metrics(metrics):
    native_url = os.getenv("NODE_NATIVE_METRICS_URL", "http://node:9101/")

    text, latency = safe_fetch_text(native_url)
    native_up = text is not None
    metrics.add(
        "blockmachine_node_native_metrics_up",
        "Whether the EL's native Prometheus endpoint responded.",
        "gauge",
        bool_value(native_up),
    )
    metrics.add(
        "blockmachine_node_native_metrics_latency_seconds",
        "Latency of the EL's native Prometheus probe.",
        "gauge",
        latency,
    )
    if not native_up:
        return None

    for source, (target, help_text, mt) in _EL_PROCESS_SAMPLES.items():
        metrics.add(target, help_text, mt, parse_prometheus_sample(text, source))

    return text
