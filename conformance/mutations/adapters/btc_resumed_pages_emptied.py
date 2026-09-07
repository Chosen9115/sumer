"""Every RESUMED history page comes back empty, successful and terminal.

The break: a `history.read` carrying a page cursor gets `observations: []`,
`fetched { page_empty: true }` and `next: null` on its way out. A first
read (no cursor) is untouched, so the wallet looks perfectly healthy right
up until a host resumes from a cursor it persisted -- at which point every
transaction above that cursor is silently dropped, and the host is told the
read succeeded and drained. Total data loss, reported as a clean sync.

This is the shape that passed the entire live checker. `live_bitcoin.rs`
§9 asserted only that nothing came back at or below the cursor, which an
empty page satisfies perfectly: a property of the observations a reply
happened to carry cannot see the reply that carries none.

**This wrapper is driven by the LIVE check, not by the mutation battery**
(`tests/mutations.rs` runs recorded fixtures; nothing recorded exercises a
resume of the real adapter). There is deliberately no `mutations/*.json`
manifest for it: the battery requires one fixture per mutant under
`conformance/cases/`, and inventing one would be a fixture that proves
nothing. It is run by hand, and by whoever next doubts §9 has teeth:

    SUMER_LIVE=1 \
    SUMER_LIVE_WRAPPER=conformance/mutations/adapters/btc_resumed_pages_emptied.py \
      cargo test -p sumer-conformance --test live_bitcoin -- --ignored --nocapture

DECLARED, exactly: the live check must fail at §9b -- "resuming at (H, T)
dropped N of the M confirmed transactions the uninterrupted read placed
above it" -- and at nothing else. Everything before §9b must still pass:
the handshake, discovery, balances, the floor, the ordering, the
cross-endpoint reconciliation, and §9a, all of which this break leaves
untouched. A failure anywhere else means this mutant got blunt and stopped
proving what it claims.
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
    if reply.get("id") not in _resumed or "ok" not in reply:
        return reply
    if "observations" not in reply["ok"]:
        return reply
    reply["ok"]["observations"] = []
    for status in r.statuses(reply):
        # An empty page, honestly declared -- and drained, so the host has
        # no reason to ask again.
        status["outcome"] = {"fetched": {"page_empty": True}}
        if isinstance(status.get("page"), dict):
            status["page"]["next"] = None
    return reply


r._pump = _pump  # noqa: SLF001 -- the documented way a wrapper hooks the pump
r.run(rewrite)
