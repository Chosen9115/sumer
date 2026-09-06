# Sumer wire protocol

Normative. If your adapter (any language) speaks to a Sumer host, this is the
contract. `sumer-host` (`core/host/`) is the reference implementation; this
document, not that code, is authoritative on disagreement. See `spec/money.md`
for the amount and asset grammar carried inside this envelope, and
`spec/observation.md` for the shape of what a read reply carries.

An adapter is a separate OS process. It does not need to be Rust, does not link
against any Sumer crate, and is conformant exactly to the extent that it emits
and accepts the bytes described here.

## 1. Transport and envelope

Framing is JSON Lines over the adapter's stdin/stdout: one UTF-8 JSON object per
LF (`\n`)-terminated line, in both directions. No other framing (no
length-prefix, no HTTP, no null-terminated records) is defined by this version
of the protocol.

The host is always the requester. In this milestone, an adapter never writes a
frame the host did not ask for — there is no push channel, no subscription
frame, and no adapter-initiated notification.

Request:

    {"id": 7, "op": "balances.read", "params": {...}}

Reply (success):

    {"id": 7, "ok": {...}}

Reply (failure):

    {"id": 7, "err": {"code": "...", "message": "...", "detail": {...}}}

`id` is a JSON number carrying a `u64`, drawn from a counter the host owns and
increments once per request. Every frame — request or reply — has exactly one
of `ok` or `err` at the top level when it is a reply, and exactly `op`+`params`
when it is a request; a frame satisfying both shapes, or neither, is malformed.

### Caps

| Cap | Value | Counted |
|---|---|---|
| `MAX_FRAME_BYTES` | 1,048,576 (1 MiB) | before the trailing LF |
| `MAX_OBSERVATION_BYTES` | 65,536 (64 KiB) | per serialized observation inside a reply — see `spec/observation.md` §6 |

A frame at or under `MAX_FRAME_BYTES` that still contains an oversized
observation is not a wire-level violation; it is handled inside the reply body
per `spec/observation.md`. A frame that itself exceeds `MAX_FRAME_BYTES` is a
wire-level violation regardless of what is inside it.

## 2. Fatal frames: no resync

A frame that is oversized, not valid UTF-8, or not parseable JSON is fatal. The
host kills the adapter process immediately. It does not attempt to recover and
read on.

**Why there is no resync attempt.** JSON Lines has no self-delimiting recovery
token once a line's structure is violated. A truncated JSON object gives no
signal for where the next valid frame begins; the only structural landmark is
the next LF byte, and that byte offers no guarantee — it may fall inside a
string that itself straddles the corruption, or the writer may have died
mid-frame with no LF ever coming. Any attempt to "skip to the next line and
keep going" can silently swallow a real frame or split one in two, which
desynchronizes request/reply correlation invisibly — exactly the failure mode
`id` tombstoning (§4) exists to make loud instead of silent. A clean kill is the
only response that does not risk misattributing a reply.

This case is a `ProtocolViolation` host-side outcome (`OversizeFrame`,
`NotJson`, or `NonUtf8`), never a wire `err` — the connection is already
considered unrecoverable, so there is nothing to reply to.

## 3. stdout / stderr discipline

`stdout` is framed JSON and nothing else, for the lifetime of the process.
`stderr` is free-form: the host drains it, caps its volume, and never parses
it. The adapter is responsible for redacting its own secrets before writing to
stderr — the host does not scrub adapter log output.

**An adapter must not write to stdout before its hello reply.** Any byte
written to stdout ahead of the hello reply (§4) is a fatal `PreHelloOutput`
protocol violation, and it is the single most common way a JSON Lines peer
breaks in practice: a runtime, framework, or transitively-pulled dependency
prints something on startup, before the adapter's own code has run a single
line, and that print lands on the same file descriptor the host is about to
read framed JSON from. The host has no way to distinguish a banner from a
malformed frame — the very first bytes on stdout are read as the hello reply,
so a banner corrupts the handshake, not a later request.

This is not hypothetical per language. The concrete leak sources and fixes:

