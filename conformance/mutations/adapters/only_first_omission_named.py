"""Reintroduces the false-retraction bug this project has now shipped
twice: when one page drops more than one oversized record, only the FIRST
is named in the resource's `degraded` list and every later one becomes an
unexplained absence -- neither in `observations` nor in `degraded`. That is
exactly what `core/host`'s single-valued `degraded` field did before it
became a list, and what the Bitcoin reference adapter's `get_or_insert` did
in `map.rs`; both are why `degraded` is a list at all
(spec/observation.md section 6).

`oversized_observation.json` drops big-untruncatable AND
big-untruncatable-2 from the same page. The real `degrade_oversized` still
runs -- both records still vanish from `observations`, matching the
fixture's `omitted: true` pair -- and this wrapper only clips the naming
afterward, which is the one thing a single-valued (or first-wins) degraded
field cannot represent."""
import _wrapper as w

_degrade = w.fake_adapter.degrade_oversized


def degrade_oversized(body):
    body = _degrade(body)
    for status in body.get("statuses", []):
        degraded = status.get("degraded")
        if isinstance(degraded, list) and len(degraded) > 1:
            status["degraded"] = degraded[:1]
    return body


w.fake_adapter.degrade_oversized = degrade_oversized
w.fake_adapter.main()
