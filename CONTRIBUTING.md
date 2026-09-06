# Contributing

Sumer is pre-alpha. The most valuable contributions right now are arguments, not
patches — the decisions being made this month are the expensive ones.

## Before you write code

Read [`constitution/FOUNDING_PLAN.md`](constitution/FOUNDING_PLAN.md) and the
[`adr/`](adr/) directory. A change that contradicts an accepted ADR is not
automatically wrong, but it has to reconcile that record explicitly rather than
quietly diverge from it.

Read [`CLAUDE.md`](CLAUDE.md) too. It states plainly what gets a change rejected,
including changes that pass their tests.

## What a good contribution looks like

Every change starts with a concrete user behavior or an observed failure. State:

1. the affected boundary (wire contract, core, adapter, CLI, conformance);
2. the invariants that matter;
3. the acceptance evidence — what would convince a skeptic this works.

Build one working slice, then refine the contract from what it revealed. Do not
build five half-complete layers at once, and do not add an abstraction for a second
case that does not exist yet.

## Evidence

Three distinct kinds, and they are not interchangeable:

- **conformance** — the implementation follows the protocol;
- **security** — authority and isolation boundaries resist abuse;
- **reconciliation** — financial observations match available external evidence.

Test denied and ambiguous paths, not just successful calls. Where persistence
affects correctness, test restart and replay. Property tests and fuzzing belong on
parsing, exact arithmetic, state transitions, and permissions.

## Money

Exact coefficient and scale. Validated decimal strings on the wire. Never a JSON
float, never an f64, never a bare integer whose scale lives only in a comment. A
patch that reintroduces floating-point money will be rejected on sight regardless
of what it enables.

## Adapters

Adapters are separate processes speaking JSON Lines. **Write one in any language you
like** — compatibility belongs to the wire contract and the conformance suite, not
to a language SDK. If the contract is ambiguous enough that your adapter and ours
disagree about meaning, that ambiguity is the bug and we want to hear about it.

Note the current limitation honestly: until OS-level isolation is defined and tested
(Milestone 4), only explicitly trusted adapters run. An adapter is a trust decision.

## Security

Do not open a public issue for a vulnerability. See [SECURITY.md](SECURITY.md).

## Licensing of contributions

By submitting a contribution you agree it is licensed under Apache-2.0, the same
license as the project, and that you have the right to submit it. There is no
separate CLA.

## Agents

Much of this codebase is written by AI agents under human review, and that is
stated openly rather than hidden. If you are running one: it does not exempt you
from the standards above, and agreement between models does not establish safety.
A human is accountable for what gets merged.
