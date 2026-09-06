# Blockmachine Miner

- [How it works](#how-it-works)
- [Node Eligibility Requirements](#node-eligibility-requirements)
- [Capacity Testing](#capacity-testing)
- [Getting started](#getting-started)
- [Pricing](#pricing)
- [TLS options](#tls-options)
- [Manual setup](#manual-setup-without-install-script)
- [Architecture](#architecture)
- [Secret rotation](#secret-rotation-zero-downtime)
- [Configuration](#configuration)
- [Day-to-day operations](#day-to-day-operations)
- [CLI reference](#cli-reference)
- [Troubleshooting](#troubleshooting)

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

1. You run your own node for your chosen chain, fronted by our nginx gateway that authenticates requests from the Blockmachine network
2. The protocol gateway routes customer RPC requests to your node based on quality score and price
3. Validators read gateway logs, verify correctness, and submit weights on-chain each epoch (~72 minutes)
4. You earn emissions proportional to the CUs you served at your bid price

## Node Eligibility Requirements

Eligibility is **per chain and per node**. A node failing on one chain does not affect your
nodes on other chains.

### Universal requirements (every chain, every node type)

| # | Requirement | What we check |
|---|---|---|
| U1 | **Correct chain** | `eth_chainId` (or the substrate genesis hash on TAO) matches the chain you registered the node on. |
| U2 | **Honest node type** | You declare `archive` or `full`. The declaration is verified (see below). Claiming archive without archive state is an eligibility failure, not a scoring nuance. |
| U3 | **Reachable as registered** | The exact endpoint URL you registered must accept WebSocket connections and answer JSON-RPC on it. If your endpoint lives under a path, register the URL **with** the path. We test what you registered, not what you meant. |
| U4 | **At the tip** | The node tracks the chain head. A node that persistently lags the network tip is not serving the chain. |
| U5 | **Standard responses** | Correct JSON-RPC 2.0 shapes, including on errors. Proxies that return HTML error pages, rewrite error codes, or inject non-standard responses fail this. |
| U6 | **Standard subscriptions** | `eth_subscribe(["newHeads"])` (or substrate equivalent) must work and actually deliver notifications. |
| U7 | **Consistent identity** | `web3_clientVersion` reports a real client honestly, matching the chain's accepted-client list (enforced at registration and re-checked continuously). Masking what you run is an eligibility failure. Known exception: Avalanche's coreth reports a bare version string with no client name, so Avalanche uses chain-ID + behaviour checks instead of a name match — an honest coreth node is never failed for its client string. |
| U8 | **Continuous service** | A required feature must be served **continuously**, not occasionally. A feature that works some of the time is treated as not served. (Brief reconnect blips are tolerated; flapping is not.) |

### Archive node requirements

A node declared **archive** must serve **state and blocks all the way back to genesis**.

- Verified by random sampling: we ask for full blocks, state reads and (where the chain's
  clients support it) traces at randomly chosen historical heights across the entire chain
  history. There is no depth that is safe to prune.
- The samples are never announced in advance and never reused, so there is nothing to warm and
  nothing to precompute. The only way to pass is to hold the data.
- An archive node must also serve everything a full node serves.

### Full node requirements

A node declared **full** must serve the chain head and the recent range correctly:

- Full blocks and state for at least the **last 100 blocks**. (This floor will be configured
  individually per chain in the near future; changes are published before they apply.)
- All universal requirements above.
- A full node is never asked archive-depth questions and is never penalised for honestly being
  a full node. Declaring `full` while actually serving archive is fine; declaring `archive`
  while serving full is not.

### Per-chain requirements

| Chain | Accepted client families | Mempool | Trace/debug | State proofs |
|---|---|---|---|---|
| **Ethereum** (`eth`, chain-id 1) | **reth** — run this. (erigon is also accepted, but only because we operate one erigon node ourselves for proof-based verification; miners should run reth.) | ✅ | ✅ | ❌ |
| **BSC** (`bsc`, 56) | geth (bsc-geth), reth (reth-bsc) | ✅ | ✅ | ✅ |
| **Base** (`base`, 8453) | reth | ❌ | ✅ | ✅ |
| **Optimism** (`optimism`, 10) | reth (op-reth) | ✅ | ✅ | ✅ |
| **Polygon** (`polygon`, 137) | bor | ✅ | ✅ | ✅ |
| **Avalanche C-Chain** (`avalanche`, 43114) | coreth / avalanchego (see U7 exception) | ❌ | ✅ | ✅ |
| **Scroll** (`scroll`, 534352) | geth (scroll-geth) | ✅ | ✅ | ✅ |
| **Mantle** (`mantle`, 5000) | geth (mantle op-geth) | ❌ | ✅ | ✅ |
| **Arbitrum One** (`arbitrum`, 42161) | nitro (a geth fork — either name is accepted in the version string) | ❌ | ✅ | ✅ |
| **Robinhood Chain** (`robinhood`, 4663) | nitro (as above) | ❌ | ✅ | ✅ |
| **TAO (Bittensor)** (`tao`) | subtensor | ✅ | ❌ | ✅ |

**Why Base has no mempool requirement.** Base's sequencer is private: public nodes cannot observe a meaningful pending-transaction feed, so there is nothing a node could serve and nothing we could fairly measure. Mempool is therefore not required on Base (same as the other chains marked ❌).

**Why proofs are first-class.** Proof-emitting methods (`eth_getProof` on EVM chains,
`state_getReadProof` on substrate) return answers that can be verified mathematically against
the block's state root — no comparison node, no trust. Where the chain's clients emit proofs,
we verify them, and a proof that does not check out is treated as a wrong answer, not a
formatting quirk.
### How testing works

- Every node is re-tested **regularly and automatically**. Tests are randomly timed; there is
  no schedule to prepare for.
- A failure that looks transient (a timeout while your node is under heavy load, a brief
  disconnect) is retried before any flag is set. A node is never flagged on a single ambiguous
  observation.
- A failure that cannot be transient (wrong chain ID, a required method returning
  "method not found", pruned state on a declared archive) flags immediately.
- **New nodes are tested first, with priority.** A newly registered node must pass the full
  eligibility battery for its chain and type **before** it becomes eligible for traffic and
  incentive. You will not wait long: new-node tests jump the queue.
- Your node's current eligibility, the reason for any flag, when it was last tested, and when
  it will be re-tested are all visible in the miner API.

### What eligibility failure means

- **No traffic** routed to that node on that chain.
- **No incentive** earned by that node on that chain — a node that is not eligible is not in
  the payout set at all.
- Fix the issue, pass the next test, and both come back automatically.

## Capacity Testing

Eligibility (see *Node Eligibility Requirements*) decides whether a node serves what the chain
requires. **Capacity testing decides how much traffic an eligible node deserves.** It measures
the hardware behind a node under a sudden heavy load and turns that into a multiplier on the
node's routing weight. Operators who run genuinely strong hardware receive more requests and
more incentive; operators who run many small boxes do not.

### What we measure

We take your node out of customer routing for a moment and fire **a large burst of heavy
requests at it all at once**: hundreds of full block traces of expensive blocks, the kind of
work paying customers actually ask for. Then we run the clock on every one of them:

- A correct answer costs the seconds it took to come back.
- An error, a malformed or incorrect answer, or anything slower than 10 seconds costs a flat
  20 seconds.

The total is your score for the burst. **Lower is better.** We run three bursts on three
different sets of blocks and take the mean.

Archive nodes are asked deep historical blocks, the same blocks for every archive node on the
chain in that run, so results compare like for like. Full nodes are asked the newest blocks at
the moment of the test, all full nodes on the chain at the same time, and "correct" is what the
majority of well-formed answers agreed on.

### What the score does

Your multiplier is your score against the best score on the chain in that run:

```
multiplier = best score ÷ your score
```

The fastest node on the chain gets 1.0. A node twice as slow gets 0.5. A node that refuses or
times out on half of a burst pays 20 seconds for every one of those requests and ends up near
zero. The multiplier concentrates traffic on the nodes that can take a surge and answer it
fast: a fleet of small, slow boxes earns by quality, not by numbers.

- The multiplier scales your node's routing weight on that chain. Higher multiplier, larger
  share of traffic and incentive.
- Scores are replaced by newer runs; they do not decay. A hardware upgrade shows up the next
  time you are measured.
- A run that could not measure your node fairly (a problem on our side, chain conditions)
  writes no score. An inconclusive run never harms you.
- Operators running nodes on several chains can expect those nodes to be tested **at the same
  moment**. Nodes that share one machine slow down together, and the scores will say so.

### What we deliberately do not publish

The burst size, the block selection, the exact request mix and the run schedule, and we change
them. Every published detail becomes a thing to tune for instead of a thing to be. What we do
publish are the properties that make the test fair:

- **The same questions for every node in a run**, fresh blocks every run, never the same block
  twice to the same node. Caches and canned answers do not help.
- **The workload is genuinely heavy.** Full traces of large blocks, not point reads a hot cache
  serves in a millisecond. A connection cap or a request limit in front of your node turns
  refused requests into 20-second penalties; the only way to score is to serve the burst.
- **The test hits the endpoint you registered.** There is no separate test endpoint to
  special-case, and the burst arrives with your node drained of customer traffic, so it
  measures your node and nothing else.

### Fairness commitments

- Every node on a chain is measured on the same questions in the same run.
- Inconclusive measurements never lower a score.
- Scores, reasons and measurement times are visible to you.
- Methodology changes are published before they change anyone's routing. This section is that
  publication for the burst test.

## Getting started

**You bring the node.** Provisioning, syncing, disk sizing and snapshots are your
responsibility — we provide the gateway that fronts your node and the CLI that registers and
manages it. Your node must meet the eligibility requirements for its chain and declared type.

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
- Ask which chain your node serves
- Ask whether you have a domain name or are using an IP address
- Set up TLS (auto-renewing Let's Encrypt for domains, or self-signed for IPs)
- Generate a bearer token secret
- Start the gateway, pointed at your node's RPC port
- Print the registration commands to run from your local machine

At the end you'll see output like:

```
========================================
 Miner is running!
========================================

  Endpoint: wss://203.0.113.50
  Chain:    tao
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

### Verify and receive traffic

Once your node is synced and the gateway is up, the network starts routing traffic to you.
Verify everything is working:

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

The endpoint includes Blockmachine-curated health metrics, host CPU/memory/load, TLS certificate expiry, and the live deployed git SHA, and appends your chain client's native Prometheus output where the gateway can reach it.

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

# Start the gateway stack, pointed at your node's RPC port:
docker compose up -d

# For Let's Encrypt, add the TLS overlay:
# docker compose -f docker-compose.yml -f docker-compose.tls.yml up -d
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
│  │  nginx   │ :443 │  your chain node  │     │
│  │ gateway  │─────▶│  (yours to run)   │     │
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
- **chain node** — yours: any accepted client for your chain, meeting the eligibility requirements for its declared type.
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
| `CHAIN` | `tao` | Chain selection |
| `METRICS_PORT` | `9100` | Internal metrics exporter port |
| `SSL_CERT_PATH` | `/etc/nginx/ssl/cert.pem` | TLS certificate path in container |
| `SSL_KEY_PATH` | `/etc/nginx/ssl/key.pem` | TLS key path in container |
| `BM_MINER_GIT_SHA` | `unknown` | Deployed repository commit exposed in metrics |
| `BM_MINER_GIT_BRANCH` | `unknown` | Deployed repository branch exposed in metrics |

Chain-specific:

| Variable | Default | Used by | Description |
|----------|---------|---------|-------------|
| `BACKEND_PORT` | `9944` | tao | Your node's RPC port |
| `BACKEND_HTTP_PORT` | `8545` | EVM chains | Your node's HTTP JSON-RPC port |
| `BACKEND_WS_PORT` | `8546` | EVM chains | Your node's WebSocket port |

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

Pull the latest gateway config and images, then restart:

```bash
cd /root/blockmachine-miner && git pull && docker compose pull && docker compose up -d
```

Or re-run the install script — it updates the repo automatically:

```bash
bash <(curl -sSL https://blockmachine.io/miner/install.sh)
```

### Stopping

```bash
docker compose down
```

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

```

When `[alias]` is omitted, the active node (set via `bm miner use`) is used.

## Troubleshooting

**Port 80 or 443 in use:** Stop the process using the port (`sudo lsof -i :443`) and try again.

**Gateway unhealthy:** Your node may still be syncing — check your node's own logs.

**Authentication errors:** Run `bm miner login` to re-authenticate, then `bm miner secret show` to verify your secret is registered.

**Node not receiving traffic:** Check `bm miner show` — status should be `active`. If `pending`, the gateway hasn't connected yet (node may still be syncing). If `unreachable`, the gateway can't reach your endpoint — check firewall rules and that ports 443 is open.

**TLS errors (self-signed):** The gateway uses certificate pinning to verify your identity. If you regenerate your certificate, you need to re-register your node (`bm miner rm` then `bm miner add`) so the new fingerprint is captured.

**TLS errors (Let's Encrypt):** Check `docker compose logs certbot`. Ensure your domain's DNS A record points to your server and port 80 is reachable for the ACME challenge.

**Firewall setup:** If using `ufw`, allow the required ports:

```bash
sudo ufw allow 80/tcp && sudo ufw allow 443/tcp
```
