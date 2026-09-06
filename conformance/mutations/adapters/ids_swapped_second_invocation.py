"""On the SECOND invocation the distinct local_ids in a reply are rotated by
one: the same SET of ids, handed out attached to each other's records."""
import _wrapper as w

_build = w.fake_adapter.build_body


def build_body(run, action, body):
    body = _build(run, action, body)
    if w.INVOCATION > 1:
        obs = [o for o in body.get("observations", []) if "local_id" in o]
        distinct = list(dict.fromkeys(o["local_id"] for o in obs))
        swapped = dict(zip(distinct, distinct[1:] + distinct[:1]))
        for o in obs:
            o["local_id"] = swapped[o["local_id"]]
    return body


w.fake_adapter.build_body = build_body
w.fake_adapter.main()
