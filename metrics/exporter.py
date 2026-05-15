#!/usr/bin/env python3
import json
import os
import shutil
import ssl
import time
import urllib.error
import urllib.request
from urllib.parse import urlparse
from datetime import timezone
from email.utils import parsedate_to_datetime
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


LISTEN_ADDR = os.getenv("METRICS_LISTEN_ADDR", "0.0.0.0")
LISTEN_PORT = int(os.getenv("METRICS_PORT", "9100"))
METRICS_SECRET_V1 = os.getenv("METRICS_SECRET_V1", "")
METRICS_SECRET_V2 = os.getenv("METRICS_SECRET_V2", "")
CHAIN = os.getenv("CHAIN", "tao")
RPC_URL = os.getenv("NODE_RPC_URL", "http://node:9944")
NATIVE_METRICS_URL = os.getenv("NODE_NATIVE_METRICS_URL", "http://node:9615/metrics")
GATEWAY_HEALTH_URL = os.getenv("GATEWAY_HEALTH_URL", "http://gateway/health")
NODE_DATA_PATH = os.getenv("NODE_DATA_PATH", "/node-data")
HOST_PROC_PATH = os.getenv("HOST_PROC_PATH", "/host/proc")
GIT_DIR = os.getenv("GIT_DIR", "/repo/.git")
SSL_CERT_PATH = os.getenv("SSL_CERT_PATH", "/etc/nginx/ssl/cert.pem")
TIMEOUT_SECONDS = float(os.getenv("METRICS_TIMEOUT_SECONDS", "4"))
START_TIME = time.time()
try:
    CLOCK_TICKS_PER_SECOND = os.sysconf(os.sysconf_names["SC_CLK_TCK"])
except (AttributeError, KeyError, OSError, ValueError):
    CLOCK_TICKS_PER_SECOND = 100


def label_escape(value):
    return str(value).replace("\\", "\\\\").replace("\n", "\\n").replace('"', '\\"')


def metric_name_escape(value):
    return str(value).replace("-", "_").replace(".", "_")


class PrometheusText:
    def __init__(self):
        self.lines = []
        self.seen = set()

    def add(self, name, help_text, metric_type, value, labels=None):
        if value is None:
            return
        if name not in self.seen:
            self.lines.append(f"# HELP {name} {help_text}")
            self.lines.append(f"# TYPE {name} {metric_type}")
            self.seen.add(name)

        label_text = ""
        if labels:
            label_parts = [
                f'{metric_name_escape(key)}="{label_escape(val)}"'
                for key, val in sorted(labels.items())
            ]
            label_text = "{" + ",".join(label_parts) + "}"

        self.lines.append(f"{name}{label_text} {value}")

    def render(self):
        return "\n".join(self.lines) + "\n"


def bool_value(value):
    return 1 if bool(value) else 0


def parse_int(value, default=0):
    try:
        return int(value)
    except (TypeError, ValueError):
        return default


def parse_float(value):
    try:
        return float(value)
    except (TypeError, ValueError):
        return None


def parse_block_number(value):
    try:
        if value is None:
            return None
        if isinstance(value, int):
            return value
        value = str(value)
        if value.startswith("0x"):
            return int(value, 16)
        return int(value)
    except (TypeError, ValueError):
        return None


def read_text_file(path):
    try:
        with open(path, "r", encoding="utf-8") as file:
            return file.read().strip()
    except OSError:
        return None


def read_git_ref(git_dir, ref):
    ref_path = os.path.join(git_dir, ref)
    value = read_text_file(ref_path)
    if value:
        return value

    packed_refs = read_text_file(os.path.join(git_dir, "packed-refs"))
    if not packed_refs:
        return None

    for line in packed_refs.splitlines():
        if not line or line.startswith("#") or line.startswith("^"):
            continue
        parts = line.split(" ", 1)
        if len(parts) == 2 and parts[1] == ref:
            return parts[0]
    return None


def read_git_metadata():
    env_sha = os.getenv("BM_MINER_GIT_SHA", "unknown")
    env_branch = os.getenv("BM_MINER_GIT_BRANCH", "unknown")
    head = read_text_file(os.path.join(GIT_DIR, "HEAD"))

    if not head:
        return env_sha, env_branch

    if head.startswith("ref: "):
        ref = head[5:].strip()
        branch = ref.removeprefix("refs/heads/")
        sha = read_git_ref(GIT_DIR, ref) or env_sha
        return sha, branch or env_branch

    return head, "detached"


