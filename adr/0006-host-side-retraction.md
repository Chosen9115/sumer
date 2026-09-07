# ADR 0006 — Retraction is host-side, derived from a complete sweep

- **Status:** accepted
- **Date:** 2026-09-07
- **Decision by:** Linus, Milestone 1

## Context

Absence is not expressible on this wire. An adapter reports what *is*; there is
no frame meaning "and nothing else exists," and the host cannot tell an adapter
what it has already seen (`spec/observation.md` §8, `spec/wire.md` §1). Yet a
transaction dropped from a mempool or reorged out of the chain really is gone,
and a history that still lists it is wrong.

ADR 0004 revision 4 deleted the Bitcoin adapter's answer to this. That answer
was adapter-side: remember what you reported, notice what stopped appearing,
emit a tombstone. It failed four adversarial rounds — four distinct defects in
one corner — and revision 4 recorded that the corner was deleted rather than
designed a fifth time. It also recorded where the capability was expected to
come back: *"Retraction needs durable state that survives a process, is written
under the host's own transaction rather than a file this adapter races itself
on, and can express 'delivered' as something other than 'a byte reached a
pipe'. That is what PR 4's persistence is."*

PR 4 is that persistence. This ADR decides where retraction lives, what
evidence licenses it, and what it costs.

## Decision

**The host derives retraction. No adapter ever reports a disappearance.**

The normative rule is `spec/observation.md` §8, which binds any host and not
only this one. This ADR records why these mechanisms were chosen over their
alternatives, and what they cost.

### 1. The evidence is a complete sweep, and it has eight gates

A **sweep** is one `history.read` for one resource. Retraction is licensed only
by a sweep that (1) began at `page: None`, (2) ran on one adapter process and
one crawl, (3) drained to `next: null`, (4) reported `fetched` on every page,
(5) carried no *anonymous* `degraded` entry, (6) reported
`cursor_resumable: exact` on every page, (7) ran over one connection whose
hello `local_id_derivation` is recorded on the crawl row, and (8) carried no
observation whose
`provenance.adapter_id` was not that connection's own.

Fail any one and the sweep is **partial**: its observations still persist as
evidence, and nothing is retracted.

Gate 8 is the newest and the least obvious. `fold` keys chains by
`(adapter_id, local_id)` and the store keys the rows a sweep is compared
against by the adapter it connected to; an observation naming a different
adapter means those two keyings have come apart and the host is holding two
notions of one chain. It is not a wire violation and the records are kept —
but a host cannot conclude an absence from a set it cannot key, so the sweep
does not license one.

**Gate 8 outranks §6's size degrade.** A foreign observation is refused
whatever its size — never measured, never dropped as oversized, never filed as
a `degraded` entry. Measuring first inverts the gate: a refusal blocks a
retraction, while a named degrade exempts one id and lets the sweep complete,
so an oversized foreign record would have licensed the retraction of
everything else absent from that read. `spec/observation.md` §8.1 states the
ordering.

**A `degraded` entry that names a `local_id` exempts that id and does not
disqualify the sweep.** `degraded` is a list (`spec/observation.md` §6),
judged entry by entry: only an anonymous entry fails gate 5, and one of those
fails it however many named entries sit beside it. The alternative was
considered and rejected as a permanent, third-party-triggerable denial of
service: the size degrade is deterministic per observation, so one oversized
record would black out retraction for that resource forever, and where record
size is provider-influenced anyone who can put bytes into your history can
switch the capability off. `spec/observation.md` §8.1 carries the full
argument.

**`refresh` always sweeps from `page: None`.** The persisted cursor exists to
resume a sweep a crash cut short (`refresh --resume`), and a resumed sweep is
partial by gates 1 and 2, so it never retracts. This costs nothing today: the
Bitcoin adapter re-walks the whole history on any crawl regardless. The cursor
is **dropped** when the resource's fingerprint or the adapter's derivation
changes, which discharges ADR 0004's binding paragraph on this PR, and no
`page: None` request ever carries a cursor.

### 2. One transaction

Everything the final page produces commits in **one** SQLite transaction: that
page's observations, the `last_seen_crawl` stamps, the cursor write, **the
retraction derivation**, the `adapter.local_id_derivation` /
`resource.fingerprint` / `resource.last_provider_id` updates, and the crawl's
open → drained transition.

