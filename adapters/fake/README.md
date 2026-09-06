# `fake_adapter.py` — the PR2 conformance adversary

Python 3, **standard library only** (`json`, `os`, `sys`, `time`, `decimal`,
`copy` — verified with `python3 -c "import ast; ..."` against the source,
see "Verification" below). It speaks the wire protocol in `spec/wire.md`
and replays one of the twelve fixtures in `conformance/cases/*.json`.

```
SUMER_FIXTURE=conformance/cases/<case>.json python3 adapters/fake/fake_adapter.py
```

`SUMER_FIXTURE_RUN` (default `"0"`) selects which element of the fixture's
`script.runs` array to execute — see "Why `runs[]` exists" below. Eleven of
the twelve fixtures have exactly one run (index 0); `protocol_violations.json`
has six.

This file is generic: it contains no case-specific logic. Every hostile
behaviour (oversize frame, garbage, never-issued id, duplicate reply,
out-of-order replies, deadline overrun, mid-batch exit, pre-hello banner, a
JSON-number amount, an oversized `provider_extra`) is driven entirely by the
fixture's `script`. All twelve fixtures were written against this one
interpreter, so `script` and `expect` are constructed together and cannot
drift apart — that is the point of keeping both in one file per case.

## Running it by hand

```
$ SUMER_FIXTURE=conformance/cases/large_amounts.json python3 adapters/fake/fake_adapter.py
{"id":0,"op":"hello","params":{"protocol":["1"]}}
{"id":0,"ok":{"protocol":"1","adapter_id":"fake-adapter", ...}}
```

Type (or pipe) one JSON object per line on stdin; the adapter writes one
JSON object per line on stdout. Nothing else ever reaches stdout, except
in the one fixture (`protocol_violations.json`, run 0) that deliberately
violates that rule to test `ProtocolViolation::PreHelloOutput`.

## The fixture schema

```jsonc
{
  "case": "<name>",
  "conformance_hints": { ... },   // OPTIONAL, ADVISORY ONLY, see below
  "script": {
    "runs": [
      {
        "label": "<free text, for humans and for expect.runs[].label>",
        "pre_hello_stdout": "<string>",   // OPTIONAL. Written verbatim to
                                           // stdout before anything is read
                                           // from stdin. Only protocol_
                                           // violations.json run 0 uses this.
        "hello": { ...fields of the hello ok reply..., "err": {...} },
                                           // if "err" is present the adapter
                                           // replies err instead of ok
        "on": {
          "<op>": [                       // resources.list | balances.read |
                                           // history.read | status.read
            {
              "when": { "<param key>": <value>, ... },  // OPTIONAL subset
                                                          // match against the
                                                          // request's params;
                                                          // omitted/empty
                                                          // matches anything
              "consume": true,             // OPTIONAL, default true. false
                                            // = this rule matches every time
                                            // (fallback/default rule)
              "do": [ <action>, ... ]
            }
          ]
        }
      }
    ]
  },
  "expect": { ... case-specific, see each fixture ... }
}
```

Rules for one op are tried **in the order listed**; the first unconsumed
rule whose `when` is a subset of the request's `params` wins. An op with no
`on` entry at all gets the wire's own answer: `err.code="unsupported"`,
`detail.op=<op>`, connection stays open (`spec/wire.md` b). A request that
matches no rule for a *declared* op is a **fixture bug**: the adapter logs
`FIXTURE BUG` to stderr and replies `err.code="internal"` rather than
hanging, so a bad fixture fails loudly instead of silently.

### Actions (`do: [...]`)

| `op` | Effect |
|---|---|
| `reply_ok` | `{"id": <the request's id>, "ok": body}` |
| `reply_err` | `{"id": <the request's id>, "err": {code, message, detail}}` |
| `reply_ok_id` | `{"id": <literal id>, "ok": body}` — never-issued / duplicate ids |
| `reply_err_id` | same, with `err` |
| `defer` | hold this request unanswered; a later rule must `reply_deferred` it |
| `reply_deferred` | pop the oldest (or newest, `which:"newest"`) deferred request and reply to *it* |
| `replay_last_reply` | resend the previous frame byte-for-byte (builds "reply to an already-answered id" without knowing its numeric value) |
| `sleep_ms` | `time.sleep(ms/1000)` |
| `stdout_raw` | write `text` verbatim (garbage, banners; include your own `\n`) |
| `stdout_raw_bytes` | write a literal list of byte values (deliberately invalid UTF-8) |
| `reply_ok_padded` | like `reply_ok`, but `pad_path` is overwritten with `pad_bytes` copies of `"x"` first — builds an oversize frame without bloating the fixture file |
| `stderr` | write `text` to stderr (never parsed by the host) |
| `exit` | `sys.exit(code)` — mid-batch crash |
| `reply_with_decimal_amounts` | see below — the one piece of real work |

