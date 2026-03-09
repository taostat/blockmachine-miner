---
name: miner-onboard
description: Walk through onboarding a Blockmachine miner end-to-end. Use when the user wants to register a new miner node, verify an existing miner is reachable, test that a miner is serving traffic, or troubleshoot miner connectivity issues. Triggers on requests like "onboard a miner", "register a new node", "test miner connectivity", "verify miner is working", or "check if my miner is serving traffic".
---

# Miner Onboard

Guided workflow for registering a Blockmachine miner and verifying it serves traffic end-to-end. All steps use the `bm` CLI tool.

## Workflow

### 1. Gather miner details

Collect from the user:
- **Endpoint**: WSS URL (e.g. `wss://203.0.113.50` or `wss://miner.example.com`)
- **Alias**: Friendly name (e.g. `eu-lite-1`)
- **Secret**: Bearer token from the server's `.env` (`SECRET_V1`)
- **Price**: USD per compute unit (e.g. `0.01`)
- **Network**: Testnet or mainnet

If the user provides all values, skip prompting. If some are missing, ask.

**Testnet prerequisite:** If the user is mining on testnet, they need a Bittensor hotkey registered as a miner on **netuid 417** (the Blockmachine testnet subnet). Verify this is done before proceeding:

```bash
btcli subnet register --netuid 417 --subtensor.network test
```

All testnet CLI commands require the `--testnet` flag (e.g. `bm --testnet miner add ...`).

### 2. Verify the miner server is reachable

Test connectivity before registering using the endpoint and secret directly:

```bash
bm miner test --endpoint <endpoint> --secret '<secret>'
```

This checks:
- TLS handshake succeeds (port 443)
- Health endpoint responds (HTTP GET to `/health` on port 80)
- Authenticated RPC call (`system_health`) returns a valid response

If any check fails, diagnose and help the user fix it before proceeding. Common issues:
- **TLS handshake fails**: Firewall blocking 443, or certs not in place
- **Health check fails**: Gateway container not running (`docker compose logs gateway`)
- **Auth fails**: Secret mismatch between `.env` on server and what user provided
- **RPC fails**: Subtensor node not synced yet (`docker compose logs node`)

### 3. Register the node

```bash
bm miner add --endpoint <endpoint> --alias <alias> --secret '<secret>' --price <price>
```

Verify registration succeeded:

```bash
bm miner show <alias>
```

Status should be `pending` (gateway hasn't connected yet) or `active`.

### 4. Test registered node

After registration, run the full test using the alias (secret is fetched automatically from the registry):

```bash
bm miner test <alias>
```

This tests TLS, health, and authenticated RPC.

### 5. Summary

Print a summary:
- Miner endpoint and alias
- Registration status
- TLS mode (pinned fingerprint or CA-verified)
- Test results (success/fail for each check, latency)
- Any warnings or follow-up actions

## References

- See [references/rpc_methods.md](references/rpc_methods.md) for the test RPC methods and expected responses
