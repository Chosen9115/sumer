#!/usr/bin/env python3
"""Sumer fake adapter -- the PR2 conformance suite's adversary.

STDLIB ONLY. Reads a fixture path from SUMER_FIXTURE (a JSON file holding
`script` and `expect`, see README.md), speaks JSON Lines on stdin/stdout
per spec/wire.md, and replays the fixture's `script.runs[N]` where N comes
from SUMER_FIXTURE_RUN (default "0"). Every hostile behaviour it performs
(oversize frame, garbage, never-issued id, duplicate reply, out-of-order
replies, a request held past its deadline, mid-batch exit, pre-hello banner, a bare
JSON-number amount, a genuinely oversized observation) is driven entirely by
the fixture's `script` -- this file contains no case-specific logic.

MONEY IS EXACT HERE, STRUCTURALLY. Every `decimal` operation this file
performs is context-INDEPENDENT (`copy_negate`, comparison, construction,
`format(d, "f")`), and the module installs a decimal context that TRAPS
`Inexact`/`Rounded` (see `_EXACT_CONTEXT`). Python's default context has
`prec=28` and silently rounds: plain `-d` on a 31-significant-digit Decimal
returns a rounded value with no error at all. That is precisely the class of
bug this project exists to prevent, so an adapter that is meant to be the
suite's adversary must not be able to commit it quietly -- under the trapping
context any context-sensitive Decimal operation added here later raises
instead of rounding.

Framing: exactly one UTF-8 JSON object per LF-terminated line, both
directions. Nothing is ever written to stdout before the hello reply
UNLESS the fixture's run explicitly sets `pre_hello_stdout` (only
protocol_violations.json's pre_hello_kill run does this, deliberately).
All diagnostics go to stderr.

See README.md for:
  - the full script/expect JSON schema this interpreter implements
  - the wire shapes (op params, reply bodies) this adapter ASSUMES, which
    are NOT fully pinned by the frozen contract and must match whatever
    the Rust conformance runner actually sends/expects
  - the `provider_json_number` recipe (json.loads(..., parse_float=Decimal)
    then format(d, 'f'), per spec/money.md section 3) and why it is the one
    piece of this file that is real work rather than table lookup.
"""
import copy
import decimal
import json
import os
import sys
from decimal import Decimal

# spec/wire.md section 1. An observation larger than this is degraded by the
# two-step rule in spec/observation.md section 6 -- see `degrade_oversized`.
MAX_OBSERVATION_BYTES = 65_536

# Money never rounds in this process. `Inexact`/`Rounded` are trapped so a
# context-sensitive Decimal operation raises instead of silently truncating
# to the default 28-digit precision; the operations this file actually uses
# (`copy_negate`, `!=`, `Decimal(int)`, `Decimal(str)` via parse_float, and
# `format(d, "f")`) are all context-independent and unaffected.
_EXACT_CONTEXT = decimal.Context(
    traps=[
        decimal.Inexact,
        decimal.Rounded,
        decimal.InvalidOperation,
        decimal.DivisionByZero,
        decimal.Overflow,
        decimal.Underflow,
        decimal.Subnormal,
        decimal.FloatOperation,
    ]
)
decimal.setcontext(_EXACT_CONTEXT)


def _write_raw(data: bytes) -> None:
    sys.stdout.buffer.write(data)
    sys.stdout.buffer.flush()


def load_fixture():
    path = os.environ["SUMER_FIXTURE"]
    with open(path, "r", encoding="utf-8") as f:
        # Plain json.load is safe here ONLY because fixture authors never put
        # a bare JSON number where a money amount belongs (that would defeat
        # the entire point). Amounts are always JSON strings already, except
        # inside the two cases that model a raw upstream provider payload
        # (provider_json_number, fdx_lossless): there the payload is embedded
        # as an escaped JSON *string*, which this adapter parses separately
        # via `reply_with_decimal_amounts`, using parse_float=Decimal -- so
        # the provider's float token never reaches a Python float, here or
        # in the outer json.load.
        return json.load(f)


def pick_run(fixture):
    runs = fixture["script"]["runs"]
    idx = int(os.environ.get("SUMER_FIXTURE_RUN", "0"))
    return runs[idx]


