# Test RPC Methods

Methods used to verify a miner is serving traffic correctly.

## Basic health (no chain data needed)

```json
{"jsonrpc":"2.0","method":"system_health","params":[],"id":1}
```

Expected: `{"result":{"peers":N,"isSyncing":bool,"shouldHavePeers":true}}`

## Chain head (requires sync)

```json
{"jsonrpc":"2.0","method":"chain_getBlockHash","params":[0],"id":1}
```

Expected: `{"result":"0x2f0555..."}` (genesis hash, always the same)

## Runtime version

```json
{"jsonrpc":"2.0","method":"state_getRuntimeVersion","params":[],"id":1}
```

Expected: `{"result":{"specName":"node-subtensor","specVersion":N,...}}`

## Historical state (archive-only test)

Request state at block 1 — lite nodes return `null`, archive nodes return data:

```json
{"jsonrpc":"2.0","method":"chain_getBlockHash","params":[1],"id":1}
```

Then use the hash to query state:

```json
{"jsonrpc":"2.0","method":"state_getMetadata","params":["<block-1-hash>"],"id":1}
```

Lite nodes: `result` is `null` for pruned blocks.
Archive nodes: `result` is a hex-encoded metadata blob.
