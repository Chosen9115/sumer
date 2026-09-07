# `sumer-bitcoin-adapter`

A **watch-only** Bitcoin adapter. It reads addresses you give it from an
Esplora deployment and speaks `spec/wire.md` on stdin/stdout. It holds no
keys, signs nothing, and has no `execute()` to sign with.

Read `PRIVACY.md` before pointing this at a public Esplora instance. What
addresses you query, and from where, is the disclosure that matters here,
and it is not revocable.

## What this adapter does NOT do: it never reports a disappearance

**This adapter emits no tombstones.** It reads balances and history and
reports what the provider says now. It does not remember what it saw last
time in order to announce that something has gone.

What that means for you, concretely: **a transaction dropped from the
mempool, or reorged out of the chain, stays in the host's live set.** The
host was told the transaction was there, nothing ever tells it otherwise,
and no later sync of this adapter corrects it. Everything else is
self-correcting — a re-mined transaction, a changed height, a new
transaction all arrive on the next crawl — but a *disappearance* does not,
because absence is not expressible on this wire without a tombstone and a
tombstone is exactly what this adapter no longer sends.

**Why it was removed rather than fixed.** Retracting needs the adapter to
remember what it reported, and a lost retraction is lost permanently:
nothing probes a transaction no baseline holds. That one unrecoverable
write forced every state write to be gated on the reply actually reaching
stdout, and four adversarial rounds each found a different defect in that
one boundary — a budget scoped wrong, a commit ordered wrong, a commit
predicate wrong, and a cursor-resumed page filtering an omitted tombstone
out of the accumulator. The fifth design was not attempted. Without
retraction there is no unrecoverable write, so there is no delivery gate
and no boundary to get wrong: every failure mode collapses to "re-derive on
the next sync".

**It comes back in PR 4**, designed once against the host's own persistence
rather than against a file this adapter races itself on. See
`adr/0004-bitcoin-adapter.md` decision 7.

## Backend

Esplora's REST API. One protocol, several deployments, one config line:

| Deployment | `--source` | Cost |
|---|---|---|
| Blockstream (default) | `https://blockstream.info/api` | free, rate-limited, run by Blockstream |
| mempool.space | `https://mempool.space/api` | free, rate-limited, run by mempool.space |
| your own `esplora`/`electrs`, on this machine | `http://localhost:3000` | your hardware; nothing leaves the machine |
| your own `esplora`/`electrs`, elsewhere | `http://nas.lan:3000` | your hardware, plus the address set crossing a network in the clear on every sync — see `PRIVACY.md` |

**"Your own instance" is not automatically "your own machine."** Only the
`localhost` row keeps the address set off a network; the adapter adds no TLS
and does not warn when `--source` is a remote `http://`. `PRIVACY.md` states
what each deployment discloses, and to whom.

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

`--state-dir` holds one thing: a **cached balance per resource**, so that a
failed `balances.read` can answer `stale { as_of }` instead of
`unavailable`. Without it, that answer is `unavailable`; the adapter says so
on stderr at startup. A `--state-dir` this process cannot write to is the
same cost, announced once per failed write — the adapter does not refuse to
start over it, because nothing it stores there is unrecoverable.

**`--source` is capped at 256 bytes and over-long values are rejected, not
truncated.** It becomes `provenance.provider_id` on every observation this
adapter emits, and a truncated provider identity is a falsified one.

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
- Adding an address to an existing wallet invalidates that wallet's cached
  balances (the address-set hash changed) and is logged to stderr: the
  figures on disk are a different address set's. **It does not, and in this
  PR cannot, invalidate a cursor the host is holding** — see "Known
  limits".

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
chains, so one wallet's records would revise the other wallet's.

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
   `(height, txid)`, ascending.
2. **By txid** — everything the provider currently reports as unconfirmed,
   ascending by txid.

**Section 2 is re-emitted in full every page request, regardless of
height.** That is what turns "a pending transaction got mined into a block
at or below the cursor" into a revision of the same `local_id` rather than
a record that stays pending — for as long as the crawl that saw it pending
is the crawl that sees it confirmed. Section 1 would suppress it as at or
below the cursor; section 2 is the only place the revision can arrive from.