### `reply_with_decimal_amounts` — the Decimal recipe

```jsonc
{
  "op": "reply_with_decimal_amounts",
  "provider_payload_json": "<a JSON document, AS A STRING>",
  "amounts": [
    {"payload_path": ["balances", "wei"], "asset": "eth-wei",
     "insert_into": ["observations", 0, "amount"], "negate": false}
  ],
  "body": { ...the reply body, with "amount": null at each insert_into path... }
}
```

`provider_payload_json` is a JSON **string**, never nested JSON — if it were
nested, the *outer* `json.load()` that reads the fixture itself would already
have run its default float parser over any float tokens inside it, before
this adapter ever got a chance to choose otherwise. The adapter parses that
string itself with `json.loads(text, parse_float=Decimal)`, walks each
`payload_path` (a bare JSON integer stays a Python `int` — arbitrary
precision, never at risk — and is converted to `Decimal` losslessly; a float
token becomes an exact `Decimal`), optionally negates it (`negate: true`,
for adapters that must normalize an unsigned amount plus a separate sign
indicator — e.g. FDX's `debitCreditMemo` — into Sumer's single signed
`Amount`, "applied before... never inside" the money layer per
`spec/money.md` §6; a zero value is left alone rather than negated, since a
signed zero is grammar-invalid), formats it with `format(d, "f")` (never
`str(Decimal(...))`, which reproduces exponent notation for small
magnitudes — `spec/money.md` §3), and splices `{"asset", "amount"}` into a
deep copy of `body` at `insert_into`.

Two fixtures use this: `provider_json_number.json` (the graded case) and
`fdx_lossless.json` (same discipline applied to a realistic payload).

## Why `script.runs[]` + `SUMER_FIXTURE_RUN` exists

**This is ruled contract, not a local convention** (Ruling A5, Amendment 1;
normative text is in `spec/wire.md` §11). It exists because two groups of
required test behaviour cannot be expressed inside one continuous adapter
process lifetime:

- **A11** requires testing `PreHelloOutput` (fatal, and must be the *very
  first* thing that happens — before the hello handshake even completes),
  `UnknownId` (fatal), and `DuplicateId` (fatal), plus a *non-fatal*
  tombstoned-reply-discard that the connection must survive. Two of those
  outcomes end the process; nothing can run after them in the same run, so
  `PreHelloOutput` in particular cannot share a run with anything else.
- **A5** requires killing the adapter mid-page and resuming with a *fresh*
  process that has no memory of the crash. A script that "crashes the first
  time it sees cursor X" is deterministic and correct for exactly one
  spawn; the resumed spawn needs different scripted behaviour at that same
  cursor value.

`protocol_violations.json` uses six runs (`pre_hello_kill`,
`survivable_then_kill`, `duplicate_kill`, `oversize_frame_kill`,
`garbage_kill`, `non_utf8_kill`) and `interrupted_pagination.json` uses six
(`exact_full` / `exact_interrupted_before` / `exact_interrupted_after`,
and the same three for `batch_restart`). **The conformance runner must know
to iterate `SUMER_FIXTURE_RUN` over these indices for these two fixtures
specifically** — every other fixture has a single run at index 0 (the
default), so a runner that always uses the default is compatible with ten
of the twelve cases and silently only exercises run 0 of the other two. The
runner MUST iterate every index in `runs[]`, not just the default.

## `conformance_hints` (advisory, non-normative)

