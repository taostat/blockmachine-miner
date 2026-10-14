# Blockmachine Miner

- [How it works](#how-it-works)
- [Node Eligibility Requirements](#node-eligibility-requirements)
- [Capacity Testing](#capacity-testing)
- [Capacity helper](#capacity-helper-optional-archive-nodes)
- [Base Incentive](#base-incentive)
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
| U4 | **At the tip** | The node tracks the chain head. It must stay within **90 seconds of chain progress** of the highest head we can see on that chain — see *Staying at the tip* below for the per-chain block allowance. A node that persistently lags the network tip is not serving the chain. |
| U5 | **Standard responses** | Correct JSON-RPC 2.0 shapes, including on errors. Proxies that return HTML error pages, rewrite error codes, or inject non-standard responses fail this. |
| U6 | **Standard subscriptions** | `eth_subscribe(["newHeads"])` (or substrate equivalent) must work and actually deliver notifications. |
| U7 | **Consistent identity** | `web3_clientVersion` reports a real client honestly, matching the chain's accepted-client list (enforced at registration and re-checked continuously). Masking what you run is an eligibility failure. Known exception: Avalanche's coreth reports a bare version string with no client name, so Avalanche uses chain-ID + behaviour checks instead of a name match — an honest coreth node is never failed for its client string. |
| U8 | **Continuous service** | A required feature must be served **continuously**, not occasionally. A feature that works some of the time is treated as not served. (Brief reconnect blips are tolerated; flapping is not.) |

### Archive node requirements

A node declared **archive** must serve **blocks, state, logs, traces (where the chain requires
them) and transaction lookups all the way back to genesis**: any transaction in any block must
be retrievable by its hash, so must its receipt, and so must the events that transaction
emitted.

- Verified by random sampling: we ask for full blocks, state reads and (where the chain's
  clients support it) traces at randomly chosen historical heights across the entire chain
  history. There is no depth that is safe to prune.
- Geth-family clients (geth, bor and their forks) keep only a recent transaction index unless
  told otherwise: `--history.transactions=0` (geth 1.14 and later) or `--txlookuplimit=0`
  (older geth, bor) keeps every transaction retrievable by hash. Geth sets this itself under
  `--gcmode=archive`; check that your client and mode do the same.
- **Logs are pruned with the receipts.** On every EVM chain the log index lives with the
  receipt data, so the setting that decides how much receipt history your client keeps also
  decides how far back `eth_getLogs` will answer: `--prune.receipts` on reth,
  `--prune.distance` on erigon, `--gcmode=archive` on geth-family clients. Keeping block
  bodies is not enough. An archive node must answer `eth_getLogs` for any block in the chain's
  history, not only for recent ones. Applies from the eligibility run after this note is
  published.
- The samples are never announced in advance and never reused, so there is nothing to warm and
  nothing to precompute. The only way to pass is to hold the data.
- An archive node must also serve everything a full node serves.

### Full node requirements

A node declared **full** must serve the chain head and the recent range correctly:

- Full blocks, state, logs and transaction lookups by hash (with their receipts) for at least
  the **last 100 blocks**, except where a chain sets its own floor (below). Changes are
  published here before they apply.
  - **Robinhood Chain: the last 5,000 blocks.** Robinhood makes a block every 100 ms, so 100
    blocks is ten seconds of history; 5,000 blocks (about eight minutes) is the floor a full
    node must serve. Applies from the eligibility run after this note is published.
  - **Polygon: the last 250 blocks.** Polygon makes a block every 2 seconds, so 100 blocks is
    three minutes of history; 250 blocks (about eight minutes) is the floor a full node must
    serve. Applies from the eligibility run after this note is published.
  - **Ethereum: the last 10,000 blocks.** This is not a new demand on your hardware: it is
    what reth `--minimal` and erigon `--prune.distance=10064` already retain by default, less
    a small safety margin, and it is the depth the gateway has always routed to non-archive
    ETH backends. Stating it here makes the published floor match what we actually ask for —
    previously the document said 100 while registration required 10,000, so a node built
    exactly to this page could be refused and not told why. Every ETH full node currently
    registered already meets it. Applies from the eligibility run after this note is
    published.
- **Logs are included in that range.** On every EVM chain `eth_getLogs` must answer for any
  block inside your node's floor above, and the log index is pruned with the receipt data
  (see the archive section for the client settings that govern it). Applies from the
  eligibility run after this note is published.
- **One number per chain, and it is both what we send you and what we ask of you.** Your
  chain's floor above is the whole of it: blocks, state, logs, receipts and transaction
  lookups alike. Requests older than it go to archive nodes, not to full nodes, so a full
  node is never sent historical work it did not promise — and eligibility is judged against
  that same floor and nothing deeper. If your chain's full nodes hold far more than the
  floor says, tell us: the way that work reaches you is a deeper published floor for the
  chain, measured against the nodes actually registered before we publish it, and it moves
  both what you are sent and what you must hold. Applies from the eligibility run after this
  note is published.
- All universal requirements above.
- **Eligibility never asks a full node an archive-depth question**, and a node is never
  penalised for honestly being a full node. Declaring `full` while actually serving archive
  is fine; declaring `archive` while serving full is not.

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

**Base requires Flashblocks.** Every Base node, full or archive, must have Flashblocks enabled, so that customers can use the `newFlashblocks` and `pendingLogs` subscriptions. A Base node without Flashblocks is not eligible. On the official Base node image (`ghcr.io/base/node`), set this environment variable on the execution client and restart it:

```
RETH_FB_WEBSOCKET_URL=wss://mainnet.flashblocks.base.org/ws
```

That is Base's public mainnet Flashblocks feed and needs no key. If you start `base-reth-node` yourself, pass the same URL with `--flashblocks-url`. Once the node is up, its log shows `Starting Flashblocks RPC`, and an `eth_subscribe` to `newFlashblocks` over WebSocket returns a subscription id and then streams updates several times a second.

**Why proofs are first-class.** Proof-emitting methods (`eth_getProof` on EVM chains,
`state_getReadProof` on substrate) return answers that can be verified mathematically against
the block's state root — no comparison node, no trust. Where the chain's clients emit proofs,
we verify them, and a proof that does not check out is treated as a wrong answer, not a
formatting quirk.
### Staying at the tip

Your node must stay within **90 seconds of chain height**. We do not ask your node
where the chain head is and measure back from there — a node that has stopped
following the chain would pass that test forever, answering questions about its own
frozen head. We compare your node's head against the **highest head we can see
across all nodes on that chain**.

Ninety seconds is the same allowance everywhere. The block count differs only
because the chains run at different speeds:

| Chain | Full node | Archive node |
|---|---|---|
| **Ethereum** (`eth`) | 9 blocks | 55 blocks |
| **BSC** (`bsc`) | 208 | 1,385 |
| **Base** (`base`) | 46 | 305 |
| **Optimism** (`optimism`) | 48 | 320 |
| **Polygon** (`polygon`) | 63 | 415 |
| **Avalanche C-Chain** (`avalanche`) | 93 | 615 |
| **Scroll** (`scroll`) | 90 | 595 |
| **Mantle** (`mantle`) | 48 | 315 |
| **Arbitrum One** (`arbitrum`) | 378 | 2,515 |
| **Robinhood Chain** (`robinhood`) | 972 | 6,475 |
| **TAO (Bittensor)** (`tao`) | 8 | 50 |

- **Archive nodes get ten minutes, not ninety seconds.** An archive node that is
  behind is usually behind because it is doing expensive historical work, which is
  the job we are paying it for.
- **The allowances are sized from the fastest we have ever seen each chain run.**
  Several chains routinely beat their own published block time, so sizing from the
  documented figure would quietly make the rule stricter than ninety seconds. This
  way ninety seconds is a floor: when a chain is running slowly, the same block
  count buys you more time, never less.
- **Falling behind is treated as temporary**, and is retried before anything is
  flagged (see *How testing works*). A node catching up after a restart is not a
  node that has failed.
- These numbers change only when a chain changes its block speed, and any change is
  published here before it applies.

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

We take your node out of customer routing for a moment and fire **a burst of requests at it
all at once**, the kind of work paying customers actually ask for, in four kinds:

| Workload | EVM chains | Weight | Tao | Weight |
|---|---|---|---|---|
| State | balances, nonces, storage, code and contract calls at old blocks | 15% | storage values at old blocks | 30% |
| Heavy | traces of transactions and calls | 35% | pages of storage, key listings and runtime calls | 40% |
| Indexed | log queries, receipts and transaction lookups | 35% | a block's events and block hashes | 20% |
| Block | blocks and headers | 15% | blocks and headers | 10% |

Each workload's questions are sent together, over many connections at once, and the clock
runs from the first question sent to the last correct answer received:

- A correct answer inside the time limit counts the time it took.
- An error, a malformed or incorrect answer, no answer, a refused connection, or an answer
  that arrives after the limit costs a flat charge of **twice the limit**. The limit is
  10 seconds for heavy work and for log queries on EVM chains, and 2 seconds for everything
  else. A late answer is never cheaper than no answer, and no answer is never cheaper than
  answering in time.

**Lower is better.** Every question names the block it is about. The questions are standard
read calls in many different shapes, drawn fresh for every run, so that what is measured is a
node serving real requests rather than a node prepared for one particular request.

Archive nodes are asked about historical blocks, the same questions for every archive node on
the chain in that run, so results compare like for like, and every answer must be exactly
right. Full nodes are asked about the newest blocks at the moment of the test, all full nodes
on the chain at the same time.

### What the score does

For each workload, your multiplier is the best time on the chain against yours, capped at 1:

```
workload multiplier = best time ÷ your time      (at most 1.0)
score               = the four workload multipliers, weighted by the table above
```

The fastest node on a workload gets 1.0 for it. A node twice as slow gets 0.5. A node that
refuses or times out on half of a burst pays the flat charge for every one of those requests
and ends up near zero on that workload. The score concentrates traffic on the nodes that can
take a surge and answer it fast: a fleet of small, slow boxes earns by quality, not by numbers.

A node that gets not one question right on a workload, or that advertises archive and cannot
answer archive questions, scores zero: that is a statement about what data it holds, not about
how fast it is.

- The score scales your node's routing weight on that chain. Higher score, larger share of
  traffic and incentive.
- Scores are replaced by newer runs; they do not decay. A hardware upgrade shows up the next
  time you are measured.
- A run that could not measure your node fairly (a problem on our side, chain conditions)
  writes no score. An inconclusive run never harms you.
- Operators running nodes on several chains can expect those nodes to be tested **at the same
  moment**. Nodes that share one machine slow down together, and the scores will say so.

### What we deliberately do not publish

Which blocks are chosen and how, the exact questions in a run, how answers are checked, and
the run schedule, and we change them. Every published detail becomes a thing to tune for
instead of a thing to be. What we do publish is how you are marked (above) and the properties
that make the test fair:

- **The same questions for every node in a run**, fresh blocks every run, never the same block
  twice to the same node. Caches and canned answers do not help.
- **The workload is genuinely heavy.** Traces, historical state and log queries, not point
  reads a hot cache serves in a millisecond. A connection cap or a request limit in front of
  your node turns refused requests into flat charges; the only way to score is to serve the
  burst.
- **The test hits the endpoint you registered**, and the burst arrives with your node drained
  of customer traffic, so it measures your node and nothing else. You may also run the optional
  [capacity helper](#capacity-helper-optional-archive-nodes) at `/capacity-test` on that same
  endpoint; it changes how much of an answer crosses the internet, never which questions are
  asked or how they are marked.
- **Distance from us is not scored.** Every connection is warmed with unscored reads before
  the questions go out, and one round trip between our tester and your node is taken off each
  workload's time.

### Run the test yourself

For an archive node you can download the questions from our last run on your chain, with the
correct answers and the times the field managed, and put them to your own node as often as you
like. Nothing it measures reaches us or changes your score.

```bash
bm miner capacity-pack --chain eth
unzip capacity-test-eth.zip && cd capacity-test
./run ws://your-node:8545
```

The zip carries the test program, `capacity-replay`. It is also published with each
[release](https://github.com/taostat/blockmachine-miner/releases/latest) as
`capacity-replay-linux-x86_64`, for running a pack you already have:

```bash
capacity-replay --pack pack.json --url ws://127.0.0.1:8545 --label "my node"
```

Your number will not match ours exactly: we measure your node through its registration with
it out of customer traffic, and this measures it directly, while it serves. A pack is a record
of a run that has already happened; every real run draws fresh questions, so practising
against it gains nothing. Full nodes have no pack, because their questions are the newest
blocks at the moment of the test.

### Fairness commitments

- Every node on a chain is measured on the same questions in the same run.
- Inconclusive measurements never lower a score.
- Scores, reasons and measurement times are visible to you.
- Methodology changes are published before they change anyone's routing. This section is that
  publication for the burst test.

## Capacity helper (optional, archive nodes)

The capacity test measures how fast your node computes answers. Some of those answers are
megabytes long, and without help, the time they take to cross the internet to our tester is
counted too, so a node far from the tester scores lower than the same node close to it.

The **capacity helper** removes most of that. It is a small program that runs next to your node.
During a capacity test it puts our questions to your node from inside your own server and sends
back a short summary of each answer instead of the whole answer. A megabyte takes several trips
across the internet to reach us and a short summary takes one, so what the helper removes is the
extra cost of carrying large answers over distance.

- **Optional.** Without it, your node is tested directly, the way every node with no
  helper is.
- **Archive nodes only, for now.** Full nodes are always tested directly.
- **It can raise your score, and that moves where everyone stands.** A capacity score is
  measured against the fastest node on your chain, so as nodes take up the helper the
  standard rises and a node without one can score lower than it did, even though nothing
  about it changed. We think that is the right trade: the time spent shipping a large
  answer across the internet is not your node computing, and it should not have counted.
  But it is a real effect and you should hear it from us.
- **It does not change what we ask or how we mark it.** Customer traffic, eligibility, and
  the questions stay the same, and your node is scored on the same answers it would have
  given anyway.
- **Light.** One program, using about 5 MB of memory when idle, and it does nothing at all
  between tests.

### Requirements

- **Run it on the same machine as your node**, or at least on the same local network. If the
  helper has to reach your node over the internet, you lose the benefit.
- **It must be reachable at `/capacity-test` on the endpoint you registered**, with the same host and
  port. If you registered `wss://203.0.113.10`, the helper must answer at
  `wss://203.0.113.10/capacity-test`.
- **It checks your node's Blockmachine secret** (`SECRET_V1` from your `.env`, the one you
  registered) and refuses every connection without it.
- **If your endpoint serves its own certificate**, which it does if you registered an IP
  address rather than a domain, `capacity-helper check` needs that certificate's fingerprint
  to connect. Run the check once without it and it prints the fingerprint, and the command to
  run pinned to it. This affects the check only: we test your node against the certificate you
  registered with us either way.
- **It reads its secret once, when it starts.** So a secret rotation leaves it holding the
  old one until you restart it. From `bm miner secret promote` onwards we present the new
  secret, the helper refuses it, and your node is tested directly. Nothing is lost while that
  is true — those runs are ordinary direct tests — but it does not fix itself, so the last
  step of [Secret rotation](#secret-rotation-zero-downtime) is where you give the helper the
  new secret and restart it.

### Install it

1. **Download it** from this repository's
   [latest release](https://github.com/taostat/blockmachine-miner/releases/latest). It is one
   file, for x86-64 and for arm64:

   These run anywhere — your home directory is fine. They put the helper on your `PATH`
   as `capacity-helper`:

   ```bash
   base=https://github.com/taostat/blockmachine-miner/releases/latest/download
   asset="capacity-helper-linux-$(uname -m)"
   curl -fsSLO "$base/$asset" -fsSLO "$base/SHA256SUMS"
   sha256sum -c SHA256SUMS --ignore-missing
   sudo install -m 0755 "$asset" /usr/local/bin/capacity-helper
   ```

   Check the checksum before you install it; `sha256sum -c` must say `OK`. Keep the
   downloaded name until you have, because that is the name the checksums are written
   against. Do not run these from a clone of this repository: there is a directory called
   `capacity-helper` there, and installing onto that name would put the binary inside it.

2. **Run it**, pointed at your node's WebSocket RPC port:

   ```bash
   CAPACITY_HELPER_SECRET='<your node secret>' capacity-helper \
     --node ws://127.0.0.1:8546 \
     --family evm \
     --listen 127.0.0.1:9955
   ```

   - **`--node`:** your node's WebSocket RPC address as the helper sees it. On a tao node this
     is typically `ws://127.0.0.1:9944`.
   - **`--family`:** `evm` for every EVM chain, `substrate` for tao.
   - **`--listen`:** where the helper answers. Which address is right depends on what
     forwards `/capacity-test` to it:
     - **Your own proxy, running directly on this machine:** use `127.0.0.1:9955`, so
       nothing outside the machine can reach the helper at all.
     - **The gateway stack in this repository:** the gateway is a container, and a
       container's `127.0.0.1` is its own, not the host's — a helper on the host's
       loopback is unreachable from it, and every capacity test gets a 502. Bind to the
       address Docker gives the host instead:

       ```bash
       docker network inspect bridge -f '{{(index .IPAM.Config 0).Gateway}}'
       ```

       That prints the address to use, almost always `172.17.0.1`, so the helper is
       started with `--listen 172.17.0.1:9955`. `0.0.0.0:9955` also works, but then
       block port 9955 at your firewall: the helper turns away anything without your
       secret, but there is no reason to offer it to the internet.

   To keep it running, there is a systemd unit at `capacity-helper/capacity-helper.service`
   in this repository; its install steps are in the comment at the top of the file, and they
   are meant to be run from the root of a clone of it.

3. **Route `/capacity-test` to it.**

   **If you run the gateway stack in this repository, the routing is already done** — it
   sends `/capacity-test` to port 9955 on the host. The one thing you must get right is the
   address the helper listens on, in the step above: the gateway cannot reach the host's
   `127.0.0.1`. If you put the helper somewhere else entirely, set `CAPACITY_HELPER_HOST`
   and `CAPACITY_HELPER_PORT` in `.env` and run `docker compose up -d gateway`.

   The route is always present, whether or not you run a helper. With no helper behind it
   a capacity test gets a 502 there and your node is tested directly, exactly as it is for
   every node that runs no helper.

   Otherwise, add the path to whatever terminates TLS on your registered endpoint. It is a
   WebSocket connection, so pass the upgrade headers through, and pass `Authorization`
   through unchanged.

   nginx:
   ```nginx
   location = /capacity-test {
       proxy_pass http://127.0.0.1:9955;
       proxy_http_version 1.1;
       proxy_set_header Upgrade $http_upgrade;
       proxy_set_header Connection "upgrade";
       proxy_set_header Authorization $http_authorization;
       proxy_read_timeout 600s;
   }
   ```

   Caddy:
   ```caddy
   handle /capacity-test {
       reverse_proxy 127.0.0.1:9955
   }
   ```

   If nothing sits in front of your node and it serves TLS itself, you need a reverse proxy
   for this path. Without the helper, your node is tested directly.

4. **Check it**, from anywhere:

   ```bash
   capacity-helper check --endpoint wss://203.0.113.10 --secret '<your node secret>'
   ```

   Put your own registered endpoint there, without a path. This connects to `/capacity-test`
   exactly as our tester will, puts one question to your node through the helper, and prints
   either `ok` or what is wrong. If your endpoint serves its own certificate, it prints that
   certificate's fingerprint and the command to run pinned to it.


### Keep it up to date

Check which version you run with `capacity-helper --version`. **Run 0.3.0 or later.** An older
helper cannot take part in the current test: our tester finds that out before the test starts
and tests your node directly, so it costs you nothing, but it also gives you none of the
helper's benefit. Version 0.1.0, from the first release, is too old on every chain; 0.2.0 is
too old on Tao. Once you have the new file, `capacity-helper check` warns you if the helper
answering on your endpoint is still an old one.

To upgrade, download and install the new file exactly as in step 1, then restart the helper
(`sudo systemctl restart capacity-helper` if you use the systemd unit). Your settings do not
change.

### What happens if the helper is down or misconfigured

**If our tester cannot reach a working helper at `/capacity-test` when a test starts, it tests
your node directly.** A helper that is stopped, crashed, not routed, or set to the wrong chain
costs you nothing: we find out before the test begins and go straight to your node.

**Once a test has started through your helper it is different.** From that point your node's
questions go through it, and anything it does not deliver is charged to your node the same way
it is when your node stops answering. That is true whether it stops part way through or never
passes a single question on. So run it under something that keeps it up — the systemd unit in
this repository restarts it — and use `capacity-helper check` after any change to your setup.

## Base incentive

Alongside what the network pays you for the traffic you serve, each chain has a **base
incentive** — a separate payment shared out among the miners serving that chain. 90% of each
chain's base incentive is set aside for its archive nodes, 10% for the lite nodes. The rule below is about that
archive part, and it changes nothing about the rest.

**The archive part is paid to the five archive nodes on each chain that served the most
traffic.** On every chain, the eligible archive nodes that served the most traffic in an epoch
are ranked, and the top five share that chain's archive base incentive in proportion to the
traffic each of them served — a node's share is its traffic as a fraction of the traffic
served by those five, not by the whole field. An archive node outside the top five on a chain
receives no archive base incentive on that chain for that epoch.

**You are still paid for every request you serve.** Base incentive is separate from, and
additional to, what the network pays you for traffic. Falling outside the top five does not
reduce what your served traffic earns, and it does not affect your eligibility, your routing,
or any other chain you run a node on.

An archive node outside the five still shares in the rest of the chain's base incentive for
the recent-block traffic it serves, exactly as before.

The ranking is redone every epoch, so a node can enter or leave the top five from one epoch to
the next.

We are making this change because fewer high capacity miners provides more stability for the
product.

Each chain is counted on its own. Being outside the five on one chain says nothing about where
you stand on another, and on a chain with five or fewer eligible archive nodes every one of
them is inside the five.

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
│  ┌──────────┐      ┌──────────────────┐      │
│  │ capacity │─────▶│ node RPC         │      │
│  │ helper   │      │ (same machine)   │      │
│  └──────────┘      └──────────────────┘      │
│    ▲ /capacity-test (optional)               │
│    │                                         │
│  ┌──────────┐                                │
│  │ certbot  │ (optional)                     │
│  └──────────┘                                │
└──────────────────────────────────────────────┘
```

- **nginx gateway** — Terminates TLS, authenticates requests via bearer token, proxies WebSocket and HTTP RPC to the chain node. Supports dual secrets for zero-downtime rotation. The eth gateway template routes by the `Upgrade` header so a single 443 endpoint serves both JSON-RPC and `eth_subscribe`.
- **chain node** — yours: any accepted client for your chain, meeting the eligibility requirements for its declared type.
- **metrics** — Exposes `/metrics` internally; nginx publishes it at `https://<endpoint>/metrics` with the same bearer auth as RPC.
- **capacity helper** — Optional, archive nodes. Puts our capacity questions to your node from
  inside your own server and returns a short summary of each answer, so the time a large answer
  takes to cross the internet is not counted against you. nginx routes `/capacity-test` to it.
  See [Capacity helper](#capacity-helper-optional-archive-nodes).
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
   **If you run the [capacity helper](#capacity-helper-optional-archive-nodes), give it the new
   secret now too and restart it** — under systemd, that is the new value in
   `/etc/blockmachine/capacity-helper.env` and `sudo systemctl restart capacity-helper`. It
   reads its secret once when it starts, so until you do, it refuses our tests and your node is
   tested directly. Nothing is lost while that is true, but it does not fix itself.

## Configuration

Environment variables in `.env`:

| Variable | Default | Description |
|----------|---------|-------------|
| `SECRET_V1` | (required) | Primary bearer token |
| `SECRET_V2` | (empty) | Secondary token for zero-downtime rotation |
| `DOMAIN` | (empty) | Domain for Let's Encrypt auto-renewal |
| `CHAIN` | `tao` | Chain selection |
| `METRICS_PORT` | `9100` | Internal metrics exporter port |
| `CAPACITY_HELPER_HOST` | `host.docker.internal` | Where the gateway looks for the optional [capacity helper](#capacity-helper-optional-archive-nodes) |
| `CAPACITY_HELPER_PORT` | `9955` | Port the capacity helper listens on |
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
