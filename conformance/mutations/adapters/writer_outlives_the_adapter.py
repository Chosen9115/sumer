"""spec/wire.md section 7: exiting means dropping what you are still holding.

This one answers the whole crawl honestly and then exits -- but not before
forking a writer that inherited its stdout. The parent is reaped
immediately, so a host that treats "the process exited" as "the stream
ended" concludes the connection closed cleanly; the write end is still
open, and the frame the writer puts on it afterwards arrives after that
verdict was already latched.

The sleep is longer than the host's drain bound on purpose: the point is
not the garbage (which the reader would catch if it were still there for
it) but that the host cannot certify it read everything.
"""
import os
import sys
import time

import _wrapper as w

w.fake_adapter.main()

# `fake_adapter.main()` returned at stdin EOF. A conforming adapter is done
# here -- including whatever it forked.
if os.fork() == 0:
    time.sleep(1.5)
    sys.stdout.write("} not a frame, and not the answer to anything\n")
    sys.stdout.flush()
    os._exit(0)
