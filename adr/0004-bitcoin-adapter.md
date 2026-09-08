# ADR 0004 — The watch-only Bitcoin adapter: Esplora, wallet-shaped resources, byte-cut pages, and no retraction

- **Status:** accepted (revision 5)
- **Date:** 2026-09-07
- **Decision by:** Linus, Milestone 1

**Revision 1** corrects two things this document got wrong. Decision 3's
page budget was per resource, which is not a frame limit; decision 5
asserted a guarantee that concurrent writers did not provide.

**Revision 2** corrects the escape hatch revision 1 left in decision 5: an
unlocked fallback when `flock` is unavailable, described there as
acceptable. It is not — it reinstates the very overwrite the merge exists
to prevent — and decision 5 now says what the code does.

**Revision 3** adds decision 6: *when* a state write happens, and what
exactly it is allowed to forget. Three successive fixes to that corner were
each locally correct and each exposed the next, so it is stated here as a
decision rather than patched a fourth time. Nothing in decisions 1–5
changes.

**Revision 4 deletes decision 4, decision 5 and decision 6, and replaces
them with decision 7: this adapter no longer retracts.** A fourth
adversarial round found the same defect class again through a new path, and
the corner is deleted rather than designed a fifth time. Decisions 1, 2 and
3 are unchanged. The three superseded decisions are kept below, marked
**Superseded in revision 4**, because what they claimed and why it stopped
being claimed is the whole argument.

**Revision 5 changes no decision in this document. It records that PR 4
discharged both obligations revision 4 left on it**: retraction has returned,
host-side, and the address-set binding below is met. Decisions 1, 2, 3 and 7
stand exactly as written — this adapter still emits no tombstone, still keeps
no memory of what it reported, and still makes no unrecoverable write. What
changed is on the other side of the wire. See
`adr/0006-host-side-retraction.md` and `spec/observation.md` §8, and the two
paragraphs marked **Discharged in revision 5** below.

Corrections are marked **Revised** in place, with the claim they replace
stated rather than deleted.

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

Five decisions had to be made together; revision 3 added a sixth, and
revision 4 replaced three of them with a seventh.

## Decision

### 1. Esplora REST, and the privacy that costs

The adapter speaks the **Esplora REST API**, defaulting to
`https://blockstream.info/api`. One protocol, three deployments, one config
line: Blockstream, mempool.space's `/api`, or your own `esplora`/`electrs`
instance. Four endpoints are ever requested (a fifth, `GET /tx/:txid`, was
the tombstone probe and went with decision 7), and the `User-Agent` is
`sumer-bitcoin/0.1` — a browser forgery would be dishonest and would not
change the disclosure that matters.

The cost is stated in `adapters/bitcoin/PRIVACY.md`, and it is not small:
**querying a wallet's addresses from one IP in one burst clusters them,
whatever the intent, and that is permanent and unrevocable.** A public
Esplora operator learns which addresses belong together and when they are
watched. The mitigation is not a setting, it is a different deployment —
run your own instance, and the disclosure goes to a party you control
instead of to a stranger. It leaves your *machine* only when that instance
runs on the same machine: an instance on a VPS, a home server or another
LAN box still puts the address set on a network, and `--source http://…` is
plaintext unless you put TLS in front of it. `PRIVACY.md` states this per
deployment.

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
above the mark, ascending by `(height, txid)`; then a by-txid section —
everything the provider reports as unconfirmed **for this crawl**, ascending
by txid, minus whatever a `:m:` cursor has already delivered. Section 2 is
not placed against the confirmed mark at all, because an unconfirmed
transaction has no height to place, and that is what the live check's
exemption is for.

**Revised in revision 4: section 2 carries only the CURRENT mempool, and
delivers no revision.** It also carried the tracked mempool set and every
height remembered from the last completed crawl, which is what turned "this
pending transaction was mined into a block below your cursor" into a
revision of the same `local_id`. That memory was the transaction baseline
decision 7 deletes, and the revision went with it: pages of one crawl come
from one frozen snapshot, so nothing can be pending on one page and
confirmed on a later one, and a cursor-resumed read that starts a fresh
crawl finds the transaction carrying a height at or below the mark, where
section 1 drops it. The cost is stated as a known limit rather than
engineered around: the record is repaired only by a `history.read` with no
`page`, which re-emits everything at its current state. It is the same
family as the address-set/cursor defect below, and closes the same way — in
PR 4, with persistence.

