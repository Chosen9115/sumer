# Sumer
## Founding plan for an open financial operating system

Revised: 2026-09-06

> Your financial environment should belong to you. Banks, wallets, brokers, payment providers, and applications should connect to it through open interfaces, with authority you can inspect and constrain.

This document establishes the direction, architecture, and first delivery sequence. It distinguishes commitments for the first implementation from capabilities that must be earned through working software. It does not by itself supersede accepted repository ADRs; implementation changes must reconcile those records explicitly.

# 1. Purpose

Sumer is an open system for understanding and, eventually, operating financial resources across providers. A user or organization can connect accounts and wallets, inspect their financial position, choose applications, and authorize specific actions without giving one application unrestricted control.

The long-term scope includes banks, Bitcoin, other blockchains, stablecoins, brokerages, securities, and regional payment providers. No financial ideology is privileged. Assets and providers retain their technical, legal, custody, and settlement differences.

The first product is deliberately small:

> Connect a Bitcoin wallet and a bank account, see accurate balances and history together, understand the source and freshness of every observation, and export the complete record.

The first release is read-only. It should be useful without an agent, a token, a marketplace, a new identity standard, or an execution engine.

# 2. Principles

## 2.1 The user controls the environment

The user chooses their client, providers, applications, and permitted actions. Financial data is exportable through documented formats. Optional hosted services must not become hidden dependencies of the core protocol.

## 2.2 Portability has honest limits

Sumer makes interfaces, software, and user records portable. It does not promise that bank approvals, legal agreements, custody arrangements, account identifiers, or provider eligibility transfer automatically.

Removing an adapter disconnects software. It does not close an account, revoke every upstream credential, or move funds. The client must explain which actions actually occurred.

## 2.3 Different assets remain different

A dollar deposit, USDC, and a Treasury fund share are distinct instruments. Common valuation does not imply common ownership rights, liquidity, redemption, or settlement behavior.

## 2.4 Authority is explicit

Read access is a permission. Execution requires stronger, separately granted authority. Applications and agents receive the minimum capabilities needed, with enforceable limits and revocation.

## 2.5 Sensitive state stays close to the user

Local operation is the default. Credentials, account associations, and financial history are sensitive even without private keys. Synchronization and hosted execution are optional deployments with explicit trust boundaries.

## 2.6 Open participation is demonstrated

The protocol, schemas, conformance tests, and reference implementation are open. A contributor must be able to implement an adapter without permission or dependence on the core team's language choice.

Choose and publish an explicit open-source license before inviting external contributions. Define ownership of contributions and a minimal security reporting process at the same time.

## 2.7 Claims require evidence

An adapter passing conformance tests proves that it follows a contract under those tests. It does not prove the provider is honest, the adapter is uncompromised, or its reported balance is true.

Every observation carries provenance. Conflicts and uncertainty are represented rather than silently resolved in favor of the newest response.

## 2.8 Build for continuity

Document installation, backup, restore, export, and independent implementation. Users should retain access to their own records if the founding team disappears. Provider availability remains an external dependency that Sumer cannot guarantee.

# 3. First user and first proof

The initial user is an individual who holds Bitcoin and maintains a bank account and wants one reliable view of both. Business ownership, delegated organizational authority, and multi-device autonomous execution are later extensions.

The initial workflow:

1. Initialize a local profile.
2. Connect a watch-only Bitcoin resource.
3. Connect one supported bank data provider with read-only access.
4. Inspect balances and history, including pending items and revisions.
5. See stale, unavailable, and conflicting information clearly.
6. Export the record and restore it into a fresh installation.

The bank integration must name a concrete provider and supported geography. Before promising delivery, verify its authentication, available scopes, data access, cost, and operating requirements. A CSV import or recorded fixture is useful for development but must be labeled as such; it is not evidence of a live bank connection.

Success means the records reconcile against both sources, including failure and revision cases. Merely displaying a portfolio total is insufficient.

# 4. Architecture

Start with one local core process, a CLI, a local database, and supervised adapter processes. Avoid a distributed service architecture until a demonstrated deployment need requires it.

```text
User / CLI / future application
              |
       Versioned core API
              |
      Trusted local runtime
      |       |          |
  Resource  Observation   Adapter supervisor
  catalog   store        and restricted transport
              |                    |
        Derived graph       Provider adapters
                                   |
                            External providers

Later, before execution:
Intent -> validated plan -> policy and reservation -> approval
       -> restricted executor / signer -> operation journal
       -> provider observations -> reconciliation
```

The financial graph is a derived view of resources, ownership associations, balances, and events. It is not a requirement for a graph database and does not establish legal ownership by itself.

Use a relational local store initially. Keep durable observations and revisions sufficient to explain the current view. Add tables and relationships for demonstrated needs rather than designing a universal ontology in advance.