`protocol_violations.json` is the only fixture with a top-level
`conformance_hints` key. It names `deadline_ms: 300` because its
`survivable_then_kill` run sleeps 600ms on one request specifically to
force a host-side timeout before replying anyway (testing that a
tombstoned reply is discarded, not that the adapter is killed for being
slow). The frozen contract's default request deadline is 30 seconds
(`spec/wire.md`, Constants) and defines **no mechanism** for a fixture to
ask the runner to shorten it for a test. `conformance_hints` is not wire
protocol, not consumed by the adapter, and the runner is free to ignore
it — in which case this one scenario needs a real 30-second-plus sleep to
stay honest, which is impractical for a fast suite. Flagging this as a gap
worth closing in the actual runner design, not solving it unilaterally.

## Wire shapes (Contract Amendment 1, ruled — no longer provisional)

Two blind PR2 workers picked different shapes for everything the frozen
contract left unpinned. Amendment 1 settled all of it; every fixture here
was reconciled to the ruling. This section states the ruled shapes, not a
guess:

- **`resource_id` is a required field on every balance observation, every
  history observation, and every `statuses` entry** (Ruling A1). Not
  optional: `balances.read`/`status.read`/`history.read` batch multiple
  resources in one call — A7's "every requested resource_id appears in
  statuses exactly once" means nothing if every call is single-resource —
  and a reply of pooled observations is unparseable without a
  per-observation resource key. `core/wire/src/observation.rs`'s
  `BalanceWire`/`ObservationWire` enforce this at deserialize time.
- **An adapter MUST NOT send `staleness` or `received_at` in `provenance`**
  (Ruling A2). Both are host-computed. `core/wire/src/observation.rs`'s
  `ProvenanceWire` has no field for either, combined with
  `#[serde(deny_unknown_fields)]`, so sending either is a hard deserialize
  failure the host maps to `invalid_request`. `stale_balance.json`'s
  adversarial half (a balance observation whose `provenance` literally
  carries `received_at`) exercises exactly this rejection deliberately, not
  simulated.
- **Op params** (Ruling A3): `resources.list` takes `{}`. `balances.read`
  and `status.read` take `{"resource_ids": [...]}` — batched, NOT
  paginated (balances have no cursor). `history.read` takes
  `{"resources": [{"resource_id": "...", "page": <PageRequest>}]}` —
  batched, with each resource carrying its own cursor, because different
  resources hold different paging state. A first-ever page for a resource
  is requested with `page: {"kind": "cursor", "cursor": ""}` (an empty
  opaque cursor means "from the start"); this adapter's fixtures use that
  convention, and split a resumption cursor's own resource ownership out
  of its `"<resource_id>:<suffix>"` naming convention — a fixture-file
  convenience, not a wire rule.