**Pages are cut at 512 KiB of serialized observations — and never past what
the REPLY has left — never at a block boundary.** The rejected alternative
— "a page never splits a block" — was a
**third-party-triggerable permanent denial of service**: roughly 1300
observations fill the 1 MiB `MAX_FRAME_BYTES`, a block holds up to ~6000
transactions, and mailing dust to a published address is free and
unstoppable. Someone else's spam, mined into one block, would make that
block's page unrepresentable; an oversized frame is a fatal kill with no
resync (ADR 0003), so that block would poison every future sync of that
wallet, forever, with no way for the victim to recover. Splitting a block is
safe because a transaction's position inside it is fixed once mined — which
is exactly what `exact` resumption promises.

**Revised: the budget is the whole reply's, not one resource's.** A
per-resource budget is not a frame limit at all — a reply carries every
requested resource's observations, every resource's status entry and every
`provider_detail` in one frame, and two wallets with ordinary transactions
produced 1,049,218 bytes against the 1,048,576-byte cap. That is the same
fatal-kill denial of service the byte-cut was chosen to prevent, arriving
from the other side of the connection. The budget is now spent once across
the reply; statuses are charged before observations, because a status entry
is mandatory for every requested `resource_id` and an observation is not;
`provider_detail.raw.body` is capped at 4 KiB with a marker; and
`write_reply` refuses to write an oversized frame at all, answering `err` on
the same `id` instead. A single observation that cannot fit runs
`spec/observation.md` §6's two-step degrade — truncate `provider_extra`,
then omit it and report it in `degraded` — rather than being withheld
forever: `block_hash` is a provider scalar, and this adapter does not get to
assume a provider scalar is small.

### 4. A tombstone requires positive evidence

**Superseded in revision 4 — this adapter emits no tombstones at all.**
What follows is the rule as it stood, kept because decision 7 is an
argument about it. The one part that survives verbatim is the first bullet:
any fetch failure anywhere in a sync still suppresses the diff entirely.

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

### 5. Concurrent writers MERGE; they never overwrite wholesale

**Superseded in revision 4.** The merge and the `flock` existed for one
reason — a txid in nobody's baseline is never probed again — and there is
no baseline and no probe any more. The balance cache that replaced it is
written temp-file + rename, unlocked, and a lost update costs one
`unavailable`.

**Revised.** This decision originally read "last-writer-wins, on purpose",
on the grounds that under positive evidence a lost update degrades to a
*missed* tombstone, caught on the next sync. **That claim was false, and
this is the correction.**

The counterexample: process B completes a sync and records a transaction T
that arrived after process A's crawl began. A then writes its own snapshot,
which does not contain T. T is now in nobody's baseline — so nothing ever
probes it, no tombstone is ever emitted for it, and if it is later dropped
from the mempool or reorged out, the `active` observation A already emitted
for it stays uncorrected **forever**. That is not a delayed tombstone, it is
a permanent one that never comes, and it is the single failure mode the
positive-evidence rule cannot absorb.

So `seen.json` is still written temp-file + rename, and still only once a
crawl has been delivered in full, but the write is now a **merge**: it keeps
every txid already on disk that this crawl did not itself prove gone. The
read-modify-write runs under an advisory `flock` on
`<state-dir>/<resource_id>.lock`.

**The merge and the lock are both necessary.** A lock alone would not close
this: the racing window is the whole crawl — load the baseline, fetch for
tens of seconds, write — and holding a lock across a network fetch is how
one slow provider wedges every other process on that wallet, past the host's
30s deadline. The lock is held for a file read and a rename, and nothing
else. The stale-lock failure this ADR previously feared belongs to a lock
*file* with a pid in it; `flock` is released by the kernel when the process
dies, so there is nothing to go stale.

