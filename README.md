# Blockmachine Miner

Run a blockchain RPC node behind an authenticated gateway and earn by serving RPC requests through the Blockmachine network (Bittensor Subnet 19).

Supported chains:

| Chain | Tiers | Client |
|-------|-------|--------|
| `tao` (Bittensor subtensor) | `lite`, `archive` | subtensor |
| `eth` (Ethereum mainnet) | `latest`, `archive` | reth (latest), erigon (archive) |

The install script picks the right stack based on your choice of chain and tier — see [Getting started](#getting-started) below.

Blockmachine is a decentralized marketplace for blockchain RPC infrastructure. Miners compete on price and quality to serve customer requests routed by the protocol gateway. You set your own price per Compute Unit (CU) and earn emissions proportional to the work you deliver.

## How it works

```
Customer → Gateway → Your Node
                ↓
         Logs & Verification
                ↓
         Validators score you
                ↓
         Emissions paid per CU served
```

1. You run a subtensor node behind an nginx gateway that authenticates requests from the Blockmachine network
2. The protocol gateway routes customer RPC requests to your node based on quality score and price
3. Validators read gateway logs, verify correctness, and submit weights on-chain each epoch (~72 minutes)
4. You earn emissions proportional to the CUs you served at your bid price

## Prerequisites

### Subtensor (chain: `tao`)

| Resource | Lite | Archive |
|----------|------|---------|
| Architecture | x86_64 | x86_64 |
| CPU | 4+ cores | 4+ cores |
| RAM | 16 GB+ | 16 GB+ |
| Disk | 100 GB SSD | 4+ TB SSD |
| Sync time | ~15 min (warp sync) | Days (or use snapshot) |
| Ports | 80, 443, 30333 | 80, 443, 30333 |

### Ethereum (chain: `eth`, mainnet only)

| Resource | Latest (reth `--full`) | Archive (erigon) |
|----------|------------------------|------------------|
| Architecture | x86_64 | x86_64 |
| CPU | 4+ cores | 8+ cores recommended |
| RAM | 16 GB+ | 32 GB+ recommended |
| Disk | 250 GB NVMe | 3.5 TB NVMe |
| Sync time | Hours (P2P) | A day or more (built-in torrent fetch) |
| Ports | 80, 443, 30303, 9000/tcp+udp, 9001/udp | 80, 443, 30303 |
| RPC | `eth_*` (read methods) + `eth_subscribe` over WS | + `debug_*`, `trace_*`, full `eth_getProof` back to genesis |

Any VPS or dedicated server with Docker will work for `tao` and ETH Latest. Archive tiers benefit substantially from NVMe storage. The install script handles all dependencies (Docker, git, certificates).

## Getting started

### Install the Blockmachine CLI

The CLI requires Python 3.10+ and can run anywhere — your laptop, a management server, or on the miner node itself. It's the control panel for registering nodes, managing secrets, and setting prices across your fleet without SSH-ing into each machine.

```bash
pip install blockmachine
```

### Testnet vs Mainnet

The install script asks whether you're running on testnet or mainnet. For testnet, all `bm` CLI commands require the `--testnet` flag:

```bash
bm --testnet miner login
bm --testnet miner add --endpoint wss://... --alias my-node --secret '...' --price 0.01
```

For mainnet (the default), use `bm` without the flag. The install script prints the correct commands for whichever network you choose.

**Testnet requirement:** You need a Bittensor hotkey registered as a miner on **netuid 417** (the Blockmachine testnet subnet). Register your hotkey before running the install script:

```bash
btcli subnet register --netuid 417 --subtensor.network test
```

### Authenticate

```bash
bm miner login              # mainnet
bm --testnet miner login    # testnet
```

This uses a device authorization flow: the CLI displays a URL and code, then polls until you approve in a browser. The browser does not need to be on the same machine — you can run `bm miner login` on a headless server and open the URL on your phone or laptop. If a browser is available locally, it opens automatically.

### On your server

SSH into your server and run the install script:

```bash
bash <(curl -sSL https://blockmachine.io/miner/install.sh)
```

The script will:
- Install git and Docker if missing
- Clone this repository (or update it if re-running)
- Ask which chain to serve (`tao` or `eth`)
- Ask whether you have a domain name or are using an IP address
- Set up TLS (auto-renewing Let's Encrypt for domains, or self-signed for IPs)
- Ask for tier (`lite`/`archive` for tao, `latest`/`archive` for eth)
- Offer to restore from a snapshot (tao archive only — eth archive fetches via torrents automatically)
- Generate a bearer token secret
- Start the gateway and the chain node(s)
- Print the registration commands to run from your local machine

At the end you'll see output like:

```
========================================
 Miner is running!
========================================

  Endpoint: wss://203.0.113.50
  Chain:    tao (lite)
  Alias:    tao-203-0-113-50
  Secret:   stored in /root/blockmachine-miner/.env

Now run these commands on your local machine:

  bm miner login                    # or: bm --testnet miner login
  bm miner add --endpoint wss://203.0.113.50 --alias tao-203-0-113-50 --secret '<secret from .env>' --price <usd-per-cu>
```

### Register the node

Using the CLI (wherever you installed it), register your node with the secret from the server's `.env` file (`SECRET_V1`):

```bash
bm miner add \
  --endpoint wss://203.0.113.50 \
  --alias my-node \
  --secret '<SECRET_V1 from your server .env>' \
  --price 0.01

# For testnet, prefix with --testnet:
bm --testnet miner add \
  --endpoint wss://203.0.113.50 \
  --alias my-node \
  --secret '<SECRET_V1 from your server .env>' \
  --price 0.01
```

Or run `bm miner add` with no flags for interactive prompts.

**What happens during registration:**
- The CLI connects to the registry and creates your node entry
- For IP-based endpoints, the CLI fetches and pins your TLS certificate fingerprint (so the gateway can verify your identity via cert pinning)
- For domain endpoints, standard CA verification is used (no pinning needed)
- Your secret is hashed and stored — the gateway uses it to authenticate when routing requests to you
- Your price bid is recorded for the next epoch

### Wait for sync and traffic

Your subtensor node needs to sync before it can serve requests. A lite node warp-syncs in about 15 minutes. An archive node takes much longer (use a snapshot to speed this up).

Check sync progress (on your server):

```bash
docker compose logs -f node
```

Once synced, the gateway will start routing traffic to your node. Verify everything is working:

```bash
bm miner test <alias>              # Test TLS, health, and authenticated RPC
bm miner show                      # Check status and last seen timestamp
```

`bm miner test` runs three checks: TLS handshake on port 443, health endpoint on port 80, and an authenticated `system_health` RPC call. You can also test before registering with `bm miner test --endpoint <url> --secret '<secret>'`.

Once traffic is flowing, check your node's performance:

```bash
bm miner metrics [alias]           # Quality score, latency, success rate
```

### Prometheus metrics

Each miner exposes authenticated Prometheus metrics through the same HTTPS gateway:

```bash
curl -fsS \
  -H "Authorization: Bearer <SECRET_V1 from your server .env>" \
  https://203.0.113.50/metrics
```

Prometheus scrape example:

```yaml
scrape_configs:
  - job_name: blockmachine-miners
    metrics_path: /metrics
    scheme: https
    authorization:
      type: Bearer
      credentials: <SECRET_V1 from your server .env>
    static_configs:
      - targets:
          - 203.0.113.50
```

Metrics are operator telemetry for visibility and alerting. They are authenticated and useful for debugging sync, peer, disk, TLS, and version rollout issues, but they are not proof that a miner is honestly serving work.

The endpoint includes Blockmachine-curated health metrics, host CPU/memory/load, node data volume usage, TLS certificate expiry, and the live deployed git SHA. It also appends the underlying client's native Prometheus output:

- **tao** — Subtensor metrics from the node's internal `:9615` endpoint
- **eth latest** — Reth metrics from `:9101`, plus a beacon-API sync probe against Lighthouse on `:5052`
- **eth archive** — Erigon metrics from `:6060/debug/metrics/prometheus`

## Pricing

You set a price in USD per Compute Unit (CU). A CU represents the normalized computational cost of serving a specific RPC method. Different methods cost different amounts of CU (a simple balance query costs less than a transaction trace).

The gateway routes traffic based on a combination of quality score and price — cheaper miners with good quality get more traffic. You only earn on successful responses (HTTP 200 with a JSON-RPC `result`).

Set or update your price:

```bash
bm miner price set --price 0.01    # USD per CU, effective next epoch
bm miner price show                # Current price
bm miner price history             # Price history
```

## TLS options

### Auto-renewing Let's Encrypt (recommended for domains)

Select "Let's Encrypt" during install. A certbot container handles issuance and auto-renewal every 12 hours. Requires a domain name with a DNS A record pointing to your server, and port 80 reachable for the ACME challenge.

### Self-signed (default for IP-only)

Generated automatically during install. Valid for 10 years. The CLI pins the certificate fingerprint during `bm miner add` so the gateway can verify your identity. No renewal needed.

### Bring your own

Place `cert.pem` and `key.pem` in the `ssl/` directory before running the install script. Select "no" when prompted about Let's Encrypt.

## Node tiers

### Subtensor: lite (default)

Syncs via warp sync in ~15 minutes. Serves recent blocks and current state. Lower storage requirements (~100 GB). Suitable for most RPC methods.

Historical state queries for pruned blocks will return `null` — the gateway routes these to archive nodes when available.

### Subtensor: archive

Full block history from genesis. Requires 4+ TB disk and takes days to sync from scratch. To speed up initial sync, use a snapshot:

```bash
# On your local machine, get a snapshot URL
bm miner snapshot --type archive

# Paste the URL when prompted during install on your server
```

To start an archive node:

```bash
docker compose -f docker-compose.yml -f docker-compose.archive.yml up -d
```

### Ethereum: latest

Reth `--full` preset: serves the latest state plus a rolling ~34-hour window of historical state. Disk footprint is around 240 GB on mainnet. `eth_getProof` works against any block in the window; older blocks return an error. Sync from scratch over P2P takes hours; a Lighthouse beacon node runs alongside reth and drives it via the Engine API (checkpoint-synced from `mainnet.checkpoint.sigp.io`, ready in minutes).

To start a latest-tier node manually:

```bash
docker compose -f docker-compose.eth.yml up -d
```

### Ethereum: archive

Erigon with `--prune.mode=archive`, plus erigon's built-in Caplin consensus client (no separate Lighthouse service needed). Disk footprint is around 3.5 TB on mainnet (state + snapshots + temp). Serves `eth_getProof`, `debug_*`, and `trace_*` against every block back to genesis. Initial sync downloads erigon's pre-built snapshots via torrents and typically takes a day or more.

The `--prune.mode=archive` flag must be set before the first sync. It cannot be changed afterwards — you would have to resync from scratch.

To start an archive-tier node manually:

```bash
docker compose -f docker-compose.eth-archive.yml up -d
```

## Manual setup (without install script)

If you prefer to set things up yourself:

```bash
git clone https://github.com/taostat/blockmachine-miner.git
cd blockmachine-miner

# Generate a secret
SECRET=$(openssl rand -base64 32 | tr -d '=/+' | head -c 43)

# Create .env
cp .env.example .env
# Edit .env: set SECRET_V1=$SECRET, set DOMAIN if using Let's Encrypt

# Generate self-signed cert (skip if using Let's Encrypt or BYO cert)
IP=$(curl -s ifconfig.me)
mkdir -p ssl
openssl req -x509 -nodes -days 3650 -newkey rsa:2048 \
  -keyout ssl/key.pem -out ssl/cert.pem \
  -subj "/CN=$IP" -addext "subjectAltName=IP:$IP"

# Start services — choose the stack for your chain and tier:
docker compose up -d                                          # tao lite (default)
# docker compose -f docker-compose.yml \
#   -f docker-compose.archive.yml up -d                       # tao archive
# docker compose -f docker-compose.eth.yml up -d              # eth latest (reth + lighthouse)
# docker compose -f docker-compose.eth-archive.yml up -d      # eth archive (erigon)

# For Let's Encrypt, add the TLS overlay (works for any chain):
# docker compose -f <chain-compose>.yml -f docker-compose.tls.yml up -d
```

Then register from your local machine:

```bash
bm miner login
bm miner add --endpoint wss://$IP --alias my-node --secret "$SECRET" --price 0.01

# For testnet:
bm --testnet miner login
bm --testnet miner add --endpoint wss://$IP --alias my-node --secret "$SECRET" --price 0.01
```

## Architecture

```
┌──────────────────────────────────────────────┐
│  Your Server                                 │
│                                              │
│  ┌──────────┐      ┌───────────────────┐     │
│  │  nginx   │ :443 │  subtensor node   │     │
│  │ gateway  │─────▶│  (lite/archive)   │     │
│  │          │ :9944│                   │     │
│  └──────────┘      └───────────────────┘     │
│    │ :80 (health + ACME)    │ :30333 (p2p)   │
│    │                        │                │
│    │ /metrics               │                │
│    ▼                        │                │
│  ┌──────────┐      ┌────────┴─────────┐      │
│  │ metrics  │─────▶│ node RPC + data  │      │
│  │          │─────▶│ native :9615     │      │
│  │          │─────▶│ host /proc       │      │
│  └──────────┘      └──────────────────┘      │
│                                              │
│  ┌──────────┐                                │
│  │ certbot  │ (optional)                     │
│  └──────────┘                                │
└──────────────────────────────────────────────┘
```

- **nginx gateway** — Terminates TLS, authenticates requests via bearer token, proxies WebSocket and HTTP RPC to the chain node. Supports dual secrets for zero-downtime rotation. The eth gateway template routes by the `Upgrade` header so a single 443 endpoint serves both JSON-RPC and `eth_subscribe`.
- **chain node** — Either a Bittensor subtensor (lite/archive), a reth execution client driven by Lighthouse over the Engine API (eth latest), or an erigon archive with built-in Caplin CL (eth archive).
- **metrics** — Exposes `/metrics` internally; nginx publishes it at `https://<endpoint>/metrics` with the same bearer auth as RPC.
- **certbot** — Optional. Auto-renews Let's Encrypt certificates. Only used with domain-based setups.

## Secret rotation (zero downtime)

Rotate your bearer token without dropping any traffic:

1. **Set the new secret as `next` in the registry:**
   ```bash
   bm miner secret set --secret '<new-secret>'
   ```

2. **Add the new secret to your server and restart the gateway:**
   ```bash
   # Edit .env: set SECRET_V2=<new-secret>
   docker compose up -d gateway
   ```
   The gateway now accepts both the old and new secret.

3. **Promote the new secret to `active`:**
   ```bash
   bm miner secret promote
   ```
   The gateway now sends requests using the new secret.

4. **Remove the old secret from your server:**
   ```bash
   # Edit .env: move SECRET_V2 value to SECRET_V1, clear SECRET_V2
   docker compose up -d gateway
   ```

## Configuration

Environment variables in `.env`:

| Variable | Default | Description |
|----------|---------|-------------|
| `SECRET_V1` | (required) | Primary bearer token |
| `SECRET_V2` | (empty) | Secondary token for zero-downtime rotation |
| `DOMAIN` | (empty) | Domain for Let's Encrypt auto-renewal |
| `CHAIN` | `tao` | Chain selection: `tao` or `eth` |
| `METRICS_PORT` | `9100` | Internal metrics exporter port |
| `SSL_CERT_PATH` | `/etc/nginx/ssl/cert.pem` | TLS certificate path in container |
| `SSL_KEY_PATH` | `/etc/nginx/ssl/key.pem` | TLS key path in container |
| `BM_MINER_GIT_SHA` | `unknown` | Deployed repository commit exposed in metrics |
| `BM_MINER_GIT_BRANCH` | `unknown` | Deployed repository branch exposed in metrics |

Chain-specific:

| Variable | Default | Used by | Description |
|----------|---------|---------|-------------|
| `BACKEND_PORT` | `9944` | tao | Subtensor node RPC port |
| `ETH_TIER` | (empty) | eth | `latest` or `archive` |
| `EL_CLIENT` | (empty) | eth | `reth` (latest) or `erigon` (archive) |
| `BACKEND_HTTP_PORT` | `8545` | eth | EL HTTP JSON-RPC port |
| `BACKEND_WS_PORT` | `8546` (reth) / `8545` (erigon) | eth | EL WebSocket port |
| `CHECKPOINT_SYNC_URL` | `https://mainnet.checkpoint.sigp.io` | eth latest | Lighthouse beacon checkpoint source |

## Day-to-day operations

### Monitoring

```bash
bm miner show                      # Node status, endpoint, last seen
bm miner ls                        # List all your nodes
bm miner metrics [alias]           # Quality score, latency, success rate
docker compose logs -f              # Container logs
curl -sf http://localhost/health    # Gateway health check
curl -fsS -H "Authorization: Bearer $SECRET_V1" https://<endpoint>/metrics
```

### Updating

Pull the latest config and chain images, then restart. Use the same compose
file the install script started — bare `docker compose` defaults to
`docker-compose.yml` (the subtensor stack), which would replace an eth stack's
containers with subtensor ones because service names collide.

```bash
# Subtensor (lite or archive):
cd /root/blockmachine-miner && git pull && docker compose pull && docker compose up -d

# Ethereum latest:
cd /root/blockmachine-miner && git pull \
  && docker compose -f docker-compose.eth.yml pull \
  && docker compose -f docker-compose.eth.yml up -d

# Ethereum archive:
cd /root/blockmachine-miner && git pull \
  && docker compose -f docker-compose.eth-archive.yml pull \
  && docker compose -f docker-compose.eth-archive.yml up -d
```

Or re-run the install script — it updates the repo automatically:

```bash
bash <(curl -sSL https://blockmachine.io/miner/install.sh)
```

### Stopping

```bash
docker compose down                                  # subtensor
docker compose -f docker-compose.eth.yml down        # eth latest
docker compose -f docker-compose.eth-archive.yml down # eth archive
```

Node data is persisted in a Docker volume (`node_data`). Restarting will resume from where it left off.

## CLI reference

All commands below default to mainnet. Add `--testnet` after `bm` for testnet: `bm --testnet miner ...`

```bash
# Authentication
bm miner login                         # Authenticate with miner scopes
bm miner status                        # Check auth status
bm miner logout                        # Clear stored tokens

# Node management
bm miner add                           # Register a node (interactive)
bm miner use <alias>                   # Set active node for commands
bm miner ls                            # List all nodes
bm miner show [alias]                  # Show node details
bm miner update [alias] --endpoint ... # Change endpoint or alias
bm miner rm [alias]                    # Remove a node

# Secrets
bm miner secret set [alias]            # Set bearer token secret
bm miner secret show [alias]           # Show secret metadata
bm miner secret promote [alias]        # Promote next secret to active

# Testing & metrics
bm miner test <alias>                  # Test TLS, health, and auth RPC
bm miner test --endpoint <url> --secret '<secret>'  # Test before registering
bm miner metrics [alias]              # Quality score, latency, success rate

# Pricing
bm miner price set [alias] --price ... # Set price per compute unit
bm miner price show [alias]            # Show current price
bm miner price history [alias]         # Show price history

# Snapshots
bm miner snapshot                      # Get snapshot download URL
```

When `[alias]` is omitted, the active node (set via `bm miner use`) is used.

## Troubleshooting

**Port 80 or 443 in use:** Stop the process using the port (`sudo lsof -i :443`) and try again.

**Gateway unhealthy:** The subtensor node takes time to sync. Check `docker compose logs node` for sync progress. A lite node should be healthy within 15-20 minutes.

**Authentication errors:** Run `bm miner login` to re-authenticate, then `bm miner secret show` to verify your secret is registered.

**Node not receiving traffic:** Check `bm miner show` — status should be `active`. If `pending`, the gateway hasn't connected yet (node may still be syncing). If `unreachable`, the gateway can't reach your endpoint — check firewall rules and that ports 443 is open.

**TLS errors (self-signed):** The gateway uses certificate pinning to verify your identity. If you regenerate your certificate, you need to re-register your node (`bm miner rm` then `bm miner add`) so the new fingerprint is captured.

**TLS errors (Let's Encrypt):** Check `docker compose logs certbot`. Ensure your domain's DNS A record points to your server and port 80 is reachable for the ACME challenge.

**Node not syncing:** Ensure port 30333 (p2p) is open for outbound connections to the Bittensor network. Check `docker compose logs node` for peer connection status.

**Firewall setup:** If using `ufw`, allow the required ports:

```bash
sudo ufw allow 80/tcp && sudo ufw allow 443/tcp && sudo ufw allow 30333/tcp
```
