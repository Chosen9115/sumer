#!/usr/bin/env python3
"""Drive a MULTI-PHASE Bitcoin corpus as one adapter command line.

    two_phase.py <runs> <adapter> [adapter args...]

`<runs>` is a comma-separated list of corpus run indices, e.g. `0,1`. Every
index but the last is a PRIMING lifetime: this script spawns the adapter
over that run, drives one full crawl (hello, balances.read, history.read to
`next: null`) so the adapter writes its balance cache, and waits for it to
exit. The LAST index is then spawned with our own stdin and stdout, so the
conformance crawl talks to it directly.

Why this exists: the stale case is two adapter lifetimes over one
`--state-dir`, and the conformance runner spawns exactly one argv per
execution. This makes "the phase-2 adapter, with a state file phase 1
actually wrote" a single command line.

Why the state directory is FRESH on every launch: the suite runs each case
twice and compares the two executions (A9), and a directory carried between
them would let phase 2 of the second execution start from a state phase 1
of the first left behind. Re-priming from an empty directory makes both
executions the same two-lifetime scenario, which is what the fixture
describes.

The adapter argv must NOT carry `--state-dir`; this script appends one.
"""
import json
import os
import shutil
import subprocess
import sys
import tempfile


def crawl(adapter_argv, env):
    """One full crawl against a priming lifetime, so state is written."""
    child = subprocess.Popen(
        adapter_argv, env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True
    )

    def call(request_id, op, params):
        child.stdin.write(json.dumps({"id": request_id, "op": op, "params": params}) + "\n")
        child.stdin.flush()
        line = child.stdout.readline()
        if not line:
            raise SystemExit(f"two_phase.py: {op}: the adapter closed stdout")
        reply = json.loads(line)
        if "ok" not in reply:
            raise SystemExit(f"two_phase.py: {op} failed: {line.strip()}")
        return reply["ok"]

    call(0, "hello", {"protocol": ["1"]})
    listed = call(1, "resources.list", {})["resources"]
    resources = [r["resource_id"] for r in listed]
    call(2, "balances.read", {"resource_ids": resources})
    request_id = 3
    for resource_id in resources:
        page = None
        # Draining every page, not just reading the first: run0 exists to
        # prove a whole crawl over this corpus succeeds before run1 breaks
        # it. (The only state a run leaves behind is the balance cache,
        # written by the `balances.read` above.)
        while True:
            query = {"resource_id": resource_id}
            if page is not None:
                query["page"] = page
            reply = call(request_id, "history.read", {"resources": [query]})
            request_id += 1
            statuses = reply.get("statuses", [])
            page = statuses[0].get("page", {}).get("next") if statuses else None
            if page is None:
                break
    child.stdin.close()
    child.wait()


def main():
    if len(sys.argv) < 3:
        raise SystemExit(__doc__)
    runs = sys.argv[1].split(",")
    adapter_argv = sys.argv[2:]
    if "--state-dir" in adapter_argv:
        raise SystemExit("two_phase.py: the adapter argv must not carry --state-dir")

    state = tempfile.mkdtemp(prefix="sumer-btc-phase-")
    argv = adapter_argv + ["--state-dir", state]
    try:
        for run in runs[:-1]:
            crawl(argv, {**os.environ, "SUMER_FIXTURE_RUN": run})
        served = subprocess.run(argv, env={**os.environ, "SUMER_FIXTURE_RUN": runs[-1]})
        return served.returncode
    finally:
        shutil.rmtree(state, ignore_errors=True)


if __name__ == "__main__":
    sys.exit(main())
