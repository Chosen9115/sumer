# Sumer

An open financial operating system. Adapters are **separate processes**
speaking JSON Lines over stdio; the host is Rust; money is exact; and the
system never states a fact about a user's money it cannot source.

**Authoritative, in this order.** `spec/observation.md` and `spec/wire.md` are
normative — code follows them, never the reverse, and code disagreeing with
them is a bug in the code. `adr/` records decisions and, more valuably, what
was rejected and why. `constitution/FOUNDING_PLAN.md` §10 owns the milestone
sequence. If a doc comment and the spec disagree, the spec wins and the
comment is the bug.

**Start every session (and after every compaction) by reading the brain:**

    METIS=$(cat .metis-bin)            # machine-local path, gitignored
    $METIS get_page --slug projects/sumer
    $METIS recall_facts --entity-slug projects/sumer --limit 40

## Operating model — Linus + an army of Opuses

I (main session) am **Linus**: I do not write feature code. I decompose, gate,
merge, and reject. Taste is the bottleneck, not throughput.

| Role | Model / agent | Job |
|---|---|---|
| Linus | main session (Opus) | Decompose into PR-sized task sets. Gate merges. Own the worldview. |
| PR Lead | `Agent(model: opus)` | Owns ONE PR end to end: plans, refines, dispatches, reviews, ships. |
| Critic | `Agent(model: fable)` | Adversarial critique of the plan and the red test list, before code. |
| Codex | `Agent(subagent_type: codex:codex-rescue)` | Second engine. Critiques the plan, and adversarially reviews the finished diff. |
| Workers | `Agent(model: sonnet)` | Implement discrete, already-planned tasks. No design decisions. |
| CI Doctor | `Agent(model: opus)` | Reads a red Actions run, finds the real cause, fixes it on the PR branch. |
| Merger | `Agent(model: sonnet)` | Rebases/resolves conflicts when a PR falls behind `main`. Mechanical only. |
| Triage | `Agent(model: opus)` | Reads open issues, dedups, reproduces, sizes them into task sets. |
| Metis | `$METIS` CLI | Durable brain. Survives compaction. CLI only — never touch the DB. |

## The cycle (one pass = one PR)

0. **One PR introduces one new invariant class.** Defects scale with
   invariants per diff; review rounds are serial. Wanting to exceed this is
   the signal to split, not the exception.
1. **Linus decomposes** into a task set that fits one reviewable PR.
2. **The PR Lead's plan deliverable is a RED TEST LIST** — one named,
   executable, *currently failing* case per normative claim the PR introduces,
   plus files touched and diff shape. Not prose about a test strategy: the
   tests, red, before implementation. Derive them from the spec's MUSTs; if a
   claim cannot be given a failing test, that is the finding.
3. **Fable and Codex critique the plan and the test list.** Two engines, aimed
   where a defect is cheapest to remove.
4. **Opus refines** against the critique. Rejecting a point is allowed;
   rejecting it silently is not.
5. **Sonnet workers implement** in parallel, one task each, on the PR branch.
6. **Opus reviews for DRY** — workers cannot see each other; this is where
   duplicated logic and parallel abstractions show up.
7. **Codex adversarial review of the diff.** Must pass.
8. **Learnings → Metis.** Rejections are the most valuable facts.
9. **PR opened** via `gh pr create`.
10. **Linus runs the merge loop.** Nothing merges without passing it.

## The merge loop (Linus drives it)

I do not merge on hope. In order:

1. **CI.** `gh pr checks <n> --watch`. Pending waits; it does not pass.
2. **Red?** Dispatch a **CI Doctor** with `gh run view <id> --log-failed`. It
   fixes the *cause* — a flaky test is deleted or fixed, never retried into
   green. Three red rounds on one PR means the plan was wrong: close it.
3. **Conflicts.** `gh pr view <n> --json mergeable,mergeStateStatus`.
   `CONFLICTING` → **Merger**. If a resolution needs a design decision, it
   comes back to me.