| Runtime | What leaks to stdout by default | Fix |
|---|---|---|
| Python | Not `warnings.warn` itself (that targets stderr by default) — the real risk is a leftover `print()` call, or a C-extension dependency that prints a version/telemetry banner on `import` straight to file descriptor 1, bypassing Python's own `sys.stdout` object. | Before importing anything beyond the standard library, `os.dup2` the real fd 1 to a saved fd, then redirect fd 1 itself to `/dev/null` or a buffer. Restore the real fd only for the code path that writes JSONL frames, and only after hello is queued to be the first thing sent. |
| Node.js | `process.emitWarning` (deprecation notices) targets stderr by default, but many popular packages call `console.log` directly for a startup banner, a "check out our docs" notice, or an anonymous-telemetry opt-out message — `console.log` always targets stdout. | At process start, before `require`-ing anything else, monkey-patch or buffer `process.stdout.write`; release the real stream only once the hello reply is the next thing to be written. |
| JVM | `System.out` is the default target for a great deal of "helpful" framework output (banners, `System.out.println` debug statements left in a dependency); only some logging frameworks default their console handler to stderr, and that is a per-framework choice, not a JVM guarantee. | Call `System.setOut` to a discarded or buffered `PrintStream` at the very top of `main`, before any dependency has a chance to run static initializers; keep a private reference to the original `System.out` (captured before the swap) for the JSONL writer. |

The general technique is the same regardless of language: capture and redirect
the real stdout handle as the first statement your process executes, and only
reconnect it — exclusively for framed JSON — once you are ready to send hello.

## 4. Handshake

The host always sends `id: 0` first:

    {"id": 0, "op": "hello", "params": {"protocol": ["1"]}}

The adapter replies with either an `ok` describing itself:

    {"id": 0, "ok": {
      "protocol": "1",
      "adapter_id": "...",
      "adapter_version": "...",
      "capabilities": ["resources.list", "balances.read", "history.read", "status.read"],
      "local_id_derivation": "<name>@<version>",
      "max_in_flight": 1
    }}

or an `err` with `code: "unsupported_protocol"` if none of the host's offered
versions are acceptable. `max_in_flight` is a `u32`; an adapter that omits it
gets the default of `1` (§7). `local_id_derivation` names and versions the pure
function the adapter uses to compute `local_id` — see `spec/observation.md` §5.

No frame may precede this exchange, and no adapter-authored byte may precede
this exchange's reply (§3).

## 5. Capabilities

Exactly four read capabilities, plus `hello`:

- `resources.list`
- `balances.read`
- `history.read`
- `status.read`

There is no `execute()` in this milestone (see `constitution/FOUNDING_PLAN.md`
§7 and §10 — execution semantics are a later milestone's gate, not something
this contract freezes early).

An op the adapter did not declare in its hello `capabilities` returns
`err.code = "unsupported"` with `detail.op` set, on that same connection. This
**never** closes the connection — an adapter that does not support
`history.read` still answers `balances.read` requests normally afterward.
Closing the connection over an unsupported op would make one capability probe
indistinguishable from a fatal protocol violation, which it is not.

## 6. Id lifecycle

`id` is a `u64` drawn from a **monotonic counter that is never reused for the
process lifetime.** The host tracks three states per id:

| State | Meaning | A reply naming this id |
|---|---|---|
| **issued** | Sent, no reply yet, deadline not expired | Matched normally; resolves the pending request |
| **tombstoned** | Deadline expired; host has already returned `Timeout` to the caller | Counted, then **silently discarded**. The connection survives. |
| **never-issued** | Above the counter's high-water mark, or a gap that was never handed out; also covers an id already answered once (**duplicate**) | Fatal `ProtocolViolation` (`UnknownId` or `DuplicateId`). **Kills the process.** |

**Why ids are never reused.** Consider the alternative: the host recycles a
small id space, and there is no cancel frame (there is not — see §7). A
request for account A's balance is sent as id 7; the host gives up on it at
the deadline and, being finite on some request-id budget, later reuses id 7
for a request for account B's balance. The adapter, still working through a
backlog or having genuinely hung, eventually answers the *original* id-7
request — now indistinguishable from a reply to the id-7 request the host
thinks is outstanding. **The envelope carries no `op` or `params` echo**, so
the host has no way to notice that the reply it just received doesn't match
what it asked for. Account A's balance is silently attributed to account B.
This is not a corrupted frame, not a crash, not a timeout — it is a clean,
well-formed, on-time-looking reply that is simply wrong, and nothing in the
wire format would ever surface the mistake. A monotonic, never-reused counter
makes this structurally impossible: an id can only ever mean the one request
it was issued for.

