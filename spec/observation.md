# Sumer observation model

Normative. This document defines what a `balances.read` or `history.read`
reply carries, how the host turns a stream of adapter-emitted observations into
a stable local history, and how pagination resumes. See `spec/wire.md` for the
envelope those replies travel in, and `spec/money.md` for `AmountWire` and
`AssetId`, which this document embeds and never redeclares.

## 1. Provenance

Every observation — a balance line or a history entry — carries a `Provenance`:

    { adapter_id, provider_id, surface,
      observed_at: Rfc3339,      // adapter-claimed, EVIDENCE ONLY
      received_at: Rfc3339,      // HOST-STAMPED
      effective_at: Option<Rfc3339>,
      staleness: Live | Cached | Unavailable,
      completeness: Complete | Partial | Unknown }

**`received_at` is host-stamped. An adapter that sends it gets
`invalid_request`.** The host writes this timestamp itself, at the moment it
receives the frame, using its own clock. It is the host's receipt stamp —
evidence of when the bytes arrived, kept for the audit trail — and it is the
input a future cache layer will classify a *replayed* observation's staleness
from (see below). **It is not an input to how staleness is computed today**:
this milestone reads no observation twice, so nothing yet needs to ask
`received_at` "is this still fresh."

**`observed_at` is adapter-claimed and is evidence only** — it is the
adapter's own account of when the provider observed the value, carried
through unmodified. **It is never an input to freshness, at any point,
present or future.** Why this split is load-bearing rather than a style
choice: if staleness read an adapter-supplied timestamp at all, an adapter
with a skewed system clock — or a compromised or simply buggy one that lies
about when it observed something — would silently control whether the host
displays data as `Live` or `Cached`. A skewed clock a few hours fast makes
genuinely stale data look fresh; a skewed clock a few hours slow makes fresh
data look stale; either way the host would be trusting a value it has no way
to verify. Keeping `observed_at` out of freshness entirely means the worst an
adapter's clock can do is make its own evidence less useful — it can never
make the host misrepresent freshness.

**How the host computes it.** Staleness is derived from the resource's own
`status` outcome (§6) in the **same reply** the observation arrived in —
never from a timestamp comparison on either side:

| That resource's outcome | Staleness stamped on its observations |
|---|---|
| `stale { as_of }` | `Cached` |
| `unavailable`, `gone` | `Unavailable` |
| every other outcome | `Live` |

Host-stamped has never meant host-invented. An adapter that answers
`stale { as_of }` has said, in the vocabulary this document gives it, that
what it is handing over is not current; stamping `Live` over that would be
the host overruling evidence it went and asked for. What the adapter still
cannot do is *name the staleness itself* — there is no `staleness` field on
the wire, and an adapter that sends one gets `invalid_request`.

Everything not covered by that table is `Live`, and honestly so: those
observations were read off the wire moments earlier. This milestone has no
cache layer, so nothing yet replays a stored observation. A host that starts
doing so MUST classify what it replays from the **stored** `received_at` —
comparing it against some freshness threshold to decide `Live` vs. `Cached`,
which is exactly why `received_at` is host-stamped and retained at all rather
than discarded once a reply is decoded. That comparison is a wider
computation than this table, not a different one — and `observed_at` still
plays no part in it.

## 2. Balances

Balances are a **list of provider-named categories, never a struct**:

    { resource_id: String,              // which resource this line belongs to
      category: String,                 // provider's verbatim name
      canonical_hint: Option<available | total | pending | held | confirmed | unconfirmed>,
      amount: Option<AmountWire>,       // null == UNKNOWN, NEVER "0"
      provenance }

