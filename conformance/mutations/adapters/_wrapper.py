"""Shared plumbing for the mutation battery's adapter wrappers.

A wrapper is a BROKEN adapter, not a simulation of one: it imports the real
`fake_adapter`, replaces one function with a subtly wrong version, and runs
it. Everything a wrapper cannot express as a static script patch lives here
-- notably `INVOCATION`, which is how a mutation says "behave differently on
the second launch of this process" (two independent launches are exactly
what A9 compares, and neither the wire nor the fixture can carry state
across them).
"""
import os
import pathlib
import sys

_ROOT = pathlib.Path(__file__).resolve().parents[3]
sys.path.insert(0, str(_ROOT / "adapters" / "fake"))

import fake_adapter  # noqa: E402


def _count_invocation():
    """1 on this fixture's first process, 2 on the next, ... Counted in a
    file beside the (temporary, per-mutant) fixture, because the host clears
    the environment down to an allowlist and nothing else survives a spawn."""
    marker = pathlib.Path(os.environ["SUMER_FIXTURE"] + ".invocations")
    seen = len(marker.read_text()) if marker.exists() else 0
    marker.write_text("x" * (seen + 1))
    return seen + 1


INVOCATION = _count_invocation()
