"""stale{as_of} becomes a plain fetched.

The balances still ride out at their remembered figures, so the numbers
look right; what is erased is that they are REMEMBERED rather than read,
which is the whole difference between a cache and a live answer.
"""
import _btc_rewrite as r


def rewrite(reply):
    for s in r.statuses(reply):
        if isinstance(s.get("outcome"), dict) and "stale" in s["outcome"]:
            s["outcome"] = {"fetched": {"page_empty": False}}
    return reply


r.run(rewrite)
