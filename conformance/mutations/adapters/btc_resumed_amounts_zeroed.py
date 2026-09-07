"""Every RESUMED observation's `amount` leaves as zero satoshis.

The break: a `history.read` carrying a page cursor comes back with the
right `local_id`s, the right `block_height`s, the right `state` and
`posting`, the right count, the right order, a successful terminal status
-- and every figure replaced by `{"asset": "sat", "amount": "0"}`. A first
read (no cursor) is untouched, so a host that never resumes sees a
perfectly healthy wallet; a host that resumes from a cursor it persisted
writes zeroes over history it had already read correctly, and is told the
read succeeded.

This is the shape §9b could not see. §9b compared the SET of `local_id`s
the resume delivered against the set the uninterrupted read owed, so a
break that keeps every id and rewrites what the ids are WORTH satisfies it
exactly: identity and state were checked, money never was. And nothing
upstream covers it either -- the cross-endpoint reconciliation in §8 runs
against the INITIAL drain and never sees a resumed value at all, and §9a
only asks where each observation sits relative to the cursor. The whole
resumed history could be zeroes and the live check was green.

`fees` and `raw_sign` are left alone on purpose. This mutant is the
minimum: one field, on one kind of reply. A §9b that only checked
`amount` -- or checked `fees` for presence rather than by value, which is
the degradation the offline suite already shipped once -- would still be
killed by this one, so the check is written to compare all three by value
and this wrapper does not pretend to prove the other two.

**This wrapper is driven by the LIVE check, not by the mutation battery**
(`tests/mutations.rs` runs recorded fixtures; nothing recorded exercises a
resume of the real adapter). As with `btc_resumed_pages_emptied.py`, there
is deliberately no `mutations/*.json` manifest: the battery requires one
fixture per mutant under `conformance/cases/`, and inventing one would be
a fixture that proves nothing.

DECLARED, exactly: the live check must fail at §9b -- "resuming at (H, T)
returned <id> carrying 0 sat, but the uninterrupted read a moment ago said
..." -- and at nothing else. Everything before §9b must still pass: the
handshake, discovery, the balance lines, the floor, the ordering, the
cross-endpoint reconciliation, and §9a, none of which this break touches.
A failure anywhere else means this mutant got blunt and stopped proving
what it claims, which is why the section is declared and CHECKED:

    SUMER_LIVE=1 \
    SUMER_LIVE_WRAPPER=mutations/adapters/btc_resumed_amounts_zeroed.py \
    SUMER_LIVE_EXPECT='§9b' \
      cargo test -p sumer-conformance --test live_bitcoin -- --ignored --nocapture

The run PASSES when the mutant is killed at §9b and FAILS otherwise --
when it survives, when something else catches it first, and (since a
skipped mutant is not a kill) when the provider rate-limits the run.
"""
import json
import sys

import _btc_rewrite as r

# Request ids of `history.read`s that carried a page cursor.
_resumed = set()


def _pump(child):
    """Our stdin to the adapter's, noting which reads are resumes."""
    try:
        for line in sys.stdin:
            try:
                request = json.loads(line)
            except ValueError:
                request = None
            if isinstance(request, dict) and request.get("op") == "history.read":
                queries = (request.get("params") or {}).get("resources") or []
                if any(isinstance(q, dict) and q.get("page") for q in queries):
                    _resumed.add(request.get("id"))
            child.stdin.write(line)
            child.stdin.flush()
    except (BrokenPipeError, ValueError):
        pass
    finally:
        try:
            child.stdin.close()
        except (BrokenPipeError, ValueError):
            pass


def rewrite(reply):
    if reply.get("id") not in _resumed:
        return reply
    for o in r.observations(reply):
        if isinstance(o.get("amount"), dict):
            o["amount"] = {"asset": o["amount"].get("asset", "sat"), "amount": "0"}
    return reply


r._pump = _pump  # noqa: SLF001 -- the documented way a wrapper hooks the pump
r.run(rewrite)
