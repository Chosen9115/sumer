# ADR 0001 — Rust core, adapters over a wire protocol

- **Status:** accepted
- **Date:** 2026-09-06
- **Decision by:** Linus, on the Critic's recommendation

## Context

Sumer needs one implementation language before any code exists. The choice is
expensive to reverse and constrains every later phase. Four axes were in genuine
tension:

- **Agents write most of this code.** Not most fluently — most *correctly*. Where
  does a compiler catch an agent's mistake before a human reviewer has to?
- **Contributor thesis** (§1.10, Phase 2, Milestone 4): strangers worldwide must
  write adapters without permission. A language that halves that pool may kill the
  project regardless of technical merit.
- **Money correctness**: no floating-point money, exact integer/decimal arithmetic,
  key material handled without footguns. Note that §2.6 does **not** specify amount
  representation, scale, rounding, or overflow behaviour — that is a gap in the
  founding plan, not a requirement it hands us. See Revision 1.
- **Supply chain** (§1.6, §5.4): third-party code is assumed malicious; forkability
  and reproducible builds must be achievable, not aspirational.

## Decision

**Rust** for everything the core team ships: protocol core, reference client,
conformance suite, and the reference adapters.

**Adapters are processes, not libraries.** The adapter boundary is a wire protocol
— JSON lines over stdio, LSP-style — not a language SDK. Conformance tests a
*process*. An adapter may be written in any language that can read stdin and write
stdout.

## Alternatives considered

- **TypeScript everywhere.** Where agents are most fluent, best ecosystem (viem,
  bitcoinjs), largest contributor pool, fastest first quarter. Rejected on
  correctness: `JSON.parse` returns `any`, `Number(weiBalance)` type-checks and
  silently corrupts above 2^53, and nothing but discipline prevents it. Discipline
  is what agent-written code at volume lacks.
- **Go.** Wins the supply-chain axis outright (sumdb, no install scripts). Loses on
  correctness: `encoding/json` yields `float64` for numbers, `int64` wraps silently,
  copy-pasted `if err != nil` swallows failures. A weak compiler with strong opinions.
- **Python.** Fails the agent-correctness and money axes outright.
- **Rust core + TypeScript adapter SDK.** Not killed — deferred, and the argument
  originally used to kill it was wrong. See Revision 1. An *in-process* TS SDK is
  rejected (it loads npm beside key material, which §1.6 forbids), but a TS SDK that
  merely speaks the subprocess wire is compatible with everything here and does not
  weaken the black-box conformance suite. We are not building one yet because
  nobody has asked for it; if adapter authors want one, it is additive.
- **OCaml/Haskell.** Trade Rust's correctness for a contributor pool of zero.

## Consequences

- The first quarter is slower. rust-bitcoin, BDK and alloy are first-rate, but
  open-banking integration means hand-rolling HTTP that TypeScript gets free.
- Agent iteration is slower per cycle (compile times).
- The *core* contributor pool shrinks; the *adapter* pool stays global, because the
  adapter boundary is language-neutral by construction.
- The conformance suite becomes the project's primary artifact, which is what §4.8
  already argued and what Phase 10 requires.

## Security implications

- serde forces types at the deserialization boundary; `rust_decimal` and `u128`
  exist; `overflow-checks = true` in release guards the arithmetic itself.
- Cargo is not clean (`build.rs` executes code) but the dependency floor is far
  lower than npm's, and cargo-vet exists. npm's structural liability under a
  financial system is not fashionable hand-wringing: event-stream was aimed at a
  Bitcoin wallet, and the 2025 chalk/debug compromise targeted crypto specifically.
- Out-of-process adapters prevent a compromised adapter from reaching key material
  **by language-level access**. That narrow claim is all that is established.
  **Separate processes under the same uid are not an isolation boundary.** Filesystem
  access, inherited credentials and environment, ptrace/debugger access, and signer
  authorization are all unspecified. Secret isolation is therefore NOT yet a property
  of this design — it is an open requirement, tracked as the blocking prerequisite
  for any adapter that touches real key material.

## Migration path / reversibility

The durable artifacts are the **spec and the conformance suite**, not the Rust
implementation. Any language can reimplement the core against them, which is also
what Phase 10 demands. Reversal cost is therefore bounded — the adapters and the
conformance corpus survive a core rewrite.

## Tripwire

The first draft of this tripwire was logically falsifiable but operationally
gameable: "comparable", "~2x", and "friction blamed on the stack" carried no
measurement rules, and CLAUDE.md's three-red-rounds abandonment rule would have
censored the hardest work out of the sample entirely. Measurement rules:

- **Matched tasks.** Each measured PR records its task class (adapter, core type,
  conformance case) so Rust work is compared against like work, not against
  whatever happened to be easy that week.
- **Count abandoned attempts.** A PR closed under the three-red-rounds rule counts
  as a failed attempt with its rounds included. Otherwise the process hides exactly
  the evidence the tripwire exists to collect.
- **Count local iterations, not just pushed commits.** Elapsed agent effort and
  defects surviving review are the units; green-CI-on-first-push is not.
- **"External adapter" means completed and conformance-passing**, not attempted.

Revisit if **any** hold by 2027-03-06:

1. Agent-written Rust work averages >2x the failed attempts or >2x elapsed effort
   of matched TypeScript work, abandoned PRs included.
2. Milestone 4 has zero *completed* external adapters.
3. The read-only financial graph is not demoable, or reaching it cost more than
   twice the projected effort.

**Two** of the three: switch to TypeScript-strict and supersede this ADR with one
that says so plainly.

## Revision 1 — 2026-09-06, corrections from independent adversarial review

An independent reviewer (different engine, deliberately hostile) found three
defects in the first draft. All three are corrected above.

1. **Factual error.** The draft credited §2.6 with requiring exact money
   representation. §2.6 specifies asset *metadata* and contains no amount
   representation, scale, rounding rule, or overflow behaviour at all. Exact money
   is an obligation we are taking on, not one the plan handed us — and the wire
   protocol needs a lossless money encoding specified explicitly, because JSON
   numbers are IEEE-754 doubles and would silently reintroduce the exact defect
   this ADR rejected TypeScript over.
2. **Logic error.** The draft killed the Rust-core/TS-SDK split by arguing that an
   in-process SDK is unsafe. That argument does not reach the conclusion: a TS SDK
   can implement the subprocess wire. The process boundary stands on its own; the
   split is merely unnecessary today, not unsound.
3. **Overreach.** The draft leaned on agent-correctness as the dominant axis with
   no matched-task data, no time-to-green measurement, and no escaped-defect count.
   Compilation does not prove correct denomination, rounding, authorization, or
   settlement handling. Rust is retained here on **money-correctness ergonomics and
   dependency-floor**, which are demonstrable, and *not* on a claim that agents
   write better Rust than TypeScript, which is currently unevidenced. The tripwire
   exists to collect that evidence.