**A lock that cannot be taken fails the write.** This decision first read
that a filesystem which cannot `flock` (some network mounts) gets an
unlocked read-modify-write, on the grounds that the merge is what makes the
guarantee and the lock only narrows the window. **That was wrong, and it
undid the merge it was written to defend.** Unlocked, both writers read the
same baseline, each merges its own additions into that copy, and the second
rename erases the first's — the permanent silence above, restored in full.
So a sync that cannot take the lock reports every observation it read and
declines to move the baseline; the next sync retries. A lost update is
acceptable — the baseline is re-derived from the provider — and a silently
lost retraction is not.

A duplicate tombstone is the price: a txid this process tombstoned can be
re-added by a racing writer that still had it, and the next sync probes it
and tombstones it again. That is a repeated true statement about a
transaction that really is gone — positive evidence both times — and
`spec/observation.md` §4 makes the chain append-only precisely so a restated
fact is a legal entry rather than a contradiction. A missed retraction is
fiction; a repeated one is noise.

The torn-file case is unchanged: an unparseable file is a first run.

### 6. A state write is a FIELD of the reply that earned it

**Superseded in revision 4.** There is nothing left to gate: see decision
7. Kept here because the four rounds it survived are the evidence for
deleting it.

**Added in revision 3.**

Nothing is marked reported until it has been sent. The write that records
"this crawl's diff was delivered" must therefore happen after the frame
carrying that diff has actually gone out — and only then. Three attempts
got this wrong in three different ways: the byte budget was scoped per
resource, the write happened before the reply, and then the write happened
after *a* reply rather than after *that* one. The last is the interesting
one, and it is why this is a decision and not a patch.

**The shape, not a guard.** An op returns `Result<Outcome, ErrorBody>`,
where `Outcome` is the reply body **and the commits that body earned, as
one value**. `main` applies the commits if and only if `write_reply`
reported that *that reply* was written. An op that fails returns `Err`, and
`?` drops the `Ok` half holding the commits.

That last sentence is the whole decision. The rejected alternative was a
queue on the adapter, drained on "was the write successful?". An envelope
error — a cursor the adapter did not mint, a `window` page request,
a `resource_id` named twice — is a *small* reply. It fits a frame. It
writes successfully. So a batch of `[a real wallet, a bad cursor]` wrote an
error to stdout and committed the real wallet's baseline for observations
the host never received. The queue made that expressible; making the
commits a field of the body they belong to makes it unrepresentable, which
is the only version of this that survives the next restructuring.

**The whole commit is delivery-gated, not just its retractions.** The
transaction map is delivery-sensitive too: `map::plan` reads recorded
heights, so recording a new height — or recording `Some(height)` for a txid
that was tracked as mempool — removes it from the by-txid re-emit set
exactly as a removal does. What survives is not a gating asymmetry but a
**recoverability** one: a lost `txs` update is re-derived from the provider
on the next crawl, and a lost retraction never is, because nothing probes a
txid no baseline holds.

**`retracted` is what was EMITTED, not what was fetched.** The crawl
subtracts every tombstone that `spec/observation.md` §6 step 2 omitted for
size, accumulated across all of that crawl's pages, from the set it records
as gone. Without that subtraction the failure is silent and permanent: an
oversized tombstone is omitted, the crawl still drains, the baseline still
forgets the txid, nothing ever probes it again, and the host is told only an
anonymous `degraded` entry. Omission is deterministic per observation — it
depends on `MAX_OBSERVATION_BYTES` and the observation, not on the page
budget — so the accumulator needs no delivery gate of its own.

**`--source` is bounded at 256 bytes, rejected and not truncated.** It
becomes `provenance.provider_id` on every observation this adapter emits,
and it was the one unbounded field a tombstone carried. Truncating it would
be a falsified provenance, so an over-long `--source` is a usage error and
exit 2, exactly like a bad `resource_id`. With the bound, every field of a
tombstone is bounded by construction and the omission above is unreachable
in production. It is still implemented and still tested: the subtraction is
what stops the bound from having to be re-argued the next time a field is
added to `ObservationWire`.

**A repeated `resource_id` is an envelope `invalid_request`.** Every
requested `resource_id` appears in `statuses` exactly once
(`spec/observation.md` §6); a repeat has no conforming answer, so the
request cannot be processed as a whole. The offending id stays out of
`err.detail` — `spec/wire.md` §8 makes an `err` payload naming a resource
the signal that a status outcome was the right channel, and here it is not:
the fault is the shape of the request, not a fact about any resource.