# 5. Minimum financial model

## 5.1 Resources and capabilities

A resource identifies a provider-backed account, wallet, or other financial object. Its record includes a stable local identifier, provider identifier, resource kind, relevant ownership association, and supported capability versions.

The first capability set is:

- discover resources;
- read balances;
- read history;
- report connection status and data freshness.

Capabilities have separate request and response schemas. Unsupported operations return an explicit unsupported result. Do not freeze a universal `execute()` method before real execution semantics have been tested against unrelated providers.

## 5.2 Money and assets

Every amount has an exact coefficient and scale, or an equivalently exact representation defined by the schema. Wire amounts use validated decimal strings, never JSON floating-point numbers. Define bounds, rounding rules, and checked arithmetic explicitly.

An amount always references a canonical asset identifier. Display symbols are labels, not identifiers. Network and contract identifiers distinguish on-chain assets; provider and account context distinguish claims where needed.

Separate:

- the instrument's identity and issuer;
- the position's custody and account relationship;
- the observed amount;
- the price used for valuation.

Valuation records include source, quote currency, observation time, and freshness. An estimated portfolio value never silently becomes an executable balance.

## 5.3 Balances

Represent provider-reported categories such as available, pending, held, and total only where their meanings are known. Do not assume every provider exposes the same categories or that they always sum identically.

Each observation includes provider, resource, source reference where available, observation time, effective time where known, and freshness or completeness information.

Unknown is distinct from zero. A failed refresh does not erase the last observation; it marks the displayed state stale or unavailable.

## 5.4 History

Preserve stable source identifiers, revisions, pagination cursors, and provider status. Pending transactions may change identity or amount when posted; events can arrive late, repeat, or disappear after a correction.

Preserve the evidence needed to explain revisions without retaining unnecessary sensitive payloads. Define deduplication and cursor resumption in the contract.

## 5.5 Reconciliation

Record discrepancies between local expectations and provider observations. Define authority per field and provider rather than declaring one universal source of truth.

Reconciliation may produce confirmed, unresolved, or disputed results. An unresolved discrepancy affecting spending must block dependent automatic execution until policy or a human resolves it.

# 6. Adapter contract and isolation

Adapters are processes communicating over a versioned wire protocol. The initial transport is JSON Lines over standard input/output. One process serves each configured adapter and multiplexes requests; do not create a process for every transaction.

The wire contract specifies:

- version negotiation and supported capabilities;
- correlation IDs and structured errors;
- maximum frame sizes and bounded queues;
- timeouts, cancellation semantics, and backpressure;
- logging on stderr, with secret redaction;
- pagination and, when implemented, subscription cursors and resumption;
- exact financial values and validation at both boundaries.

A Rust implementation and a small non-Rust test adapter must pass the same suite.

Process separation is a crash boundary, not a complete security boundary. Before running untrusted adapters, define and test operating-system restrictions for filesystem access, process inspection, inherited environment, credentials, and network destinations. Until that gate passes, only explicitly trusted adapters may run, and the limitation must be visible.

Adapters receive scoped access through the host's credential mechanism where feasible. Some providers require an adapter to hold a bearer credential; document that exposure and its upstream scopes rather than claiming that no adapter ever handles a secret.

Adapters never receive wallet private keys. Execution adapters must not have authority that bypasses the policy boundary. If a provider credential inherently grants broader authority, either constrain it upstream, contain access behind a trusted broker, or explicitly reject that integration for unattended execution.

Signed releases and pinned dependencies improve provenance. Neither establishes that code is safe. Adapter installation and updates are explicit trust decisions.

# 7. Execution: requirements before implementation

Execution is a later milestone. The following requirements establish its entry gate, not a mandate to build the engine during the read-only phase.

## 7.1 Intents describe acceptable outcomes

A payment intent specifies recipient, accepted instruments and destinations, required net receipt, deadline, allowed conversion exposure, and fee limits.

“Send Alice USD 100” is incomplete if Alice's accepted delivery methods are unknown. ACH, USDC, and Lightning are candidate routes only when they satisfy the actual recipient requirements.

Resolve mutable destination names before approval. Bind approval to the resolved destination, asset, amount, fees or fee bound, route, and expiry. A changed plan requires fresh authorization.

## 7.2 Operations have durable identities

Persist an operation identity and its authorized plan before external submission. Track provider references and evidence as they arrive.

Distinguish at least prepared, authorized, submitting, accepted, rejected, and outcome-unknown conditions. Model settlement progress and later returns or reversals separately using typed provider semantics. One universal terminal `success` cannot honestly describe every rail.