Split any of them and a crash leaves a crawl recorded as drained whose
retractions were never derived — and resume sees a finished sweep, so they
never will be. One transaction makes that state unrepresentable rather than
merely unlikely.

### 3. A separate, append-only retraction table

A host cannot author an `Observation`: it has no amount, no surface, no posting
and no provider provenance for a record it did not read, and filling those in
on its own behalf is fabricating provider evidence. So a derived absence is not
written into the observation chain. It is
`retraction(adapter_id, local_id, revision, reason, crawl_id, retracted_at)`,
append-only, with no provider field to fabricate, where `revision` is the head
revision it retracts.

**A record is live iff its chain head's revision exceeds every retraction
revision for that key.** Revival then needs no special case at all: a later
sweep appends revision N+1 > N and the record is live again by the same rule
that hid it. `show` unions the two tables by revision.

The reason is chosen by *why* the record is absent, in this order: the head's
derivation differs from the sweep's → `derivation_changed`; the head's
fingerprint differs → `resource_definition_changed`; otherwise →
`absent_from_complete_sweep`. A derivation bump or an address-set change is a
**migration**, recorded in the sweep's own transaction — old ids can never be
re-emitted by a new pure function, so they retire under a reason naming the
*software* change, never under one that blames the provider. Each of the first
two also writes one discrepancy row per sweep.

### 4. Three exemptions, none of them a percentage

A complete sweep still does not retract everything absent from it:

- **`history_start`.** `refresh` already calls `status.read`; if that
  resource's status entry carries `history_start`, every live head whose
  `effective_at ?? observed_at` precedes it is exempt, and the sweep writes a
  `history_start_exempt` discrepancy. The comparison is between **instants**,
  never between strings — `Rfc3339` admits fractional seconds and numeric
  offsets, so text order and time order disagree on reachable input in both
  directions — and a timestamp that cannot be ordered exempts rather than
  retracts, because a wrong exemption costs a stale row and a wrong retraction
  destroys a record. A pruned or re-pointed Esplora is the wrong retraction
  that never self-corrects. It earns a discrepancy row because it is the one
  exemption that otherwise leaves no trace: the other two visibly retract
  nothing, this one quietly retracts less.
- **A changed vantage.** If the sweep's `provider_id` differs from
  `resource.last_provider_id`, the sweep retracts nothing, writes a
  `vantage_changed` discrepancy, and updates the column. The next sweep from
  that vantage retracts normally — **the rule terminates.**
- **An empty sweep.** A qualifying sweep with zero observations against a
  non-empty live set retracts nothing, writes an `empty_sweep` discrepancy, and
  prints it. `refresh --confirm-empty` is the only way to retract 100% of a
  resource.

None of these is a threshold. `history_start` exists in the status entry
precisely to bound what absence is evidence of; a different vantage is a
different question; and 100% is the one constant that needs no justification —
every fraction below it would have to be argued for.

### 5. One writer at a time

`connect`, `refresh` and `import` take an exclusive `File::try_lock` (standard
library — no new dependency) on `<profile>/lock` for the whole run. A second
writer exits 3 naming the profile. Read-only commands do not take it.

Without it, a cron `refresh` and a manual one each load the live set at start,
collide on revision numbers, and the older crawl re-activates what the other
correctly retracted. That is the same class of defect ADR 0004's decision 5
found in `seen.json`, arriving from a different direction, and it is answered
once here rather than per command.

### 6. `fold` and `paging` get their caller

`refresh` replays the resource's stored adapter observations into one
`fold::Fold` in stored order, then ingests the sweep's; `Fold::live_set()`
minus the retraction rule is the diff base, and `paging::ResumeState` drives
the page loop. Revision assignment stays in **one** implementation. Until this
PR both modules had no caller inside the host and the conformance suite was
their only consumer — a stated known limit in three files, now closed.

## Why the host-side version needs no delivery gate

This is the whole reason the capability is here and not in the adapter.

Adapter-side retraction had exactly one unrecoverable write. Everything else an
adapter records is re-derived from the provider on the next crawl; a lost
retraction is lost permanently, because nothing ever probes a txid no baseline
holds. That single asymmetry forced the baseline write to be gated on the reply
actually reaching stdout, and the gate is what four adversarial rounds failed
to get right: a byte budget scoped per resource instead of per reply, a commit
applied before the reply went out, a commit applied after *a* reply rather than
after *that* one, and a cursor-resumed page filtering an omitted tombstone out
of the crawl's accumulator while the baseline forgot it forever.