**Why a tombstoned reply is discarded, not fatal.** The host gave up waiting
because *it* set a deadline, not because the adapter did anything wrong. An
adapter that is merely slow — talking to a rate-limited upstream, say — and
answers after the host's timeout has already returned `Timeout` to the caller
is an honest, if late, adapter. Killing it for that would penalize adapters for
exactly the kind of provider latency this protocol has to tolerate. The reply
still can't be delivered anywhere useful (the caller already got an answer),
so it is discarded, but the connection is not treated as broken.

**Why a never-issued or duplicate reply is fatal.** There is no legitimate way
for either to happen. An id above the high-water mark, or a gap, or a second
reply to an id already answered, means the adapter's model of what the host
asked it disagrees with reality — the exact kind of state divergence that,
left running, could next manifest as a silent cross-attribution like the one
above. The host cannot tell a benign bug in the adapter from active
misbehavior from this signal alone, so it does not try; it kills the process.

## 7. Concurrency, queuing, and deadlines

`max_in_flight` (declared in hello, default `1`) is the number of requests the
host will have outstanding on one adapter connection at once. **A serial
adapter — `max_in_flight: 1` — is fully legal.** The host never assumes an
adapter can process concurrent requests; if it declares `1`, the host waits for
each reply before sending the next request on that connection.

Outbound requests to one adapter connection go through a bounded `mpsc` queue
of capacity 64. When the queue is full, the caller (inside the host) awaits —
it does not drop the request or open a second connection.

**The deadline starts at send, not enqueue.** A request sitting in the bounded
queue behind 64 others is not yet "in flight," so its clock has not started.
If a queued entry's deadline (default 30s) expires before it is ever written
to the adapter's stdin, it is purged from the queue and never sent at all —
there is no point handing an adapter a request the host has already given up
on.

Once sent, a request that times out is tombstoned (§6) and the host returns
`Timeout` to the caller — a host-side outcome, never a wire `err.code`, because
the adapter itself never said anything; the host simply stopped waiting.

