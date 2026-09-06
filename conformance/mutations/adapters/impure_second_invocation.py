"""local_id is pure; the record it names is not. On the SECOND invocation
every observation keeps its local_id and changes its description."""
import _wrapper as w

_build = w.fake_adapter.build_body


def build_body(run, action, body):
    body = _build(run, action, body)
    if w.INVOCATION > 1:
        for obs in body.get("observations", []):
            if "local_id" in obs:
                obs["description"] = "SECOND INVOCATION, DIFFERENT RECORD, SAME local_id"
    return body


w.fake_adapter.build_body = build_body
w.fake_adapter.main()
