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

## A9's requirement: two process invocations, not one

Whenever a case's fold produces any `local_id`s, this runner spawns your
adapter a **second, independent** time for that same fixture and diffs the
set of `local_id`s the two runs produced. A `local_id` derived from
anything process-local -- a freshly generated UUID, an in-memory counter
that doesn't survive a restart -- fails here even though a single run
would have looked perfectly fine.

## Assertions implemented (A1-A11)

Every assertion in the frozen contract's section (g) is implemented with
its stated mitigation, not just its happy-path check:

| # | What it proves | The degenerate implementation it kills |
|---|---|---|
| A1 | Exact-value fidelity (`Amount::cmp_same_asset`, plus digit-count/scale evidence where a fixture asks for it) | An adapter (or host) that silently round-trips an amount through a float |
| A2 | Live-set equality after folding | Ignoring `state`/`supersedes` entirely |
| A3 | `null` (unknown) is never zero | Emitting `null` for every category, or `0` for every unknown one |
| A4 | Error class is never plain text; ops around a failure still succeed | Returning the same error for everything, or killing the connection on any hiccup |
| A5 | Exactly-resumable cursor, bracketed both ways | Re-emitting everything (passes only because dedup absorbs it) *and* emitting nothing (fails the live-set half) |
| A6 | The live set floor: non-empty when expected | "Report success" with nothing behind it |
| A7 | Every requested `resource_id` in `statuses`, exactly once | One blanket status for a whole batch |
| A8 | Full chain retention, in fold order | Keeping only the final state |
| A9 | `local_id` purity across two independent process launches | A freshly generated UUID per run |
| A10 | The oversized-observation two-step degrade, three ways at once | Truncating (or dropping) the whole page over one bad record |
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
- **`fdx_lossless.json`'s claim that `spec/fdx-6.4-mapping.md`'s LOSSY
  column is empty is only checked to the extent this suite asserts
  amounts, categories, and postings** (`expect.balances` /
  `expect.history_live_set`). The fixture's `expect.fdx_field_map`
  documents where every FDX field lands, including into
  `provider_extra`; this runner does not independently re-derive that
  mapping table field-by-field.
- **`expect.provenance.*.staleness` is not checked**, deliberately. The
  wire forbids an adapter from ever sending `staleness` (it is
  host-computed, spec/observation.md §1), and this milestone's
  `sumer-host` always stamps `Staleness::Live` -- there is no cache layer
  yet for `Cached`/`Unavailable` to describe (see the module docs on
  `core/host/src/lib.rs`). Asserting `stale_balance.json`'s
  `expect.provenance.checking-2.staleness: "cached"` against the shipped
  host would therefore fail unconditionally, for a reason outside this
  suite's mandate rather than a real nonconformance. This runner checks
  `completeness` (which the wire *can* carry and which this fixture's
  script does set) and leaves `staleness` out; see the worker report for
  this PR for the same note addressed to Linus.
- This suite never inspects an adapter's source, memory, or process
  internals. It is exactly as strong as its wire evidence and no
  stronger.

## A known API gap in `sumer-host`, not routed around

`unsupported_op.json`'s whole point is sending an op no capability list
ever names (`execute`, reserved for a future milestone). `sumer_host::AdapterHandle`
exposes exactly four typed methods (`resources_list`, `balances_read`,
`history_read`, `status_read`), each hardcoding its own op string --
there is no public way to ask it to send an arbitrary op. Adding one was
out of scope for this crate's file list, so this one case is driven by a
small, private, single-purpose raw JSONL client (`runner::RawLink`)
instead of `AdapterHandle`. It reimplements no id-lifecycle or
protocol-violation logic -- every other case, including all of A11, goes
through the real `AdapterHandle` -- it is strictly "write one line, read
the next line back," in order, and exists only because there was no other
way to exercise this one fixture without modifying `sumer-host` itself.