**The residual hole, stated rather than engineered around.** Committing
after delivery means there is a window in which the host has been told a
transaction is active, the process dies or the state write fails before the
commit, and the transaction vanishes before the next crawl. Nothing probes
it, and no retraction is ever emitted. **That window is one `try_lock` plus
one `rename`** — the state write itself, with no network call inside it.

The alternative considered was committing *before* delivery. Its window is
the entire reply: serialization, the frame ceiling check, and the write to
a pipe whose reader may be gone. It also fails in the opposite and worse
direction — the baseline says "already reported" for a retraction the host
never received, which is exactly the loss the positive-evidence rule cannot
absorb, and it fails silently and permanently at scale. This is the cheaper
side of the trade, and the cost is written here rather than hidden.

A **startup probe** narrows it further and does not close it. A
`--state-dir` the operator declared and this process cannot create, write
or lock is exit 2 at startup, before the hello reply — an operator's
declared configuration must work, and the previous behaviour was to run for
weeks reporting `unavailable`, emitting no tombstone, and saying so only in
a stderr line per failed write. Omitting `--state-dir` stays an announced
warning: that is a different configuration, and it works. The probe is
`create_dir_all`, a temp write and a rename, then `File::try_lock` on
`<dir>/.probe.lock` — `try_lock` and not `lock`, with `WouldBlock` counting
as success, so a directory another adapter already holds cannot hang
startup. **ENOSPC, a quota reached at write time, and a directory deleted
mid-run all survive the probe verbatim**, and so does a wedged network
mount that blocks inside `open` itself. It narrows the hole; it does not
close it.

### 7. This adapter does not report disappearances

**Added in revision 4, and it deletes decisions 4, 5 and 6.**

**The rule.** The adapter reads balances and history and reports what the
provider says now. It keeps no record of what it saw last time, performs no
`GET /tx/:txid` probe, and emits no `tombstoned` observation under any
circumstance. `map.rs` has no tombstone constructor left to call.

**Why, and why now rather than after a fifth design.** The transactional
boundary between writing a reply and committing local state took four
adversarial rounds and produced four different defects: the byte budget
scoped per resource instead of per reply; the commit applied before the
reply went out; the commit applied after *a* reply rather than after *that*
one; and finally a cursor-resumed page filtering an omitted tombstone out of
the crawl's accumulator while the baseline still forgot it forever. Four
rounds, one corner. The fourth was judged evidence that the design was
wrong rather than that the refinement was incomplete.

**That corner existed for exactly one reason.** A retraction is the only
unrecoverable write this adapter makes. Everything else it records is
re-derived from the provider on the next crawl. Absence is not expressible
on this wire — there is no "and nothing else exists" frame — and the host
cannot tell an adapter what it has already seen, so the adapter has to
remember it, and a lost retraction is lost permanently: nothing probes a
txid no baseline holds. That single asymmetry is what forced the write to
be gated on delivery, and the gate is what four rounds failed to get right.

**Remove the retraction and the asymmetry disappears.** With no
unrecoverable write there is no delivery gate, no `Outcome` carrying
commits, no omitted-tombstone accumulator, no `flock`, no wholesale-overwrite
merge, and no startup probe that refuses to run without a usable state
directory. Every failure mode — a dead process, a failed rename, a refused
reply, a full disk, a directory deleted mid-run, two adapters racing the
same file — collapses to *re-derive on the next sync*, which needs no
transactional reasoning at all.

**What `seen.json` is now.** A balance cache: `schema`,
`address_set_sha256`, and the `confirmed`/`unconfirmed` figures the last
successful `balances.read` observed with the instant it observed them. It
exists so a failed balance read can answer `stale { as_of }` instead of
`unavailable`. Nothing it holds is unrecoverable, and that is the whole
point: losing the file, failing to write it, or writing it for a reply the
host never received all cost the same thing — one `unavailable` where a
`stale` was possible, until the next successful read. So the write is not
gated, is not locked, and is not merged. A schema-2 file (which carried the
transaction baseline) is a first run.

