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
receives the frame, using its own clock. **`observed_at` is adapter-claimed and
is evidence only** — it is the adapter's own account of when the provider
observed the value, carried through unmodified, and is never treated as ground
truth for freshness decisions.

**Staleness is computed by the host from `received_at`, never from
`observed_at`.** Why this split is load-bearing rather than a style choice: if
staleness were computed from an adapter-supplied timestamp, an adapter with a
skewed system clock — or a compromised or simply buggy one that lies about
when it observed something — would silently control whether the host displays
data as `Live` or `Cached`. A skewed clock a few hours fast makes genuinely
stale data look fresh; a skewed clock a few hours slow makes fresh data look
stale; either way the host would be trusting a value it has no way to verify.
By computing staleness only from the host's own receipt time, the worst an
adapter's clock can do is make its `observed_at` evidence less useful — it can
never make the host misrepresent freshness.

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

## 3. History observations

**Adapters emit observations, not revisions.** A history observation is:

    { resource_id: String,     // which resource this observation belongs to
      local_id, provider_id?, supersedes_provider_id?,
      state: active | tombstoned, tombstone_reason?,
      surface,
      posting: pending | posted | unknown,
      amount, fees?,
      raw_sign: provider_positive | provider_negative,
      description,             // PLAIN TEXT — markup is invalid_request
      provider_extra: {name: value},
      provenance }

`description` is plain text. An adapter that sends markup (HTML, Markdown, or
any other markup convention) gets `invalid_request` — this field is displayed
directly, and accepting markup here would make every consumer of the field a
markup sanitizer by necessity.

### The host assigns `revision`

**`revision: u64` is assigned by the host, by arrival order, per
`(adapter_id, local_id)`.** It does not appear in what an adapter emits.

Why this is a requirement and not a convenience: a restarted or stateless
adapter — which describes most real adapters, since this milestone's cursor
and revision stores are in-memory on the host side (see
`constitution/FOUNDING_PLAN.md` §10) — has no durable memory of how many times
it has previously emitted an observation for a given `local_id`. It cannot
know, correctly, that the observation it is about to emit is "revision 3."
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

**Dedup by `provider_id` alone is forbidden.** `provider_id` is a *field on*
an observation, carried through for evidence and cross-reference — it is not
the deduplication key, and is not present on every observation to begin with
(it is optional; a pending-only surface may have nothing the provider itself
calls an id yet). The live set is always the fold over observations keyed by
`local_id`, never a naive unique-by-`provider_id` pass.

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
- **`batch_restart`** — the durable cursor advances only on `next: null`, i.e.
  only once the whole batch is exhausted. This is Plaid's model: an
  interrupted batch resumes from its *start*, not from wherever it stopped,
  because the provider does not promise that an intermediate cursor value
  remains valid or meaningful across a resume.
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
produced any observations. The possible outcomes:

    fetched { page_empty: bool }, not_fetched, stale { as_of },
    rate_limited { retry_after_ms }, unavailable, reauth_required, revoked,
    gone, sca_required, oversized_observation { local_id?, bytes }

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
   by step 1), the adapter **omits that observation entirely**, reports
   `oversized_observation { local_id?, bytes }` in `statuses`, and
   **continues the page** — it keeps emitting every other observation that
   fits.

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
