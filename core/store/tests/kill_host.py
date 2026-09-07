#!/usr/bin/env python3
"""Drive `adapters/fake/fake_adapter.py` and SIGKILL the HOST at a chosen
point in the conversation.

    kill_host.py <mode> <n> <fixture> <run>

This process is spawned BY the host as its adapter, so its parent is the
host: `os.getppid()` is exactly the process the restart cases need to kill,
and SIGKILL is exactly the signal they name -- no shutdown hook runs, no
destructor, no flush.

Two modes, for the two restart cases:

  before_request <n>   Proxy everything, but when REQUEST number n arrives
                       from the host, kill the host instead of forwarding
                       it. This is DETERMINISTIC: the host only issues
                       request n after it has committed everything reply
                       n-1 earned. So `before_request 6` on a two-page
                       history means "page 1 is committed and durable; page
                       2 never happened".

  after_reply <n>      Proxy everything, and the instant REPLY number n has
                       been flushed to the host, kill the host. The host
                       has received that reply and has not yet committed
                       the transaction it opens on it.

`after_reply` is the one point in the run where no wire event separates
"received" from "committed", so it is a margin rather than an interlock:
the kill is delivered in microseconds while the host still has to hash and
insert the whole final page inside one `synchronous = FULL` transaction.
The fixtures that use it put thousands of observations on that page
precisely to make the margin large. This is stated rather than hidden --
the alternative would be a hook in production code that exists only for a
test.

The fake adapter is a child of this process, so its own SUMER_FIXTURE /
SUMER_FIXTURE_RUN come from here rather than from the host's environment,
which is deliberately allowlisted and would not forward them.
"""
import os
import signal
import subprocess
import sys


def main():
    if len(sys.argv) != 5:
        raise SystemExit(__doc__)
    mode, count, fixture, run = sys.argv[1], int(sys.argv[2]), sys.argv[3], sys.argv[4]
    if mode not in ("before_request", "after_reply"):
        raise SystemExit(f"kill_host.py: unknown mode {mode!r}")

    here = os.path.dirname(os.path.abspath(__file__))
    adapter = os.path.join(here, "..", "..", "..", "adapters", "fake", "fake_adapter.py")
    child = subprocess.Popen(
        [sys.executable, os.path.normpath(adapter)],
        env={**os.environ, "SUMER_FIXTURE": fixture, "SUMER_FIXTURE_RUN": run},
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
    )

    host = os.getppid()
    requests = 0
    replies = 0
    for line in sys.stdin.buffer:
        requests += 1
        if mode == "before_request" and requests == count:
            os.kill(host, signal.SIGKILL)
            # The host is gone; nothing downstream of this matters.
            child.kill()
            return 0
        child.stdin.write(line)
        child.stdin.flush()
        reply = child.stdout.readline()
        if not reply:
            break
        sys.stdout.buffer.write(reply)
        sys.stdout.buffer.flush()
        replies += 1
        if mode == "after_reply" and replies == count:
            os.kill(host, signal.SIGKILL)
            child.kill()
            return 0
    child.stdin.close()
    child.wait()
    return 0


if __name__ == "__main__":
    sys.exit(main())
