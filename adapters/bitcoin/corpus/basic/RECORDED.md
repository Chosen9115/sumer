# `basic` — RECORDED from blockstream.info on 2026-09-07

**RECORDED.** Every response body under `run0/` is real Esplora output,
field-projected (see below); nothing in this corpus is invented.

**This is development scaffolding. It is never evidence of a live
connection**: a replayed corpus proves what the adapter does with bytes it
once received, and says nothing about whether any provider is reachable
now, or was reachable when a test ran.

| | |
|---|---|
| Source | `https://blockstream.info/api` (Esplora REST) |
| Recorded | 2026-09-07, between 10:35 and 14:43 UTC |
| Tip height at recording | 965942 |
| User-Agent | `sumer-bitcoin/0.1` |

## The wallet

One watch-only wallet, `vault`, over two addresses. Both are public
artefacts documented outside this repository, and neither is anyone's
personal wallet:

| Address | What it is | Confirmed txs |
|---|---|---|
| `17SkEw2md5avVNyYgj6RiXuQKNwkXaxFyQ` | the address that received the 10,000 BTC of the 2010 "Bitcoin pizza" payment (`a1075db5…`, block 57043) | 17 |
| `bc1qrp33g0q5c5txsp9arysrx4k6zdkfs4nce4xj0gdcccefvpysxf3qccfmv3` | the P2WSH example address from BIP-173's test vectors (its witness script is published in the BIP) | 27 |

Requests made, in the order the adapter makes them:

```
GET /address/17SkEw2md5avVNyYgj6RiXuQKNwkXaxFyQ
GET /address/17SkEw2md5avVNyYgj6RiXuQKNwkXaxFyQ/txs/chain
GET /address/17SkEw2md5avVNyYgj6RiXuQKNwkXaxFyQ/txs/mempool
GET /address/bc1qrp33g0…fmv3
GET /address/bc1qrp33g0…fmv3/txs/chain
GET /address/bc1qrp33g0…fmv3/txs/chain/30570b95cb45d15787a88bd58086bd78fc6bb2cc6cb16afbaf3bc0bd3733875a
GET /address/bc1qrp33g0…fmv3/txs/mempool
```

The second address needs two chain pages (25 then 2), which is what makes
this corpus a multi-page crawl: the adapter walks
`.../txs/chain/{last txid of the previous page}` until a page comes back
short.

## The projection

Address objects (`address_*.json` without a `_txs_` part) are the
response body **verbatim**.

Transaction listings are **field-projected**: each transaction object is
reduced to the fields `adapters/bitcoin/README.md` lists as the ones this
adapter reads —

```
txid, fee,
status.{confirmed, block_height, block_hash, block_time},
vin[].prevout.{scriptpubkey_address, value},
vout[].{scriptpubkey_address, value}
```

— each **kept verbatim**, with every other key dropped. No array element is
added, removed or reordered, and no value is altered. The reason is size:
verbatim, these two listings are 676 KB (scriptSigs and witnesses for
transactions with up to 201 inputs); projected they are 158 KB. The cost is
that this corpus no longer exercises "unknown provider fields are ignored";
that property is held by `map::Tx`'s serde derive and its unit tests.

Re-deriving it: fetch the seven URLs above and apply that projection. The
listings themselves grow at the tip, so a later fetch will not reproduce
these bytes — the checksums below identify what was committed, not what the
API will return tomorrow.

## The clock

`run0/now` is `1788792208` = 2026-09-07T14:43:28Z, the instant of the last
recording request. It pins `observed_at` so a replay is reproducible.

## SHA-256

```
852c846a0ca7601831cd77fe9c9922b6a005a20aed0edd1dc72935a764773422  run0/address_17SkEw2md5avVNyYgj6RiXuQKNwkXaxFyQ.json
8447d3cb37e8463dad5c3a34d29ecdcdf509fb1ef3ca07d739c07e669cfb6954  run0/address_17SkEw2md5avVNyYgj6RiXuQKNwkXaxFyQ_txs_chain.json
37517e5f3dc66819f61f5a7bb8ace1921282415f10551d2defa5c3eb0985b570  run0/address_17SkEw2md5avVNyYgj6RiXuQKNwkXaxFyQ_txs_mempool.json
5113212d7d4e29dfb4fb0f6279cddd85eb839fb6a2b1614265c5a21041e9e2cf  run0/address_bc1qrp33g0q5c5txsp9arysrx4k6zdkfs4nce4xj0gdcccefvpysxf3qccfmv3.json
752f78bac7dee68e5404852fa4bbb292cc70e2569ab71656ba21863db01029cc  run0/address_bc1qrp33g0q5c5txsp9arysrx4k6zdkfs4nce4xj0gdcccefvpysxf3qccfmv3_txs_chain.json
772836dd96c28d65f1128197f21ce9fc734e0e8c377a00f3cf045be97c97a3b3  run0/address_bc1qrp33g0q5c5txsp9arysrx4k6zdkfs4nce4xj0gdcccefvpysxf3qccfmv3_txs_chain_30570b95cb45d15787a88bd58086bd78fc6bb2cc6cb16afbaf3bc0bd3733875a.json
37517e5f3dc66819f61f5a7bb8ace1921282415f10551d2defa5c3eb0985b570  run0/address_bc1qrp33g0q5c5txsp9arysrx4k6zdkfs4nce4xj0gdcccefvpysxf3qccfmv3_txs_mempool.json
6768b4122616b874d44f6762794a486bd5eaf4cda8aa2671ab5e0a3e9d715a6c  run0/now
b502a93bd5df93f9c9d72b38730c2d82ca6c43652901648bde7a087c6f17c4ba  wallets.json
```

## Reconciliation

The 44 net deltas in `conformance/cases/bitcoin/btc_basic.json` sum to
**379983 sat**, which equals `funded_txo_sum − spent_txo_sum` summed over
the two `address_*.json` objects (`1000000379983 − 1000000000000` plus
`104118 − 104118`). Those two counters are the provider's own, computed on
its side and never touched by the mapping; the agreement is an arithmetic
check on the hand-written expectations that no adapter code takes part in.

**Fees reconcile too, and independently of the balance.** The twelve
transactions where this wallet spent declare a fee in
`conformance/cases/bitcoin/btc_basic.json`, and each figure is that
transaction's own `fee` field, read from the JSON above. Each one is also
checked a second way against the same recording: for every transaction in
this corpus, the sum of `vin[].prevout.value` minus the sum of
`vout[].value` equals `fee` exactly — the provider's own arithmetic, on a
projection that keeps every input and every output. None of these
transactions is a coinbase, so the identity applies to all 44 of them.

Note what the fee check does NOT establish: it says the figure is the right
figure for that transaction, not that this wallet is the party that paid it.
That second half is the mapping rule — `fees` only where an input of the
transaction spends an output of this wallet — and it is asserted by the 32
entries that declare `fees: null` beside the 12 that declare a figure.
