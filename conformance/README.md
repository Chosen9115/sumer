# `sumer-conformance`

The executable conformance suite for the Sumer wire protocol
(`spec/wire.md`, `spec/observation.md`). Per the founding plan, this suite
-- not the Rust host implementation -- is the artifact expected to outlive
this milestone: any adapter, in any language, that survives every case
here is a conformant Sumer adapter.

## Running it against your own adapter

```
cargo run -p sumer-conformance -- --adapter "<argv>" --cases conformance/cases
```

`<argv>` is your adapter's full command line as one string, split on
whitespace (no shell quoting or escaping is performed -- if your adapter's
path or arguments need a space, wrap your adapter in a small shell script
and pass that script's path instead). `<dir>` is a directory of `*.json`
fixtures: `adapters/fake/README.md` documents the `script` half (what the
adapter replies), and "The `expect` block" below documents the half this
runner reads.

**Your adapter is never asked for a cursor it did not hand out.** A
resource's first `history.read` carries no `page` field at all -- absent
means "from the start of available history" (`spec/observation.md` §5,
Ruling A8) -- and every page after it echoes back a cursor the previous
reply's `statuses[].page.next` returned. An adapter whose cursors are
opaque tokens it alone mints, and which rejects anything else, is exactly
what the contract permits and passes this suite unmodified.

Examples:

```
# The reference Python adapter:
cargo run -p sumer-conformance -- \
  --adapter "python3 adapters/fake/fake_adapter.py" \
  --cases conformance/cases

# A hypothetical Go adapter, built to a binary:
cargo run -p sumer-conformance -- \
  --adapter "./my-adapter" \
  --cases conformance/cases

# A hypothetical TypeScript adapter:
cargo run -p sumer-conformance -- \
  --adapter "node dist/my-adapter.js" \
  --cases conformance/cases
```

The runner spawns exactly that argv twice per case (more often for the two
cases that need it -- see "What each run does" below), sets
`SUMER_FIXTURE` (and, when relevant, `SUMER_FIXTURE_RUN`) in its
environment, and speaks nothing but the JSON-Lines wire described in
`spec/wire.md` over its stdin/stdout. **It contains nothing specific to the
reference Python adapter** -- swapping `--adapter` for a different
program's argv is the only thing a stranger's adapter needs to do to run
against this same suite.

Exit code `0` means every case passed. Exit code `1` means at least one
assertion failed (printed to stdout, one line per failure, naming the
case, the assertion it violates, and the concrete actual-vs-expected
difference). Exit code `2` means the runner itself was misused (bad flags,
missing `--cases` directory).

## What each run does

Every case except `protocol_violations` is driven by **one crawl**, run
twice (see A9 below). One crawl is one adapter process, one connection,
and these calls in this order:

1. `resources.list`
2. the undeclared-op probe, if the fixture declares one (`expect.probe`)
3. `balances.read` — one batched call naming every discovered resource
4. `history.read`, one resource at a time, paginated to exhaustion
5. `status.read` — one batched call naming every resource
6. any envelope-level rejection the fixture demands
   (`expect.envelope_errors`), followed by a `resources.list` that must
   still succeed

**The crawl awaits serially.** The transcript it is judged from is
*dispatch* order, and reply content is ordered the same way only under
serial dispatch — an adapter declaring `max_in_flight` above `1` may
answer out of order. Every sequence this suite compares is an ordering
claim, so the crawl never dispatches concurrently.

**Your adapter must answer the whole crawl for every resource it lists.**
A resource named by `resources.list` gets a `balances.read`, a paginated
`history.read` and a `status.read`, whether or not the case is *about*
that resource. A fixture that does not want a resource crawled should not
list it.

Two cases need more than one crawl, or none:

- **`interrupted_pagination.json`** spawns your adapter three times per
  cursor family (`exact` and `batch_restart`): once for an uninterrupted
  read, once for a run that gets killed mid-page, and once more, fresh, to
  resume. The resume request is computed by
  [`sumer_host::paging::ResumeState`] — never reimplemented in this
  crate — and the live set spanning the kill is folded by one
  [`sumer_host::fold::Fold`], also never reimplemented here.
- **`protocol_violations.json`** spawns your adapter six times, one per
  entry in its fixture's `script.runs[]`, selected via the
  `SUMER_FIXTURE_RUN` environment variable (a 0-based index, per
  `spec/wire.md` §11). Some of the outcomes it tests are fatal by
  construction (`PreHelloOutput`, `UnknownId`, `DuplicateId` each end the
  adapter process), and one run dispatches two reads concurrently on
  purpose — its hello declares `max_in_flight: 2`, and the point is that
  the host correlates replies by id and not by arrival order. Neither is
  representable as an ordered ledger, so this case keeps its own
  bespoke, order-free assertions.

If you write your own multi-run fixture, the contract is: `SUMER_FIXTURE_RUN`
selects one independent adapter lifetime out of `script.runs[]`, defaulting
to `"0"` when unset, and a runner must iterate every index actually present
rather than only ever using the default.

`conformance_hints.deadline_ms`, when a fixture sets it, is advisory only
— never part of the wire protocol, never sent to or read by the adapter.
It lets a fixture request a shorter per-connection request deadline than
the wire's 30-second default, so a scenario whose whole point is a deadline
*expiring* doesn't cost 30 real seconds per run — a deliberate host-side
timeout, or an adapter caught not exiting at stdin EOF (`spec/wire.md` §7),
neither of which is observable any sooner. A runner that ignores it
entirely is still conformant.

## The `expect` block: what a fixture declares

### `expect.ledger` — the ordered evidence, per resource

    "ledger": {
      "<resource_id>": {
        "balances": [ <entry>, ... ],
        "history":  [ <entry>, ... ]
      }
    }

Each array is the **exact sequence** that resource must emit, in the order
it must emit it, across the whole crawl. `balances` and `history` are two
independent sequences: they come from different ops, a balance line has no
`local_id`, and pooling them would make the order of either unassertable.

**An absent sequence asserts EMPTY.** Leaving `history` (or `balances`, or
a whole resource) off is not opting out of the comparison — it is the claim
that nothing may arrive there, and anything the adapter emits under an
undeclared key is an orphan against a declared length of zero. Absence
meaning "unchecked" would let an adapter invent a whole history unchallenged
and let a fixture drop a check by deleting one key; `large_amounts.json`
spells no `history` at all and the
`large_amounts__history_invented_where_none_declared` mutant is what holds
that.

An `<entry>` is a subset of the observation as it went on the wire — name
only the fields the case is about. Two fields are special:

- **`amount`** is compared with `Amount::cmp_same_asset`, never as a
  string: `"42.50"` and `"42.500"` are equal.
- **`provider_extra`** is compared **exactly**, not as a subset. A subset
  comparison cannot express "and nothing else", and that is the whole
  assertion for A10 step 1 (`{"_truncated": true, "_original_bytes": N}`
  and nothing beside it) and for `fdx_lossless`.

One field is removed before comparison: `provenance.received_at`, a fresh
clock reading on every run that nobody can predict. **`provenance.staleness`
is compared.** It is host-derived rather than adapter-sent, but the host
derives it per resource from that resource's own outcome
(`spec/observation.md` §1's freshness table), so it is a genuine
per-observation claim and a reproducible one — and it is exactly what breaks
when a degrade overwrites a stale outcome
(`oversized_observation.json` declares all three of its surviving
observations `cached`).

An entry may carry **`"omitted": true`**, which means the adapter was
required to drop that record entirely (`spec/observation.md` §6 step 2,
still oversized after truncating `provider_extra`). Such an entry is taken
*out* of the declared sequence and its `local_id` is separately asserted
never to appear anywhere in the execution. It must name a `local_id`;
there is nothing else to check for absence.

**Known limit: an `omitted` entry cannot assert its POSITION.** Because it
is lifted out of the sequence, the sequence closes over the gap. The absence
itself and the relative order of everything around it both survive; "the
sibling arrived *after* the record that had to be dropped" does not.
Positional-absence machinery for one hypothetical case is not worth its
weight, so this is written down rather than built.

Sequence equality replaces the membership list this suite used to compare.
Membership could not see a reordering, could not see one record emitted
twice and another not at all, and — being a loop over the fixture's own
entries — could not see anything the fixture had not named. Equality sees
all three, in both directions, at once.

### `expect.live` — the live set, declared

    "live": { "<resource_id>": ["<local_id>", ...] }

The `local_id`s that must be live once `Fold` settles. **Declared, not
derived**: folding `expect.ledger` to compute this would compare `Fold`
against itself and could never disagree.

### Everything else

| Key | What it names |
|---|---|
| `resources` | Every resource `resources.list` may return, and only those (the count is half the assertion) |
| `statuses` | Per resource, a status entry that must have been observed on some `balances.read`/`history.read` reply. A field spelled `null` means "must be absent or null" |
| `status_read` | The same, against the `status.read` reply alone — the only one that carries `credential_expires_at` / `strong_auth_expires_at` / `history_start` |
| `provenance` | Per resource, fields required on a balance line's `provenance` |
| `grammar_check` | `"<category>_digit_count"` / `"<category>_scale"`: digit-level evidence beyond `cmp_same_asset` |
| `probe` | `{op, params, expected_err: {code, detail: {op}}}` — an op no capability declares, which must come back as an envelope `err` |
| `must_still_succeed` | Ops that must have succeeded after the probe, spelled `"resources.list"` / `"balances.read(<rid>)"` / `"history.read(<rid>)"` / `"status.read(<rid>)"` |
| `envelope_errors` | `"<rid>_balances_read"` / `"<rid>_history_read"` -> `{code}`: that call must be rejected at the envelope level, and the connection must survive it |
| `fdx_field_map` | Prose naming `provider_extra.<key>` landing sites, checked in both directions |
| `assertions`, `notes` | Documentation. Nothing reads them |

**There is no fixture key for how *much* of the ledger must be emitted.**
A run that is deliberately cut short is relaxed to "a contiguous slice of
the declared sequence" by the *runner*, at four call sites in
`case_interrupted_pagination`, because only the runner knows a run was
killed or resumed. A fixture is an adapter-adjacent file; one that could
declare its own leniency would be the thing under test deciding how hard
the test is.

**And that relaxation applies to `history` only.** Interruption is a
pagination event, and `history.read` is the only paginated op:
`balances.read` is one batched, unpaginated call (Contract Amendment 1
Ruling A3), so an interrupted run still owes its whole declared `balances`
sequence. Relaxing every sequence of an interrupted execution let a resumed
run drop its balance line and pass — the empty slice is a contiguous slice
of anything — which is what
`interrupted_pagination__resumed_balance_vanishes` now holds.

## One execution, both views

Most of this suite reads through `sumer_host::AdapterHandle`'s typed
calls, which is the point — your adapter is exercised by a real host. But
a host is also a *remediator*: it omits every observation over
`MAX_OBSERVATION_BYTES` at decode (`spec/observation.md` §6), and it
stamps fields onto what it keeps. Both hide adapter behaviour two
assertions exist to judge.

The crawl therefore runs on a **recorded** connection
(`AdapterHandle::spawn_recorded`). The typed evidence and the raw reply
frames come off the same connection, from the same bytes of the same
process. A10 is measured on the raw frames, for every execution.

An earlier revision did this with a separate "wire pass": a second, fresh
process re-issuing the same reads. An adapter could tell the two apart —
the typed crawl opened with `resources.list`, the wire pass with
`balances.read` — and returning real data to one and empty arrays to the
other passed the suite. There is now no second pass to diverge from.

Why A10 has to be measured on the adapter's own bytes, in both directions:

* An over-cap observation is omitted by the host *before* any decoded
  reply exists, so a suite measuring decoded observations could never see
  the violation: it would certify an adapter that skipped the degrade
  entirely.
* The decoded form is not the wire form — `Observation` serializes absent
  optionals as explicit nulls that `ObservationWire` omits, which charges
  an adapter for bytes it never sent and rejects a legal record sitting
  exactly at the cap.

One thing this cannot see: an oversized observation riding in on a reply
that arrived after its own deadline. The host discards a tombstoned reply
before it reaches any caller, including the recording, and that discard is
legally not a violation (`spec/wire.md` §6). **The aperture is one id, not
one deadline** — a tombstoned slot is never cleared, so every later reply
naming that id is discarded for the rest of the connection's life, however
long after the deadline it arrives. What keeps the hole narrow is that only
a request the host already gave up on is ever tombstoned. It is accepted
rather than fixed, and stated as it actually behaves: an overstated safety
claim would be worse than the hole.

## An execution has an end, and the end is judged

The crawl closes its connection before it judges: the child's stdin is
dropped, the adapter exits, the reader loop runs to the end of the stream
and finalizes the framing there, and the terminal reason is read. A
connection that ended in a protocol violation fails the execution.

**That is not the same as "every byte the adapter wrote was judged", and
this suite does not claim it.** The host's reader loop also returns
normally on a read *error* — the pipe failed, and whatever was buffered may
have been truncated by that failure rather than by the adapter, so it is
not held against the adapter — and the supervisor treats the reader task
finishing as drainage either way. What the close establishes is that the
reader ran to completion and nothing it decoded was a violation.

Without that boundary an execution had no end at all. The host delivers a
valid reply *before* it reports a violation in whatever follows it, so an
adapter could answer the entire measured crawl and then put garbage (or a
second answer to an id already answered) on the wire — killing a connection
nobody was still waiting on — and the case passed. Adding one more probe
read after the crawl would only have moved that hole one reply further out;
`pending_to_posted__garbage_after_the_final_reply` is the mutant that holds
the boundary itself.

**The end of the stream is what is certified, not the end of the process**,
because the two are not the same fact and treating them as one left two
holes a reviewer executed:

- trailing garbage with **no** terminating LF sat in the decoder as an
  in-progress frame, the reader returned at EOF without judging it, and the
  case passed — the one byte between `garbage\n` and `garbage` was the
  whole difference. Framing is now finalized at end of stream
  (`UnterminatedFrame`), and both shapes have a mutant
  (`..._garbage_after_the_final_reply`, `..._unterminated_garbage_after_the_final_reply`);
- a writer the adapter FORKED inherits stdout and outlives it, so awaiting
  the process established nothing about the stream: the host reported an
  ordinary exit and the writer's frames landed after the verdict. A stdout
  still open once the host stops waiting is now `StdoutHeldOpen`
  (`pending_to_posted__writer_outlives_the_adapter`), and it is deliberately
  a different kind from `StdinEofIgnored` — that one names a process still
  running, and an adapter that exited on time must never be failed for it.

**Two limits on `StdoutHeldOpen`, and neither is a tuning detail.**

- **The drain is a fixed one-second observation budget**
  (`process::READER_DRAIN`), not a measurement. What it times is the
  host's reader *task* completing, which is not the fact "no descriptor for
  that pipe is still open" — the two are merely correlated, because the
  task returns at EOF and EOF arrives when the last write end closes. So an
  **honest adapter can be reported `StdoutHeldOpen`** if host scheduling
  delays the drain past the bound: a loaded CI box or a starved runtime is
  enough, there is no retry, and the connection is then reported as having
  broken the protocol. That is a false-attribution risk this suite accepts,
  not a constant someone forgot to tune.
- **The kind cannot identify what holds the stream.** Task completion
  cannot identify descriptor ownership, so an adapter that forked a writer
  and a host whose reader was simply not scheduled in time are
  indistinguishable from here. Read `StdoutHeldOpen` as "nobody saw the end
  of this stream", never as "the adapter held it open".

## A9's requirement: two executions, and the whole history

Every crawl-driven case runs the crawl **twice**, independently, and
compares what the two executions emitted. A `local_id` derived from
anything process-local — a freshly generated UUID, an in-memory counter
that doesn't survive a restart — fails here even though a single run would
have looked perfectly fine.

Both executions are fully judged on their own first. That is what stops an
adapter from making A9 vacuous by emptying both sides: two identical empty
runs agree with each other perfectly, and fail their own `expect.ledger`.

**The request shapes are gated first.** If the two executions asked
different questions, a content difference says nothing about purity, so
the divergence is reported as itself. A request shape is the op plus its
params with cursor bytes replaced by the literal `"<cursor>"` — and *only*
`resources[].page.cursor` is replaced. Window bounds, resource ids and
page counts all survive. A cursor carries no cross-invocation purity
obligation (`spec/observation.md` §5 calls an intermediate
`batch_restart` cursor untrusted); only `local_id` does. Comparing raw
cursor bytes would fail a conforming adapter that mints session-scoped
cursors — Plaid's model, and the reason `batch_restart` exists.

What is then compared is the **association**, over the **whole observation
history**: each `local_id` against every record that carried it, in
arrival order. Two weaker comparisons this deliberately is not, both of
which this suite has made:

- The bare **set of ids** is satisfied by an adapter that hands out the
  same ids on the second launch attached to *different records*: swap two
  observations' ids and the set is identical while every id now names the
  wrong thing.
- The final **live set** ignores every record that was later superseded or
  tombstoned. Mis-derive the `local_id` of the tombstone in a reorg chain,
  or of the pending row a posted one supersedes, and the live set is still
  identical. Purity is a claim about every record the derivation touches.

## Assertions implemented (A1-A11)

Every assertion in the frozen contract's section (g) is implemented with
its stated mitigation, not just its happy-path check:

| # | What it proves | The degenerate implementation it kills |
|---|---|---|
| A1 | Exact-value fidelity (`Amount::cmp_same_asset`, plus digit-count/scale evidence where a fixture asks for it), and the balances list is compared by count as well as by content | An adapter (or host) that silently round-trips an amount through a float, *and* one that appends a balance line — any category, any amount — the provider never reported (nothing else in the model can contradict an invented category, so only the count catches it) |
| A2 | Per-resource **sequence equality** of the emitted history against `expect.ledger`, plus the declared live set after folding | Ignoring `state`/`supersedes` entirely, *and* emitting one record twice and another not at all (identical live set, identical count), *and* emitting the right records in the wrong order (pending after posted, tombstone after re-mine) |
| A3 | `null` (unknown) is never zero | Emitting `null` for every category, or `0` for every unknown one |
| A4 | Error class is never plain text; ops around a failure still succeed | Returning the same error for everything, or killing the connection on any hiccup |
| A5 | Exactly-resumable cursor, bracketed both ways, over the live set folded across the kill and the resume — and each of the three runs judged against a **contiguous slice** of the family's declared sequence | Re-emitting everything (passes only because dedup absorbs it), emitting nothing (fails the live-set half), re-emitting the right `local_id`s carrying different amounts (which a key-set comparison could not see), *and* resuming into records that were never declared at all |
| A6 | The live set floor: non-empty when expected | "Report success" with nothing behind it |
| A7 | Every requested `resource_id` in `statuses`, exactly once; and `status.read`'s own fields — the two independent clocks and `history_start` — compared against values a fixture makes deliberately *differ* | One blanket status for a whole batch, *and* an implementation that reports one clock for both `credential_expires_at` and `strong_auth_expires_at` (which passes whenever a fixture lets the two values coincide), *and* one that renders an unknown clock or `history_start` as a date instead of omitting it (`expect` spells "must not claim to know this" as `null`) |
| A8 | Full history retention, in emission order, as part of A2's sequence | Keeping only the final state |
| A9 | `local_id` purity across two independent, fully judged executions, gated on request-shape equality, compared as record-to-id associations over the full observation history | A freshly generated UUID per run, a derivation that reuses the same ids for different records on the second run, one that mis-identifies only a record that is later superseded or tombstoned (identical live set, different history), *and* an adapter that empties both executions so they agree (each execution still fails its own ledger) |
| A10 | The oversized-observation two-step degrade, five ways at once, driven by genuinely oversized input, measured on the adapter's own frames from **every** execution | Truncating (or dropping) the whole page over one bad record, leaving leaked payload beside the truncation marker (`provider_extra` is compared exactly), reporting the degrade *as* the resource's outcome so its freshness is erased, *and* skipping the degrade entirely and letting the host omit the record for you |
| A11 | Fatal violations kill; a tombstoned reply is discarded and the connection survives; and every crawl is judged up to its **close**, not up to the last reply someone was waiting for | A host that kills the connection on *any* anomaly, *and* an adapter that answers the whole measured crawl and only then breaks the protocol — by refusing to exit when its stdin closes, by leaving trailing bytes no newline ever terminated, or by leaving a forked writer holding stdout after it exits |

## Honest limits of black-box testing

This suite proves what crosses the wire, nothing about what happened
before it got there. Concretely:

- **A1 cannot prove no `f64` was touched internally.** It can only prove
  that whatever value the adapter emitted survived byte-for-byte through
  `Amount::parse` and compares numerically equal (via `cmp_same_asset`) to
  the fixture's expected value. An adapter that rounds internally to a
  `f64` and then gets lucky on a specific test value would still pass; the
  large-magnitude and high-precision fixtures (`large_amounts.json`,
  `provider_json_number.json`) are chosen specifically to make that luck
  as hard as possible (78 significant digits, 27-digit fractions well
  past `f64`'s ~15-17 significant decimal digits), not to make it
  impossible.
- **A1 likewise cannot prove no *decimal context* rounded.** It can only prove the
  emitted digits survived. `provider_json_number.json` makes that as hard
  as possible by asserting a 31-significant-digit negated amount together
  with its exact digit count and scale, which a 28-digit default context
  cannot produce.
- **`fdx_lossless.json` checks the FDX mapping's landing sites, not its
  meaning.** `expect.fdx_field_map` is now read: every
  `provider_extra.<key>` its prose names must have arrived on the wire, and
  every `provider_extra` key that arrived must be named by it, with each
  observation's `provider_extra` compared exactly. What that cannot check is
  whether the prose *describes the right FDX field* -- that a row saying
  `transactions[].status` lands in `provider_extra.fdx_status` is true of
  the schema, not just of this fixture. That reconciliation lives in
  `spec/fdx-6.4-mapping.md` and was done against the real 6.4 schema; the
  suite holds the fixture to the table, not the table to FDX.
- **Some of this is structural; some of it is still discipline, and the
  difference matters.** *Structural* — impossible for a driver to forget —
  is: per-execution sequence equality, the A10 size measurement, status
  coverage, and the resources list. A driver cannot obtain a
  `JudgedExecution` without them having run, because `run_crawl` is the
  only thing that builds one and every one of its exits judges first. That
  last part is held by those exits, not by a type: a sixth `return exec;`
  added tomorrow would compile unjudged. *Discipline* — cross-execution
  claims no type in this crate can force —
  is: the A5 resume bracket, A9's comparison, A4's must-still-succeed
  probes, and A11's violation-kind checks. Historically most of this
  suite's hollow assertions lived exactly there, and a reviewer hunting for
  the next one should look there first.
- **Mutation coverage is not mechanically guaranteed end to end.** Every
  claim in the battery's `COVERAGE` table is backed by a mutant that
  actually kills, and a claim naming a protocol-violation *kind* is bound
  to the mechanism the run established rather than to the `A11` label all
  nine kinds share. Two gaps under that are held by review, not by
  machinery, and `conformance/tests/mutations.rs` states both in full:
  three `COVERS_VIA` rows (`A8`→`A2`, `A3`→`A1`, `A10`→`A2`) are bound to
  a *label*, so nothing checks that the A2 failure a mutant claiming A8
  provoked was about history retention rather than a drifted amount in the
  same comparison; and nothing binds a mutant to the specific field it
  patched, so a patch that does not match its own `why` reads as identical
  if it trips the same ids. Treat the table as "this pair has a killing
  mutant behind it", not as "this pair is proven by machine".
- **`sumer_host::fold::Fold` and `sumer_host::paging::ResumeState` have no
  caller inside the host.** `AdapterHandle` never assigns a revision and
  never computes a resume request — `history_read` hands back observations
  and says the fold is somebody else's step. This suite is their only
  consumer. So "the host assigns `revision: u64` by arrival order"
  (`spec/observation.md` §1) is proven against the standalone `Fold` and
  against a `Fold` this suite drives over real adapter reads, and never
  once end to end through a host that does it on its own behalf. An
  integration built on `AdapterHandle` alone would get no revisions at all
  and nothing here would notice. This closes when the CLI arrives and
  becomes that caller.
- **The framer's fuzz target has never executed anywhere.** `cargo fuzz run
  codec` is wired into `.github/workflows/nightly.yml` only, against
  `main`, and no nightly run has happened yet. Shipping it unrun is
  acceptable; counting it is not. **It earns no evidence credit until it
  has actually run once** — the invariants listed in
  `core/wire/src/codec.rs` (never panics, never allocates past
  `MAX_FRAME_BYTES + 1`, split-invariance) are today held by the unit and
  property tests alone, and the existence of the workflow must not be read
  as coverage.
- This suite never inspects an adapter's source, memory, or process
  internals. It is exactly as strong as its wire evidence and no
  stronger.

## One client, no second one

Every case in this suite -- `unsupported_op.json`'s undeclared-op probe
included -- goes through `sumer_host::AdapterHandle`, so every connection
gets the same spawn (including the environment allowlist), the same
`FrameDecoder` (so `MAX_FRAME_BYTES` and non-UTF-8 rejection are enforced),
and the same id lifecycle. The probe reaches the wire through
`AdapterHandle::call_raw`, which sends an arbitrary op string and returns
the envelope reply verbatim -- an `err` comes back as `Ok(Reply::Err {..})`
rather than a transport failure, which is exactly the distinction that case
is testing.

An earlier revision of this crate carried a second, private JSONL client for
that one probe. It was not a smaller version of `AdapterHandle`, it was a
divergent one: it skipped the environment allowlist and read lines instead
of frames, so two of the guarantees this suite exists to check were simply
absent on that connection. It is gone.