A timeout or adapter crash does not prove failure. Before retrying, perform durable operation lookup or provider reconciliation. If the outcome cannot be established, preserve uncertainty and block an automatic duplicate.

Do not promise exactly-once external execution across providers that cannot support it.

## 7.3 Policy precedes submission

Every action passes deterministic authorization independent of any language model. Grants identify principal, action, resource, asset, destination, amount bounds, duration, and relevant policy version.

Limits reserve capacity atomically before concurrent requests can spend it. Define how reservations are consumed, released, or held for unknown outcomes. Recheck relevant authorization immediately before submission.

Revocation prevents future authorized submissions; it cannot recall a transaction already accepted externally. The UI and audit record must expose that boundary.

For the first executable release, one local authority owns grants and spending reservations. Multi-device and offline execution require a separate consistency design.

## 7.4 Approval and signing are trusted operations

The signer validates the exact authorized payload and rejects mismatches. The approval display comes from trusted structured data, not adapter-supplied prose alone.

Applications and agents may propose actions. They cannot modify policy, replace an approved destination, or invoke a signer through an alternate path.

## 7.5 Recovery is specific to the route

Define fees, cancellation windows, partial execution, return behavior, and required human intervention for each supported route. Multi-step routes can leave intermediate assets or exposures.

A compensating transaction is a new authorized action with its own costs and risks. Do not call it rollback when the original action cannot be undone.

# 8. Local data, identity, and deployment

Start with a local profile and device authentication. Store only identity information necessary for the current provider connections. Keep provider-issued approval separate from user-controlled identifiers.

Do not build portable KYC credentials or a global identity graph for the first release. Combining financial relationships in one profile creates sensitive information; minimize disclosure and cross-provider correlation.

Specify encryption key storage, locked-state behavior, backup encryption, recovery, and deletion. Encryption at rest protects stored data under a defined threat model; it does not protect an unlocked process from a compromised host.

Test export and restore, including schema migration. Restoring data does not automatically restore external grants or credentials. The client must identify connections requiring reauthorization.

Hosted synchronization, always-on agents, and routing services each need a separate deployment design naming the operator, data held, authority exercised, and outage behavior. Provider obligations alone do not establish the operator's legal position. Review the concrete service and jurisdiction before offering execution there.

# 9. Language and storage decisions

Use Rust for the core runtime, policy enforcement, reconciliation, initial CLI, and reference adapters. Its type system supports explicit operation states and financial types, while ownership helps constrain memory and concurrency errors.

Rust does not make arithmetic or financial logic correct automatically. Use checked operations, exact money types, validated inputs, and tests of financial invariants.

Third-party adapters may use any language. Compatibility belongs to the wire contract and conformance suite.

A future browser interface may use TypeScript. TypeScript can represent money exactly when designed appropriately; static types do not replace runtime validation. Avoid a permanent rule requiring every UI and tool to use Rust.

Start with SQLite for local structured records. Database choice does not supply encryption automatically: select and test the encryption and key-management approach explicitly before the encrypted-state milestone is considered complete. Introduce a server database only when a hosted deployment has an established need.

Keep the language decision reviewable. Track delivery friction, correctness failures, and external adapter participation. Reconcile any change with existing ADRs and their recorded revisit criteria.

# 10. Delivery sequence

## Milestone 0 — Executable read-only contract

Deliver a minimal schema, Rust host, fake adapter in another language, and conformance suite. Fixtures come from Bitcoin and a selected bank data source and include exact large amounts, pending-to-posted revisions, stale balances, duplicate events, and interrupted pagination.

Choose a candidate bank provider now so the contract reflects real constraints. Keep fixtures sanitized and distributable.

Exit: both implementations agree on meaning and error behavior, not merely field names. No execution interface is declared stable.

## Milestone 1 — Bitcoin vertical slice

Connect a watch-only wallet; discover resources; ingest balances and history; persist observations; display them in the CLI; export them.

Exit: results reconcile with the source, restart and refresh preserve correctness, and unavailable data is never rendered as zero. Document the address or extended-public-key privacy exposure to the selected data backend.

## Milestone 2 — Bank vertical slice

Implement one live read-only provider connection. Handle authentication expiry, rate limits, pending and revised records, partial history, and outages.

Exit: one user can inspect bank and Bitcoin records together with provenance and freshness. If live access is unavailable, retain the fixture-based work but report the milestone blocked on provider access; another chain is not a substitute for this proof.

## Milestone 3 — Useful local product and independent extension

Finish encrypted local state, locked-state behavior, export, restore, and migration. Document the adapter contract and contributor workflow. Invite an independent developer to add an adapter; EVM/Base is a useful candidate for testing token identity and precision.

Exit: the initial user can use the product routinely, restore into a fresh installation, and connect an external adapter without modifying core code. A core-team fake adapter proves language independence; an outside contributor proves developer usability.

