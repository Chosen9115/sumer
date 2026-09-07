"""Shared plumbing for the mutation battery's BITCOIN wrappers.

The reference Python adapter's wrappers (`_wrapper.py`) import it and
replace one of its functions. That cannot work for a compiled adapter, and
patching its corpus would not be a mutation of the adapter at all: it would
mutate the PROVIDER, and the expected output would legitimately change.

So a Bitcoin wrapper is a MAN IN THE MIDDLE. It spawns the real adapter
(the argv the mutant's manifest names, handed to it on its own command
line), forwards stdin to it untouched, and rewrites the replies it writes
on their way out. What that proves is that the case can still see the
difference -- nothing about the adapter's internals, which are not mutated
here at all.

Stream discipline matters, because the suite judges it (`spec/wire.md` §7,
and the `StdinEofIgnored` / `StdoutHeldOpen` violations): stdin EOF closes
the child's stdin, and this process exits as soon as the child's stdout
ends, so nothing holds the pipe open after the adapter is gone.
"""
import json
import subprocess
import sys
import threading


def _pump(child):
    """Our stdin to the adapter's, EOF included."""
    try:
        for line in sys.stdin:
            child.stdin.write(line)
            child.stdin.flush()
    except (BrokenPipeError, ValueError):
        pass
    finally:
        try:
            child.stdin.close()
        except (BrokenPipeError, ValueError):
            pass


def run(rewrite):
    """Spawn `sys.argv[1:]`, passing every reply through `rewrite(reply)`."""
    child = subprocess.Popen(
        sys.argv[1:], stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True
    )
    threading.Thread(target=_pump, args=(child,), daemon=True).start()
    for line in child.stdout:
        line = line.strip()
        if not line:
            continue
        sys.stdout.write(json.dumps(rewrite(json.loads(line))) + "\n")
        sys.stdout.flush()
    sys.exit(child.wait())


def observations(reply):
    """The observations of an `ok` reply, or an empty list."""
    return reply.get("ok", {}).get("observations", []) if "ok" in reply else []


def statuses(reply):
    return reply.get("ok", {}).get("statuses", []) if "ok" in reply else []
