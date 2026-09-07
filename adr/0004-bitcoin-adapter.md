# ADR 0004 — The watch-only Bitcoin adapter: Esplora, wallet-shaped resources, byte-cut pages, positive-evidence tombstones

- **Status:** accepted
- **Date:** 2026-09-07
- **Decision by:** Linus, Milestone 1

## Context

ADR 0001 (Revision 2) says reference adapters are written in Rust and speak
the wire as processes; ADR 0003 froze that wire. Milestone 1 is the first
adapter reading a *real* provider, and the choice of provider and of what a
"resource" means are not implementation details — they decide what the user
discloses, what a `local_id` means forever, and what the host is allowed to
believe when a record stops appearing.

Bitcoin is also the first place where `spec/observation.md`'s harder
promises have to be met against a system nobody controls: history is
append-only until it is reorganised, a transaction can be replaced or
dropped from a mempool without anyone being told, an address listing is
paginated newest-first by the provider, and a wallet is not an account —
it is a set of scriptPubKeys with no server-side identity at all.

Five decisions had to be made together.

## Decision

### 1. Esplora REST, and the privacy that costs

The adapter speaks the **Esplora REST API**, defaulting to
`https://blockstream.info/api`. One protocol, three deployments, one config
line: Blockstream, mempool.space's `/api`, or your own `esplora`/`electrs`
instance. Five endpoints are ever requested, and the `User-Agent` is
`sumer-bitcoin/0.1` — a browser forgery would be dishonest and would not
change the disclosure that matters.

The cost is stated in `adapters/bitcoin/PRIVACY.md`, and it is not small:
**querying a wallet's addresses from one IP in one burst clusters them,
whatever the intent, and that is permanent and unrevocable.** A public
Esplora operator learns which addresses belong together and when they are
watched. The mitigation is not a setting, it is a different deployment —
run your own, and nothing leaves your machine.

**Never a service that accepts an xpub.** An extended public key hands a
third party every address the wallet will ever derive, past and future, in
one request. There is no code path in this adapter that transmits one. When
xpub support lands, derivation happens inside the adapter and only derived
addresses are queried — and gap-limit lookahead will still leak future,
unused addresses, which `PRIVACY.md` states as a commitment binding that PR
rather than a footnote to be discovered later.

### 2. A resource is one wallet, and its `resource_id` is immutable

**One resource = one watch-only wallet = a set of scriptPubKeys.** Never one
address: a balance and a history belong to the set, and per-address
resources would report a self-transfer as a payment out plus a payment in.

`local_id_derivation = "btc-txid@1"`, and
`local_id = "<resource_id>:<txid>"`. **The prefix is required, not
decoration.** The host keys observation chains by `(adapter_id, local_id)`
and *not* by resource (`spec/observation.md` §3). Two of your own wallets
paying each other is routine and they share a txid; a bare-txid `local_id`
would merge their chains, and one wallet's tombstone would delete the
other's transaction.

**`resource_id` is immutable: renaming it creates a NEW resource.** The host
already keys resources by `(adapter_id, resource_id)`, so a rename forks the
observation chain and starts a fresh one. This adapter deliberately derives
**no** "stable key" from the address set to paper over that, because such a
key would make *adding an address* rename the resource — trading a
surprising rename for a much worse one.

### 3. The cursor is compound, and pages are cut by BYTES

The cursor is `"<height>:<txid>"`, optionally suffixed `":m:<mempool_txid>"`,
and `cursor_resumable` is `exact`. The confirmed high-water mark is always
present, so a cursor persisted mid-mempool still resumes confirmed reads
correctly. One sync emits two sections: confirmed transactions strictly
above the mark, ascending by `(height, txid)`; then a by-txid section — the
tracked mempool set, everything currently unconfirmed, and every tombstone —
re-emitted **in full every sync**, which is what turns "this pending
transaction was mined into a block below your cursor" into a revision of the
same `local_id` instead of a record that stays pending forever.