The rules that DO survive from decision 4, because they are about what the
adapter is allowed to claim rather than about what it remembers:

- **Any fetch failure anywhere in a sync suppresses the diff entirely.** No
  observations, no `page`, and the resource reports `unavailable`. There are
  no partial diffs: half a wallet's history reported as if it were the whole
  of it is a claim with no evidence behind it.
- **A missing, unreadable, unparseable or hash-mismatched cache is a FIRST
  RUN.** Never a partial parse. A figure this adapter cannot stand behind,
  under a date it did not observe, is worse than no figure.
- **`--source` is bounded at 256 bytes, rejected and not truncated**, and a
  **repeated `resource_id` is an envelope `invalid_request`**. Both are out
  of this corner and both stand as revision 3 wrote them.

**What the caller loses, stated plainly.** A transaction dropped from the
mempool or reorged out of the chain **stays in the host's live set**. The
host was told it was active; nothing ever tells it otherwise; no later sync
of this adapter corrects it. Everything else self-corrects on the next
crawl. This is the one thing that does not, and it is written at the top of
`adapters/bitcoin/README.md` and in `PRIVACY.md` rather than left to be
discovered.

**It returns in PR 4, designed once.** Retraction needs durable state that
survives a process, is written under the host's own transaction rather than
a file this adapter races itself on, and can express "delivered" as
something other than "a byte reached a pipe". That is what PR 4's
persistence is. Retraction lands there or not at all; it does not come back
as a fifth patch to a file-based baseline.

**Discharged in revision 5.** It landed there, and not as a fifth patch to
anything here. The host derives absence itself from a **complete sweep** —
one `history.read` that began at `page: None`, drained, and passed nine
gates — and writes it to an append-only retraction table under the same
SQLite transaction as the sweep's final page. The three requirements this
paragraph set are met and one of them turned out to be unnecessary: the
state is durable and survives a process, the write is the host's own
transaction, and "delivered" needs no expression at all, because the party
that commits is the party that consumed the reply. The normative rule is
`spec/observation.md` §8; the argument is `adr/0006-host-side-retraction.md`.
**Nothing in this adapter changed to make that work**, which is the strongest
available evidence that decision 7 removed a capability from the wrong
layer rather than removing it from the product.

**The wire's tombstone shape is unaffected and stays proven.**
`spec/observation.md` §4's tombstone semantics, the host fold, and the
append-only chain are all exercised by `conformance/cases/reorg_vanish.json`
against the reference adapter — one `local_id` going
active → tombstoned → active, with
`reorg_vanish__tombstone_page_dropped` as the mutant that keeps it honest.
Nothing about the protocol changed here; one adapter stopped claiming a
capability it could not implement safely.

**`bitcoin/btc_reorg` and its corpus are deleted, not weakened.** The case
drove the real adapter over `corpus/reorg/` and declared a `reorged_out`
tombstone; the adapter cannot produce one, so the case asserts a claim that
is no longer made. Editing its `expect` to drop the tombstone would be
weakening a fixture, which this project does not do — so the fixture, the
corpus, the `btc_reorg__tombstone_page_dropped` mutant and its wrapper go
together, and the `COVERAGE` row with them. What that corpus proved was
never a live capability in the first place: its own `RECORDED.md` says
`run1/` is a synthesized counterfactual and "development scaffolding …
never evidence of a live connection". It is recoverable from git when PR 4
restores the capability it was written for.

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

**Discharged in revision 5.** PR 4 meets it twice over. The host records a
`fingerprint` per resource and **drops the stored cursor** whenever that
fingerprint or the adapter's `local_id_derivation` changes; and separately,
`refresh` always sweeps from `page: None`, so the ordinary path never carries
a stored cursor at all (the cursor exists to resume a sweep a crash cut
short, and a resumed sweep is partial by the gates in
`spec/observation.md` §8.1, so it never retracts either). The requirement is
met by the first rule; the second makes it unreachable in the common case
rather than merely handled.

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
- **Locking `seen.json` across the whole crawl.** Rejected: the lock would
  be held across a network fetch, so one slow provider wedges every other
  process on that wallet past the host's 30s deadline. The lock decision 5
  does take is held for a file read and a rename only.
