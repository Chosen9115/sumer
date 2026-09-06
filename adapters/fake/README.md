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

Close stdin (Ctrl-D, or the end of the pipe) and it exits. That is
`spec/wire.md` §7's rule — an adapter MUST exit when its stdin reaches EOF
— and the read loop gets it for free: `for line in sys.stdin` ends at EOF,
`main()` returns, the process is gone. It is deliberately *not* written as
`while True:` around `sys.stdin.readline()`, which is the shape that spins
forever on the empty string EOF returns. The mutation battery breaks
exactly that (`mutations/adapters/stays_alive_after_stdin_eof.py`) and
requires the host to report `ProtocolViolation::StdinEofIgnored`.

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
| `defer` | hold this request unanswered; a later rule must `reply_deferred` it. **This is also how you force a host-side timeout.** There is deliberately no `sleep_ms`: sleeping only beats a deadline if the machine cooperates, and under CI load it does not — a test whose pass depends on the scheduler is not a test. A deferred request is never answered until the fixture says so, so the timeout is certain at any deadline. Sleeping also blocked this adapter's own read loop, which forced every *other* request on the connection into the same short deadline. |
| `reply_deferred` | pop the oldest (or newest, `which:"newest"`) deferred request and reply to *it* |
| `replay_last_reply` | resend the previous frame byte-for-byte (builds "reply to an already-answered id" without knowing its numeric value) |
| `stdout_raw` | write `text` verbatim (garbage, banners; include your own `\n`) |
| `stdout_raw_bytes` | write a literal list of byte values (deliberately invalid UTF-8) |
| `reply_ok_raw` | writes `body_json` — raw JSON **text** — as the `ok` value, verbatim. The only way to put a bare JSON *number* where an `AmountWire` belongs (`spec/money.md` §2) without the value ever passing through a Python `float` on the way there. |
| `stderr` | write `text` to stderr (never parsed by the host) |
| `exit` | `sys.exit(code)` — mid-batch crash |
| `reply_with_decimal_amounts` | see below — the one piece of real work |

Two **modifiers** apply to `reply_ok` and `reply_with_decimal_amounts`, in
this order, just before the reply is written:

| Key | Effect |
|---|---|
| `pad: [{"path": [...], "bytes": N}]` | overwrite each `path` in the body with `N` copies of `"x"`. Lets a fixture be *genuinely* oversized — a 200 KB description, a 120 KB `provider_extra` — without carrying 200 KB of literal JSON. |
| `degrade: true` | run `spec/observation.md` §6's two-step degrade over the padded body, measuring real serialized bytes: an observation over `MAX_OBSERVATION_BYTES` (65,536) has its `provider_extra` replaced by exactly `{"_truncated": true, "_original_bytes": N}` and its `completeness` set to `partial`; if it is *still* too large it is omitted entirely and its resource's status entry gains `degraded {local_id, bytes}` with the measured size — **beside** its `outcome`, never replacing it (a resource can be `stale` *and* have dropped a record; overwriting the outcome erased the freshness the staleness table reads). Every other observation on the page is emitted regardless. |

`pad` + `degrade` together are what make `oversized_observation.json` a real
test rather than a declaration: the fixture states no byte counts, it states
the padding, and every number in its `expect` block is one the degrade
*produced*. `pad` on its own (no `degrade`) builds the `OversizeFrame`
violation in `protocol_violations.json`.

A rule may also carry `"consume": false`, which keeps it matching every time
instead of firing once — needed by any op the runner legitimately calls more
than once on one connection (`resources.list`, which the suite re-issues to
prove a connection survived an envelope error).

### Defaults a fixture does not have to write

Three things were repeated verbatim in every fixture and carried no test
content, so the adapter fills them in. **Every one is overridable: an
explicitly written value always wins, and nothing here can change a value a
fixture stated.**

| Omitted | Filled in with |
|---|---|
| a run's `hello` | `{protocol:"1", adapter_id:"fake-adapter", adapter_version:"0.1.0", capabilities:[all four], local_id_derivation:"fixture-literal@1", max_in_flight:1}`. `protocol_violations.json`'s `survivable_then_kill` run still writes its own, because it needs `max_in_flight: 2`. |
| keys of an observation's `provenance` | the run-level `"provenance"` object, for keys the observation omits. `adapter_id`/`surface`/`observed_at` are usually constant across a run; `completeness` usually is not, so it is usually written per observation. |
| a `statuses` entry's `outcome` | `fetched {page_empty: <did this resource contribute any observation to THIS reply>}` — **computed from the reply**, not declared, so it cannot drift out of step with the observations beside it. Any other outcome (`stale`, `rate_limited`, `revoked`, ...) is written out in full. `degraded` is never a default — only `degrade: true` sets it, from real measured bytes. |