If the adapter process crashes, every request currently in flight on that
connection resolves as `AdapterCrashed{status}`. There is **no auto-restart**
in this milestone — a crashed adapter stays crashed until something outside
this protocol relaunches it — and there is **no cancel frame**: the host
cannot ask an adapter to abandon work it has already accepted. (Both are
tracked as deliberately deferred; see `constitution/FOUNDING_PLAN.md` §10 and
the frozen contract's "Deliberately NOT building" list.)

## 8. Error channels: `err` versus `status`

Two different mechanisms report failure, and mixing them up is the single
easiest way for two independent implementations of this protocol to disagree
about what a given failure means.

**If a reply can name a `resource_id`, it is a `status` outcome, not an
envelope `err`.** The envelope's `err` field is reserved for a request that
could not be processed *as a whole* — an unsupported protocol version, an
undeclared op, malformed params that fail to parse before any resource lookup
happens, or an adapter-internal fault with no resource to attribute it to.
Anything the adapter can pin to a specific requested resource — rate limiting,
a stale cache, revoked credentials, a resource that no longer exists — is
reported as one of the `status` outcomes carried inside a normal `ok` reply's
`statuses` array (see `spec/observation.md` §7), keyed by that `resource_id`.

The wire-level `err.code` vocabulary is closed and small: `unsupported_protocol`,
`unsupported`, `invalid_request`, `not_ready`, `internal`. None of these name a
resource. If you find yourself wanting to put a `resource_id` in an `err`
payload, that is the signal you want a `status` outcome instead.

**A reply with zero observations and every status `unavailable` is a SUCCESS
envelope.** It is `{"id": N, "ok": {"observations": [], "statuses": [...]}}` —
not an `err`. The request was processed correctly; the adapter is honestly
reporting that it currently has nothing to say about every resource asked
about. This is the exact case two implementations would resolve differently
without this rule stated plainly: one might reach for `err` because "nothing
came back feels like failure," and would then be indistinguishable, on the
wire, from a request that could not be processed at all. It must not be.

Host-side outcomes — `Timeout`, `AdapterCrashed{status}`, and
`ProtocolViolation{kind}` (`OversizeFrame`, `NotJson`, `NonUtf8`, `UnknownId`,
`DuplicateId`, `PreHelloOutput`) — never appear on the wire at all. They are
things the host concludes *about* the adapter (or its absence of an answer),
not something the adapter emits.

## 9. Isolation boundary

Stated verbatim, and deliberately not softened in either direction:

An adapter process runs under the **same uid**, on the **same filesystem**,
with **inherited file descriptors** as the host.

**Not defended** by this boundary: filesystem reads, `ptrace`, network
egress, environment scraping, or host memory. A malicious or compromised
adapter can read any file the host user can read, attach a debugger to the
host process if the platform permits it, make arbitrary outbound network
connections, and read the host's environment variables. None of that requires
breaking anything this protocol defines — it is simply available to any
process running as the same user.

**Bought** by this boundary: an adapter panic, memory corruption, or hang
cannot corrupt host state or wedge the host process. Process separation gives
crash isolation and hang isolation — a misbehaving adapter's failure mode is
"that one adapter connection is now dead," handled by §7, not "the host is
now in an undefined state."

**Environment scrubbing is hygiene, not a boundary.** Restricting which
environment variables an adapter inherits reduces accidental disclosure — an
adapter that never needed `AWS_SECRET_ACCESS_KEY` should not receive it by
default — but it does not stop a same-uid process from finding secrets by
other means (reading them from disk, from another process's environment via
`/proc`, and so on, subject to the "not defended" list above). It is worth
doing. It does not change what this section claims.

Overclaiming a security boundary here would be worse than having none: a
consumer of this spec deciding whether to run an untrusted adapter needs the
honest answer, not a reassuring one. Per
`constitution/FOUNDING_PLAN.md` §6 and ADR 0001 (Revision 2), **only
explicitly trusted adapters may run** until OS-level restrictions on
filesystem access, process inspection, inherited environment, credentials, and
network destinations are defined and tested — that gate is Milestone 4's, not
this one's, and this document does not pretend it has already been cleared.

## 10. Resource keys

Resources are keyed `(adapter_id, resource_id)` throughout every capability —
`resources.list`, `balances.read`, `history.read`, and `status.read` all
address a resource by this pair, never by `resource_id` alone. Two adapters
are free to both use `"main"` as a `resource_id` without collision.

## 11. Conformance fixtures: multi-run adapters and advisory hints

Two conformance assertions need behavior a single continuous adapter process
cannot express in one lifetime:

- **A11** requires testing `PreHelloOutput` (fatal, and must be the very
  first thing that happens, before the handshake even completes),
  `UnknownId` (fatal), and `DuplicateId` (fatal) — each of which ends the
  process — alongside a *non-fatal* tombstoned-reply-discard the connection
  must survive. `PreHelloOutput` in particular cannot share a process with
  anything that comes after it.
- **A5** requires killing the adapter mid-page and resuming with a *fresh*
  process that has no memory of the crash. A script that "crashes the first
  time it sees cursor X" is correct for exactly one spawn; the resumed spawn
  needs different scripted behavior at that same cursor value.

A fixture's `script.runs[]` is an ordered list of independent adapter
lifetimes. The runner selects one per process launch via the
`SUMER_FIXTURE_RUN` environment variable (a 0-based index into `runs[]`,
defaulting to `"0"` when unset) and **MUST iterate every run in `runs[]`**,
launching one fresh adapter process per index — not just run 0. A fixture
with a single run (the common case) is unaffected by a runner that always
uses the default; a fixture with several runs is silently only partially
exercised by one, which is exactly the gap this section closes.

A fixture may also carry a top-level `conformance_hints` object. Every key
in it is **advisory and non-normative**: it is not part of the wire protocol,
never sent to or read by the adapter, and a runner is free to ignore it
entirely. The one hint defined so far is `deadline_ms`, which lets a fixture
ask the runner to shorten its request deadline (default 30s, §7) below the
default for that run — used by a scenario that must force a host-side
timeout deliberately (e.g. proving a tombstoned reply is discarded rather
than the adapter being killed for being slow) without costing the full
default deadline's wall-clock time per test run. A runner that ignores
`conformance_hints` entirely is still conformant; it just pays the full
default deadline for any scenario that wanted a shorter one.