**Pages are cut at 512 KiB of serialized observations, never at a block
boundary.** The rejected alternative — "a page never splits a block" — was a
**third-party-triggerable permanent denial of service**: roughly 1300
observations fill the 1 MiB `MAX_FRAME_BYTES`, a block holds up to ~6000
transactions, and mailing dust to a published address is free and
unstoppable. Someone else's spam, mined into one block, would make that
block's page unrepresentable; an oversized frame is a fatal kill with no
resync (ADR 0003), so that block would poison every future sync of that
wallet, forever, with no way for the victim to recover. Splitting a block is
safe because a transaction's position inside it is fixed once mined — which
is exactly what `exact` resumption promises.

### 4. A tombstone requires positive evidence

**A tombstone requires a direct `GET /tx/:txid` answering 404. Absence from
a listing is never evidence.** The reason — `reorged_out` versus
`dropped_from_mempool` — is decided by the state recorded in `seen.json`,
never guessed.

Two rules hold that line:

- **Any fetch failure anywhere in a sync suppresses the diff entirely.** No
  observations, no page, and the resource reports `stale{as_of}` or
  `unavailable`. There are no partial diffs, because a diff computed from a
  half-read chain tombstones transactions that were merely unreachable — and
  an append-only chain never forgets a tombstone.
- **A missing, unreadable, unparseable, version-mismatched,
  derivation-mismatched or hash-mismatched `seen.json` is a FIRST RUN**:
  zero remembered transactions, therefore zero tombstones. Never a partial
  parse, never a best-effort salvage.

The asymmetry is deliberate. A missed tombstone is noticed on the next sync.
An invented one is a retraction written into a permanent record of something
that never happened.

### 5. Concurrent writers are last-writer-wins, on purpose

`seen.json` is written temp-file + rename (atomic within one directory), and
only once a crawl has been delivered in full. Two adapter processes syncing
the same wallet can lose one another's update, and **that is accepted rather
than locked against**, because of decision 4: under positive evidence a lost
update degrades to a *missed* tombstone, caught on the next sync, and can
never produce an invented one. The torn-file case is covered by the same
rule — an unparseable file is a first run.

A lock would buy a stronger guarantee than the failure mode needs, and would
introduce a failure mode the current design does not have (a stale lock file
wedging a wallet).

## Binding on PR 4: the address-set hash must invalidate the cursor

**Adding an address to a wallet puts pre-cursor history at heights at or
below the host's stored cursor, which `exact` forbids re-emitting, and this
adapter has no channel to invalidate a cursor the host holds.**

In this PR the defect is latent, and only because cursors die with the
crawl: a drained page returns `next: null` and nothing persists one. For
tombstone purposes the adapter already treats a changed address-set hash as
a first run, and says so on stderr.

**PR 4's cursor persistence MUST invalidate the stored cursor when the
address-set SHA-256 recorded in `seen.json` differs from the wallet's
current one.** Without that, "adding an address is a revision" is false for
history: the new address's past is silently never delivered. This paragraph
is the requirement; a PR 4 that persists cursors without it does not pass.

## Reconciliation with ADR 0001 (Revision 2)

Satisfied, in the narrow sense that ADR mandates. Revision 2 scopes Rust to
"the core runtime, policy enforcement, reconciliation, the initial CLI, and
reference adapters" — this is a reference adapter, and it is Rust. Revision
2 also says Rust does not make financial logic correct: accordingly, this
adapter does no arithmetic on money that is not `sumer_money::Amount`,
performs **no BTC conversion anywhere** (a conversion is a division by 10⁸,
which is the arithmetic `spec/money.md` exists to keep away from money), and
carries `asset: "sat"`, scale 0, integers only.

The language-neutrality claim ADR 0001 rests on is *not* re-argued here: it
was banked by the Python reference adapter passing the same conformance
suite this adapter now passes. Nothing in this adapter is privileged by
being written in Rust; it reaches the host through the same JSON Lines
process boundary as any other.

## Alternatives considered