def fetch_text(url):
    started = time.monotonic()
    request = urllib.request.Request(url, method="GET")
    with urllib.request.urlopen(request, timeout=TIMEOUT_SECONDS) as response:
        body = response.read()
    latency = time.monotonic() - started
    return body.decode("utf-8", errors="replace"), latency


def safe_fetch_text(url):
    try:
        return fetch_text(url)
    except (OSError, ValueError, urllib.error.URLError):
        return None, None


def parse_prometheus_sample(text, name):
    for line in text.splitlines():
        line = line.strip()
        if not line or line.startswith("#") or not line.startswith(name):
            continue
        next_char = line[len(name):len(name) + 1]
        if next_char not in ("", " ", "{"):
            continue
        parts = line.split()
        if len(parts) < 2:
            continue
        value = parse_float(parts[-1])
        if value is not None:
            return value
    return None


def bearer_authorized(headers):
    allowed = {
        f"Bearer {secret}"
        for secret in (METRICS_SECRET_V1, METRICS_SECRET_V2)
        if secret
    }
    if not allowed:
        return True
    return headers.get("Authorization", "") in allowed


def json_rpc(method, params=None):
    payload = json.dumps({
        "jsonrpc": "2.0",
        "method": method,
        "params": params or [],
        "id": 1,
    }).encode("utf-8")
    request = urllib.request.Request(
        RPC_URL,
        data=payload,
        headers={"Content-Type": "application/json"},
        method="POST",
    )

    started = time.monotonic()
    with urllib.request.urlopen(request, timeout=TIMEOUT_SECONDS) as response:
        body = response.read()
    latency = time.monotonic() - started

    decoded = json.loads(body.decode("utf-8"))
    if "error" in decoded:
        raise RuntimeError(decoded["error"])
    return decoded.get("result"), latency


def http_probe(url):
    started = time.monotonic()
    request = urllib.request.Request(url, method="GET")
    with urllib.request.urlopen(request, timeout=TIMEOUT_SECONDS) as response:
        response.read()
        status = response.status
    return 200 <= status < 500, time.monotonic() - started


def safe_rpc(method, params=None):
    try:
        return json_rpc(method, params)
    except (OSError, ValueError, RuntimeError, urllib.error.URLError):
        return None, None


def add_build_metrics(metrics):
    git_sha, git_branch = read_git_metadata()
    git_short_sha = git_sha[:12] if git_sha != "unknown" else "unknown"

    metrics.add(
        "blockmachine_miner_build_info",
        "Blockmachine miner deployment version information.",
        "gauge",
        1,
        {
            "git_sha": git_sha,
            "git_short_sha": git_short_sha,
            "branch": git_branch,
        },
    )
    metrics.add(
        "blockmachine_metrics_exporter_start_time_seconds",
        "Unix timestamp when the metrics exporter process started.",
        "gauge",
        int(START_TIME),
    )


def add_tao_mode_metrics(metrics):
    mode = os.getenv("NODE_MODE", "lite")
    sync = os.getenv("NODE_SYNC", "warp")
    database = os.getenv("NODE_DATABASE", "paritydb")
    metrics.add(
        "blockmachine_node_mode_info",
        "Configured subtensor node mode.",
        "gauge",
        1,
        {"mode": mode, "sync": sync, "database": database},
    )
    metrics.add(
        "blockmachine_node_archive_mode",
        "Whether the subtensor node is configured as an archive node.",
        "gauge",
        1 if mode == "archive" else 0,
    )