**`resource_id` is required** (Contract Amendment 1, Ruling A1). `balances.read`
and `history.read` both batch multiple resources into one call and one reply
(`spec/wire.md` §5's op params carry `resource_ids`/`resources`, plural), so a
reply's `observations` array pools balance lines and history observations for
every resource requested in that call. Without a per-observation `resource_id`
the host has no way to attribute a pooled observation back to the resource it
came from — this is also what makes §6's "every requested `resource_id`
appears in `statuses` exactly once" a guarantee actually worth checking,
rather than one that is vacuously true because every call only ever named one
resource.

`category` is whatever name the provider uses, unmodified. `canonical_hint` is
an optional, best-effort mapping onto a small closed set the host understands
well enough to render meaningfully — it is a hint for display, not a
guarantee that any two providers' `"available"` mean the same thing.
**`amount: null` means UNKNOWN, and is never interpreted as zero.** Nothing in
this model sums balance entries together, and nothing assumes any particular
pair of categories (e.g. "available" and "total") is present at all.

This is not a hypothetical caution — real providers make it necessary:

- **Teller**'s balance object carries `ledger` and `available` as two
  independently nullable string fields, documented to guarantee only that *at
  least one* of the two is present on any given response — never both, never
  neither guaranteed to be non-null.
- **Berlin Group**'s PSD2 XS2A `balanceType` enumeration alone lists a dozen
  non-overlapping category names (`closingBooked`, `closingAvailable`,
  `expected`, `openingBooked`, `openingAvailable`, `previouslyClosingBooked`,
  `information`, `interimAvailable`, `interimBooked`, `forwardAvailable`,
  `nonInvoiced`, `authorized`), and no bank exposes all of them, or the same
  subset as its neighbor.
- **Bitcoin** wallets routinely expose `confirmed` and `unconfirmed` as
  distinct, non-summable-by-default balances — treating a mempool balance as
  interchangeable with a confirmed one is a real category of user-facing bug,
  not a theoretical one.

A balances list is the only shape that survives contact with all three without
either inventing categories a provider never reported or silently dropping
ones it did.

### A host-authored marker is not a balance the host read

A host that keeps a balance history — this one appends every line it reads and
overwrites nothing — has a problem the wire itself does not. A figure it did
**not** read this time stays on top of its category's history, still carrying
the staleness of the last read that *did* succeed, and goes on rendering as
`Live` long after the reads stopped working. So the host records a **marker**
for every category it could not refresh: when a `balances.read` fails
outright, and when a reply comes back without naming a category that resource
has reported before. The
marker withholds the figure and downgrades the freshness; the last known
amount is untouched, one row further down.

A marker is a host-authored row in an adapter-authored stream, which is the
situation §8.4 confronts for retractions. There the answer is a separate
table, so there is **no provider field to fabricate**. A balance stream has no
second table to move to, so the same principle is stated on the row instead:

> **A host-authored balance marker carries no field it did not copy from the
> adapter row it marks.** `category`, `canonical_hint`, `provider_id`,
> `surface`, `observed_at` and `effective_at` are copied forward verbatim from
> the most recent stored line for that `(resource_id, category)` — never
> re-derived, never defaulted, never blanked. The host authors only the four
> that were already its own: `amount` is **null**, because nothing was read
> (and null is UNKNOWN, never zero); `received_at` is the host's own receipt
> stamp (§1); `staleness` is `Unavailable`; and `completeness` is `Unknown`,
> the explicit no-claim variant — carrying a `Complete` forward onto a read
> that never happened would be precisely the fabrication this rule exists to
> stop.

**A marker must also be distinguishable, and that is not a nicety.** A host
stores the §6 `outcome` of the read each balance line came from. A marker's
`outcome` is the outcome that occasioned it — the resource's own outcome, or
`no_observation` when the reply named the resource and left this category out,
or the failure when the call itself did not return — carried under an
**`unread:` prefix**, and no adapter-derived row's outcome ever begins that
way. Without the prefix the marker is byte-identical to an adapter row
reporting `unavailable` with no amount, and those are two different claims:
"the provider could not tell me" is evidence about the provider, while "I did
not read this" is a statement about the host. A consumer that cannot tell them
apart cannot report either one honestly.

A category with no stored line is left alone. There is no figure there to
protect from looking falsely current, and writing a marker for it would be the
host asserting the category exists on the strength of nothing.

## 3. History observations

**Adapters emit observations, not revisions.** A history observation is:

    { resource_id: String,     // which resource this observation belongs to
      local_id, provider_id?, supersedes_provider_id?,
      state: active | tombstoned, tombstone_reason?,
      surface,
      posting: pending | posted | unknown,
      amount, fees?,
      raw_sign: provider_positive | provider_negative,
      description,             // PLAIN TEXT — see the rule below
      provider_extra: {name: value},
      provenance }

`description` is plain text, and "plain text" is exactly this rule:

> A `description` MUST NOT contain any byte in `U+0000`–`U+001F` other than
> horizontal tab (`U+0009`), MUST NOT contain `U+007F`, and MUST NOT contain
> `<` (`U+003C`) or `>` (`U+003E`). Every other character is permitted. An
> adapter that sends one of those bytes gets `invalid_request`.

Both sides run the same check, byte by byte, and get the same answer.

**Why this rule and not "no markup".** An earlier version of this document
said an adapter sending "HTML, Markdown, or any other markup convention" gets
`invalid_request`. That is not implementable. There is no decidable test for
"is this Markdown" — the dialect has no closed grammar, and its markers are
characters that appear constantly in real provider data: `PAYPAL *STEAM`,
`SQ *COFFEE #4471`, `***ATM FEE`, `A_B_CORP`, `[ATM] 24H`. Rejecting those
would reject legitimate transactions; rejecting only some of them would be a
rule no second implementation could reproduce, which is the one thing a wire
spec cannot afford. `<` and `>` are different in kind: they are the two bytes
every tag-based dialect needs, and they carry no meaning of their own inside
a payee name, so banning them is both exact and cheap.

**The obligation the old wording was reaching for lands on the consumer.**
`description` is provider-authored text and is rendered **as text**: never as
HTML, never as a Markdown source string, never interpolated into a template
that interprets either. A consumer that renders it as markup is the bug; the
`<`/`>` ban is a second line of defence, not the first.

### The host assigns `revision`

**`revision: u64` is assigned by the host, by arrival order, per
`(adapter_id, local_id)`.** It does not appear in what an adapter emits.

Why this is a requirement and not a convenience: a restarted or stateless
adapter — which describes most real adapters, and the reference Bitcoin
adapter by deliberate design (`adr/0004-bitcoin-adapter.md` decision 7) — has
no durable memory of how many times it has previously emitted an observation
for a given `local_id`. It cannot know, correctly, that the observation it is
about to emit is "revision 3."
Requiring it to know that would demand a capability no real adapter has:
either persistent state the adapter must maintain and keep consistent with the
host forever, or a round-trip query to the host before every emission to ask
"what revision am I on." The host, by contrast, already sees every observation
in arrival order on its side of the wire, so it is the one place a
monotonically-assigned revision number can be assigned without asking anyone
else first.

### Fold total order

When observations for one `local_id` arrive from more than one `surface` —
the case that matters is two surfaces reporting the same underlying event in
the same page — the fold is fully defined by this total order, ascending:

    (received_at, surface, arrival_index)

`surface` is compared **bytewise** (not locale-aware, not case-folded). This
order is defined precisely so that any two conforming implementations, given
the same set of observations in any interleaving, produce the identical fold
result — see `spec/wire.md`'s conformance obligations and the proptest
fold-order property in the frozen contract.

### `local_id` is a pure function, and is versioned

**`local_id` must be a documented pure function of provider data**, and that
function is named and versioned in the hello handshake as
`local_id_derivation` (`spec/wire.md` §4). It must be deterministic across
process restarts and across process invocations — the same provider data
always yields the same `local_id`, with nothing random, time-based, or
process-local (a UUID generated fresh each run fails this outright).

An upgrade that changes the derivation forks the observation chain for every
`local_id` it touches — the new function produces different ids for records
the old function had already established. This is unavoidable: any
purity-preserving improvement to the derivation is still a different function.
What the version makes possible is **detecting** the fork rather than suffering
it silently: a host that sees `local_id_derivation` change between runs of the
same adapter knows the ids it is about to receive are not comparable to the
ones already on file, and can react (flag the chain, warn the user,
re-anchor) instead of quietly treating two different `local_id`s for the same
real-world record as two different records, which would double-count history.

### `local_id` is namespaced by `adapter_id`

**A `local_id` means nothing outside the adapter that derived it. Every
per-record structure the host keeps — the observation chain, the revision
counter, the live set, and every lookup into them — is keyed by
`(adapter_id, local_id)`, never by `local_id` alone.**

This follows from the derivation itself: `local_id` is a pure function of
*one* provider's data, chosen and versioned by *one* adapter
(`local_id_derivation`, above). Nothing coordinates those functions across
adapters, so two adapters emitting `"tx-1"`, or `"2026-09-06:42.00:acme"`,
for two unrelated real-world records is expected, not pathological — exactly
as two adapters are free to both use `"main"` as a `resource_id`
(`spec/wire.md` §10).

A host that keys on `local_id` alone silently merges those two records into
one chain. The consequences are not cosmetic: the later adapter's amount
overwrites the earlier one's in the live set, and a tombstone emitted by one
adapter deletes the other adapter's transaction. Both are silent — the fold
produces a well-formed, entirely wrong answer, with nothing on the wire to
signal it.

**Dedup by `provider_id` alone is forbidden.** `provider_id` is a *field on*
an observation, carried through for evidence and cross-reference — it is not
the deduplication key, and is not present on every observation to begin with
(it is optional; a pending-only surface may have nothing the provider itself
calls an id yet). The live set is always the fold over observations keyed by
`local_id`, never a naive unique-by-`provider_id` pass.

### One `local_id`, one `resource_id` at a time

**An adapter MUST NOT report the same `local_id` under two `resource_id`s at
the same time.** This is the rule above seen from the adapter's end. Because a
record is keyed `(adapter_id, local_id)` and `resource_id`s are free to
collide, the key carries no resource in it — so a `local_id` that turns up
under a second `resource_id` can only mean the record **moved**, and the host
will treat it as exactly that: the move is a changed fact, so it appends a
revision, the record is live under the new resource, and the old resource's
listing no longer contains it. Nothing is retracted there — the fold's head for
that key now names the other resource, so the old resource's next sweep does
not find it in the live set it diffs against (§8) and has nothing to retract.

The host can tell a move from a re-report only because the two differ, so
`resource_id` is one of the fields the host's content hash discriminates on.
Leave it out and the second resource's observation is deduplicated against the
first's head: nothing is stored under the new resource, the head keeps the old
resource's attribution, and the old resource's next sweep retracts a record the
adapter reported in that very refresh — with the outcome decided by which
resource happened to be swept first.

**The counterexample is real, and the escape hatch is the `local_id` itself.**
A transfer between two of the user's own wallets is genuinely visible from
both, and the provider will hand the adapter the same id for it twice. Two
resources reporting one real-world event are two records in this model, not
one, so the adapter must derive **distinct `local_id`s** for them — by taking
the resource id as an input to the derivation alongside the provider data. The
reference Bitcoin adapter already does this: `local_id = "<resource_id>:<txid>"`
(`adapters/bitcoin/src/map.rs`, `adr/0004-bitcoin-adapter.md` decision 2). This
rule makes that convention normative for every adapter rather than a local
habit of one. It costs the derivation nothing: a resource id is ordinary
adapter-scoped input, and the function stays the deterministic pure function
this section requires.

**An adapter that ignores this produces a well-defined, useless history.** One
`local_id` reported under two resources on every refresh appends one revision
per sweep for ever — a chain that grows without a single fact changing — and
the record renders under whichever resource swept last. The host cannot do
better: "moved" and "reported twice" are the same bytes, and choosing between
them would mean inventing evidence. This is the adapter's bug, and it is the
adapter's to fix.

## 4. One model, two domains: pending→posted and reorg

The same observation shape and the same fold expresses both a bank's
pending-to-posted transition and a Bitcoin reorg, side by side:

| Host revision | Bank (`L1`) | Bitcoin (`L2`) |
|---|---|---|
| 1 | `pend_abc` / pending / `42.00` / active | `txid` / pending / active |
| 2 | `post_xyz` / supersedes=`pend_abc` / posted / `42.37` / active | `txid` / **tombstoned** / `reorged_out` |
| 3 | — | `txid` / posted / active (re-mined) |

`L1` and `L2` here are `local_id` values (kept short for the table). Note
revision 3: the Bitcoin transaction, having been tombstoned as `reorged_out`
in revision 2, reappears as `active` in revision 3 once it is re-mined.

**Tombstone is not terminal.** A reorged-out transaction can be re-mined — a
transaction the network dropped from the best chain is not the same claim as
"this transaction will never exist," and a model that treated `tombstoned` as
a permanent dead end would have no way to represent the re-mine except as a
brand-new, unrelated record, severing its history from the pending state that
preceded it. **This is exactly why the chain is append-only**: nothing is ever
deleted or overwritten in place, a tombstoned state is just another
observation in the chain like any other, and a later `active` observation for
the same `local_id` is not a contradiction — it is the next entry.

## 5. Pagination and resumption

    PageRequest = Cursor { cursor } | Window { resource_id, asset?, start, end }   // half-open

A read reply reports how resumable it is:

    cursor_resumable: exact | batch_restart | none
    next: Option<PageRequest>
    window_capped_to?
    page_size_reduced_to?

Three distinct resumption families, because real providers do not agree on
what a cursor durably promises:

- **`exact`** — the durable cursor advances *per page*. This is Bitcoin's
  model: a block-height cursor is exact because block height only moves
  forward and every page boundary is a safe place to persist "resume from
  here."
- **`batch_restart`** — the durable resume point is the batch's *start*, and
  in this version of the protocol it never advances. This is Plaid's model:
  an interrupted batch resumes from its start, not from wherever it stopped,
  because the provider does not promise that an intermediate cursor value
  remains valid or meaningful across a resume. When the batch drains
  (`next: null`) there is nothing to advance *to*: a drained page's `next` is
  null by definition, and `PageRequest` has no separate slot for "the point a
  future batch should start from." So a `batch_restart` resource has no
  drained/terminal resume state — asking it for a next page again resends the
  same start. Persisting a post-batch cursor is deliberately deferred until
  there is a real paginating provider to define what it means
  (`constitution/FOUNDING_PLAN.md` §10); until then this document, the code
  in `core/host/src/paging.rs`, and its tests all say this one thing.
- **`none`** — there is no durable cursor at all. A caller resumes by
  re-issuing a `Window { resource_id, asset?, start, end }` request instead,
  identifying the gap by its boundaries rather than by an opaque token the
  provider never promised to honor.

**No offset / `nextOffset`.** FDX itself deprecates offset-based pagination
(see `spec/fdx-6.4-mapping.md`), and this protocol does not adopt it at all —
there is no numeric offset anywhere in `PageRequest`.

A caller resuming after a **mid-page failure** does exactly what
`cursor_resumable` says to: for `exact`, resume from the last page's returned
`next` cursor and expect no event strictly before it to be re-emitted; for
`batch_restart`, resume from the start of the whole batch (the intermediate
`next` value the failed page returned is not trusted for anything but
progress display); for `none`, reissue the same `Window` and treat the reply
as authoritative for that window, re-deduplicating against `local_id` as
described in §3 rather than assuming the provider skips what it already sent.

## 6. Partial success and oversized observations

Every read reply has the shape:

    { "observations": [...], "statuses": [...] }

**Every requested `resource_id` appears in `statuses` exactly once** — never
zero times, never more than once, regardless of whether that resource
produced any observations. A status entry is:

    { resource_id, outcome, degraded?, provider_detail?, page?,
      credential_expires_at?, strong_auth_expires_at?, history_start? }

`outcome` names exactly one of:

    fetched { page_empty: bool }, not_fetched, stale { as_of },
    rate_limited { retry_after_ms }, unavailable, reauth_required, revoked,
    gone, sca_required

**`outcome` carries the freshness fact and nothing else.** It is what §1's
staleness table reads, so anything that overwrites it changes how every
observation from that resource is stamped. The oversized-observation
degrade below is not a freshness fact — a resource can be serving cached
data *and* have dropped one record for size — so it rides in its own
optional field:

    degraded?: [ { local_id?, bytes } ]

**`degraded` is a list, one entry per dropped record.** An absent list and
an empty one say the same thing — nothing was dropped — so the field is
omitted when empty rather than sent as `[]`. Whichever side did the omitting
(the adapter, or the host enforcing the cap at decode) **appends** an entry;
no side ever *sets* the field, and it never replaces `outcome`: a resource
that answers `stale { as_of }` and drops an oversized record reports both,
on one entry, and its surviving observations are still stamped `Cached`.

**Append, never set — nothing a host writes may weaken what an adapter
said.** A single-valued `degraded` loses every drop but the last: two
oversized records on one page leave the first an unexplained absence, and an
unexplained absence is retracted (§8). Worse, the two kinds of entry are not
interchangeable — an **anonymous** entry disqualifies a sweep (§8.1
condition 5) while a **named** one merely exempts one id — so a
host-authored named entry overwriting an adapter's anonymous one converts a
signal that blocks a retraction into one that permits it. Appending is what
lets both facts survive on one entry.

**Oversized-observation handling** is a two-step degrade, never a hard
failure of the page:

1. If a single observation's serialized size exceeds `MAX_OBSERVATION_BYTES`
   (65,536 bytes — `spec/wire.md` §1), the adapter truncates its
   `provider_extra` to `{"_truncated": true, "_original_bytes": N}` and sets
   `completeness: "partial"` on that observation's provenance. The rest of the
   observation — `local_id`, `amount`, `posting`, `description`, and so on —
   is unaffected; only `provider_extra` is the truncation target, because it
   is the one field with no bounded shape.
2. If the observation is **still** too large after truncating
   `provider_extra` (an oversized `description`, for instance, is not fixed
   by step 1), the adapter **omits that observation entirely**, appends a
   `{ local_id?, bytes }` entry to that resource's status entry's `degraded`
   list — leaving the entry's `outcome` exactly as it would have been
   otherwise — and **continues the page**: it keeps emitting every other
   observation that fits.

**The host enforces the cap too, at decode.** Steps 1 and 2 are the
adapter's obligations, and an adapter that skips them is non-conforming — but
"the adapter promised" is not enforcement. A host MUST measure each decoded
observation and, for any that still exceeds `MAX_OBSERVATION_BYTES`, perform
step 2 itself: omit that observation, append a `{ local_id?, bytes }` entry
to its resource's status entry's `degraded` list, and continue the page. The
host does not attempt step 1 (truncating `provider_extra` on the adapter's
behalf) — that would hand a caller a record the adapter never emitted,
silently altered.

Because **every requested `resource_id` appears in `statuses` exactly once**,
the host appends to that resource's existing entry's `degraded` list rather
than adding a second status entry. The two rules are separate and both hold:
one status entry per resource, and on it as many `degraded` entries as
records were dropped. Everything else on the entry — `outcome`,
`provider_detail`, and `page`, so the resource stays resumable — is left
untouched, and so is every entry already in the list, including any the
adapter wrote. `bytes` is the size the host measured on its own serialization
of the decoded observation, which differs from the adapter's bytes only in
JSON whitespace and key order.

**One observation is exempt from that measurement**: one whose
`provenance.adapter_id` is not the connection's own. §8.1 condition (8)
refuses such an observation outright, whatever its size, and that refusal
outranks this section — it is never measured, never dropped for being
oversized, and never turned into a `degraded` entry. §8.1 carries the
reason.

**Why `degraded` is a field and not an outcome.** An earlier version of this
document spelled the degrade as an outcome, `oversized_observation`, which
forced whoever reported it to overwrite whatever the resource had said
about its own freshness. An adapter that degraded a record on a `stale`
resource therefore erased the `stale { as_of }` — and §1's table then
stamped that resource's perfectly good cached observations `Live`. The two
facts are independent: one describes the records that arrived, the other
describes a record that did not. Keeping them in separate fields is what
lets a single entry state both without either overwriting the other.

The reason this is a two-step degrade rather than a single reject-and-move-on:
**a resource must never be bricked by one large event.** An adapter (or a host
reading it) that aborted the whole page — or worse, the whole resource — over
one oversized record would let a single pathological transaction (say, a
merchant description field a provider let balloon) deny access to every other,
perfectly normal transaction on that same resource forever. Truncating first
preserves as much of the record as fits; omitting only the one record that
still doesn't, while continuing everything else, is what keeps one bad event
from taking an entire page down with it.

## 7. Provider errors are evidence, never identifiers

Provider-side errors ride as `provider_detail { code, message, raw }` attached
to a `status` outcome. This is **evidence for a human or a log, never an
identifier** the host branches on — the host's own `status` outcome vocabulary
(§6) is what code is written against; `provider_detail` explains *why* in the
provider's own words, verbatim, for debugging and audit, and nothing in this
protocol parses or matches against `provider_detail.code` to make a decision.

`status.read` additionally reports two independent clocks —
`credential_expires_at?` and `strong_auth_expires_at?` — because a bearer
credential's expiry and a strong-customer-authentication (SCA) session's
expiry are not the same event on most providers that have both, and
collapsing them into one field would make the host unable to tell "reconnect
this credential" from "re-run this authentication step" apart. It also
reports `history_start?`, since a provider may only ever have promised history
back to a given point, distinct from any pagination cursor.

## 8. When a host may derive absence

Every section above this one describes something an adapter says. This one
describes something no adapter ever says, and that a host must therefore work
out for itself: that a record the provider used to report is **gone**.

**Absence is not expressible on this wire.** An adapter reports what *is*.
There is no frame meaning "and nothing else exists," no per-resource manifest,
and no way for the host to tell an adapter what it has already seen — the host
is always the requester (`spec/wire.md` §1), a request carries no prior state,
and a reply carries observations and statuses and nothing that quantifies over
their complement.

A tombstone (§4) is not a counterexample. A `tombstoned` observation is an
adapter's *positive* claim about one named `local_id` it went and checked. An
adapter that cannot make that claim — because it keeps no memory of what it
reported last time, which is what `adr/0004-bitcoin-adapter.md` decision 7
records — says nothing at all about the record, and "said nothing" is
indistinguishable on the wire from "never existed."

So a host that wants to know a record disappeared has to **derive** it, from
the shape of a read rather than from anything carried inside it. The rest of
this section is the rule for that derivation. It binds any host, not only this
project's. It is written this tightly because the failure mode is not an error
message: deriving absence wrongly means telling a user that their money's
history is missing.

### 8.1 A complete sweep: eight conditions

A **sweep** is one `history.read` for one resource, pursued to its end. A host
MUST NOT derive absence from anything else. A sweep is **complete** only if all
eight of these hold:

| # | Condition | Why it is not optional |
|---|---|---|
| 1 | It began at `page: None` | A read resumed from a stored cursor has, by construction, not looked at what lies behind that cursor. It cannot speak for it. |
| 2 | Every page came from **one adapter process** and **one** host-side crawl | A sweep stitched across a restart, or across two crawl attempts, is two partial views of two different moments — never one view of one. |
| 3 | It drained: the final page returned `next: null` | An undrained read has, by its own report, more to say. |
| 4 | Every page's status entry for that resource carried outcome `fetched` | `fetched { page_empty }` (either value) is the only outcome that claims the page was actually read. Every other outcome in §6 — `stale`, `not_fetched`, `rate_limited`, `unavailable`, `reauth_required`, `revoked`, `gone`, `sca_required` — says something else. |
| 5 | No page carried an **anonymous** `degraded` entry for that resource | See below; a `degraded` entry that names a `local_id` is a different fact and does not disqualify. `degraded` is a list (§6), and it is judged entry by entry: one anonymous entry fails this condition however many named entries sit beside it. |
| 6 | Every page reported `cursor_resumable: exact` | Derived from §5: `batch_restart`'s intermediate cursor is explicitly not trusted across a resume and `none` has no durable cursor at all, so neither family supports the claim "this read covered the whole span between its start and its drain." A resource in either family therefore never produces a complete sweep, and never retracts. |
| 7 | It ran over **one connection**, whose hello `local_id_derivation` (`spec/wire.md` §4) the host recorded against that crawl | The ids a sweep is compared against are only comparable to the ids it emitted if one derivation produced both. |
| 8 | Every observation's `provenance.adapter_id` was the connection's own | §3 keys every per-record structure by `(adapter_id, local_id)`, and a host keys the rows it is about to compare against by the adapter it is connected to. An observation naming another adapter means those two keyings have come apart, and the host is holding two notions of one chain. **A host cannot conclude an absence from a set it cannot key.** |

**Failing any one of the eight makes the sweep PARTIAL.** A partial sweep still
persists every observation it read — those are evidence, and evidence is never
discarded for being incomplete — and **retracts nothing**. There is no partial
retraction, and no threshold that turns a partial sweep into a whole one.

**Condition (8) is the single exception, and it is one observation wide.** An
observation naming another adapter is refused rather than stored: there is
nowhere to put it. Under this connection's `adapter_id` the host would be
recording, as fact, a provenance the record itself denies; under the
`adapter_id` the record names, one adapter would be appending to another's
history, which is the reason §3 keys ids per-adapter at all. There is no third
place. The rest of the page persists exactly as it would under any other
disqualifier — a refused observation is not a poisoned page. A refused
observation is also **not** counted as one this adapter reported: it can never
suppress a retraction, or a host would learn to hide a disappearance by
mislabelling its provenance.

**Condition (8) outranks §6.** An observation whose `provenance.adapter_id` is
not the connection's own is refused **whatever its size**. It is never
measured against `MAX_OBSERVATION_BYTES`, never dropped for being oversized,
and never turned into a `degraded` entry — the foreign provenance is decided
first, and nothing about the record's size can change that answer.

The order matters because the two dispositions point opposite ways. A refusal
**blocks** a retraction: the sweep is partial and nothing retracts. A named
`degraded` entry **exempts one id and permits the sweep to complete**: every
other absent record still retracts. So a host that measured first and filed an
oversized foreign observation as a degrade would convert the strongest signal
in this gate into a permission — one record's provenance breaking the keying
would license the retraction of everything else absent from that read, instead
of blocking it. That was a live bug, and this ordering is the fix.

**A `degraded` entry naming a `local_id` exempts that id and does not
disqualify the sweep.** §6's `degraded` list carries one
`{ local_id?, bytes }` entry per record omitted for size. When an entry names
the id, the host knows exactly which record did not arrive and why: it exempts
that `(adapter_id, local_id)` from retraction on this sweep and treats the
rest as complete. When `local_id` is absent, the host knows only that
*something* is missing and cannot say what, and condition 5 fails.

**Every entry is judged on its own, and the disqualifier wins.** A page whose
`degraded` list holds one anonymous entry and nine named ones fails condition
5: the nine tell the host which nine records to exempt, and the one still
leaves an unnamed record missing. A disqualifier cannot be diluted by adding
named entries beside it — which is exactly what a single-valued `degraded`
allowed, by letting a later named entry overwrite an earlier anonymous one
(§6).

Why that split is load-bearing rather than a nicety: the alternative — any
degrade disqualifies — hands one oversized record a permanent veto. §6's
degrade is deterministic per observation: it depends on `MAX_OBSERVATION_BYTES`
and the observation, so the same record is omitted on every read of it. A
resource holding one such record would never produce a complete sweep again,
and retraction would be dead for that resource **forever**. Where record size
is provider-influenced — a `description` or a `provider_extra` blob a third
party can grow — that veto is **third-party-triggerable**: anyone who can put
bytes into your history can switch off your host's ability to ever notice a
disappearance. Naming the id costs a field and turns a permanent blackout into
one exempt record.

### 8.2 Three exemptions, none of them a threshold

A complete sweep still does not retract everything absent from it. Three
exemptions apply. **None of them is a percentage.** A rule of the form "refuse
if more than N% would go" is a number with nothing behind it; each of these has
a reason instead.

**`history_start`.** A host refreshing a resource already calls `status.read`,
which reports `history_start?` per resource (§7). If that resource's status
entry carries `history_start`, every live head whose
`effective_at ?? observed_at` **precedes** it is exempt from this sweep's
retraction. This is not a threshold — it is `history_start` doing the exact job
it exists for: bounding what an absence is allowed to be evidence of. The
provider has said how far back it answers at all, so absence before that point
is evidence of nothing. Without the exemption, a provider that prunes its
history — or an Esplora deployment re-pointed at a differently-pruned one —
retracts a user's entire early history, and **nothing later corrects it**: the
next sweep is just as complete, and just as empty back there.

**That comparison is between instants, never between strings.** Both
timestamps are `Rfc3339` (§1), which admits fractional seconds and numeric
offsets, so the two spellings of one moment are not the same text:
`"…T00:00:00Z"` sorts *after* `"…T00:00:00.500Z"` as text and *before* it as a
time, and `2026-01-01T01:00:00+02:00` and `2026-01-01T00:00:00Z` are one
instant written two ways. Both cases are reachable on the wire, and both put a
record that lies inside the provider's window on the wrong side of the bound
and retract it. A host MUST parse both values and order the instants.

**A timestamp that cannot be ordered exempts rather than retracts.** If either
value fails to parse, the head is exempt. The two errors are not symmetric: a
wrong exemption leaves a stale row that the next sweep can still retract, and
a wrong retraction destroys a record of the user's money. Where the host
cannot tell, it keeps the record.

An exempted sweep still records a `history_start_exempt` discrepancy. This is
the only one of the three exemptions that otherwise leaves no trace anywhere —
the other two visibly retract nothing where a retraction was expected, and this
one silently retracts *some* of what was absent — so without the row a user has
no way to learn that a bound was applied at all.

**A changed vantage.** If the sweep's `provider_id` (§1) differs from the last
`provider_id` recorded for that resource, the sweep **retracts nothing**,
records a `vantage_changed` discrepancy, and stores the new value. A different
vantage is a different question: what one deployment can see now is not
evidence about what another one held. **The rule terminates** — the next sweep
from that same vantage finds `provider_id` unchanged and retracts normally. It
exempts one sweep, not the resource.

**An empty sweep against a non-empty live set.** A complete sweep carrying zero
observations for a resource whose live set is not empty **retracts nothing**,
records an `empty_sweep` discrepancy, and reports it to the user. Retracting
100% of a resource requires an explicit, per-invocation confirmation from the
user (in this project's CLI, `refresh --confirm-empty`). **100% is the one
constant that needs no justification.** Every fraction below it is a threshold
in disguise and has to be argued for; "everything you had is gone" is the one
claim worth stopping to ask about no matter what any percentage rule would have
concluded.

### 8.3 Three reasons, in this order

A retraction records **why** the record is absent. The reason is chosen by the
first of these tests that matches, in this order:

| Order | Test | Reason |
|---|---|---|
| 1 | The head's recorded derivation ≠ this sweep's derivation | `derivation_changed` |
| 2 | The head's recorded fingerprint ≠ this sweep's fingerprint | `resource_definition_changed` |
| 3 | Neither | `absent_from_complete_sweep` |

A resource's **fingerprint** is the host's record of the resource *definition*
the observations were read under — for a watch-only Bitcoin wallet, its address
set. How it is computed is a host matter (this project's host digests the
resource descriptor the adapter itself reports, so a definition change the
adapter does not describe is invisible to it); what is normative here is only
that a sweep run under a fingerprint different from a record's own retires that
record under row 2 rather than row 3.

**A software change must never retire records under a reason that blames the
provider.** A derivation bump (§3 — a new `local_id_derivation` is a different
pure function and produces different ids for the same real-world records) and a
change to the resource definition are both **migrations**, recorded in the
sweep's own transaction, not provider absences. Old-derivation ids can never be
re-emitted by the new function, so they must retire — but under a reason that
names the software change. Records that still exist come back on that same
sweep under their new ids with a new revision, and are never retracted at all.
Each of the first two reasons also writes one discrepancy row per sweep, so a
migration is one visible event rather than a silent mass retirement.

**A record a sweep re-observes is re-stamped under that sweep's rules.** A host
that recognises an unchanged record and appends nothing to its chain MUST still
refresh the derivation *and* the fingerprint it holds against that chain's
head, not only whatever "last seen" marker it keeps. This is a correctness
rule, not a bookkeeping nicety. A record that **survived** an address change
was genuinely re-emitted under the new address set; leaving the old fingerprint
on it means its ordinary disappearance three sweeps later matches row 2, and
reports `resource_definition_changed` — a software change from three sweeps ago
blamed for a record the provider simply stopped reporting. That is the exact
failure this ordering exists to prevent, arriving through the back door.

**Condition 7 gates on the per-sweep hello value only.** The derivation
compared in row 1 is the one *this sweep's* connection announced in hello and
the host recorded on that crawl. Two other readings of the same sentence are
available and both are catastrophic. Comparing the sweep against every
derivation present in stored history deadlocks retraction forever: a chain that
has ever mixed derivations never qualifies again. Comparing it against the
adapter's current stored derivation buries an entire pre-upgrade history under
`absent_from_complete_sweep` — a provider-absence reason for a change the
provider had no part in.

### 8.4 A retraction is not an observation

**A host cannot author an Observation.** It has no amount, no surface, no
posting and no provider provenance for a record it did not read: §1's
provenance is an adapter's account of a provider, and a host filling those
fields in on its own behalf is fabricating provider evidence — the one thing
this document is arranged throughout to prevent. A derived absence therefore
never enters the observation chain. It is a separate, append-only record:

    retraction { adapter_id, local_id, revision, reason, crawl_id, retracted_at }

with **no provider field to fabricate**, where `revision` is the **highest
revision in the chain** it retracts and `crawl_id` names the sweep that derived
it.

Liveness is then one rule over both tables:

> A record is live iff the fold's live set (§3) contains it **and** its
> chain's **highest** `revision` exceeds every retraction `revision` recorded
> for that `(adapter_id, local_id)`.

**The chain's highest revision, not the head's — those are two different
questions.** `revision` is **arrival order**: the host stamps it as
observations come off the wire (§3). The chain *head* is not the newest
arrival; it is the winner of §3's fold total order,
`(received_at, surface, arrival_index)`. Both orders are correct for what they
answer. The head answers "what is true about this record now," which is a
question about the provider's account of it. Liveness answers "has this host
learned anything about this key since the retraction at revision N," which is a
question about arrival — so it has to be asked in arrival order.

Compare a retraction's revision against the *head's* and the two orders are
mixed, which can bury a record for ever. Both ways they disagree are ordinary,
not exotic. Two surfaces reporting the same event in one page already sort by
`surface` ahead of arrival index, so the later arrival need not be the head. And
a wall clock that steps **backwards** between refreshes — an NTP correction, a
VM snapshot restore, a machine resuming with a bad RTC — stamps every
re-emission with a `received_at` below the buried head's, so every honest sweep
sorts *under* that head and the head's revision never grows past the
retraction. The record stays retracted for ever while its chain grows one row
per refresh, because dedup is off for a record the host believes buried (below).
Reading the chain's highest revision has neither failure: a revision, once
assigned, is never outranked by an older arrival.

**Head *selection* is unchanged.** §3's total order still decides which
observation the fold presents as the record's current state and which one the
user sees, and it is still that head whose derivation and fingerprint §8.3's
re-stamp refreshes. Only the liveness comparison moved.

**This is why revival needs no special case.** A later sweep that re-emits the
record appends revision N+1, which exceeds the retraction's N, and the record
is live again by the same rule that hid it — no un-retraction, no deletion, no
second mechanism to keep consistent with the first. It is the same reason §4
makes the chain append-only: a restated fact is a legal entry, not a
contradiction.

**Which is why a host must not deduplicate a record its own retraction has
buried.** Recognising a re-emitted record as byte-identical to its chain head
and appending nothing is right in the ordinary case and wrong in exactly this
one: a reorg that re-mines a transaction re-emits it **byte-identically**, so a
dedup firing there would append no revision, leave the chain's highest
revision at N, and keep the record retracted forever — no matter how many
honest sweeps carried it. **"Live again" is a changed fact even when the
content is not**, and a revision is the only way this model has to express a
changed fact. So while a key's **highest** chain revision is at or below a
retraction revision for that key, every re-observation of it appends — the same
revision the liveness rule compares, asked the same way, so dedup and liveness
can never disagree about whether a record is buried.

**Nothing is deleted.** A retraction hides a record from the live set and from
everything derived from it; it removes nothing. The chain, the retraction rows,
and the crawl that produced them all survive, which is what makes a *wrong*
retraction correctable rather than merely regrettable — and what that does and
does not bound is stated in `adr/0006-host-side-retraction.md`, not softened
here.
