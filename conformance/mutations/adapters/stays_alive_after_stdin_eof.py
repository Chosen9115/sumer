"""spec/wire.md section 7: an adapter MUST exit when its stdin reaches EOF.

This one answers the whole crawl honestly and then refuses to leave. The
refusal is written as the exact mistake the spec's Python row names -- at
EOF `sys.stdin.readline()` returns `""` forever, and a loop that reads that
as "nothing to read *yet*" never ends -- rather than as a bare sleep,
because the bug this mutant exists to catch is a read loop, not a nap. The
sleep is only here so the loop does not peg a core while the host waits out
its deadline.

Nothing is written after the crawl: the point is that a process which will
not close is unjudgeable whether it emits anything or not.
"""
import sys
import time

import _wrapper as w

w.fake_adapter.main()

# `fake_adapter.main()` returned because `for line in sys.stdin` ended at
# EOF -- a conforming adapter exits here.
while True:
    if not sys.stdin.readline():
        time.sleep(0.05)