- **`resources.list` reply** (Ruling A4): `{"resources": [{resource_id,
  provider_id, kind, label, provider_extra?}]}` — no `surface` (surface is
  a property of an *observation*, carried on `Provenance`, not of a
  resource: one resource can be observed through several surfaces, e.g.
  Wise's activities-vs-statements case), and no `provenance` at all —
  nothing has been observed yet at discovery time. `resources.list` and
  `status.read` replies have **no** `observations` and **no** `statuses`
  key on `resources.list`, and no `observations` key on `status.read` —
  only `balances.read` and `history.read` carry
  `{"observations": [...], "statuses": [...]}`.
- **`history.read`'s per-resource paging state** lives nested inside each
  entry of `statuses` as a `"page"` object (`cursor_resumable`, `next`,
  and optionally `window_capped_to`/`page_size_reduced_to`) — never as a
  top-level sibling of `observations`/`statuses`. This is the direct
  consequence of keeping `history.read` batched: two different resources
  in the same batched call can be at two different points in their own
  pagination, so there is no single reply-wide cursor to report.
- Enum wire tagging follows serde's *default* (no `#[serde(tag = ...)]`):
  unit variants (`not_fetched`, `unavailable`, `live`, `complete`,
  `pending`, `active`, `provider_positive`, canonical hints, ...) are bare
  JSON strings, snake_case exactly as `core/wire/src/observation.rs`
  renders them; variants carrying fields (`fetched{page_empty}`,
  `stale{as_of}`, `rate_limited{retry_after_ms}`,
  `oversized_observation{local_id,bytes}`) are single-key objects,
  `{"variant_name": {...fields}}`.

None of this changes what the fixtures assert about money, folding,
pagination, or violations — only the addressing of that content (which
JSON keys carry which values). See each fixture's `expect.notes` for
case-specific reasoning.

## What each fixture is actually testing

| Fixture | Proves |
|---|---|
| `large_amounts.json` | 78-digit uint256 wei, 18-decimal ETH, satoshi integers all round-trip exactly (A1) |
| `pending_to_posted.json` | bank pending→posted forks `local_id`; both chain entries retained (A2, A8) |
| `reorg_vanish.json` | one `local_id` goes active→**tombstoned**→active; tombstone is not terminal (A2, A8) |
| `stale_balance.json` | `stale{as_of}` outcome + `Cached` staleness survive; a literal `received_at` on the wire is rejected without killing the connection (A3, A4, A7) |
| `duplicate_events.json` | same `provider_id` on two surfaces is two different `local_id`s — dedup-by-`provider_id`-alone is forbidden (A2, A8) |
| `interrupted_pagination.json` | both `exact` and `batch_restart` cursor families survive a mid-batch adapter kill and fresh-process resume (A2, A5) |
| `null_category.json` | exactly one null category, exactly one asserted non-null — kills emit-null-for-everything (A3) |
| `provider_json_number.json` | `json.loads(..., parse_float=Decimal)` + `format(d,'f')` recipe, proven against a 27-sig-digit fraction and a uint256-scale float token (A1) |
| `oversized_observation.json` | truncation marker + `Partial`, an untouched small sibling in the same page, and a fully-omitted third observation reported via `oversized_observation{local_id,bytes}` (A10) |
| `unsupported_op.json` | an undeclared op (`execute`) never closes the connection; ops before and after still succeed (A4) |
| `protocol_violations.json` | `PreHelloOutput`, `UnknownId`, `DuplicateId`, `OversizeFrame`, `NotJson`, `NonUtf8` all kill; tombstoned-reply-discard and out-of-order replies do not (A4, A11) |
| `fdx_lossless.json` | one sanitized FDX 6.4-shaped payload (account + 2 balances + 3 transactions, one deliberately using the `AUTHORIZATION` status that has no direct Sumer `posting` equivalent) maps to Sumer types with every field landing somewhere named in `expect.fdx_field_map` — nothing silently dropped (A1) |

`fdx_lossless.json`'s field names/enums (`accountId`, `debitCreditMemo`,
`CREDIT`/`DEBIT`, transaction `status` ∈
{`AUTHORIZATION`,`MEMO`,`PENDING`,`POSTED`}, `currentBalance`/
`availableBalance`, `postedTimestamp`/`transactionTimestamp`) are reconciled
(Ruling A6, Amendment 1) against the actual `plaid/core-exchange` **6.4**
schema — branch `mj-add-corex-6.4`, `versions/6.4/corex.yaml`,
`info.version: 6.4.0`, fetched via `gh api` — the same source
`spec/fdx-6.4-mapping.md` is built from. An earlier draft of this fixture was
built from Plaid's browsable **6.3** reference docs instead and had invented
three fields that do not exist on 6.4's `DepositAccount`/`Transaction` schema
objects: `account.balanceAsOf` (real in 6.4, but only on `InvestmentAccount`,
not `DepositAccount`), and per-transaction `accountId`/`currency` (the
account is scoped by the request path, and `Transaction` has no plain
`currency` field at all). All three were removed rather than kept as
6.3-only fields — see `expect.fdx_field_map._removed_from_6.3_draft` in the
fixture and `spec/fdx-6.4-mapping.md`'s Accounts/Balances sections. All identifiers,
names, and account numbers in the payload are obvious placeholders
(`acct-sample-0001`, `XXXX0000`, `"... (fixture placeholder)"` in every
free-text field) — no real person, account, or institution.

## Verification performed

- `python3 -m json.tool` on all twelve files in `conformance/cases/`: all
  valid JSON.
- `python3 -c "import ast; ..."` walked over `fake_adapter.py`'s own AST:
  imports are exactly `{json, os, sys, time, decimal, copy}` — all stdlib.