def add_tao_rpc_metrics(metrics):
    health, latency = safe_rpc("system_health")
    rpc_up = isinstance(health, dict)
    metrics.add(
        "blockmachine_node_rpc_up",
        "Whether the subtensor JSON-RPC endpoint responded to system_health.",
        "gauge",
        bool_value(rpc_up),
    )
    metrics.add(
        "blockmachine_node_rpc_latency_seconds",
        "Latency of the system_health JSON-RPC probe.",
        "gauge",
        latency,
    )

    if not rpc_up:
        return

    peers = parse_int(health.get("peers", 0))
    syncing = bool(health.get("isSyncing", False))
    should_have_peers = bool(health.get("shouldHavePeers", True))
    healthy = (not should_have_peers or peers > 0) and not syncing

    metrics.add(
        "blockmachine_node_healthy",
        "Whether the subtensor node has peers when expected and is not major-syncing.",
        "gauge",
        bool_value(healthy),
    )
    metrics.add("blockmachine_node_peers", "Connected subtensor peer count.", "gauge", peers)
    metrics.add(
        "blockmachine_node_should_have_peers",
        "Whether the subtensor node expects to have peers.",
        "gauge",
        bool_value(should_have_peers),
    )
    metrics.add(
        "blockmachine_node_syncing",
        "Whether the subtensor node reports major syncing.",
        "gauge",
        bool_value(syncing),
    )

    sync_state, _ = safe_rpc("system_syncState")
    if isinstance(sync_state, dict):
        starting = parse_block_number(sync_state.get("startingBlock"))
        current = parse_block_number(sync_state.get("currentBlock"))
        highest = parse_block_number(sync_state.get("highestBlock"))
        metrics.add(
            "blockmachine_node_starting_block",
            "Starting block reported by system_syncState.",
            "gauge",
            starting,
        )
        metrics.add(
            "blockmachine_node_current_block",
            "Current block reported by system_syncState.",
            "gauge",
            current,
        )
        metrics.add(
            "blockmachine_node_highest_block",
            "Highest known block reported by system_syncState.",
            "gauge",
            highest,
        )
        if current is not None and highest is not None:
            metrics.add(
                "blockmachine_node_sync_lag_blocks",
                "Difference between highest known block and current block.",
                "gauge",
                max(highest - current, 0),
            )

    chain, _ = safe_rpc("system_chain")
    node_name, _ = safe_rpc("system_name")
    node_version, _ = safe_rpc("system_version")
    node_roles, _ = safe_rpc("system_nodeRoles")
    if isinstance(node_roles, list):
        roles = ",".join(str(role) for role in node_roles)
    else:
        roles = "unknown"
    metrics.add(
        "blockmachine_node_info",
        "Subtensor node identity information.",
        "gauge",
        1,
        {
            "chain": chain or "unknown",
            "node_name": node_name or "unknown",
            "node_version": node_version or "unknown",
            "roles": roles,
        },
    )

    best_header, _ = safe_rpc("chain_getHeader")
    best_block = None
    if isinstance(best_header, dict):
        best_block = parse_block_number(best_header.get("number"))
        metrics.add(
            "blockmachine_node_best_block",
            "Best block number reported by chain_getHeader.",
            "gauge",
            best_block,
        )

    finalized_hash, _ = safe_rpc("chain_getFinalizedHead")
    finalized_block = None
    if finalized_hash:
        finalized_header, _ = safe_rpc("chain_getHeader", [finalized_hash])
        if isinstance(finalized_header, dict):
            finalized_block = parse_block_number(finalized_header.get("number"))
            metrics.add(
                "blockmachine_node_finalized_block",
                "Finalized block number reported by chain_getFinalizedHead.",
                "gauge",
                finalized_block,
            )

    if best_block is not None and finalized_block is not None:
        metrics.add(
            "blockmachine_node_finality_lag_blocks",
            "Difference between best block and finalized block.",
            "gauge",
            max(best_block - finalized_block, 0),
        )


def add_tao_native_metrics(metrics):
    text, latency = safe_fetch_text(NATIVE_METRICS_URL)
    native_up = text is not None
    metrics.add(
        "blockmachine_node_native_metrics_up",
        "Whether the native Subtensor Prometheus endpoint responded.",
        "gauge",
        bool_value(native_up),
    )
    metrics.add(
        "blockmachine_node_native_metrics_latency_seconds",
        "Latency of the native Subtensor Prometheus endpoint probe.",
        "gauge",
        latency,
    )
    if not native_up:
        return None

    native_samples = {
        "process_cpu_seconds_total": (
            "blockmachine_node_process_cpu_seconds_total",
            "Total user and system CPU time consumed by the subtensor process.",
            "counter",
        ),
        "process_resident_memory_bytes": (
            "blockmachine_node_process_resident_memory_bytes",
            "Resident memory used by the subtensor process.",
            "gauge",
        ),
        "process_virtual_memory_bytes": (
            "blockmachine_node_process_virtual_memory_bytes",
            "Virtual memory used by the subtensor process.",
            "gauge",
        ),
        "process_open_fds": (
            "blockmachine_node_process_open_fds",
            "Open file descriptors held by the subtensor process.",
            "gauge",
        ),
        "process_max_fds": (
            "blockmachine_node_process_max_fds",
            "Maximum file descriptors available to the subtensor process.",
            "gauge",
        ),
    }

    for source_name, (target_name, help_text, metric_type) in native_samples.items():
        metrics.add(
            target_name,
            help_text,
            metric_type,
            parse_prometheus_sample(text, source_name),
        )

    return text