**It reads nothing about previous syncs.** Section 2 used to also carry the
tracked mempool set and every height revision remembered from the last
completed crawl, which made the revision survive a cursor the host had
persisted across processes. That memory lived in the same file the
retraction did, and went with it. A `history.read` with no `page` re-emits
everything at its current state and repairs any record left behind — see
"Known limits".

> **The live-check invariant "resuming at C returns no confirmed
> transaction at or below C" exempts section 2, and section 2 only.**
> Re-emitting it is required behaviour; without the exemption the invariant
> would fail this conforming adapter. The checker must use exactly this
> definition, or it fails a conforming adapter — which is worse than not
> checking:
>
> The checker keeps all four clauses below, because they are the contract
> for *any* conforming adapter and narrowing them would fail one. This
> adapter now only ever produces clause 1: it emits no tombstones
> (clause 2) and remembers nothing across crawls (clauses 3 and 4).
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
>    since confirmed — this is the case the exemption exists for), or with
>    a different `block_height` (a reorg re-mined it, possibly *downwards*,
>    which the confirmed section would otherwise suppress forever).
>
> **Everything else is section 1 and is NOT exempt**: a `posting: "posted"`
> observation whose `local_id` was never seen pending must be strictly
> above C, with no exception.

**Pages are cut by SERIALIZED BYTES, never by block.** A page spends at
most 512 KiB, and never more than the reply has left. Blocks split freely,
and `page_size_reduced_to` reports the count when a page cuts early. The
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

### `MAX_FRAME_BYTES` bounds the whole reply, not one resource

A reply carries **every** requested resource's observations, plus one
status entry per resource with its `provider_detail` evidence, in one
frame. So the byte budget is spent once across the whole reply, not once
per resource: two wallets with ordinary transactions used to produce a
1,049,218-byte frame against a 1,048,576-byte cap, and an oversized frame
is a fatal kill with no resync — the same denial of service the byte-cut
exists to prevent, arriving from the other side. What rides along and is
therefore counted:

- **Every observation**, plus the comma that joins it to the last one.
- **Every status entry**, reserved for *every* requested `resource_id`
  before *any* observation is admitted — not charged one at a time as
  each resource's turn comes. A status entry is mandatory and an
  observation is not, so what the statuses cost is knowable before paging
  begins; charging them in turn let the resources at the front of a batch
  spend bytes the ones behind them were always going to need, and one
  wallet followed by 6,100 unknown resources put 1,061,875 bytes on the
  wire.
- **`provider_detail.raw.body`**, capped at 4 KiB with a
  `[truncated: N bytes]` marker. It is evidence and it goes verbatim
  (`spec/observation.md` §7) — but *verbatim* is not *unbounded*, and one
  provider answering 503 with a megabyte of HTML would otherwise make the
  reply unwritable.

A page that cannot afford even one observation emits none and returns the
cursor it started from, so the host asks again with the whole budget.
`write_reply` enforces the ceiling regardless: a reply that would still
exceed `MAX_FRAME_BYTES` is answered with an `err` on the same `id` rather
than written. Reaching that means the request asked for more resources than
a frame can carry *in statuses alone*, which no paging mechanism can shed —
every requested `resource_id` appears in `statuses` exactly once. Ask for
fewer resources per request; the error says so.

### Oversized observations: `spec/observation.md` §6's two-step degrade

`block_hash` is a provider-supplied scalar with no length this adapter gets
to assume. When an observation exceeds `MAX_OBSERVATION_BYTES` (64 KiB):

1. its `provider_extra` — the one field with no bounded shape — is replaced
   by exactly `{"_truncated": true, "_original_bytes": N}` and its
   provenance `completeness` becomes `partial`. Nothing else about the
   record changes;
2. if it is *still* too large, it is omitted entirely, its resource's status
   entry gains `degraded {local_id, bytes}` **beside** its `outcome` (never
   instead of it), and the page keeps emitting everything else. One
   pathological record must never brick a resource.