- **Proceeding unlocked when `flock` is unavailable.** Rejected in revision
  2: see decision 5. It reads as a narrower window and is in fact no window
  at all — two unlocked writers overwrite each other's additions exactly as
  last-writer-wins did.
- **Last-writer-wins on `seen.json`.** Rejected in revision 1: see decision
  5. This ADR asserted it degraded to a missed tombstone, and it did not.
- **A queue of pending state writes, drained on "did the write succeed?".**
  Rejected in revision 3: see decision 6. An envelope error fits a frame,
  so it succeeded, so it settled a different resource's commit.
- **Committing the baseline BEFORE delivering the reply.** Rejected in
  revision 3: its window is the whole reply rather than one `rename`, and
  it fails in the direction that cannot be recovered from.
- **Truncating an over-long `--source`.** Rejected in revision 3: a
  truncated provider identity is a falsified provenance, which is a worse
  answer than refusing to start.
- **A fifth design of the commit boundary.** Rejected in revision 4: see
  decision 7. Four rounds produced four distinct defects in one corner, and
  the fifth attempt would have been the fourth time the same class was
  called a bounded refinement.
- **Keeping the transaction baseline for the by-txid re-emission set
  only.** Rejected in revision 4. It is not unrecoverable, so it would not
  have needed a gate — but it keeps `seen.json`'s history section, the
  merge, the lock and the schema rules around a section that, in this PR,
  buys exactly one thing: a revision surviving a cursor a host persisted
  across processes, which is a path this PR already documents as PR 4's
  (cursors die with the crawl here). Speculative generality, paid for in
  the file that caused four defects.
- **Emitting a tombstone from a crawl's own view alone**, with no memory —
  "it was in an earlier page of this crawl and is not in a later one".
  Rejected: within one snapshot nothing disappears, so this can only be
  built from absence across snapshots, which is decision 4 again with a
  shorter memory.

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
- **Only balances are stamped in `seen.json`, because only balances are in
  it.** They shared one `as_of` with the history baseline until revision 1,
  which made a failed balance read report the previous day's amounts under
  the day a *history* read happened — a false freshness claim about money,
  which is the one claim this project refuses to make. Revision 4 removes
  the other half of the file entirely, so a history read structurally cannot
  restamp a balance.
- **A batch too large to answer.** A reply must carry one status entry per
  requested `resource_id`, and no paging mechanism in this protocol can shed
  a status. So a request naming enough resources that their statuses alone
  exceed `MAX_FRAME_BYTES` is answered with an envelope `err` rather than a
  fatal oversized frame. Fewer resources per request is the answer, and the
  error says so.
- **Balance category names are this adapter's own.** Esplora exposes no
  balance field at all, so `spec/observation.md`'s "the provider's verbatim
  name" has no answer to be faithful to; `confirmed` and `unconfirmed` are
  named by us, computed from `chain_stats`/`mempool_stats` funded minus
  spent, never summed, and `unconfirmed` may legitimately be negative.
- **No disappearance is ever reported *by this adapter*.** See decision 7.
  Since revision 5 the host retracts such a record itself from a complete
  sweep, so it no longer stays in the live set — but the reason is coarser
  than a tombstone's was: a host deriving absence cannot tell `reorged_out`
  from `dropped_from_mempool` and does not guess, so both arrive as
  `absent_from_complete_sweep` (`adr/0006-host-side-retraction.md`).
- **A cursor-resumed read never carries a mined revision.** A transaction
  reported `pending` and later confirmed at or below the cursor is
  suppressed, not revised — in one process as much as across two. See
  decision 3 as revised. Repaired only by a `history.read` with no `page` —
  which, since revision 5, is what every ordinary `refresh` issues, so this
  costs a Sumer host nothing in practice. It remains a real limit for any
  host that resumes from a stored cursor by default.
- **`history.read` never answers `stale`.** Nothing about a history is
  cached, so a failed history read has no prior answer to be stale about:
  it is `unavailable`. Balances are unaffected — that is what the cache is
  for.
- **The riskiest thing left is not mechanical.** The adapter cannot report a
  vanish at all, so the remaining exposure is a
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
