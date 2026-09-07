"""Every tombstone the adapter emits is deleted on its way out.

The wallet then reads as "that transaction is still there", which is what
an adapter that never noticed the reorg would say -- and the LIVE SET is
identical either way, because a tombstoned record is not live. Only
retaining the whole history, in order, can tell the two apart.
"""
import _btc_rewrite as r


def rewrite(reply):
    kept = [o for o in r.observations(reply) if o.get("state") != "tombstoned"]
    if "ok" in reply and "observations" in reply["ok"]:
        reply["ok"]["observations"] = kept
    return reply


r.run(rewrite)
