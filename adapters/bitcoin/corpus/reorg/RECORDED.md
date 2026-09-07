# `reorg` — RECORDED, plus ONE SYNTHESIZED counterfactual

**`run0/` is RECORDED** from blockstream.info on 2026-09-07.
**`run1/` is that same recording with a reorg synthesized into it**, and
the three edits are listed in full below. A chain does not reorg on
request, and waiting for one to hit a chosen address is not a test
strategy; what is real here is the wallet, and what is invented is the
disappearance.

**This is development scaffolding. It is never evidence of a live
connection.**

| | |
|---|---|
| Source | `https://blockstream.info/api` (Esplora REST) |
| Recorded | 2026-09-07, 14:4x UTC |
| Tip height at recording | 965942 |
| User-Agent | `sumer-bitcoin/0.1` |

## The wallet

One watch-only wallet, `watch`, over one address:

`15mKKb2eos1hWa6tisdPwwDC1a5J1y9nma` — the BIP-32 test vector 1 address for
chain `m/0'`, whose private key is published in the BIP. Two confirmed
transactions and nothing else, which is the whole reason it was chosen:

| txid | height | what the wallet did |
|---|---|---|
| `884a2eb058b4360cbc5a4fa8131f3e0ec34aff5c635e34feb3c457d12d0bc93d` | 246485 | received 60000 sat |
| `6d483d103cdc3b0735b5490a22c143c837926ffdde03d874a5a3168abf7d29ca` | 246488 | spent that output, fee 50000 sat |

Requests recorded:

```
GET /address/15mKKb2eos1hWa6tisdPwwDC1a5J1y9nma
GET /address/15mKKb2eos1hWa6tisdPwwDC1a5J1y9nma/txs/chain
GET /address/15mKKb2eos1hWa6tisdPwwDC1a5J1y9nma/txs/mempool
```

Transaction listings are field-projected exactly as
`../basic/RECORDED.md` describes; the address object is verbatim.

## What `run1/` changes, and nothing else

`run1/` is a copy of `run0/` in which the spend `6d483d10…` was reorged out
of the best chain:

1. `address_…_txs_chain.json`: the `6d483d10…` element is removed. The
   surviving element is byte-identical to its `run0` form.
2. `address_….json`: `chain_stats.spent_txo_count` 1 → 0,
   `spent_txo_sum` 60000 → 0, `tx_count` 2 → 1. A block that no longer
   exists does not leave its spend in the provider's counters, so the
   confirmed balance goes back to 60000.
3. `tx_6d483d10….status` is added: `404`, the direct-probe answer, which
   is the ONLY thing that authorises a tombstone. Edit 1 alone — absence
   from the listing — must produce no tombstone at all.

`run1/now` is `run0/now` + 86400 (2026-09-08T14:43:28Z): the second
lifetime is a day later.

Reorging a transaction 700,000 blocks deep is not something Bitcoin does.
What is being tested is the adapter's rule, not the chain's behaviour, and
the rule reads the same at any depth: `GET /tx/:txid` answered 404, the
state file says the transaction was in a block, so the reason is
`reorged_out`.

## SHA-256

```
6d79e3f37e2f3e3fc367cb92044b4e6d3a85bd8d01cc307ddf80e2f2c4f726aa  run0/address_15mKKb2eos1hWa6tisdPwwDC1a5J1y9nma.json
51efae4b09cf4a60268f1e61964f7fdc23d67efc514f4c2d6cfc9ef9c97cdd2c  run0/address_15mKKb2eos1hWa6tisdPwwDC1a5J1y9nma_txs_chain.json
37517e5f3dc66819f61f5a7bb8ace1921282415f10551d2defa5c3eb0985b570  run0/address_15mKKb2eos1hWa6tisdPwwDC1a5J1y9nma_txs_mempool.json
6768b4122616b874d44f6762794a486bd5eaf4cda8aa2671ab5e0a3e9d715a6c  run0/now
c5e39d55ca95d3c7079a009e16e1f2e7eee9f8fc330000c4582a658ba2e46a84  run1/address_15mKKb2eos1hWa6tisdPwwDC1a5J1y9nma.json
be4af0981998085c724cf7a082fbe1c1126a7e82c5e04418ad7a8d5905408b4c  run1/address_15mKKb2eos1hWa6tisdPwwDC1a5J1y9nma_txs_chain.json
37517e5f3dc66819f61f5a7bb8ace1921282415f10551d2defa5c3eb0985b570  run1/address_15mKKb2eos1hWa6tisdPwwDC1a5J1y9nma_txs_mempool.json
cbd6f77f7aab98f09e6dda88a43e0ac64e31297d08e7270345ae579ca08270dc  run1/now
e5d68f75af3a24076d125716c459c45f7abd77b8c1268eb9b9c03d9ec22962d3  run1/tx_6d483d103cdc3b0735b5490a22c143c837926ffdde03d874a5a3168abf7d29ca.status
c2a20425c9065b46d56a60b3150d5a0c13929226bb5bd57529582ae76ab7e371  wallets.json
```

## Reconciliation

The two hand-derived deltas (+60000 and −60000) sum to 0, which is
`funded_txo_sum − spent_txo_sum` in `run0`'s address object
(`60000 − 60000`). After the synthesized reorg, `run1`'s counters give
60000, which is what the case declares as the confirmed balance of the
second lifetime.

Both transactions also satisfy the provider's own fee arithmetic --
inputs minus outputs equals `fee` -- and the spend's 50000 sat fee is
not asserted as a fee anywhere, because the case tombstones that
transaction and a tombstone carries no `fees` at all.
