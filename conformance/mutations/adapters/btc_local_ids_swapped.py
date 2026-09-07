"""The distinct local_ids in each reply are rotated by one: the same SET of
ids, handed out attached to each other's transactions.

Every id the wallet ever had is still emitted, exactly once, and the live
set is unchanged -- so only comparing each record against the id it was
supposed to carry can see this.
"""
import _btc_rewrite as r


def rewrite(reply):
    obs = [o for o in r.observations(reply) if "local_id" in o]
    distinct = list(dict.fromkeys(o["local_id"] for o in obs))
    if len(distinct) > 1:
        swapped = dict(zip(distinct, distinct[1:] + distinct[:1]))
        for o in obs:
            o["local_id"] = swapped[o["local_id"]]
    return reply


r.run(rewrite)
