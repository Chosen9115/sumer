# `fetch_fail` — RECORDED, plus SYNTHESIZED provider failures

**The address data is RECORDED** from blockstream.info on 2026-09-07. The
`503` responses are **synthesized**: an outage cannot be recorded on
demand, and every `*.status` file in this corpus is listed below.

**This is development scaffolding. It is never evidence of a live
connection.**

| | |
|---|---|
| Source | `https://blockstream.info/api` (Esplora REST) |
| Recorded | 2026-09-07, between 10:35 and 14:43 UTC |
| Tip height at recording | 965942 |
| User-Agent | `sumer-bitcoin/0.1` |

## The wallets

| Resource | Addresses | Recorded? |
|---|---|---|
| `cold` | `15mKKb2eos1hWa6tisdPwwDC1a5J1y9nma` (BIP-32 test vector 1, `m/0'`; 2 txs) and `17SkEw2md5avVNyYgj6RiXuQKNwkXaxFyQ` (the 2010 pizza recipient; 17 txs) | both, in `run0` |
| `fresh` | `bc1qnjg0jd8228aq7egyzacy8cys3knf9xvrerkf9g` (BIP-84 test vector, `m/84'/0'/0'/0/1`) | no — this address answers 503 in BOTH runs, so the adapter never has an answer to remember |

Recorded requests: `/address/{a}`, `/address/{a}/txs/chain` and
`/address/{a}/txs/mempool` for the two `cold` addresses. Listings are
field-projected exactly as `../basic/RECORDED.md` describes; address
objects are verbatim.

## The synthesized failures

Every `.status` file has the same two-line body: `503` and
`Esplora is unavailable`.

- `run0/`: the three `bc1qnjg0jd…` endpoints. `fresh` is unreadable from
  the very first sync, so it never records a prior answer.
- `run1/`: the three `17SkEw2md5…` endpoints, plus the same three
  `bc1qnjg0jd…` ones. In the second lifetime one of `cold`'s two addresses
  has gone dark; the other still answers, byte-identically to `run0`.

That is the point of the corpus: seventeen of `cold`'s transactions are now
absent from everything the adapter can read, and there is no `tx_*.status`
file anywhere in it. A tombstone needs a direct `GET /tx/:txid` answering
404; absence behind a failed fetch is not evidence, and this corpus offers
the adapter every opportunity to decide otherwise.

`run1/now` is `run0/now` + 86400 (2026-09-08T14:43:28Z).

## SHA-256

```
6d79e3f37e2f3e3fc367cb92044b4e6d3a85bd8d01cc307ddf80e2f2c4f726aa  run0/address_15mKKb2eos1hWa6tisdPwwDC1a5J1y9nma.json
51efae4b09cf4a60268f1e61964f7fdc23d67efc514f4c2d6cfc9ef9c97cdd2c  run0/address_15mKKb2eos1hWa6tisdPwwDC1a5J1y9nma_txs_chain.json
37517e5f3dc66819f61f5a7bb8ace1921282415f10551d2defa5c3eb0985b570  run0/address_15mKKb2eos1hWa6tisdPwwDC1a5J1y9nma_txs_mempool.json
852c846a0ca7601831cd77fe9c9922b6a005a20aed0edd1dc72935a764773422  run0/address_17SkEw2md5avVNyYgj6RiXuQKNwkXaxFyQ.json
8447d3cb37e8463dad5c3a34d29ecdcdf509fb1ef3ca07d739c07e669cfb6954  run0/address_17SkEw2md5avVNyYgj6RiXuQKNwkXaxFyQ_txs_chain.json
37517e5f3dc66819f61f5a7bb8ace1921282415f10551d2defa5c3eb0985b570  run0/address_17SkEw2md5avVNyYgj6RiXuQKNwkXaxFyQ_txs_mempool.json
6fb8d32a6065ac613580cdf9db93802d0d6461fb113a3e4dad8d224dfb332f3b  run0/address_bc1qnjg0jd8228aq7egyzacy8cys3knf9xvrerkf9g.status
6fb8d32a6065ac613580cdf9db93802d0d6461fb113a3e4dad8d224dfb332f3b  run0/address_bc1qnjg0jd8228aq7egyzacy8cys3knf9xvrerkf9g_txs_chain.status
6fb8d32a6065ac613580cdf9db93802d0d6461fb113a3e4dad8d224dfb332f3b  run0/address_bc1qnjg0jd8228aq7egyzacy8cys3knf9xvrerkf9g_txs_mempool.status
6768b4122616b874d44f6762794a486bd5eaf4cda8aa2671ab5e0a3e9d715a6c  run0/now
6d79e3f37e2f3e3fc367cb92044b4e6d3a85bd8d01cc307ddf80e2f2c4f726aa  run1/address_15mKKb2eos1hWa6tisdPwwDC1a5J1y9nma.json
51efae4b09cf4a60268f1e61964f7fdc23d67efc514f4c2d6cfc9ef9c97cdd2c  run1/address_15mKKb2eos1hWa6tisdPwwDC1a5J1y9nma_txs_chain.json
37517e5f3dc66819f61f5a7bb8ace1921282415f10551d2defa5c3eb0985b570  run1/address_15mKKb2eos1hWa6tisdPwwDC1a5J1y9nma_txs_mempool.json
6fb8d32a6065ac613580cdf9db93802d0d6461fb113a3e4dad8d224dfb332f3b  run1/address_17SkEw2md5avVNyYgj6RiXuQKNwkXaxFyQ.status
6fb8d32a6065ac613580cdf9db93802d0d6461fb113a3e4dad8d224dfb332f3b  run1/address_17SkEw2md5avVNyYgj6RiXuQKNwkXaxFyQ_txs_chain.status
6fb8d32a6065ac613580cdf9db93802d0d6461fb113a3e4dad8d224dfb332f3b  run1/address_17SkEw2md5avVNyYgj6RiXuQKNwkXaxFyQ_txs_mempool.status
6fb8d32a6065ac613580cdf9db93802d0d6461fb113a3e4dad8d224dfb332f3b  run1/address_bc1qnjg0jd8228aq7egyzacy8cys3knf9xvrerkf9g.status
6fb8d32a6065ac613580cdf9db93802d0d6461fb113a3e4dad8d224dfb332f3b  run1/address_bc1qnjg0jd8228aq7egyzacy8cys3knf9xvrerkf9g_txs_chain.status
6fb8d32a6065ac613580cdf9db93802d0d6461fb113a3e4dad8d224dfb332f3b  run1/address_bc1qnjg0jd8228aq7egyzacy8cys3knf9xvrerkf9g_txs_mempool.status
cbd6f77f7aab98f09e6dda88a43e0ac64e31297d08e7270345ae579ca08270dc  run1/now
ad9ac141eb76c53a81ec67f18bfaacf7be56fc7e55d303068708f115e5c8f55b  wallets.json
```

## Reconciliation

`cold`'s recorded confirmed balance is **379983 sat**:
`60000 − 60000` for the `m/0'` address plus
`1000000379983 − 1000000000000` for the pizza one, both read off the
provider's own counters in `run0`. That figure is what the second lifetime
must report as `stale`: the balance is the one `run0` recorded, not one
`run1` could have computed — `run1` cannot read the address that holds all
of it.

Every transaction recorded here satisfies the provider's own fee
arithmetic (inputs minus outputs equals `fee`). This case declares no
history at all, so it asserts no fee: what it asserts is that a failed
read produces nothing.