def add_gateway_metrics(metrics):
    try:
        gateway_up, latency = http_probe(GATEWAY_HEALTH_URL)
    except (OSError, urllib.error.URLError):
        gateway_up = False
        latency = None

    metrics.add(
        "blockmachine_gateway_up",
        "Whether the gateway health endpoint responded.",
        "gauge",
        bool_value(gateway_up),
    )
    metrics.add(
        "blockmachine_gateway_health_latency_seconds",
        "Latency of the gateway health endpoint probe.",
        "gauge",
        latency,
    )


def add_disk_metrics(metrics):
    if not os.path.exists(NODE_DATA_PATH):
        metrics.add(
            "blockmachine_node_data_volume_available",
            "Whether the node data volume path is available to the exporter.",
            "gauge",
            0,
        )
        return

    usage = shutil.disk_usage(NODE_DATA_PATH)
    metrics.add(
        "blockmachine_node_data_volume_available",
        "Whether the node data volume path is available to the exporter.",
        "gauge",
        1,
    )
    metrics.add(
        "blockmachine_node_data_volume_total_bytes",
        "Total bytes in the filesystem backing the node data volume.",
        "gauge",
        usage.total,
    )
    metrics.add(
        "blockmachine_node_data_volume_used_bytes",
        "Used bytes in the filesystem backing the node data volume.",
        "gauge",
        usage.used,
    )
    metrics.add(
        "blockmachine_node_data_volume_available_bytes",
        "Available bytes in the filesystem backing the node data volume.",
        "gauge",
        usage.free,
    )
    if usage.total > 0:
        metrics.add(
            "blockmachine_node_data_volume_usage_ratio",
            "Used ratio of the filesystem backing the node data volume.",
            "gauge",
            usage.used / usage.total,
        )


def add_host_metrics(metrics):
    meminfo = read_text_file(os.path.join(HOST_PROC_PATH, "meminfo"))
    if meminfo:
        values = {}
        for line in meminfo.splitlines():
            if ":" not in line:
                continue
            key, raw_value = line.split(":", 1)
            parts = raw_value.strip().split()
            if not parts:
                continue
            values[key] = parse_int(parts[0]) * 1024

        total = values.get("MemTotal")
        available = values.get("MemAvailable", values.get("MemFree"))
        if total is not None:
            metrics.add(
                "blockmachine_host_memory_total_bytes",
                "Total memory on the miner host.",
                "gauge",
                total,
            )
        if available is not None:
            metrics.add(
                "blockmachine_host_memory_available_bytes",
                "Available memory on the miner host.",
                "gauge",
                available,
            )
        if total and available is not None:
            metrics.add(
                "blockmachine_host_memory_usage_ratio",
                "Used memory ratio on the miner host.",
                "gauge",
                max(min((total - available) / total, 1), 0),
            )

    loadavg = read_text_file(os.path.join(HOST_PROC_PATH, "loadavg"))
    if loadavg:
        parts = loadavg.split()
        if len(parts) >= 3:
            metrics.add("blockmachine_host_load1", "1 minute host load average.", "gauge", parse_float(parts[0]))
            metrics.add("blockmachine_host_load5", "5 minute host load average.", "gauge", parse_float(parts[1]))
            metrics.add("blockmachine_host_load15", "15 minute host load average.", "gauge", parse_float(parts[2]))

    stat = read_text_file(os.path.join(HOST_PROC_PATH, "stat"))
    if not stat:
        return

    first_line = stat.splitlines()[0].split()
    if len(first_line) < 5 or first_line[0] != "cpu":
        return

    modes = [
        "user",
        "nice",
        "system",
        "idle",
        "iowait",
        "irq",
        "softirq",
        "steal",
        "guest",
        "guest_nice",
    ]
    for mode, raw_value in zip(modes, first_line[1:]):
        metrics.add(
            "blockmachine_host_cpu_seconds_total",
            "Host CPU time by mode.",
            "counter",
            parse_int(raw_value) / CLOCK_TICKS_PER_SECOND,
            {"mode": mode},
        )


