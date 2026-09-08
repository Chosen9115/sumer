# ADR 0006 — Retraction is host-side, derived from a complete sweep

- **Status:** accepted (revision 1)
- **Date:** 2026-09-07
- **Decision by:** Linus, Milestone 1

**Revision 1** corrects decision 3 on one point and adds two rules the
implementation of this ADR made necessary. The liveness comparison reads the
chain's **highest** revision, not the head's — those are two different orders,
and mixing them buries records permanently (decision 3, and
`spec/observation.md` §8.4, which carries the reasoning). Head *selection* is
untouched. The two additions are the `resource_id` amendment to the content
hash and the adapter obligation that pairs with it (decision 7), and the rule
that decides whether a stored balance is fresh (decision 8) — where the
first design, a host-authored marker row, was rejected outright and the
reasoning for rejecting it is the part worth keeping.

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
only this one (with the two rules revision 1 added stated in §2 and §3). This ADR records why these mechanisms were chosen over their
alternatives, and what they cost.

### 1. The evidence is a complete sweep, and it has nine gates

A **sweep** is one `history.read` for one resource. Retraction is licensed only
by a sweep that (1) began at `page: None`, (2) ran on one adapter process and
one crawl, (3) drained to `next: null`, (4) reported `fetched` on every page,
(5) carried no *anonymous* `degraded` entry, (6) reported
`cursor_resumable: exact` on every page, (7) ran over one connection whose
hello `local_id_derivation` is recorded on the crawl row, (8) carried no
observation whose `provenance.adapter_id` was not that connection's own, and
(9) ran on a connection that broke no wire contract anywhere in that refresh.

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

**The ninth gate is refresh-scoped, and it is a gate rather than a taint.**
Conditions 1–8 judge one sweep. Condition 9 judges the connection across the
whole refresh: a `balances.read` that broke the wire contract disqualifies
every sweep that follows it on that connection. The alternative considered was
a refresh-scoped *taint* — a flag outside §8.1 that suppresses retraction
without joining the list. Rejected: §8.1's value is that it is exhaustive, and
a second disqualification path that does not appear in it turns the list into a
trap for the next reader. Conditions 2 and 7 are already connection-scoped, so
the list was never purely per-page, and the audit row
(`crawl.disqualified_reason`) keeps one vocabulary instead of two.

The taint is forward-only: a violation disqualifies every sweep that starts
after it, and the adapter-wide reads all precede every sweep. A sweep that
already committed decided on the evidence it had, and decision 3's revision
comparison revives a wrongly retracted record for free on the next complete
sweep — so nothing here needs to reach backwards.

Forward-only is about *commitment*, not about *entry*. The gate reads the
connection's violation state at the moment the sweep commits, not the value it
was handed on the way in, because a violation does not have to arrive as a
failed call: an adapter can answer a qualifying final page and break the
protocol behind that reply, and the host — which cannot un-deliver a reply it
has already handed over — sees no error at all. Judging the gate on the entry
snapshot commits the retraction *after* the host has established the connection
was lying. Re-reading at the commit is still forward-only: detection precedes
commitment, and no sweep that has already finished is ever reconsidered.

**A violation the host can only establish at the CLOSE is reported, not
tainted.** Closing the connection is the last thing a refresh does to it, so by
then every sweep it served has committed; there is nothing left to disqualify,
and the next refresh gets a different connection that this one's death says
nothing about. Withholding a retraction is therefore not available and not
wanted — but silence is not available either. `refresh` records the terminal
reason as an adapter error, which is what a cron job's exit code carries: a
refresh must not exit 0 when the last thing the connection did was break the
wire contract.

The narrowness matters as much as the rule. Only a **contract violation** taints
— `invalid_request`, or a fatal protocol violation. An honest `err`, a timeout,
a crash: those are failures in the vocabulary the contract provides, and a host
that treated them as taint would disable retraction on any rate-limited
balances call.

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
append-only, with no provider field to fabricate, where `revision` is the
chain's **highest** revision at the moment it retracts.

**A record is live iff its chain's highest revision exceeds every retraction
revision for that key.** Revival then needs no special case at all: a later
sweep appends revision N+1 > N and the record is live again by the same rule
that hid it. `show` unions the two tables by revision.

**Highest, not the head's — revised in revision 1.** `revision` is arrival
order; the head is the winner of the fold's total order
(`received_at`, `surface`, `arrival_index`). Liveness asks "has anything been
learned about this key since revision N," which is an arrival-order question,
so it must be answered in arrival order. Asking the head instead compares
across two orders and buries a record for ever the first time they disagree —
and they disagree on ordinary input: two surfaces in one page sort by
`surface` ahead of arrival, and a backwards wall-clock step (NTP correction, VM
snapshot restore) makes every later re-emission sort *below* the buried head,
so the head's revision never grows past the retraction while the chain gains a
row per refresh. Which observation the user sees as current is still the head,
chosen by the same total order as before; only the comparison moved.
`spec/observation.md` §8.4 states it normatively.

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

### 7. `resource_id` is hashed, and an adapter may not report a `local_id` twice

**Added in revision 1.**