- **A full node / Bitcoin Core RPC instead of Esplora.** Rejected for now:
  it is the best privacy answer and remains available through
  `--source http://localhost:3000` against a local `electrs`, but requiring
  a synced node to read a balance makes the adapter unusable for the person
  who most needs to be told what it discloses. The deployment choice is one
  config line precisely so the privacy-maximising option is not a rewrite.
- **A wallet as an xpub.** Rejected: see decision 1. Deferred, not refused
  forever, and its privacy cost is written down in advance.
- **A stable resource key derived from the address set.** Rejected: it hides
  a rename by inventing a worse one — adding an address would fork the
  chain.
- **Per-address resources.** Rejected: a self-transfer becomes a payment out
  plus a payment in, and "the balance" becomes a number no resource holds.
- **Pages that never split a block.** Rejected: see decision 3. This is the
  only alternative in this ADR rejected as a *security* matter rather than
  an aesthetic one.
- **Absence from a listing as evidence of a vanish.** Rejected: see decision
  4. It is the cheapest possible implementation and it writes fiction into
  an append-only chain the first time a provider is flaky.
- **Locking `seen.json` across processes.** Rejected: see decision 5.

## Consequences

- **The crawl repeats per page request.** Esplora pages address history
  newest-first with no "from height H" entry point, so reaching old history
  means walking all of it, and this adapter holds no state between requests
  — `O(wallet history)` per page, bounded by a 15s per-request timeout
  inside the host's 30s deadline. A large wallet on a slow provider can
  exceed it. This closes when PR 4's persistence can cache the crawl.
- **`window` page requests are refused** with an envelope `invalid_request`:
  Bitcoin history is ordered by block, not by wall-clock time.
- **Four outcomes are never emitted** — `reauth_required`, `revoked`,
  `sca_required`, `gone` — and `credential_expires_at` /
  `strong_auth_expires_at` are always absent. A watch-only wallet has no
  credential, no session, and nothing that can be revoked. The live
  invariant check asserts it.
- **Balance category names are this adapter's own.** Esplora exposes no
  balance field at all, so `spec/observation.md`'s "the provider's verbatim
  name" has no answer to be faithful to; `confirmed` and `unconfirmed` are
  named by us, computed from `chain_stats`/`mempool_stats` funded minus
  spent, never summed, and `unconfirmed` may legitimately be negative.
- **The riskiest thing left is not mechanical.** Positive evidence means the
  adapter can no longer invent a vanish, so the remaining exposure is a
  **wrong mapping baked identically into `map.rs` and into the hand-written
  `expect` blocks** of the replay cases — which every automated gate passes,
  including the mutation battery, whose Bitcoin mutants rewrite the
  adapter's stdout and prove only that those cases have teeth. It resolves
  by hand: review reads the expects against the checksummed provider JSON,
  and the recorded corpora carry an arithmetic reconciliation (the net
  deltas sum to the balance the provider's own counters report) that no
  adapter code participates in.

## Security and privacy implications

The disclosure is the address set and the timing of the queries, and it goes
to whoever operates the Esplora deployment plus anyone on the path who can
see the TLS SNI. It is unrevocable: an address queried once is associated
forever. `PRIVACY.md` is the Milestone 1 exit criterion for exactly this
reason, and it is written for the user, not for the reviewer.

This adapter holds no keys, signs nothing, and has no `execute()` to sign
with. It is a read-only surface onto public data, which is the least
dangerous thing an adapter can be, and it still leaks the one thing Bitcoin
users cannot get back.

## Reversibility

High for the backend, low for identity. Changing `--source` is one config
line and changes nothing else. Changing the *identity* rules —
`local_id_derivation`, or what a resource is — is not reversible in the
usual sense: it re-identifies every record already written, which is why the
derivation is versioned (`btc-txid@1`) and a change to it is a new
derivation name, not a redefinition of this one. The cursor grammar is
adapter-private and may change freely, provided a stored cursor from an
older version is treated as unparseable rather than misread — the same
discipline `seen.json`'s schema number applies to state.
