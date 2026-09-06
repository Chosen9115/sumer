"""The subtler form: honest ids to a connection that opened with
resources.list, junk-prefixed ids to any other."""
import _wrapper as w

_handle = w.fake_adapter.Adapter.handle
_build = w.fake_adapter.build_body
_seen_discovery = []


def handle(self, req):
    if req.get("op") == "resources.list":
        _seen_discovery.append(True)
    return _handle(self, req)


def build_body(run, action, body):
    body = _build(run, action, body)
    if not _seen_discovery:
        for obs in body.get("observations", []):
            if "local_id" in obs:
                obs["local_id"] = "undiscovered:" + obs["local_id"]
    return body


w.fake_adapter.Adapter.handle = handle
w.fake_adapter.build_body = build_body
w.fake_adapter.main()