The content hash that decides "same record, seen again" covers the frozen
contract's field list **plus `resource_id`**, a deliberate amendment recorded
in `core/store/src/hash.rs`. Chains are keyed `(adapter_id, local_id)` and
`resource_id`s are free to collide (`spec/observation.md` §3), so a
spec-conforming adapter emitting a bare provider id under two resources is
reachable — and with `resource_id` outside the hash, its second observation
deduped against the head stored under the *first* resource. Nothing was stored
under the new resource, the head kept the old resource's attribution, and the
old resource's next sweep then retracted — as `resource_definition_changed` —
a record the adapter had reported in that very refresh. The record ended up
live under **no** resource, and which resource was swept first decided it.
Nothing constrains that order.

A hash that omits a field the system discriminates on is a trap for whoever
reads it next, so the field goes in the hash. A record appearing under a
different resource is a **changed fact** and appends a revision, on the same
principle that makes a byte-identical re-emission of a buried record append.

The pairing rule is on the adapter, and it is normative in
`spec/observation.md` §3: **an adapter MUST NOT report one `local_id` under
two `resource_id`s at the same time.** The natural counterexample — a transfer
between two of the user's own wallets, genuinely visible from both — is
answered by giving the two sides distinct `local_id`s, which is what
`adapters/bitcoin/src/map.rs` already does by prefixing with the resource id.
Revision 1 makes that convention normative rather than accidental. An adapter
that ignores it appends a revision per sweep for ever and shows the record
under whichever resource swept last: well-defined, useless, and the adapter's
bug. The host cannot do better, because "moved" and "reported twice" are the
same bytes.

### 8. Balance freshness is derived from the read, not marked on the row

**Added in revision 1.**

Balance lines are appended and never overwritten, so a figure the host did not
read this time stays on top of its category, still stamped with the staleness
of the last read that succeeded, and renders as `Live` indefinitely. The rule
is now one comparison, stated normatively in `spec/observation.md` §2: **a
stored balance line is fresh iff the read that wrote it is that adapter's most
recent read.** A refresh opens a read for the adapter before anything that can
fail and stamps every line it stores; a read that reaches no adapter stamps
nothing, and every figure it did not refresh stops reading `live` with no
failure path having had to say so.

**What this replaced, and why the rejection is the lesson.** The first design
was the obvious one: on every path where a balance was not read, write a
host-authored **marker** row — amount withheld, freshness downgraded, last
figure untouched one row below — carrying an `unread:` prefix on its stored
`outcome` so it could not be confused with an adapter reporting `unavailable`.
Four adversarial review rounds found five bugs in it, and they were all one
bug. A failed `balances.read` wrote no marker. A resource that still reported
one category left a dropped category reading `live`. Spawn, `resources.list`
and `status.read` failures all returned before the marker code. A resource
dropped from a **successful** listing was visited by neither marker loop and
read `live` for ever. And the marker that did get written named the wrong
reason.

Each round found one path nobody had. That is the transferable finding, and it
is not about balances: **an enumeration of failure paths cannot be completed by
inspection.** A design whose correctness is the completeness of such a list is
wrong however many rounds it survives — the next round finds the next path.
Inverting it costs an integer and a column and cannot have that defect, because
the paths never have to be named. The marker machinery was deleted (−60 lines
of production code, `refresh.rs` alone −156), and with it the `unread:` prefix:
the host now writes no balance row of its own, so decision 3's "no provider
field to fabricate" has nothing left to apply to on this stream. A stored
`outcome` is always the outcome the adapter reported.

The derivation also made one refusal safe that was not. A `balances.read`
reply carrying a balance whose `provenance.adapter_id` is not the connection's
own is refused **whole**, not line by line — there is no judge downstream of
the decode, as there is for a history page, to tell a refused line from an
absent one. Refusing whole is only safe because a refused read writes no row:
what is on screen goes stale rather than staying `live` with another adapter's
number on it. Under the marker scheme that refusal would have owed a marker
path of its own — a sixth path to enumerate.

**Bounded by the request, not by the status.** The rule refuses a volunteered
balance whether or not the reply carries a status entry for it. The
status-bounded version — refuse a balance whose resource has no status — was
the obvious one and is rejected: it accepts the reply that carries *both* a
volunteered balance and a volunteered `fetched` status for the resource it just
stopped listing, which is the same figure on the same screen reading the same
`live`, with better paperwork. The request is the one bound the adapter does
not also author. It is also the cheaper rule to hold: bounding by the request
makes "a balance whose resource has no status entry" unreachable rather than
merely unlikely, since coverage already gives every requested resource exactly
one status.

**And one admission rule the derivation did not cover.** Deriving freshness
answers *which read wrote this row*; it says nothing about which rows are
allowed in. A reply that volunteered a balance for a resource just dropped from
a **successful** listing passed every check there was — coverage validates the
*requested* resources, and the provenance was honest — so the line was admitted
carrying a host-synthesized outcome and the `Live` staleness a missing status
entry defaults to, and rendered `live`. The alternative considered was to keep
accepting such figures and define what a host does with a balance for a
resource it has no `resource` row for: a storage case, a rendering case, and a
standing way for an unlisted resource to look current. Refusing the reply
instead (`spec/observation.md` §2) adds no case at all — a `balances.read`
names exactly the resources the listing produced, so an accepted balance always
has a row and an adapter-reported outcome — and it inherits the derivation's
own safety property: a refused read writes no row, so what is on screen goes
stale rather than wrong.

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
  rule: while a key's highest chain revision is at or below a retraction
  revision for that key, every re-observation appends. "Live again" is a
  changed fact even when the content is not, and a revision is the only
  thing this model has to
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
nine conditions are spec text rather than an implementation detail.