Host-side, the asymmetry is gone, for two independent reasons:

- **The evidence is the sweep, not a remembered baseline.** Nothing has to be
  carried from one run to the next for a retraction to be derivable. If a
  refresh dies, the next complete sweep derives the same retraction from the
  same evidence: what the provider reports now, against what the store already
  holds. There is nothing to lose, so nothing needs a gate to protect it.
- **Nothing is deleted.** A retraction is an append to a second table, and
  liveness is a comparison between two revisions. A wrong one is corrected by
  the next honest sweep appending a higher revision — no un-retraction path, no
  deletion, no second mechanism to keep consistent with the first.

And "delivered" is no longer "a byte reached a pipe": the host is the party
that consumed the reply, so the commit and the consumption are the same event,
inside one transaction (decision 2). The boundary that took four rounds to
place does not exist here to be placed.

## Alternatives considered

- **Adapter-side retraction — the design that consumed PR 3.** Rejected; see
  ADR 0004 revisions 1–4 for the full history. Four adversarial rounds produced
  four distinct defects in one corner, all downstream of the same fact: a
  retraction was the adapter's only unrecoverable write, which forced a
  delivery-gated commit that nobody got right. The fifth design was not
  attempted. What is recorded here rather than there: the corner was not deep
  Bitcoin trivia, it was structural, and any adapter that remembers what it
  reported inherits it.

- **A percentage threshold — "refuse to retract more than N% of a resource."**
  Rejected. Every value of N is unjustifiable: at 50% a provider serving half a
  history retracts the other half, at 5% the common case of a wallet losing one
  transaction out of ten is refused, and no value is derivable from anything.
  The three exemptions each rest on a fact instead. The empty-sweep refusal is
  the *only* case that looks like a threshold and is not: 100% needs no
  argument.

- **Retracting from a partial sweep, with the retraction marked
  low-confidence.** Rejected. A partial sweep did not look at the whole
  resource — that is what makes it partial — so a record absent from it may
  simply be behind the page it never fetched. Marking the guess as a guess does
  not make it evidence, and the CLI still has to decide whether to show the
  record or hide it. Persisting the observations and deriving nothing is the
  answer that keeps the evidence and refuses the claim.

- **Writing retractions into the observation chain as host-authored
  tombstones.** Rejected: it requires the host to author a `Provenance` naming
  a provider it never spoke to for that record, and an amount, surface and
  posting it does not have. Fabricated provider evidence is unfalsifiable
  downstream — nothing in an export would distinguish it from something the
  provider actually said.

- **Deleting retracted rows, or flagging them with a boolean.** Rejected. A
  delete destroys the evidence that a wrong retraction is corrected from, and a
  boolean needs an explicit un-set path for revival — a second mechanism that
  can disagree with the first. The revision comparison gets revival for free
  and keeps everything.

- **A cross-run baseline on the host — remembering what the last sweep
  contained and diffing against it.** Rejected: it reintroduces the exact
  asymmetry that killed the adapter-side design, one process boundary further
  in. The live set the store already holds *is* the baseline, derived from
  durable observations rather than remembered separately, and it needs no
  gate because it is not a second copy of anything.

- **A `retract` op on the wire — letting the host tell an adapter what it has
  already seen, so the adapter can answer "and nothing else."** Rejected for
  this milestone: it is a new op, a new frame shape, and a per-resource
  manifest whose size is unbounded by anything in `spec/wire.md`'s caps, in
  exchange for moving a derivation the host can already make. It also puts the
  host's stored state on the wire, where a compromised adapter reads it.

## Consequences

- **The Bitcoin adapter's headline limit closes without the adapter changing.**
  A transaction dropped from the mempool or reorged out is now retracted by the
  host from a complete sweep. The adapter still emits no tombstone, still keeps
  no memory, and still has no unrecoverable write.
- **The reason is coarser than a tombstone's was.** ADR 0004 decision 4
  distinguished `reorged_out` from `dropped_from_mempool` using state the
  adapter remembered. A host deriving absence has no such state and does not
  guess: both arrive as `absent_from_complete_sweep`. That is a real loss of
  detail, and it is the honest one — the host genuinely does not know which
  happened, and `spec/observation.md` §4's tombstone vocabulary remains
  available to any adapter that can make the positive claim.