## Milestone 4 — Authorization and operation semantics

Implement grants, restrictions, durable operation records, reservations, exact-plan approval, signer separation, and the required isolation boundary. Model two unrelated execution providers using sandboxes or controlled fixtures.

Exit: adversarial tests demonstrate rejection of revoked authority, changed destinations, concurrent overspending, stale data, duplicate submission, and malicious signing requests. Crash-after-acceptance leaves a recoverable or explicitly unknown outcome.

No live money movement is part of passing this milestone.

## Milestone 5 — One approved send, then a second rail

Enable one narrowly scoped live payment workflow only after its provider integration, operational responsibilities, and security review are complete. Require human approval and explicit limits. Add a second unrelated rail and revise the abstraction where their semantics differ.

Exit: both routes satisfy a fully specified recipient outcome and reconcile correctly through acceptance, settlement, and any supported return behavior. Do not add bridging or solver competition to manufacture route diversity.

## Milestone 6 — Constrained unattended operation

Allow one useful automated workflow using the same authorization and execution APIs. Introduce an agent only where it improves that workflow.

Exit: the agent can propose and execute within granted limits but cannot change those limits, bypass approval thresholds, or conceal uncertainty. Test prompt injection through provider descriptions, transaction memos, and tool results.

# 11. Development method

Each change starts with a concrete user behavior or observed failure. State the affected boundary, important invariants, and acceptance evidence. Build one working slice, then refine the contract from what it revealed.

Keep ADRs for consequential choices. Use RFCs for changes that need cross-implementation agreement; routine fixes do not require a committee.

Maintain three distinct kinds of evidence:

- conformance: implementations follow the protocol;
- security: authority and isolation boundaries resist abuse;
- reconciliation: financial observations and operations match available external evidence.

Use property tests and fuzzing for parsing, exact arithmetic, state transitions, and permissions. Use restart and replay tests where persistence affects correctness. Test denied and ambiguous paths, not just successful calls.

A failure should leave the smallest durable artifact that prevents recurrence: a regression case, corrected invariant, migration check, or concise decision record. Avoid accumulating rules that cannot be enforced or explained.

# 12. Agents and maintainers

Agents assist with implementation, tests, investigation, and documentation. Roles are assigned when a task benefits from them; nine permanent agent services are not an architectural prerequisite.

Keep build-time development agents separate from runtime financial agents. Neither receives production secrets by default. Runtime agents remain untrusted callers of the same constrained interfaces available to applications.

Sensitive changes require review independent of their author and meaningful verification. Agreement between models does not establish safety. Human maintainers remain accountable for releases, security response, and consequential decisions.

Project specifications, executable contracts, and accepted ADRs belong in the repository so contributors can reproduce decisions. Sumer's session continuity, review outcomes, and durable project state are recorded through the Metis CLI under `projects/sumer` and its sub-slugs. Never write directly to the memory database.

# 13. Minimal repository

```text
sumer/
  README.md
  LICENSE
  CONTRIBUTING.md
  SECURITY.md
  constitution/FOUNDING_PLAN.md
  spec/                 # Implemented or actively tested contracts
  adr/                  # Consequential decisions
  core/                 # Local runtime and persistence
  cli/                  # First reference client
  adapters/             # Reference provider integrations
  conformance/          # Language-independent fixtures and expectations
  tests/                # Integration, recovery, and adversarial cases
  docs/                 # Setup, export, recovery, contributor guide
```

Add applications, registries, solvers, and hosted services when the corresponding work begins. Empty directories do not prove an architecture.

# 14. Later expansion

After the initial contracts survive real use, Sumer can add business permissions, payment applications, treasury workflows, regional providers, optional synchronization, portable credentials, independent clients, and competing routing services.

Each extension must identify a real user, a concrete provider or implementation, a trust boundary, and evidence that the existing abstraction is sufficient or needs revision.

Regional rails require accessible provider relationships and operational support; geography cannot be reduced to code alone. Adapter maintenance, provider access charges, security review, and incident response need named owners and a sustainable funding model before promising broad coverage.

Registries and governance should grow around actual independent contributors. An open specification and an export path remain required even if the project offers paid hosting or support.

# 15. Standard for progress

Measure progress by what users and independent developers can safely do:

- Can a user understand and restore their financial record?
- Can two unrelated providers fit without hiding material differences?
- Can a third party implement the contract in another language?
- Can stale or malicious information be contained?
- Can a crash leave an operation uncertain without causing a duplicate payment?
- Can an application do useful work without unrestricted authority?
- Can another team continue the software without the founders?

Sumer earns its scope one reliable capability at a time. The long-term ambition stays broad; the next implementation remains concrete.