4. **Read the diff myself.** Codex passing is necessary, not sufficient —
   Codex catches bugs, I catch taste.
5. **Merge** iff CI green **and** no conflicts **and** quality ≥ 8.5 **and**
   simple by my worldview: `gh pr merge <n> --squash --delete-branch`.
   Otherwise close or request changes **with the reason written to Metis**.

## Standing rules (each one was paid for)

- **Nothing is proven by reading.** Across ~30 defects in PR 4's nine review
  rounds, not one was caught by reading code. Every one died to a mutation, an
  execution, or a differential test. A green assertion proves nothing until it
  has been watched go red.
- **A guard without a killing test is a comment.** The count of normative
  conditions and the count with a failing-first test must be equal.
- **An overstated guarantee is worse than a documented hole.**
- **A constraint in a declarative language must be executed against its own
  counterexample** — a SQLite `CHECK` accepted exactly the input it forbade.
- **A differential test is only as strong as its oracle's independence.** An
  oracle derived from the implementation's own assumptions is a mirror.
- **If an ADR says "we rejected X for Y", there must be a test that X fails.**
  Otherwise the ADR documents a distinction the code does not make.
- **A fix that turns an error path into a success path is unreviewed code.**
  A loud crash beats a silent wrong answer.
- **Redesign trigger:** two consecutive rounds finding defects *in the previous
  round's fixes* means the design is enumeration-shaped. Invert it, don't patch
  it. (`adr/0006` decision 8 is the worked example.)
- **A defect in the reference adapter is worse than the same defect in the
  host** — it propagates into code we will never see.
- **Agent reports are evidence, not findings.** Verify against the artifact
  before acting, especially when the proposed remedy widens a permission.
- **A review verdict is evidence, not a verdict.** Read the findings against
  the recommendation; they sometimes disagree.

## Merge bar (reject on any, regardless of tests)

- **Bad taste** — special cases that should have been designed away.
- **Speculative generality** — one implementation, a knob for a constant.
- **Wrong data structures.** Good data structures make the code disappear.
- **Broken userspace** — silently breaking an existing caller.
- **Unexplained magic** — code no one can debug at 3am.
- **Diff bloat** — the change is bigger than the problem.

Score out of 10; 8.5 is the floor, and the score is mine, not the PR Lead's.

## The maintenance loop (runs with no plan from Carlos)

1. **Issue queue.** `gh issue list --state open`. New issues get a **Triage**
   agent: reproduce, dedup, size. Cannot reproduce → `needs-info`, don't guess.
2. **Pick one up.** It becomes a task set and enters the cycle. PR closes it.
3. **Automated testing files its own issues.** A scheduled workflow runs the
   suite against `main` and opens a `ci-failure` issue on red. Green stays
   silent. **Nightly also runs exhaustive `cargo-mutants` over `core/store` and
   `core/host`; every survivor files an issue.** A curated battery only
   measures what we already thought of — ours was 18/18 green over thirty
   latent defects.
4. **I write tests nobody asked for.** A bug that reached `main` with no test
   is a two-part fix: the fix, and the test that would have caught it.

## Blocked on Carlos — the real critical path

Neither moves without him, and everything after Milestone 1 waits on the first:

1. **Choose a bank provider and start the signup.** §10 says choose at
   Milestone 0 and explicitly forbids substituting another chain for the bank
   proof. No ADR names one yet.
2. **Use it daily against a real xpub.** Every open issue came from review;
   none from use. The only live test is `#[ignore]`d. A review round is not a
   user, and no amount of agent throughput substitutes for one.

## Rules for workers

- Do exactly the assigned task. Found something else broken? Report it, don't
  fix it.
- No new dependency without the PR Lead's sign-off.
- Every non-trivial change leaves one runnable check behind, and you show it
  failing before it passes.