- Ran the adapter by hand (piping hand-written request lines on stdin) for:
  - `large_amounts.json` — confirmed framing is one JSON object per
    LF-terminated line in both directions, and hello is the first line out.
  - `provider_json_number.json` — confirmed the three extracted amounts
    match `expect.balances` exactly (see the transcript below).
  - `fdx_lossless.json` — confirmed DEBIT/CREDIT sign normalization
    produces `-42.50` / `1500.00` / `-75.00`.
  - `protocol_violations.json`, all six `SUMER_FIXTURE_RUN` values —
    confirmed: run 0 writes garbage with no hello reply anywhere near it;
    run 1's timing (`time` showed ~0.6s) and reply order (`id 5` then
    `id 4`, i.e. genuinely out of arrival order) matched the script, and
    the final frame carries `id: 18446744073709551615`; run 2's last two
    frames are byte-identical (`replay_last_reply`); run 3's second output
    line is 1,200,387 bytes, over `MAX_FRAME_BYTES`; run 4 emits plain
    text instead of a JSON reply; run 5's last frame's tail bytes are
    `0xFF 0xFE`, invalid UTF-8.
- **Cross-checked every amount string this adapter emits against the real
  `sumer-money` crate** (a throwaway `cargo run` binary depending on
  `core/money` by path, not committed anywhere): every amount produced by
  `large_amounts.json`, `provider_json_number.json`, and `fdx_lossless.json`
  parses via `Amount::parse` and round-trips byte-identically through
  `Display`. This is the strongest evidence available that the fixtures
  satisfy `spec/money.md`'s grammar exactly, not just "look like decimals".

### The `provider_json_number` transcript

```
=== Value A (27 sig digits) ===
Decimal exact : 123456789012345678.987654321
float() lossy : 123456789012345680
round-trip equal to input? True
float matches input exactly? False

=== Value B2 (uint256-scale, forced float token) ===
Decimal exact : 115792089237316195423570985008687907853269984665640564039457584007913129639935.0
float() lossy : 115792089237316195423570985008687907853269984665640564039457584007913129639936
round-trip equal to input? True
float matches input exactly? False

=== json.loads with parse_float=Decimal, over a payload STRING ===
token_balance_wei type: <class 'int'>
token_balance_wei value: 115792089237316195423570985008687907853269984665640564039457584007913129639935
token_balance_wei_float type: <class 'decimal.Decimal'>
token_balance_wei_float value (format d,'f'): 115792089237316195423570985008687907853269984665640564039457584007913129639935.0
precise_fraction type: <class 'decimal.Decimal'>
precise_fraction value (format d,'f'): 123456789012345678.987654321

=== what a NAIVE adapter (plain json.loads, no parse_float) would emit ===
naive precise_fraction: 1.2345678901234568e+17 -> str: 1.2345678901234568e+17
naive token_balance_wei_float: 1.157920892373162e+77
```

And the adapter's actual reply for `provider_json_number.json`, unmodified:

```json
{"id":1,"ok":{"observations":[
  {"resource_id":"wallet-decimal","category":"token_balance_wei_bare_int","canonical_hint":null,
   "amount":{"asset":"eth-wei","amount":"115792089237316195423570985008687907853269984665640564039457584007913129639935"},
   "provenance":{...}},
  {"resource_id":"wallet-decimal","category":"token_balance_wei_float_token","canonical_hint":null,
   "amount":{"asset":"eth-wei-reported-as-float","amount":"115792089237316195423570985008687907853269984665640564039457584007913129639935.0"},
   "provenance":{...}},
  {"resource_id":"wallet-decimal","category":"precise_fraction","canonical_hint":null,
   "amount":{"asset":"USDC-precise","amount":"123456789012345678.987654321"},
   "provenance":{...}}
],"statuses":[{"resource_id":"wallet-decimal","outcome":{"fetched":{"page_empty":false}}}]}}
```

Every emitted amount string is byte-identical to the source decimal
literal — the provider's float token never touched a Python `float`,
anywhere in the pipeline.

## What could not be verified

The real conformance runner (`sumer-conformance`) either does not exist yet
or was not available in this workspace — **no cargo-run pass against these
fixtures was performed, and none is claimed.** Everything above is (a)
JSON validity, (b) hand-driven wire-level behaviour of this adapter in
isolation, and (c) cross-verification of amount strings against the real
`sumer-money` crate. Whether the runner actually agrees with the op/params
shapes this README documents as assumptions is the one thing that cannot
be checked from this side of the fence.