def subset_match(when, params):
    """True if every key in `when` is present in `params` with an equal
    value. An empty/missing `when` matches anything (used for simple
    single-shot ops where sequencing alone disambiguates)."""
    if not when:
        return True
    for k, v in when.items():
        if params.get(k) != v:
            return False
    return True


def get_path(obj, path):
    for k in path:
        obj = obj[k]
    return obj


def set_path(obj, path, value):
    for k in path[:-1]:
        obj = obj[k]
    obj[path[-1]] = value


# The one hello every fixture but `protocol_violations` wants. A run that
# needs a different one (a different `max_in_flight`, an `err`) still writes
# its own; this is a default, not a policy.
DEFAULT_HELLO = {
    "protocol": "1",
    "adapter_id": "fake-adapter",
    "adapter_version": "0.1.0",
    "capabilities": [
        "resources.list",
        "balances.read",
        "history.read",
        "status.read",
    ],
    "local_id_derivation": "fixture-literal@1",
    "max_in_flight": 1,
}


def apply_defaults(run, body):
    """Fills in the parts of a reply body a fixture did not bother to spell
    out. Every default is overridable per reply: an explicitly written key
    always wins, and nothing here can change a value a fixture stated.

    - `run["provenance"]` supplies fields for any observation `provenance`
      that omits them (adapter_id, surface, observed_at ... are usually
      constant across a whole run; `completeness` usually is not).
    - A `statuses` entry with no `outcome` gets `fetched {page_empty: <did
      this resource contribute any observation to THIS reply>}` -- computed
      from the reply, not declared, so it cannot drift out of step with the
      observations beside it. A fixture asserting anything else writes the
      outcome out.
    """
    prov_defaults = run.get("provenance")
    if prov_defaults:
        for obs in body.get("observations", []):
            merged = dict(prov_defaults)
            merged.update(obs.get("provenance") or {})
            obs["provenance"] = merged
    for status in body.get("statuses", []):
        if "outcome" not in status:
            rid = status.get("resource_id")
            empty = not any(
                o.get("resource_id") == rid for o in body.get("observations", [])
            )
            status["outcome"] = {"fetched": {"page_empty": empty}}
    return body


def json_bytes(obj):
    """The serialized size of `obj` in the exact encoding `send` uses."""
    return len(json.dumps(obj, separators=(",", ":")).encode("utf-8"))


def degrade_oversized(body):
    """spec/observation.md section 6's two-step degrade, performed for real
    on the bytes about to go out -- not declared by a fixture as a number.

    Step 1: an observation whose serialized size exceeds
    MAX_OBSERVATION_BYTES has its `provider_extra` replaced by exactly
    `{"_truncated": true, "_original_bytes": N}` (N = the serialized size of
    the provider_extra being discarded) and its provenance `completeness`
    set to "partial".

    Step 2: if it is STILL too large (an oversized `description`, say), the
    observation is omitted entirely, the resource's status entry gains
    a `degraded {local_id, bytes}` entry carrying its REAL measured size --
    one entry per omitted record, APPENDED, never overwriting an earlier
    one -- and every other observation on the page is emitted regardless:
    one bad event must never brick a resource.

    `degraded` sits BESIDE `outcome` and never replaces it (spec/observation
    .md section 6). A resource can be serving cached data and have dropped
    one record for size at the same time; writing the degrade into `outcome`
    erased the `stale {as_of}` the freshness table reads, which stamped that
    resource's perfectly good cached observations `live`.
    """
    kept = []
    for obs in body.get("observations", []):
        size = json_bytes(obs)
        if size > MAX_OBSERVATION_BYTES and isinstance(obs.get("provider_extra"), dict):
            obs["provider_extra"] = {
                "_truncated": True,
                "_original_bytes": json_bytes(obs["provider_extra"]),
            }
            obs["provenance"]["completeness"] = "partial"
            size = json_bytes(obs)
        if size > MAX_OBSERVATION_BYTES:
            for status in body.get("statuses", []):
                if status.get("resource_id") == obs.get("resource_id"):
                    status.setdefault("degraded", []).append(
                        {"local_id": obs.get("local_id"), "bytes": size}
                    )
            continue
        kept.append(obs)
    body["observations"] = kept
    return body


