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
fixtures in the schema documented in `adapters/fake/README.md`.

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

The runner spawns exactly that argv once per case (more than once for the
handful of cases that need it -- see "Multi-run cases" below), sets
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

Most cases drive the standard read crawl -- `resources.list`,
`balances.read`, paginated `history.read`, `status.read` -- against your
adapter and check the results against that fixture's `expect` block. Two
cases need more than one adapter process:

- **`interrupted_pagination.json`** spawns your adapter three times per
  cursor family (`exact` and `batch_restart`): once for an uninterrupted
  read, once for a run that gets killed mid-page, and once more, fresh, to
  resume. The resume request is computed by
  [`sumer_host::paging::ResumeState`] -- never reimplemented in this
  crate -- and the live set is folded by [`sumer_host::fold::Fold`], also
  never reimplemented here. Both are the actual host's logic, not a copy of
  it: this suite would not catch a real host and this suite silently
  disagreeing about resumption or revision assignment if it carried its
  own second implementation of either.
- **`provider_json_number.json`** and **`stale_balance.json`** each make one
  extra call on the *same* connection whose reply must be rejected at the
  envelope level (`expect.envelope_errors`), then re-issue `resources.list`
  to prove the rejection did not take the connection with it.
- **`protocol_violations.json`** spawns your adapter six times, one per
  entry in its fixture's `script.runs[]`, selected via the
  `SUMER_FIXTURE_RUN` environment variable (a 0-based index, per
  `spec/wire.md` §11). Some of the outcomes it tests are fatal by
  construction (`PreHelloOutput`, `UnknownId`, `DuplicateId` each end the
  adapter process), so they cannot share one continuous connection with
  the run's other checks.

If you write your own multi-run fixture, the contract is: `SUMER_FIXTURE_RUN`
selects one independent adapter lifetime out of `script.runs[]`, defaulting
to `"0"` when unset, and a runner must iterate every index actually present
rather than only ever using the default.

`conformance_hints.deadline_ms`, when a fixture sets it, is advisory only
-- never part of the wire protocol, never sent to or read by the adapter.
It lets a fixture request a shorter per-connection request deadline than
the wire's 30-second default, so a scenario that deliberately forces a
host-side timeout doesn't cost 30 real seconds per run. A runner that
ignores it entirely is still conformant.

## The wire pass: what the adapter sent, not what survived the host

Most of this suite reads through `sumer_host::AdapterHandle`'s typed calls,
which is the point -- your adapter is exercised by a real host. But a host
is also a *remediator*: it omits every observation over
`MAX_OBSERVATION_BYTES` at decode (spec/observation.md §6), and it stamps
fields onto what it keeps. Both of those hide adapter behaviour that two
assertions exist to judge.

So after the typed crawl, the runner makes one or more **wire passes**:
fresh adapter processes, driven through `AdapterHandle::call_raw`, which
returns the reply envelope verbatim. Same spawn, same frame decoder, same
id lifecycle -- there is still exactly one client in this crate -- but what
comes back is what your adapter actually emitted.

* **A10** is measured there and only there. An over-cap observation is
  omitted by the host *before* any decoded reply exists, so a suite that
  measured decoded observations could never see the violation: it would
  certify an adapter that skipped the degrade entirely. Measuring the
  decoded form is also wrong in the other direction -- `Observation`
  serializes absent optionals as explicit nulls that `ObservationWire`
  omits, which charges an adapter for bytes it never sent and rejects a
  legal record sitting exactly at the cap.
* **A9** compares two wire passes against each other.

One thing the wire pass cannot see: a duplicate JSON key inside a reply
body. `call_raw` hands back the body as a parsed value, and a value
collapses duplicates. That is not a hole -- the typed crawl reads the same
frames' original *text*, where a duplicate key is rejected outright.

## A9's requirement: two process invocations, and the whole history

For every case that reads history, this runner spawns your adapter
**twice more**, independently, and compares what the two runs emitted. A
`local_id` derived from anything process-local -- a freshly generated
UUID, an in-memory counter that doesn't survive a restart -- fails here
even though a single run would have looked perfectly fine.

What is compared is the **association**, over the **whole observation
history**: each `local_id` against every raw record that carried it, in
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
| A2 | Live-set equality after folding | Ignoring `state`/`supersedes` entirely |
| A3 | `null` (unknown) is never zero | Emitting `null` for every category, or `0` for every unknown one |
| A4 | Error class is never plain text; ops around a failure still succeed | Returning the same error for everything, or killing the connection on any hiccup |
| A5 | Exactly-resumable cursor, bracketed both ways, over the resumed live set's full **financial content** | Re-emitting everything (passes only because dedup absorbs it), emitting nothing (fails the live-set half), *and* re-emitting the right `local_id`s carrying different amounts (which a key-set comparison could not see) |
| A6 | The live set floor: non-empty when expected | "Report success" with nothing behind it |
| A7 | Every requested `resource_id` in `statuses`, exactly once; and `status.read`'s own fields — the two independent clocks and `history_start` — compared against values a fixture makes deliberately *differ* | One blanket status for a whole batch, *and* an implementation that reports one clock for both `credential_expires_at` and `strong_auth_expires_at` (which passes whenever a fixture lets the two values coincide), *and* one that renders an unknown clock or `history_start` as a date instead of omitting it (`expect` spells "must not claim to know this" as `null`) |
| A8 | Full chain retention, in fold order | Keeping only the final state |
| A9 | `local_id` purity across two independent process launches, compared as record-to-id associations over the full observation history | A freshly generated UUID per run, a derivation that reuses the same ids for different records on the second run, *and* one that mis-identifies only a record that is later superseded or tombstoned (identical live set, different history) |
| A10 | The oversized-observation two-step degrade, five ways at once, driven by genuinely oversized input, measured **at the wire** | Truncating (or dropping) the whole page over one bad record, leaving leaked payload beside the truncation marker (`provider_extra` is compared exactly), reporting the degrade *as* the resource's outcome so its freshness is erased, *and* skipping the degrade entirely and letting the host omit the record for you (every observation is measured as the adapter emitted it, before host remediation) |
| A11 | Fatal violations kill; a tombstoned reply is discarded and the connection survives | A host that kills the connection on *any* anomaly |

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
