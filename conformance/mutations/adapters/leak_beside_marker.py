"""spec/observation.md section 6 step 1 says provider_extra is REPLACED by
the truncation marker. This one leaves `leak_bytes` of the discarded payload
sitting beside it -- the leak the fixture's script sets, so one wrapper
serves both the 70 KB form (still over the cap) and the 8-byte form (under
it, and visible only to an exact comparison)."""
import json
import os

import _wrapper as w

_LEAK = json.load(open(os.environ["SUMER_FIXTURE"], encoding="utf-8"))
_LEAK = _LEAK["script"]["runs"][0].get("leak_bytes", 8)
_degrade = w.fake_adapter.degrade_oversized


def degrade_oversized(body):
    body = _degrade(body)
    for obs in body.get("observations", []):
        extra = obs.get("provider_extra")
        if isinstance(extra, dict) and extra.get("_truncated"):
            extra["bulk_provider_payload"] = "x" * _LEAK
    return body


w.fake_adapter.degrade_oversized = degrade_oversized
w.fake_adapter.main()
