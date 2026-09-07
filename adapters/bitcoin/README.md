# `sumer-bitcoin-adapter`

A **watch-only** Bitcoin adapter. It reads addresses you give it from an
Esplora deployment and speaks `spec/wire.md` on stdin/stdout. It holds no
keys, signs nothing, and has no `execute()` to sign with.

Read `PRIVACY.md` before pointing this at a public Esplora instance. What
addresses you query, and from where, is the disclosure that matters here,
and it is not revocable.

## Backend

Esplora's REST API. One protocol, three deployments, one config line:

| Deployment | `--source` | Cost |
|---|---|---|
| Blockstream (default) | `https://blockstream.info/api` | free, rate-limited, run by Blockstream |
| mempool.space | `https://mempool.space/api` | free, rate-limited, run by mempool.space |
| your own `esplora`/`electrs` | `http://localhost:3000` | your hardware; nothing leaves your machine |

**Never a service that wants an xpub.** Handing a third party an extended
public key hands it every address you will ever derive, past and future, in
one request. This adapter has no code path that transmits one.

## Running it

```
sumer-bitcoin-adapter --wallets wallets.json [options]

  --wallets <path>     wallet definitions (required)
  --source <target>    https://host/api    an Esplora deployment
                       file:<dir>          a recorded corpus (see below)
                       default: https://blockstream.info/api
  --state-dir <dir>    where per-resource state lives
  --record <dir>       write every HTTP response into <dir> as a replayable
                       corpus (HTTP source only)
```

`--state-dir` is optional and its absence is not free: without it every
sync is a first run, so **no tombstone is ever emitted** and a failed read
answers `unavailable` instead of `stale`. The adapter says so on stderr at
startup.

### `wallets.json`

```json
{
  "wallets": [
    {
      "resource_id": "cold",
      "label": "Cold storage",
      "addresses": ["bc1q...", "1A1zP..."]
    }
  ]
}
```

- **A resource is one WALLET — a set of scriptPubKeys — never one address.**
  A balance and a history belong to the set. Per-address resources would
  report a self-transfer as a payment out plus a payment in.
- **`resource_id` is immutable. Renaming it creates a NEW resource.** The
  host keys resources by `(adapter_id, resource_id)` (`spec/wire.md` §10),
  so a rename forks the observation chain and starts a fresh one. This is
  host behaviour, not a surprise this adapter adds — and this adapter
  deliberately does **not** derive a "stable key" from the address set to
  hide it, because that would make adding an address rename the resource.
- `label` defaults to the `resource_id`.
- `resource_id` must be 1–64 bytes of `[A-Za-z0-9._-]`: it names a file
  under `--state-dir`. Addresses must be 10–100 ASCII alphanumeric bytes,
  which every base58check and bech32/bech32m address is. A duplicate
  address in one wallet is rejected — it would double-count the balance.
- Adding an address to an existing wallet is treated as a first run for
  tombstone purposes (the address-set hash changed) and logged to stderr.
  **It does not, and in this PR cannot, invalidate a cursor the host is
  holding** — see "Known limits".

## What it emits

### Identity

```
local_id_derivation = "btc-txid@1"
local_id            = "<resource_id>:<txid>"
```

One observation is one **wallet-relevant transaction** — not one per
output, not one per address. `amount` is the net delta: outputs to the
wallet minus inputs from the wallet.

The `resource_id` prefix is required, not decoration. The host keys
observation chains by `(adapter_id, local_id)` and **not** by resource
(`spec/observation.md` §3). Two of your own wallets paying each other is
routine, and they share a txid; a bare-txid `local_id` would merge their
chains, so one wallet's tombstone would delete the other wallet's
transaction.

- `fees` is set **only when the wallet spent** — a payment in was paid for
  by whoever sent it. The fee is already inside the net delta (inputs =
  outputs + fee); `fees` rides beside it as evidence.