This removed ~600 lines of copy-paste across the twelve fixtures without
changing a single byte of meaning on the wire: replaying every fixture
through the adapter before and after produced semantically identical replies
for all 22 runs (only the key order inside `provenance` moved, and
`spec/wire.md` §1 makes field names the only way a value is addressed).

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
`spec/money.md` §6 — and the negation is `Decimal.copy_negate()`, **never**
unary `-`: unary minus is a context-aware decimal operation that silently
rounds to the active context's precision (28 digits by default), so
`-Decimal("12345678901234567890123456789.01")` returns
`-12345678901234567890123456790` with no float involved and no error raised.
The module also installs a decimal context that traps `Inexact`/`Rounded`, so
any context-sensitive operation added to this file later raises instead of
rounding quietly; a zero value is left alone rather than negated, since a
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

Two fixtures carry a top-level `conformance_hints` key, and both do it for
the same reason: their scenario is only observable once a deadline has
*expired*, and at the 30-second default (`spec/wire.md` §7) that is 30 real
seconds of wall clock per execution.

* `protocol_violations.json` names `deadline_ms: 2000`. Its
  `survivable_then_kill` run `defer`s a request and answers it later, from
  another rule — so the host's deadline is certain to expire first, and the
  discard of that late reply is what the run tests (not the adapter being
  killed for being slow).
* `pending_to_posted.json` names `deadline_ms: 3000`, because one mutant of
  it refuses to exit at stdin EOF and "has not exited" is only knowable
  after the deadline. The honest adapter never comes near it: it answers in
  milliseconds and exits the moment stdin closes.

`conformance_hints` is not wire protocol, is never sent to or read by the
adapter, and a runner is free to ignore it entirely and stay conformant
(`spec/wire.md` §11) — it would just pay the full default deadline for
these two.

## Wire shapes

`spec/wire.md` (envelope, ops, caps, id lifecycle) and `spec/observation.md`
(provenance, balances, observations, statuses, pagination) are normative and
own every shape these fixtures emit; `core/wire/` is the reference decoder
for them. This file deliberately does not restate them — a second copy of a
normative table is a copy that drifts, and this one had: it still documented
`page: {"kind": "cursor", "cursor": ""}` as the way to ask for a first page
after Ruling A8 replaced that with an **absent** `page` field.

Two things worth knowing before writing a fixture, both consequences of
those documents rather than additions to them:

- The first page of a resource's history is requested with **no `page` key
  at all**. A `when` clause for it is `{"resources": [{"resource_id": "..."}]}`
  — matching a literal empty cursor will never fire.
- `history.read`'s per-resource paging state lives nested inside each
  `statuses` entry as a `"page"` object, never as a top-level sibling of
  `observations`/`statuses`: two resources in one batched call can be at
  different points in their own pagination.

## What each fixture is actually testing

| Fixture | Proves |
|---|---|
| `large_amounts.json` | 78-digit uint256 wei, 18-decimal ETH, satoshi integers all round-trip exactly (A1) |
| `pending_to_posted.json` | bank pending→posted forks `local_id`; both chain entries retained (A2, A8) |
| `reorg_vanish.json` | one `local_id` goes active→**tombstoned**→active; tombstone is not terminal (A2, A8) |
| `stale_balance.json` | `stale{as_of}` outcome + `Cached` staleness survive; a literal `received_at` on the wire is rejected without killing the connection; and `status.read` reports the two independent clocks with genuinely different values, one of them absent because the credential never expires until revoked (A3, A4, A7) |
| `duplicate_events.json` | same `provider_id` on two surfaces is two different `local_id`s — dedup-by-`provider_id`-alone is forbidden (A2, A8) |
| `interrupted_pagination.json` | both `exact` and `batch_restart` cursor families survive a mid-batch adapter kill and fresh-process resume (A2, A5) |
| `null_category.json` | exactly one null category, exactly one asserted non-null — kills emit-null-for-everything (A3) |
| `provider_json_number.json` | `json.loads(..., parse_float=Decimal)` + `format(d,'f')` recipe, proven against a 27-sig-digit fraction and a uint256-scale float token (A1) |
| `oversized_observation.json` | truncation marker + `Partial`, untouched small siblings either side of it in the same page, and a fully-omitted third observation reported via `degraded {local_id, bytes}` on a resource whose `outcome` stays `stale` (A10) |
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