- **A resource whose adapter reports `cursor_resumable: batch_restart` or
  `none` never retracts**, because gate 6 can never hold for it. That is
  correct rather than a gap — neither family supports the claim that a read
  covered the whole span between its start and its drain — but it means the
  first bank adapter (Milestone 2) may arrive without this capability, and it
  will need its own answer rather than inheriting this one.
- **`refresh` holds the whole resource's history in memory.** The fold replays
  every stored observation for the resource before ingesting the sweep, so the
  ceiling is O(resource history) per refresh. Marked in the code and paged when
  a wallet outgrows RAM; not engineered for today.
- **A second writer fails instead of waiting.** Exit 3, naming the profile. A
  cron `refresh` that collides with a manual one does not queue.
- **Dedup and revival contradict each other on a byte-identical re-emission,
  and revival wins.** Dedup by content hash exists so a 5,000-transaction
  wallet does not grow 5,000 rows per refresh. But a reorg that re-mines a
  transaction re-emits it **byte-identically**, so a dedup firing on a key a
  retraction has buried would append no revision, leave the head at N, and
  strand the record retracted forever however many honest sweeps carried it —
  the reorg-remine case, which is the one this capability was built for. The
  resolution is not a special case in the liveness rule, it *is* the liveness
  rule: while a key's head revision is at or below a retraction revision for
  that key, every re-observation appends. "Live again" is a changed fact even
  when the content is not, and a revision is the only thing this model has to
  say it with. Recorded in `spec/observation.md` §8.4.
- **A dedup hit refreshes the stored derivation and fingerprint, not only the
  last-seen marker.** Otherwise a record that *survived* an address change
  keeps the stale fingerprint, and its ordinary disappearance three sweeps
  later reports `resource_definition_changed` — the software change blamed for
  a provider absence, which is the failure the reason ordering exists to
  prevent. `spec/observation.md` §8.3 states it normatively.

### The riskiest thing, stated rather than engineered around

**The gate rests entirely on adapter-supplied status.** An adapter that answers
`fetched { page_empty: true }` with `next: null` while its provider is down is
asking the host to retract a history, and **the wire cannot verify it**.
`spec/wire.md` §9's boundary is a crash boundary, not a trust one: nothing in
this protocol distinguishes an honest empty page from a dishonest one, and
nothing outside it does either.

The empty-sweep exemption covers the **total** case — a resource that goes from
a full history to nothing does not retract without an explicit
`--confirm-empty`. A **partial** lie — an adapter that reports nine of ten
transactions as a drained, `fetched`, `exact` sweep — is covered by nothing,
and no threshold would make it so (see the rejected alternative above). This is
stated as the residual exposure of the design, not as an argument that it is
small.

### Self-correcting is not harmless

The obvious defence of the above is that a wrong retraction is undone by the
next honest sweep. That defence is true and it is not sufficient, and writing
it down as though it were would be the dishonest part.

Between a wrong retraction and the next honest sweep, the CLI is **required**
to hide records the user still owns. That is not a degraded display, it is a
user staring at a transaction that is missing from their own history. And "the
next sweep revives it" assumes two things nobody controls: that the user keeps
refreshing, and that the provider heals.

What is genuinely bounded is **destruction**:

- nothing is deleted — the chain, the retraction rows and the crawl all
  survive;
- `show` names the crawl behind every retraction;
- `status` lists the discrepancy;
- export carries the whole chain.

So a wrong retraction is recoverable and auditable. It is not invisible, and it
is not free.

## Reconciliation with ADR 0004

ADR 0004 revision 4 left two obligations on PR 4, and both are discharged here.
Its decision 7 said retraction "lands there or not at all"; it lands, host-side,
and the adapter is unchanged. Its binding paragraph required that persisted
cursors be invalidated when the address-set hash changes; decision 1 drops the
cursor on a fingerprint or derivation change, and `refresh` sweeps from
`page: None` regardless. ADR 0004 records this in its revision 5.

## Reversibility

Medium. The rule is normative in `spec/observation.md` §8 and binds any host, so
changing it is a spec change with a conformance cost, not a local refactor. What
is cheap to reverse is the *mechanism*: the retraction table is append-only and
additive to the observation chain, so a host that stopped deriving absence
entirely would show a superset of records and lose no evidence. What is
expensive is loosening a gate — a host that retracts on weaker evidence than
this cannot un-tell a user that their history was missing, which is why the
eight conditions are spec text rather than an implementation detail.