- A net delta of exactly **zero is legal** (a self-transfer whose fee
  someone else's input paid). `spec/money.md` bans only the *signed* zero.
- `raw_sign` is `provider_positive` whenever the delta is `>= 0`.
  Deterministic, and unit-tested.

### Money

Asset `sat`, scale 0, integers only. **No BTC conversion anywhere** — a
conversion is a division by 10⁸, which is exactly the arithmetic
`spec/money.md` exists to keep away from money.

### Balances

Two lines per wallet, `confirmed` and `unconfirmed`, **never summed**:

| line | computed from |
|---|---|
| `confirmed` | Σ addresses of `chain_stats.funded_txo_sum − chain_stats.spent_txo_sum` |
| `unconfirmed` | Σ addresses of `mempool_stats.funded_txo_sum − mempool_stats.spent_txo_sum` |

Esplora exposes **no balance field at all**, so `spec/observation.md`'s
"`category` is the provider's verbatim name" has no answer to be faithful
to here: `"confirmed"` and `"unconfirmed"` are this adapter's names, and
this paragraph is the disclosure of that.

`unconfirmed` **may legitimately be negative** — a wallet spending
unconfirmed change has a mempool that records the spend of an output the
mempool never funded.

`amount: null` means **UNKNOWN and is never `"0"`**. A wallet whose
balance could not be read is not a wallet holding nothing. Both lines are
emitted for every configured resource on every `balances.read`, whatever
the outcome; on a failure they carry the last recorded values (`stale`) or
`null`.

### Cursor — compound, `exact`

```
"<height>:<txid>"                       confirmed high-water mark
"<height>:<txid>:m:<mempool_txid>"      ...plus a position in the by-txid section
"0:"                                    nothing emitted yet (the bottom)
```

The confirmed mark is **always present**, so a cursor persisted mid-mempool
still resumes confirmed reads. One sync emits two sections, in this order:

1. **Confirmed** — every confirmed transaction strictly above the cursor's
   `(height, txid)`, ascending, *excluding* anything in section 2.
2. **By txid** — the tracked mempool set (whatever the last completed sync
   recorded as unconfirmed), everything currently in the mempool, and every
   tombstone; ascending by txid, each at its **current** state.

**Section 2 is re-emitted in full every sync, regardless of height.** That
is what turns "a pending transaction got mined into a block at or below the
cursor" into a revision of the same `local_id` rather than a record that
stays pending forever.

> **The live-check invariant "resuming at C returns no confirmed
> transaction at or below C" exempts section 2, and section 2 only.**
> Re-emitting it is required behaviour; without the exemption the invariant
> would fail this conforming adapter. The checker must use exactly this
> definition, or it fails a conforming adapter — which is worse than not
> checking:
>
> **An observation is EXEMPT if, and only if, its cursor is a section-2
> cursor** — that is, if the cursor that resumes after it carries a `:m:`
> suffix. Equivalently, and observably from replies alone, an observation
> is exempt if any of these hold:
>
> 1. its `posting` is `pending` (currently unconfirmed), or
> 2. its `state` is `tombstoned`, or
> 3. a previous reply in the same connection reported the same `local_id`
>    with `posting: "pending"` (it was in the tracked mempool set, and has
>    since confirmed — this is the case the exemption exists for).
>
> **Everything else is section 1 and is NOT exempt**: a `posting: "posted"`
> observation whose `local_id` was never seen pending must be strictly
> above C, with no exception.

**Pages are cut by SERIALIZED BYTES, never by block.** Budget 512 KiB per
reply, half of `MAX_FRAME_BYTES`. Blocks split freely, and
`page_size_reduced_to` reports the count when a page cuts early. The
rejected alternative — "a page never splits a block" — was a
third-party-triggerable permanent denial of service: ~1300 observations
fill a 1 MiB frame, a block holds up to ~6000 transactions, anyone can mail
a published address hundreds of dust transactions that mine together, and
an oversized frame is a fatal kill with no resync. That one block would
poison every future sync of that wallet forever. Splitting is safe because
a transaction's position inside a block is fixed once mined — which is
exactly what `exact` resumption promises.

A drained page returns `next: null` (`exact`'s terminal state). A page
request of kind `window` is refused with an envelope `invalid_request`:
this adapter serves cursor pages only, and Bitcoin history is ordered by
block, not by wall-clock time.

### A crawl is one point-in-time snapshot

**The whole crawl is taken at the first page request and every later page
of it is served from that snapshot, in memory, for as long as the process
lives.** Nothing is written to disk, and `map.rs` stays pure.

- A `history.read` with **no `page`** starts a **new** crawl: the address
  listings are re-fetched, the tombstone probes are re-run, and the
  snapshot is replaced. That is what "from the start of available history"
  (Ruling A8) means here.
- A `history.read` with a **cursor** continues the crawl in hand. If this
  process has no memory of one — a fresh process resuming a cursor the host
  persisted — it takes a new crawl and serves the requested page from it.
- A crawl that **drains** (`next: null`) writes its baseline to
  `seen.json` and drops its snapshot. A crawl the host abandons mid-page
  writes nothing and is held until the process exits.
- Every page of one crawl reports the same `observed_at`: the instant the
  crawl was taken. Pages two and three did not observe anything.

This is not a cache and it is not persistence. The host spawns one adapter
process and multiplexes many requests over it, so a snapshot held between
two pages of one crawl is work already done inside one connection.

The correctness argument matters more than the speed one. **Pages of one
crawl must come from one snapshot.** Re-walking live data between pages
would let a transaction arriving mid-pagination shift every subsequent
page — observations duplicated, skipped or reordered across a page
boundary, with the cursor advancing over a dataset that changed underneath
it. That is wrong regardless of how fast the re-walk is.

### Outcomes — exactly five

| outcome | when |
|---|---|
| `fetched { page_empty }` | the read succeeded |
| `rate_limited { retry_after_ms }` | HTTP 429; `Retry-After` in seconds if present, else a 60s backoff. Every later resource in that batch gets `not_fetched`. |
| `unavailable` | connect error, DNS, 5xx, an unreadable body, any other status |
| `stale { as_of }` | a read failed **and** state holds a prior answer. A NORMAL outcome, not an error path. |
| `not_fetched` | a resource abandoned after an earlier rate limit, or a `resource_id` this adapter has no wallet for |

`reauth_required`, `revoked`, `sca_required` and `gone` are **never
emitted**, and `credential_expires_at` / `strong_auth_expires_at` are
**always absent**: a watch-only wallet has no credential, no
authentication session, and nothing that can be revoked. An unconfigured
`resource_id` is `not_fetched`, not `gone` — `gone` would claim the
resource used to exist, which this adapter cannot know.

`provider_detail` carries the HTTP status and body verbatim, as evidence
for a human (`spec/observation.md` §7). Nothing branches on it.

`status.read` never answers `stale`: a cached answer says nothing about
whether the provider is reachable *now*, which is the only question that op
asks.

### Tombstones — positive evidence only

State lives in `<state-dir>/<resource_id>.json`, one file per resource:

```json
{
  "schema": 1,
  "local_id_derivation": "btc-txid@1",
  "address_set_sha256": "…",
  "as_of": "2026-01-01T00:00:00Z",
  "balances": {"confirmed": "130000", "unconfirmed": "-5000"},
  "txs": {
    "<txid>": {"height": 800000, "delta": "150000"},
    "<txid>": {"delta": "-20000"}
  }
}
```

An entry with no `height` was in the mempool. `delta` is what the
transaction was worth to this wallet when last seen, so a tombstone carries
the figure it retracts instead of a fabricated zero.

- **A tombstone requires a direct `GET /tx/:txid` returning 404.** Absence
  from a listing is **never** evidence. The reason — `reorged_out` vs
  `dropped_from_mempool` — is decided by the recorded state, not guessed.
- **Any fetch failure anywhere in a sync suppresses the diff ENTIRELY**:
  zero observations, `stale { as_of }` or `unavailable`, and no `page`
  either (a read that did not happen claims no resume point). There are no
  partial diffs.
- A missing, unreadable, unparseable, version-mismatched,
  derivation-mismatched or hash-mismatched file is a **FIRST RUN**: zero
  remembered transactions, zero tombstones. Never a partial parse.
- Writes are temp-file + rename (atomic), and only once a sync has been
  delivered **in full** (`next: null`). Concurrent processes are
  last-writer-wins on purpose: combined with positive evidence, a lost
  update degrades to a *missed* tombstone caught next sync, never an
  invented one.
- A tombstone is not terminal. A re-mined transaction comes back `active`
  on the same `local_id` (`spec/observation.md` §4).

## The corpus layout

`--source file:<dir>` replays recorded Esplora JSON; `--record <dir>` on an
HTTP source writes it. **Both use the same function, so a recording is
replayable without editing.**

### Directory

```
<dir>/run<N>/…
```

`<N>` is `SUMER_FIXTURE_RUN` (`spec/wire.md` §11), default `0`. There is no
fallback to `<dir>` itself: run 0 lives in `run0/`, always. A two-phase
scenario is two adapter lifetimes over `run0/` and `run1/`, sharing one
`--state-dir`.

### `run<N>/now`

Unix seconds, decimal, optionally with trailing whitespace. Pins the clock
so `observed_at` and `as_of` are reproducible. **Every conformance corpus
must have one**; without it the adapter falls back to the wall clock, says
so on stderr, and that corpus's timestamps are not reproducible.

### Response files

The file name is the request path with the leading `/` dropped and every
remaining `/` replaced by `_`. Two forms:

- `<name>.json` — an HTTP 200 whose body is the file's contents verbatim.
- `<name>.status` — a non-200 response: the **first line** is the decimal
  HTTP status, and **everything after the first newline** is the body.

A request with neither file is a corpus bug: the adapter reports
`unavailable` (so it can never invent a tombstone out of a missing file)
and names the two paths it looked for on stderr.

| Request | Corpus file | Returns |
|---|---|---|
| `GET /address/{addr}` | `address_{addr}.json` | the address object with `chain_stats` and `mempool_stats` |
| `GET /address/{addr}/txs/chain` | `address_{addr}_txs_chain.json` | a JSON array of up to 25 confirmed transactions |
| `GET /address/{addr}/txs/chain/{last_seen_txid}` | `address_{addr}_txs_chain_{last_seen_txid}.json` | the next page, same shape |
| `GET /address/{addr}/txs/mempool` | `address_{addr}_txs_mempool.json` | a JSON array of unconfirmed transactions |
| `GET /tx/{txid}` | `tx_{txid}.json` / `tx_{txid}.status` | the transaction, or `404` — **the tombstone probe** |

Notes for corpus authors:

- **The chain listing is paged**: the adapter keeps asking for
  `.../txs/chain/{last txid of the previous page}` until a page comes back
  with **fewer than 25** entries (an empty array also ends it). A one-page
  history therefore needs only `address_{addr}_txs_chain.json` with fewer
  than 25 transactions in it.
- The mempool listing is not paged.
- The adapter probes `GET /tx/{txid}` **only** for txids recorded in
  `seen.json` that no longer appear in any listing. A first-run corpus
  needs no `tx_*` files at all.
- Only these five endpoints are ever requested.
- Fields the adapter reads: `txid`, `fee`, `status.{confirmed,
  block_height, block_hash, block_time}`, `vin[].prevout.{scriptpubkey_address,
  value}`, `vout[].{scriptpubkey_address, value}`, and
  `{chain,mempool}_stats.{funded_txo_sum, spent_txo_sum}`. Any other field
  is ignored, so a recording can be kept verbatim.

### A worked example

```
corpus/
  run0/
    now                                    "1767225600"
    address_bc1qexample.json               {"chain_stats":{…},"mempool_stats":{…}}
    address_bc1qexample_txs_chain.json     [ {…tx…} ]           (< 25 → last page)
    address_bc1qexample_txs_mempool.json   [ {…tx…} ]
  run1/
    now                                    "1767312000"
    address_bc1qexample.json
    address_bc1qexample_txs_chain.json     [ {…tx…} ]
    address_bc1qexample_txs_mempool.json   []
    tx_2222…2222.status                    "404\nTransaction not found"
```

Run 0 records the mempool transaction; run 1 drops it from the listings
**and** answers its probe with a 404, which is the only thing that makes it
a `dropped_from_mempool` tombstone. A run 1 that dropped it from the
listings without the `.status` file would produce **no** tombstone — that
is the rule working, not a corpus that failed.

## Known limits

- **The first page of a crawl pays for the whole history.** Esplora pages
  address history newest-first with no "from height H" entry point, so
  reaching old history means walking all of it: one round trip per 25
  transactions per address, serially, before the first page can be
  answered. Later pages are free (see "A crawl is one point-in-time
  snapshot"), but a very large wallet on a slow provider can still miss the
  host's 30s deadline on page one. Slow is the remaining cost; it is not a
  dead connection on every page. Closes when PR 4's persistence lets a
  crawl fetch only what arrived since the last one.
- **An abandoned crawl holds its snapshot until the process exits.** One
  per resource, dropped as soon as the crawl drains. A host that starts
  crawls and never finishes them grows the adapter's memory by one wallet
  history per resource, once.
- **Adding an address does not invalidate a host-held cursor.** The new
  address's history sits at heights at or below the cursor, which `exact`
  forbids re-emitting, and this adapter has no channel to tell the host.
  Latent in this PR only because cursors die with the crawl (`next: null`
  on a drained page, and nothing persists them). **PR 4's cursor
  persistence must invalidate the stored cursor when the address-set hash
  changes**, or "adding an address is a revision" is false for history. See
  ADR 0004.
- `window` page requests are not served (see above).
- The mempool listing is capped by the provider at 50 transactions per
  address and is not paged.

## Tests

```
cargo test -p sumer-bitcoin-adapter
```

Unit tests pin the mapping (`map.rs` is pure — no I/O, no clock, no
network), the cursor codec, the byte-cut pager, the state file's first-run
degradation, and the full appear → confirm → 404 → revive lifecycle. Three
end-to-end tests drive the adapter over a temporary corpus, including the
one that matters most: a failed fetch produces zero tombstones,
`stale { as_of }`, and the preserved balances.

The live invariant check (`SUMER_LIVE=1`, `#[ignore]`, nightly only, never
required CI) and the replay conformance cases live in `conformance/`.
