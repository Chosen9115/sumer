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
- **Money correctness** (§2.6): no floating-point money, integer/decimal exactness,
  key material handled without footguns.
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
- **Rust core + TypeScript adapter SDK.** Killed explicitly. An in-process TS SDK
  loads npm code next to key material, which §1.6 forbids — so adapters must be
  sandboxed out-of-process anyway. Once they are, the SDK language is fiction and
  the split just doubles toolchains while halving the conformance suite's authority.
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
- Out-of-process adapters mean a compromised adapter cannot reach key material by
  language-level access. This is the invariant that drove the boundary decision.

## Migration path / reversibility

The durable artifacts are the **spec and the conformance suite**, not the Rust
implementation. Any language can reimplement the core against them, which is also
what Phase 10 demands. Reversal cost is therefore bounded — the adapters and the
conformance corpus survive a core rewrite.

## Tripwire (adopted from the Critic's falsifiable prediction)

Revisit this ADR if **any** of these hold by 2027-03-06:

1. Agent-written Rust PRs average more than ~2x the red-CI rounds of comparable
   TypeScript work.
2. Milestone 4 draws zero external adapter attempts, with friction blamed on stack.
3. The read-only financial graph is not demoable.

**Two** of the three: switch to TypeScript-strict and supersede this ADR with one
that says so plainly.