def add_tls_metrics(metrics):
    if not os.path.exists(SSL_CERT_PATH):
        metrics.add(
            "blockmachine_gateway_tls_cert_readable",
            "Whether the gateway TLS certificate can be read by the exporter.",
            "gauge",
            0,
        )
        return

    try:
        decoded = ssl._ssl._test_decode_cert(SSL_CERT_PATH)
        not_after = parsedate_to_datetime(decoded["notAfter"])
        if not_after.tzinfo is None:
            not_after = not_after.replace(tzinfo=timezone.utc)
        expiry = int(not_after.timestamp())
    except (OSError, KeyError, ValueError):
        metrics.add(
            "blockmachine_gateway_tls_cert_readable",
            "Whether the gateway TLS certificate can be read by the exporter.",
            "gauge",
            0,
        )
        return

    metrics.add(
        "blockmachine_gateway_tls_cert_readable",
        "Whether the gateway TLS certificate can be read by the exporter.",
        "gauge",
        1,
    )
    metrics.add(
        "blockmachine_gateway_tls_cert_expiry_timestamp_seconds",
        "Unix timestamp when the gateway TLS certificate expires.",
        "gauge",
        expiry,
    )
    metrics.add(
        "blockmachine_gateway_tls_cert_expires_in_seconds",
        "Seconds until the gateway TLS certificate expires.",
        "gauge",
        max(expiry - int(time.time()), 0),
    )


def collect_metrics():
    started = time.monotonic()
    metrics = PrometheusText()
    metrics.add(
        "blockmachine_metrics_exporter_up",
        "Whether the Blockmachine metrics exporter is running.",
        "gauge",
        1,
    )
    metrics.add(
        "blockmachine_metrics_exporter_scrape_timestamp_seconds",
        "Unix timestamp for this metrics scrape.",
        "gauge",
        int(time.time()),
    )

    add_build_metrics(metrics)

    native_text = None
    if CHAIN == "tao":
        add_tao_mode_metrics(metrics)
        add_tao_rpc_metrics(metrics)
        native_text = add_tao_native_metrics(metrics)

    add_gateway_metrics(metrics)
    add_disk_metrics(metrics)
    add_host_metrics(metrics)
    add_tls_metrics(metrics)

    metrics.add(
        "blockmachine_metrics_exporter_scrape_duration_seconds",
        "Time spent collecting metrics for this scrape.",
        "gauge",
        time.monotonic() - started,
    )
    rendered = metrics.render()
    if native_text:
        rendered += "\n# Native Subtensor metrics from node:9615\n"
        rendered += native_text
        if not rendered.endswith("\n"):
            rendered += "\n"
    return rendered


class Handler(BaseHTTPRequestHandler):
    def do_GET(self):
        path = urlparse(self.path).path
        if path == "/health":
            self.send_response(200)
            self.send_header("Content-Type", "text/plain")
            self.end_headers()
            self.wfile.write(b"OK\n")
            return

        if path != "/metrics":
            self.send_response(404)
            self.send_header("Content-Type", "text/plain")
            self.end_headers()
            self.wfile.write(b"not found\n")
            return

        if not bearer_authorized(self.headers):
            self.send_response(401)
            self.send_header("Content-Type", "text/plain")
            self.end_headers()
            self.wfile.write(b"unauthorized\n")
            return

        try:
            body = collect_metrics().encode("utf-8")
            self.send_response(200)
        except Exception:
            body = b"blockmachine_metrics_exporter_up 0\n"
            self.send_response(500)
        self.send_header("Content-Type", "text/plain; version=0.0.4; charset=utf-8")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, fmt, *args):
        return


def main():
    server = ThreadingHTTPServer((LISTEN_ADDR, LISTEN_PORT), Handler)
    print(f"Blockmachine metrics exporter listening on {LISTEN_ADDR}:{LISTEN_PORT}", flush=True)
    server.serve_forever()


if __name__ == "__main__":
    main()