def build_body(run, action, body):
    """The steps every reply body goes through, in the only order that makes
    sense: fill in the fixture's defaults, then `pad` inflates fields to a
    real byte count (so a fixture can be genuinely oversized without carrying
    260 KB of literal JSON), then `degrade` applies the section 6 rule to
    whatever came out."""
    apply_defaults(run, body)
    for spec in action.get("pad", []):
        set_path(body, spec["path"], "x" * spec["bytes"])
    if action.get("degrade"):
        degrade_oversized(body)
    return body


class Adapter:
    def __init__(self, run):
        self.run = run
        self.last_sent_line = None
        self.deferred = []  # FIFO queue of held (unreplied) request dicts
        self.consumed = {
            op: [False] * len(rules) for op, rules in run.get("on", {}).items()
        }

    def send_line(self, line):
        sys.stdout.write(line)
        sys.stdout.flush()
        self.last_sent_line = line

    def send(self, obj):
        self.send_line(json.dumps(obj, separators=(",", ":")) + "\n")

    def run_actions(self, actions, req):
        for action in actions:
            kind = action["op"]
            if kind == "reply_ok":
                body = build_body(self.run, action, copy.deepcopy(action["body"]))
                self.send({"id": req["id"], "ok": body})
            elif kind == "reply_ok_raw":
                # `body_json` is raw JSON TEXT, written to the wire verbatim.
                # The only way this adapter can put a bare JSON *number*
                # where an Amount belongs (spec/money.md section 2 requires
                # that to be rejected at the deserializer) without the value
                # ever passing through a Python float on the way there.
                self.send_line(
                    '{"id":%d,"ok":%s}\n' % (req["id"], action["body_json"])
                )
            elif kind == "reply_err":
                self.send(
                    {
                        "id": req["id"],
                        "err": {
                            "code": action["code"],
                            "message": action.get("message", ""),
                            "detail": action.get("detail"),
                        },
                    }
                )
            elif kind == "reply_ok_id":
                # Explicit id, not the requester's -- used for never-issued
                # id and duplicate-answer hostile replies.
                self.send({"id": action["id"], "ok": build_body(self.run, action, copy.deepcopy(action["body"]))})
            elif kind == "reply_err_id":
                self.send(
                    {
                        "id": action["id"],
                        "err": {
                            "code": action["code"],
                            "message": action.get("message", ""),
                            "detail": action.get("detail"),
                        },
                    }
                )
            elif kind == "defer":
                # Hold this request unanswered; a later rule must reply to
                # it via reply_deferred. Used to build out-of-order replies.
                self.deferred.append(req)
            elif kind == "reply_deferred":
                which = action.get("which", "oldest")
                target = self.deferred.pop(0) if which == "oldest" else self.deferred.pop()
                self.send({"id": target["id"], "ok": build_body(self.run, action, copy.deepcopy(action["body"]))})
            elif kind == "replay_last_reply":
                # Resend the exact previous frame byte-for-byte -- the
                # cheapest way to build a "reply to an already-answered id"
                # (DuplicateId) without knowing the id's numeric value.
                assert self.last_sent_line is not None, "replay_last_reply before any reply was sent"
                sys.stdout.write(self.last_sent_line)
                sys.stdout.flush()
            elif kind == "stdout_raw":
                _write_raw(action["text"].encode("utf-8"))
            elif kind == "stdout_raw_bytes":
                # Literal byte values (0-255) for deliberately invalid UTF-8.
                _write_raw(bytes(action["bytes"]))
            elif kind == "stderr":
                sys.stderr.write(action["text"])
                sys.stderr.flush()
            elif kind == "exit":
                sys.stdout.flush()
                sys.exit(action.get("code", 1))
            elif kind == "reply_with_decimal_amounts":
                self._reply_with_decimal_amounts(action, req)
            else:
                raise ValueError(f"fixture bug: unknown action op {kind!r}")

    def _reply_with_decimal_amounts(self, action, req):
        """The one piece of real work in this file. `provider_payload_json`
        is the RAW upstream payload as a JSON-encoded string (never nested
        JSON -- that would let the outer json.load's default float parser
        touch it first). It is parsed here with parse_float=Decimal so every
        float token in the provider's payload becomes an exact Decimal, not
        an IEEE-754 double. Each entry in `amounts` pulls one Decimal out of
        that payload by path and formats it with format(d, 'f') -- never
        str(Decimal(...)), which reproduces exponent notation for small
        magnitudes (spec/money.md section 3) -- then splices the resulting
        {"asset", "amount"} wire pair into a deep copy of `body` at
        `insert_into`.
        """
        payload = json.loads(action["provider_payload_json"], parse_float=Decimal)
        body = copy.deepcopy(action["body"])

        for spec in action["amounts"]:
            value = get_path(payload, spec["payload_path"])
            if isinstance(value, int) and not isinstance(value, bool):
                # A bare JSON integer (no '.', no exponent) never reaches
                # parse_float at all -- Python's json scanner hands it to
                # int() directly, which is exact and arbitrary-precision.
                # int -> Decimal is always exact, so this is still safe.
                value = Decimal(value)
            if not isinstance(value, Decimal):
                # Fixture bug guard: the path pointed at something that was
                # neither a float token nor a bare integer (e.g. a string).
                raise TypeError(
                    f"expected a Decimal or int (numeric token) at {spec['payload_path']!r}, "
                    f"got {type(value).__name__}={value!r}"
                )
            if spec.get("negate") and value != 0:
                # Sign normalization is the ADAPTER's job, applied before the
                # money layer ever sees the value (spec/money.md section 6) --
                # e.g. FDX reports an unsigned amount plus a separate
                # debitCreditMemo field; this flips DEBIT to negative here,
                # never inside sumer-money itself. Zero is left alone: money.md
                # rejects a signed zero (SignOnZero), so negating a zero
                # amount would produce a string the wire grammar forbids.
                #
                # `copy_negate`, NOT `-value`. Unary minus is a context-aware
                # decimal operation: under Python's default 28-digit context
                # it ROUNDS, so -Decimal("12345678901234567890123456789.01")
                # silently becomes -12345678901234567890123456790. No float
                # is involved -- it is the decimal context alone. copy_negate
                # only flips the sign bit and is context-independent, so it is
                # exact at any precision. (`_EXACT_CONTEXT` traps the rounding
                # too, but the right operation is the fix; the trap is the net
                # under it.)
                value = value.copy_negate()
            # format(d, "f") -- never str(Decimal(...)), which reproduces
            # exponent notation for small magnitudes (spec/money.md section 3).
            # __format__ reads the coefficient directly and is likewise
            # context-independent, so it does not round either.
            amount_str = format(value, "f")
            set_path(body, spec["insert_into"], {"asset": spec["asset"], "amount": amount_str})
        self.send({"id": req["id"], "ok": build_body(self.run, action, body)})

    def handle(self, req):
        op = req.get("op")
        if op == "hello":
            hello = self.run.get("hello", DEFAULT_HELLO)
            if "err" in hello:
                self.send({"id": req["id"], "err": hello["err"]})
            else:
                self.send({"id": req["id"], "ok": hello})
            return

        rules = self.run.get("on", {}).get(op)
        if rules is None:
            # Undeclared op: never closes the connection (spec/wire.md b).
            self.send(
                {
                    "id": req["id"],
                    "err": {
                        "code": "unsupported",
                        "message": "operation not supported by this adapter",
                        "detail": {"op": op},
                    },
                }
            )
            return

        params = req.get("params", {}) or {}
        flags = self.consumed[op]
        for i, rule in enumerate(rules):
            if flags[i]:
                continue
            if subset_match(rule.get("when"), params):
                if rule.get("consume", True):
                    flags[i] = True
                self.run_actions(rule["do"], req)
                return

        sys.stderr.write(
            f"[fake_adapter] FIXTURE BUG: no matching rule for op={op!r} params={params!r}\n"
        )
        self.send(
            {
                "id": req["id"],
                "err": {
                    "code": "internal",
                    "message": "fixture has no scripted rule for this request",
                    "detail": {"op": op, "params": params},
                },
            }
        )


def main():
    fixture = load_fixture()
    run = pick_run(fixture)

    pre = run.get("pre_hello_stdout")
    if pre is not None:
        # Deliberate PreHelloOutput violation: written before anything is
        # even read from stdin, let alone before a hello reply is sent.
        _write_raw(pre.encode("utf-8"))

    adapter = Adapter(run)
    for line in sys.stdin:
        line = line.rstrip("\n")
        if not line:
            continue
        req = json.loads(line)
        try:
            adapter.handle(req)
        except (BrokenPipeError, OSError):
            return
    return


if __name__ == "__main__":
    main()