### A crawl is one point-in-time snapshot

**The whole crawl is taken at the first page request and every later page
of it is served from that snapshot, in memory, for as long as the process
lives.** Nothing is written to disk, and `map.rs` stays pure.

- A `history.read` with **no `page`** starts a **new** crawl: the address
  listings are re-fetched and the snapshot is replaced. That is what "from the start of available history"
  (Ruling A8) means here.
- A `history.read` with a **cursor** continues the crawl in hand. If this
  process has no memory of one — a fresh process resuming a cursor the host
  persisted — it takes a new crawl and serves the requested page from it.
- A crawl that **drains** (`next: null`) drops its snapshot. It writes
  nothing: a `history.read` touches no state at all. A crawl the host
  abandons mid-page is held until the process exits.
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

### A `history.read` writes no state, so there is nothing to gate

The state writes a reply could earn used to be a field of that reply,
applied if and only if that reply was the frame that went to stdout. The
whole machinery existed for one write — the retraction baseline — because
a retraction the host never received is one no later sync re-derives.

There is no such write any more. A `history.read` records nothing at all,
and the one thing a `balances.read` records is a cached figure, written as
it is read and not gated on anything: if the reply is refused for size, the
cache holds a figure that was genuinely observed and the next successful
read overwrites it. Losing it, or writing it for a reply nobody received,
costs at most one `unavailable` where a `stale` was possible.

**A repeated `resource_id` is refused** with an envelope `invalid_request`,
in all three batch ops. Every requested `resource_id` appears in `statuses`
exactly once, so a repeat has no conforming answer. The offending id is
deliberately **not** in `err.detail` (`spec/wire.md` §8).

### Outcomes — exactly five

| outcome | when |
|---|---|
| `fetched { page_empty }` | the read succeeded |
| `rate_limited { retry_after_ms }` | HTTP 429; `Retry-After` in seconds if present, else a 60s backoff. Every later resource in that batch gets `not_fetched`. |
| `unavailable` | connect error, DNS, 5xx, an unreadable body, any other status |
| `stale { as_of }` | a **balance** read failed and the cache holds a prior answer. A NORMAL outcome, not an error path. |
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

**`history.read` never answers `stale` either**, for a different reason:
nothing about a history is cached, so a failed history read has no prior
answer to be stale about. It is `unavailable`, with no `page` — a read that
did not happen claims no resume point.

### State — a balance cache, and nothing else

State lives in `<state-dir>/<resource_id>.json`, one file per resource:

```json
{
  "schema": 3,
  "address_set_sha256": "…",
  "balances": {
    "as_of": "2026-01-01T00:00:00Z",
    "confirmed": "130000",
    "unconfirmed": "-5000"
  }
}
```

That is the whole file. It exists so a failed `balances.read` can answer
`stale { as_of }` carrying the figures the last successful one observed,
rather than `unavailable`. Nothing in it is unrecoverable: lose it, fail to
write it, or write it for a reply that never went out, and the cost is one
`unavailable` until the next successful balance read.

- A missing, unreadable, unparseable, version-mismatched or hash-mismatched
  file is a **FIRST RUN**: `unavailable` on a failed read. Never a partial
  parse — a figure this adapter cannot stand behind, under a date it did
  not observe, is worse than no figure.
- The `address_set_sha256` is the wallet's address set. A different one
  means those figures are a different wallet's balances.
- Writes are temp-file + rename (atomic). There is **no lock**: two
  processes racing this file can lose one section's update, and the loser is
  re-derived by the next successful read.
- **Schema 2 files are a first run.** A schema-2 file also carried a
  transaction baseline — every txid the last completed crawl saw, with its
  height and its net delta — which is what made a tombstone possible. That
  section is gone; see the top of this file.

> **What is not stored here any more, and what it cost.** The transaction
> baseline was the one unrecoverable write in this adapter: a retraction
> the host never received is never re-derived, because nothing probes a
> txid no baseline holds. Keeping it correct required a delivery-gated
> commit, an advisory `flock`, a merge that never overwrote wholesale, and
> a startup probe that refused to run without a usable state directory.
> All four are deleted along with the write they protected.

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
`unavailable` (never a partial history out of a missing file) and names the
two paths it looked for on stderr.

