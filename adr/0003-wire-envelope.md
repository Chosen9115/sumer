# ADR 0003 — The wire envelope: JSON Lines, monotonic ids, host-assigned revisions

- **Status:** accepted
- **Date:** 2026-09-06
- **Decision by:** Linus, ratifying founding plan §6 and §10 (Milestone 0)

## Context

ADR 0001 decided that adapters are processes speaking JSON Lines over stdio,
not a language SDK, and that a Rust host and a non-Rust fake adapter must pass
one conformance suite. It did not decide the shape of that JSON, how ids
survive timeouts and crashes, who assigns a history record's revision number,
or what happens to a corrupted frame. Milestone 0 ("executable read-only
contract," plan §10) requires those decisions to be frozen and testable before
any real adapter is built against them — a black-box conformance suite
(`sumer-conformance`) is the artifact meant to catch two implementations
silently disagreeing about meaning, and it can only do that against a wire
that is fully specified, not just a transport that is.

Four consequential decisions had to be made together, because each closes off
options for the others: how frames are delimited, how request ids behave
across the timeout/crash boundary, who assigns a durable revision number to a
history record, and what a malformed frame does to the connection.

## Decision

**JSON Lines over stdio**, one UTF-8 JSON object per LF-terminated line, both
directions, host always the requester. Two hard caps: `MAX_FRAME_BYTES` =
1 MiB (counted before the trailing LF) and `MAX_OBSERVATION_BYTES` = 64 KiB
(counted per serialized observation inside a reply body).

**Monotonic, never-reused request ids**, tracked by the host in three states
— issued, tombstoned, never-issued — with no cancel frame. A reply to a
tombstoned id is discarded and the connection survives; a reply to a
never-issued or already-answered id is a fatal `ProtocolViolation` that kills
the adapter process.

**Host-assigned revisions.** Adapters emit observations, not revisions; the
host assigns `revision: u64` by arrival order per `(adapter_id, local_id)`.

**Kill without resync** on any oversized, non-UTF-8, or unparseable frame.
There is no attempt to recover a byte stream once its framing is violated.

Full rationale for each of these — including the id-reuse cross-attribution
scenario, the fold total order, and the isolation boundary this envelope runs
inside — is in `spec/wire.md` and `spec/observation.md`, which this ADR does
not duplicate. This ADR records why these specific mechanisms were chosen over
their alternatives, and what they cost.

## Alternatives considered

- **Length-prefixed framing** (a 4-byte length header before each JSON
  payload) instead of JSON Lines. Rejected: it buys nothing JSON Lines
  doesn't already have for this protocol's traffic (small, text-shaped
  request/reply pairs), while making every adapter implementation's I/O layer
  more complex — a length prefix must be read as raw bytes before the payload
  can even be handed to a JSON parser, which is friction in exactly the
  languages ADR 0001 is trying to keep the adapter boundary open to. JSON
  Lines is `readline()` in every language with a standard library, which is
  the actual bar (ADR 0001: "an adapter may be written in any language that
  can read stdin and write stdout"). The cost — no ability to detect
  corruption before a full line is buffered — is accepted as a wash against
  "kill on violation" (below): both framings still end in a kill on
  corruption; JSON Lines just doesn't need extra machinery to get there.

- **gRPC / protobuf.** Rejected on the same axis ADR 0001 already ruled on:
  it would require a compiled schema and a generated-code toolchain in every
  target language, which is a much heavier bar than "read stdin, write
  stdout" and works directly against the global-adapter-pool thesis
  (`constitution/FOUNDING_PLAN.md` §1, ADR 0001's contributor-thesis axis).
  It also reintroduces exactly the JSON-number-as-money temptation ADR 0001
  Revision 1 flagged for the wire format generally — protobuf has native
  numeric types that are easy to reach for instead of a validated decimal
  string, and nothing forces the discipline `spec/money.md` requires.

- **A socket instead of stdio.** Rejected: stdio composes for free with
  process supervision (the host already owns the child process's lifecycle;
  a crashed child's stdio simply closes, which is exactly the signal §7 of
  `spec/wire.md` needs to resolve in-flight requests as `AdapterCrashed`) and
  requires no port allocation, no bind-address decision, and no additional
  attack surface for a component ADR 0001 already says is not isolated from
  the host beyond a same-uid process boundary. A socket would need its own
  answer to "who else on this machine can connect," which is a security
  question this milestone does not need to open.

- **Adapter-assigned revisions.** Rejected — this is the one alternative
  that looked plausible and had to be worked through rather than dismissed on
  transport grounds. An adapter could, in principle, track how many times it
  has emitted a given `local_id` and stamp that count as `revision` itself.
  It fails on the very case this milestone's fixtures were chosen to exercise
  (plan §10: "pending-to-posted revisions" from a real bank feed): a
  restarted or fundamentally stateless adapter — which is most real adapters,
  since PR 2's cursor and revision stores are host-side and in-memory by
  design — has no durable memory of its own prior emission count, and cannot
  answer "which revision is this" correctly across a restart without either
  persisting revision state itself (duplicating the host's job, and now two
  implementations of revision-counting that can silently diverge) or querying
  the host before every emission (a round trip this protocol does not have,
  and a chicken in front of an egg — the query would itself need a request id
  and a reply, i.e. the mechanism being defined). Host-assigned revision by
  arrival order needs nothing from the adapter but the observation itself.

- **An in-process plugin ABI** (dynamically loaded adapter code sharing the
  host's address space) instead of a subprocess. Rejected on the same ground
  ADR 0001 already stood on for the core-language question, restated for the
  wire specifically: a plugin sharing the host's address space can corrupt
  host memory or crash the host process outright on a bug that, as a
  subprocess, would only ever take down its own connection
  (`spec/wire.md` §9 — "an adapter panic, memory corruption, or hang cannot
  corrupt host state or wedge the host" is a property this envelope is
  designed to buy, and an ABI plugin gives it up entirely). It also collapses
  the language-neutral adapter boundary into whatever the host's ABI happens
  to be, which is a much narrower contributor pool than "any language that
  can read stdin and write stdout."

## Consequences

- Every adapter, in any language, pays a small, well-understood tax: read a
  line, parse JSON, write a line. Nothing about this envelope rewards writing
  an adapter in Rust over any other language capable of that.
- The conformance suite (`sumer-conformance`) is the thing that actually
  proves two implementations of this envelope agree — this ADR fixes the
  mechanisms; `spec/wire.md` and the conformance assertions (A1–A11 in the
  frozen milestone contract) are what make disagreement detectable rather
  than merely undesirable.
- Kill-without-resync and kill-on-never-issued-id both mean a single
  malformed frame or a single confused adapter ends that connection, full
  stop — there is no partial-credit recovery path. This is deliberate (see
  `spec/wire.md` §2 and §6) but it does mean an adapter author gets one shot
  per connection at not desyncing the protocol; a flaky adapter pays for that
  flakiness in dead connections, not silently-wrong data.
- No cancel frame and no auto-restart (both explicitly deferred, per the
  frozen milestone contract's "Deliberately NOT building" list) mean the host
  cannot currently ask an adapter to abandon in-flight work, and a crashed
  adapter stays down until something outside this protocol relaunches it.
  Both are scoped to a later milestone once real usage shows what they need
  to look like.

## Security implications

This envelope makes no claim beyond ADR 0001's and `spec/wire.md` §9's: same
uid, same filesystem, inherited file descriptors. It buys crash and hang
isolation — a malformed frame or a wedged adapter takes down its own
connection, never the host process — and nothing more. It does not defend
against a same-uid adapter reading arbitrary files, attaching a debugger,
making network connections, or scraping the host's environment. Kill-on-
protocol-violation is a correctness and availability mechanism (stop trusting
a byte stream that has already proven it can't be trusted), not a security
control against an adversarial adapter, which per ADR 0001 (Revision 2) and
`constitution/FOUNDING_PLAN.md` §6 remains an open gate: only explicitly
trusted adapters run until OS-level isolation is defined and tested.

## Reversibility

Medium. The envelope is versioned at the handshake (`hello`'s
`protocol: ["1"]`), so a breaking change to any mechanism in this ADR is a
new protocol version negotiated at connection time, not a silent redefinition
of version `"1"` — the same discipline `spec/money.md` §7 applies to its own
bounds. Existing adapters built against version `"1"` keep working unless a
host operator chooses to stop offering it. The cost of reversing any single
decision here (say, adding a cancel frame, or moving to length-prefixed
framing under a new version) is bounded to that version bump and the
conformance suite's coverage of it; it does not require re-deciding the
process-versus-library adapter boundary ADR 0001 already settled.

## Revision 1 — 2026-09-07, superseded by PR 4's store

The reasoning above rests in one place on cursor and revision stores being
"host-side and in-memory by design" (see the paragraph near the frame-cap
discussion). That was true when this was decided and is no longer: PR 4's
`core/store` persists both, and `refresh` is the first caller `fold` and
`paging` have ever had.

The decision is unaffected — nothing in the envelope depended on the stores
being transient — but a reader reaching that sentence today would be misled,
which is why this is recorded rather than edited. What replaces the
in-memory assumption is stated normatively in `spec/observation.md` §8 and
argued in ADR 0006: a persisted cursor is written only inside the
transaction that commits the page which minted it, and it is dropped when
the resource fingerprint or the adapter's `local_id_derivation` changes.