| Request | Corpus file | Returns |
|---|---|---|
| `GET /address/{addr}` | `address_{addr}.json` | the address object with `chain_stats` and `mempool_stats` |
| `GET /address/{addr}/txs/chain` | `address_{addr}_txs_chain.json` | a JSON array of up to 25 confirmed transactions |
| `GET /address/{addr}/txs/chain/{last_seen_txid}` | `address_{addr}_txs_chain_{last_seen_txid}.json` | the next page, same shape |
| `GET /address/{addr}/txs/mempool` | `address_{addr}_txs_mempool.json` | a JSON array of unconfirmed transactions |

Notes for corpus authors:

- **The chain listing is paged**: the adapter keeps asking for
  `.../txs/chain/{last txid of the previous page}` until a page comes back
  with **fewer than 25** entries (an empty array also ends it). A one-page
  history therefore needs only `address_{addr}_txs_chain.json` with fewer
  than 25 transactions in it.
- The mempool listing is not paged.
- **Only these four endpoints are ever requested.** `GET /tx/{txid}` was a
  fifth — the tombstone probe — and a corpus never needs a `tx_*` file.
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
    address_bc1qexample.status             "503\nEsplora is unavailable"
```

Run 0 answers everything; run 1's provider has gone dark, which is the
two-lifetime scenario `btc_fetch_fail` drives — run 0 records a balance,
run 1 reports it as `stale { as_of }` and emits no history at all.

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
- **No disappearance is ever reported.** The headline limit, stated in full
  at the top of this file. A transaction dropped from the mempool or reorged
  out stays in the host's live set until PR 4 lands the capability against
  the host's own persistence.
- **A revision can be missed by a host that persists a cursor across
  processes.** A transaction this adapter reported `pending` and a *later
  crawl* sees confirmed at a height at or below that cursor is not
  re-emitted: section 2 carries only what the provider says is unconfirmed
  now, and section 1 suppresses anything at or below the cursor. A
  `history.read` with no `page` re-emits it at its current state and repairs
  the record. This is the same family as the limit below, and closes the
  same way.
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
network), the cursor codec, the byte-cut pager and the state file's
first-run degradation. End-to-end tests drive the adapter over a temporary
corpus.

**The checks with teeth are the liveness ones** — an adapter that answered
nothing would pass every safety rule in this crate, and that failure species
is the one this project keeps finding:

- `later_pages_of_a_crawl_are_served_from_the_snapshot` drains a
  1,200-transaction crawl across page boundaries with the corpus deleted
  after page one and asserts the **exact id sequence**, in order, with no
  duplicate and no gap.
- `two_resources_share_one_frames_budget` asserts every transaction of
  **both** wallets arrives exactly once out of one shared byte budget: a
  budget that starves a resource fails here.
- `a_failed_fetch_suppresses_the_diff_entirely` asserts positive content
  before it asserts the failure — the id, the amount, the drained page —
  and then that the failed lifetime emits nothing and reports the recorded
  balance as `stale`.
- `conformance/cases/bitcoin/btc_basic.json` compares 44 hand-derived
  observations against the adapter's replies as a **sequence**, and its
  amounts reconcile arithmetically against the provider's own counters
  (`adapters/bitcoin/corpus/basic/RECORDED.md`).

`tests/commit_boundary.rs` is gone with the boundary it existed for. It
proved six rules over request sequences; four of them (nothing commits for
an undelivered reply, nothing leaves a baseline unretracted, a 404 baseline
txid must be retracted, a drained crawl commits a history write) are claims
this adapter no longer makes at all, and the two that remain — every frame
fits `MAX_FRAME_BYTES`, every requested id appears in `statuses` exactly
once — are held by the frame-limit tests above and by conformance A7.

The live invariant check (`SUMER_LIVE=1`, `#[ignore]`, nightly only, never
required CI) and the replay conformance cases live in `conformance/`.
