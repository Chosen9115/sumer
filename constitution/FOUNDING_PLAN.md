# Open Financial System
## Founding Plan for an Agent-Driven, Community-Built Financial Operating System

> **Working thesis:** Your money should not live inside a financial institution's interface. Financial institutions, networks, assets, and financial products should live inside an open system that the user controls.

---

# 0. Purpose

This document defines the initial operating model for a global, open-source, agent-driven financial system.

The goal is not to build another neobank, crypto wallet, brokerage, payment app, or fintech super-app.

The goal is to build an **open financial operating system**: a common layer through which people, businesses, developers, institutions, networks, and autonomous agents can interact with financial resources across the world.

The system should allow:

- a bank account to coexist with a Bitcoin wallet;
- USDC on Base to coexist with cash in a traditional bank;
- Ethereum assets to coexist with government securities;
- local payment rails such as ACH, SPEI, SEPA, Pix, UPI, or Faster Payments to plug into a common interface;
- regulated institutions to enforce their own legal and compliance requirements;
- users to control their own keys, permissions, policies, applications, and providers;
- developers anywhere in the world to add support for new financial resources without permission from a central company;
- agents to continuously extend, test, audit, document, and improve the system.

The project should be designed so that its usefulness increases as more independent contributors, providers, applications, and agents join it.

The project should not depend on a single company, blockchain, bank, government, custodian, cloud provider, or jurisdiction in order to survive.

---

# 1. Manifesto

## 1.1 The user is the center

Financial software today is organized around institutions.

A bank gives users a bank interface.  
A brokerage gives users a brokerage interface.  
A crypto wallet gives users a crypto interface.  
A payment application gives users a payment interface.

This architecture is backwards.

The system should instead begin with the user and represent the full financial world around them.

The primary object is not the bank account.

The primary object is not the wallet.

The primary object is not the blockchain address.

The primary object is the **financial identity and financial graph of the user or organization**.

Everything else is a resource attached to that graph.

---

## 1.2 Financial providers should be replaceable

A provider should be an implementation detail.

A bank, broker, blockchain, custodian, stablecoin issuer, payment rail, exchange, or treasury provider should be able to compete for the user's business without owning the user's entire financial environment.

The system should make it progressively easier to replace:

- banks;
- payment processors;
- wallets;
- custodians;
- chains;
- brokers;
- market makers;
- KYC providers;
- FX providers;
- data providers;
- treasury products;
- execution venues.

The system should reduce provider lock-in over time.

---

## 1.3 No financial ideology should be privileged

The system is not Bitcoin-only.

It is not Ethereum-first.

It is not stablecoin-first.

It is not traditional-bank-first.

It is not DeFi-first.

It is not anti-bank.

It is not anti-government.

It is not pro-custody or anti-custody.

The system is **pro-choice at the protocol level**.

If a user wants Bitcoin, they should be able to use Bitcoin.

If a user wants USDC on Base, they should be able to use USDC on Base.

If a user wants a traditional bank account, they should be able to connect one.

If a user wants to hold government securities, they should be able to use a provider that offers them.

The architecture should expose differences honestly rather than pretending all financial assets have identical legal, technical, liquidity, custody, or risk properties.

---

## 1.4 Open source is structural, not cosmetic

The project is only meaningfully open if:

- the core protocol is open;
- the reference client is open;
- the provider interfaces are open;
- the application interfaces are open;
- the permission model is open;
- the governance process is open;
- the security model is inspectable;
- alternative clients can exist;
- alternative providers can exist;
- independent implementations can exist;
- users can leave without losing their financial identity or data.

An open-source frontend over a proprietary backend is not enough.

---

## 1.5 Regulation belongs at the correct boundary

The system should not pretend that regulation disappears because software is decentralized or open source.

Banks remain responsible for bank regulation.

Brokerages remain responsible for securities regulation.

Custodians remain responsible for custody obligations.

Providers remain responsible for KYC, AML, sanctions, licensing, investor eligibility, and jurisdictional rules where those apply.

The protocol should make these requirements legible and composable.

The core system should not attempt to become the compliance department of the world.

---

## 1.6 Users own permissions

Applications and agents should never receive unlimited authority by default.

Every capability should be explicit.

Examples:

- read balance;
- read transactions;
- request quote;
- execute a swap under a certain value;
- move funds only to approved destinations;
- rebalance within a policy;
- withdraw;
- change permissions;
- sign transactions;
- rotate keys.

Permissions should be:

- minimal;
- inspectable;
- revocable;
- time-bounded where useful;
- value-bounded where useful;
- asset-bounded where useful;
- destination-bounded where useful.

The security model should assume that third-party code will occasionally fail or become malicious.

---

## 1.7 The system should remain useful if the founding team disappears

A successful protocol should not depend on permanent central leadership.

If the original maintainers vanish:

- user data should still exist;
- user keys should still work;
- wallets should still work;
- providers should still be reachable;
- adapters should still be forkable;
- applications should still be forkable;
- the protocol should still be implementable;
- the community should still be able to continue development.

The project should be designed for survival beyond its founders.

---

## 1.8 Local-first whenever possible

The default architecture should minimize centralized collection of financial data.

Where practical:

- user state lives locally;
- credentials live locally;
- permissions live locally;
- sensitive data is encrypted locally;
- cloud synchronization is optional;
- keys are never exposed to applications;
- adapters request capabilities, not secrets.

Central infrastructure should exist only where required for coordination, indexing, routing, availability, or regulated execution.

---

## 1.9 The system should make complexity disappear without hiding reality

Users should not need to understand:

- chain IDs;
- bridging routes;
- correspondent banks;
- payment message formats;
- RPC providers;
- settlement windows;
- smart contract implementations;
- quote APIs;
- routing logic.

But the system must remain transparent enough that sophisticated users can inspect:

- execution route;
- fees;
- counterparty;
- settlement risk;
- custody model;
- legal wrapper;
- asset issuer;
- provider;
- permissions;
- source code.

Abstraction should reduce unnecessary complexity, not conceal material risk.

---

## 1.10 Build the protocol so the world can extend it

The project's long-term success should be measured by how much useful functionality is created by people who never needed approval from the core maintainers.

A healthy system allows a developer in:

- Brazil to build Pix;
- Mexico to build SPEI;
- India to build UPI;
- Europe to build SEPA modules;
- Nigeria to integrate local rails;
- Argentina to build local FX access;
- any jurisdiction to expose compatible banks, brokers, wallets, or regulated products.

The system should evolve through contribution rather than central planning.

---

# 2. Proposed Architecture

## 2.1 System model

The proposed system is composed of three major layers:

1. **Protocol**
2. **Reference client**
3. **Ecosystem**

The protocol defines what things are.

The client makes them usable.

The ecosystem makes them abundant.

---

## 2.2 Core system diagram

```text
                          USER / ORGANIZATION
                                  │
                                  ▼
                        Financial Identity
                                  │
                                  ▼
                         Financial Graph
                                  │
                  ┌───────────────┼────────────────┐
                  │               │                │
                  ▼               ▼                ▼
             Applications     Intent Engine    Policy Engine
                  │               │                │
                  └───────────────┼────────────────┘
                                  │
                                  ▼
                           Resource Layer
                                  │
       ┌────────────┬─────────────┼────────────┬─────────────┐
       │            │             │            │             │
       ▼            ▼             ▼            ▼             ▼
    Bitcoin       EVM/Base      Banks       Brokers      Securities
       │            │             │            │             │
       ▼            ▼             ▼            ▼             ▼
 Lightning       USDC/ETH       ACH/SEPA      APIs        Treasuries
                                SPEI/Pix
```

---

## 2.3 Financial Identity

The identity layer answers:

- Who is the user?
- Which devices can act for the user?
- Which wallets belong to the user?
- Which legal identities are associated with the user?
- Which organizations can the user represent?
- Which credentials has the user received?
- Which providers have approved the user?
- Which permissions has the user granted?

Identity may include:

- passkeys;
- device keys;
- recovery methods;
- wallet addresses;
- legal identity attestations;
- business identity attestations;
- KYC evidence;
- jurisdiction;
- provider-specific approvals;
- reusable credentials.

The identity layer must distinguish between:

- user-owned identity;
- provider-issued approval;
- third-party credentials;
- legal eligibility.

---

## 2.4 Financial Graph

The financial graph is the core abstraction.

It represents every financial resource attached to a user or organization.

Example:

```text
User
├── Bank Account
│   ├── USD
│   ├── ACH
│   └── Wire
├── Bitcoin Wallet
│   ├── BTC
│   └── Lightning
├── Base Wallet
│   ├── ETH
│   └── USDC
├── Brokerage Account
│   ├── Cash
│   ├── ETFs
│   └── Treasuries
└── Treasury Provider
    └── Government Securities
```

The graph must support:

- multiple owners;
- organizations;
- permissions;
- account relationships;
- delegated authority;
- read-only connections;
- custodial resources;
- self-custodied resources;
- regulated products;
- programmable resources.

---

## 2.5 Resource Model

Every financial resource implements a common capability model.

A conceptual interface:

```ts
interface FinancialResource {
  identify(): ResourceIdentity
  balances(): Balance[]
  assets(): Asset[]
  capabilities(): Capability[]
  history(query): Transaction[]
  quote(intent): Quote[]
  execute(intent, authorization): ExecutionResult
}
```

No resource is required to implement every capability.

Example capabilities:

```text
READ_BALANCE
READ_HISTORY
RECEIVE
SEND
EXCHANGE
BUY
SELL
SUBSCRIBE
REDEEM
BORROW
REPAY
SIGN
STAKE
VOTE
WITHDRAW
DEPOSIT
```

This enables heterogeneous financial systems to participate without pretending they are identical.

---

## 2.6 Asset Model

The system must distinguish:

- currencies;
- cryptocurrencies;
- stablecoins;
- securities;
- tokenized securities;
- deposits;
- claims;
- commodities;
- fund shares;
- debt instruments;
- synthetic assets.

Each asset should contain metadata such as:

```text
asset_id
issuer
network
currency
legal_type
custody_model
settlement_model
transferability
jurisdiction
risk_metadata
pricing_sources
```

The system should never assume that two assets with the same displayed currency are legally equivalent.

For example:

```text
USD bank deposit
USDC
USDT
tokenized Treasury fund share
money market fund share
physical USD cash
```

may all approximate one dollar economically while having different risks and legal structures.

---

## 2.7 Provider Adapters

Providers connect the protocol to real-world systems.

Examples:

```text
bitcoin-core-adapter
lightning-adapter
ethereum-adapter
base-adapter
solana-adapter
plaid-adapter
open-banking-adapter
interactive-brokers-adapter
spei-adapter
pix-adapter
sepa-adapter
treasury-provider-adapter
```

A provider adapter should declare:

- resources exposed;
- capabilities;
- authentication method;
- custody model;
- jurisdiction;
- compliance requirements;
- permission requirements;
- limits;
- execution semantics;
- settlement semantics;
- fees;
- failure modes.

---

## 2.8 Application Layer

Applications consume financial resources.

Applications should not need to know the internal implementation details of every provider.

Examples:

- payments;
- portfolio;
- payroll;
- recurring purchases;
- accounting;
- FX optimization;
- treasury management;
- savings;
- subscriptions;
- tax estimation;
- invoice payments;
- business expenses;
- charitable giving.

Applications interact through capabilities.

---

## 2.9 Intent Engine

The intent engine translates user goals into execution plans.

Example:

```text
Intent:
Send Alice $1,000 USD

Possible routes:
1. Bank → ACH
2. USDC Base → Base wallet
3. USDC Ethereum → bridge → Base
4. Bitcoin → Lightning
5. Wise → bank transfer
```

The engine evaluates routes against policy:

```text
cost
speed
counterparty risk
settlement certainty
network risk
liquidity
user preference
tax implications
jurisdiction
privacy
limits
```

The intent engine may be:

- deterministic;
- rules-based;
- agent-assisted;
- market-based;
- solver-based.

Execution must remain constrained by permissions.

---

## 2.10 Policy Engine

The policy engine defines what applications and agents are allowed to do.

Example:

```text
Treasury Agent

May:
- read balances;
- read Treasury quotes;
- rebalance up to $5,000/day;
- use only approved providers;
- maintain at least $20,000 liquid.

May not:
- send funds to third parties;
- change security settings;
- export keys;
- use leverage;
- buy unapproved assets.
```

Policy should be machine-readable.

---

## 2.11 Permission Model

Permissions should be capability-based.

Possible dimensions:

```text
action
asset
resource
amount
destination
frequency
time period
provider
jurisdiction
application
agent
```

Example permission:

```json
{
  "action": "TRANSFER",
  "asset": "USDC",
  "network": "BASE",
  "max_amount": "500",
  "period": "DAY",
  "destinations": ["alice.eth", "vendor_allowlist"],
  "expires": "2027-01-01"
}
```

---

## 2.12 Registry Layer

The ecosystem requires registries for discovery.

Possible registries:

- provider adapters;
- applications;
- agents;
- asset metadata;
- identity providers;
- security audits;
- trust attestations;
- schemas;
- intent solvers.

The registry should distinguish:

```text
exists
maintained
verified
audited
recommended
deprecated
unsafe
```

No single registry should be mandatory forever.

---

## 2.13 Governance

Governance should apply to:

- protocol specifications;
- naming;
- compatibility rules;
- module registry standards;
- security requirements;
- reference implementations;
- grant allocation;
- documentation;
- upgrade processes.

Governance should not directly control:

- user money;
- provider compliance decisions;
- lending decisions;
- KYC approvals;
- securities eligibility;
- private keys.

---

# 3. Iterative Growing Pieces

The project should grow by proving one abstraction at a time.

---

## Phase 0 — Constitution and Protocol Skeleton

Goal:

> Make the idea precise enough that multiple independent developers can reason about the same system.

Build:

- manifesto;
- terminology;
- resource specification;
- asset specification;
- capability specification;
- provider interface;
- permission model;
- initial architecture decision records;
- threat model;
- contribution model.

Success condition:

Two developers independently implement compatible toy adapters.

---

## Phase 1 — Read-Only Financial Graph

Goal:

> Demonstrate that multiple financial worlds can appear inside one system.

Initial resources:

- Bitcoin wallet;
- EVM wallet;
- Base wallet;
- bank connection;
- one investment or Treasury source.

Features:

- connect resource;
- detect balances;
- normalize assets;
- show transaction history;
- show aggregate portfolio;
- local encrypted state.

Success condition:

A user can see traditional and decentralized assets in one coherent interface.

---

## Phase 2 — Provider SDK

Goal:

> Make extension possible without modifying core code.

Build:

- provider SDK;
- adapter lifecycle;
- capability declarations;
- test harness;
- local sandbox;
- example adapter;
- provider documentation;
- compatibility test suite.

Success condition:

A contributor outside the founding team builds a working provider adapter.

This is the first major proof of openness.

---

## Phase 3 — Send Intent

Goal:

> Prove the system can execute across heterogeneous providers.

Build:

- recipient abstraction;
- send intent;
- quote interface;
- route comparison;
- approval screen;
- transaction execution;
- confirmation;
- failure handling.

Success condition:

One intent can be executed through at least two unrelated financial systems.

Example:

```text
Send $100
→ ACH
→ USDC on Base
```

---

## Phase 4 — Permission System

Goal:

> Make third-party applications and agents safe enough to exist.

Build:

- capability grants;
- scoped permissions;
- revocation;
- transaction limits;
- destination allowlists;
- expiration;
- human approval;
- audit log;
- simulation.

Success condition:

An external application can perform useful actions without receiving unrestricted access.

---

## Phase 5 — Application SDK

Goal:

> Move from provider extensibility to application extensibility.

Build:

- application SDK;
- financial graph query API;
- intent API;
- permission request API;
- UI extension model;
- local application sandbox.

Example applications:

- recurring BTC purchase;
- savings sweeps;
- treasury optimizer;
- subscription manager;
- expense categorizer.

Success condition:

An independent developer builds an application that works across multiple providers.

---

## Phase 6 — Agent Runtime

Goal:

> Allow autonomous software to operate within user-defined financial policies.

Build:

- agent identity;
- agent permissions;
- policy enforcement;
- planning;
- execution simulation;
- approval thresholds;
- audit trail;
- rollback or compensating actions where possible.

Success condition:

An agent can perform a constrained financial workflow safely without arbitrary access.

---

## Phase 7 — Global Rail Expansion

Goal:

> Turn geography into adapters.

Community additions:

```text
ACH
RTP
FedNow
SPEI
Pix
SEPA
Faster Payments
UPI
Interac
Lightning
Solana
local brokerages
regional wallets
```

Success condition:

Multiple regional communities maintain their own integrations.

---

## Phase 8 — Solver and Routing Ecosystem

Goal:

> Allow providers to compete to satisfy financial intents.

Build:

- quote protocol;
- route scoring;
- solver interface;
- execution guarantees;
- slippage rules;
- settlement verification;
- reputation;
- failure penalties.

Success condition:

Multiple independent solvers compete to execute the same intent.

---

## Phase 9 — Portable Identity and Credentials

Goal:

> Reduce duplicated onboarding while respecting provider-specific regulatory obligations.

Build:

- credential storage;
- reusable attestations;
- identity proofs;
- provider approval records;
- selective disclosure;
- consent.

Success condition:

A user can reuse identity evidence across multiple integrations without centralized identity ownership.

---

## Phase 10 — Independent Clients

Goal:

> Prove the protocol is larger than the reference implementation.

Success condition:

At least one major independent client can use the same providers, applications, identities, and financial graph.

At that point the project has become a protocol ecosystem rather than a product.

---

# 4. Systematic Building Approach

## 4.1 Build contracts before features

Every new capability should begin with:

1. problem statement;
2. threat model;
3. protocol interface;
4. invariants;
5. compatibility tests;
6. reference implementation;
7. documentation;
8. adversarial tests.

Do not begin with UI.

---

## 4.2 Every feature must identify its layer

Every proposal must state whether it changes:

```text
protocol
resource
provider adapter
application
intent
policy
identity
registry
reference client
agent runtime
```

This prevents architecture from collapsing into an unstructured codebase.

---

## 4.3 Invariants before implementation

Core invariants should include:

### Security

- no application receives private keys;
- permissions default to denied;
- execution requires explicit capability;
- revoked permissions cannot execute;
- modules cannot silently escalate privileges.

### Portability

- providers can be removed;
- resources can be exported;
- clients can be replaced;
- protocols remain documented independently of implementations.

### Interoperability

- compatibility is testable;
- implementations must declare supported protocol version;
- adapters must expose capability metadata.

### Transparency

- fees are visible;
- provider is visible;
- route is inspectable;
- custody model is visible;
- asset issuer is discoverable.

---

## 4.4 Build one vertical slice at a time

A vertical slice means completing a small capability across all necessary layers.

Example:

```text
Connect Bitcoin
↓
Resource discovery
↓
Balance normalization
↓
Financial graph
↓
Client display
↓
Tests
↓
Docs
```

Do not build five half-complete layers simultaneously.

---

## 4.5 Separate protocol maturity from implementation maturity

A protocol can be:

```text
experimental
draft
candidate
stable
deprecated
```

An implementation can independently be:

```text
prototype
alpha
beta
production
unsupported
```

Do not confuse a working demo with a stable protocol.

---

## 4.6 Architecture Decision Records

Major decisions should be recorded permanently.

Each ADR should contain:

```text
Context
Decision
Alternatives considered
Consequences
Security implications
Migration path
Reversibility
```

Agents and humans should treat ADRs as part of the project's memory.

---

## 4.7 Request for Comment process

Significant protocol changes should use RFCs.

An RFC should contain:

```text
Summary
Motivation
Specification
Examples
Security considerations
Compatibility
Migration
Alternatives
Open questions
```

Agents may draft RFCs.

Humans and agents may review them.

Protocol changes should never occur through undocumented implementation drift.

---

## 4.8 Compatibility test suites are first-class infrastructure

The most valuable artifact in a protocol project is often not the reference implementation.

It is the test suite that tells independent implementations whether they are compatible.

Every interface should eventually have:

- fixtures;
- conformance tests;
- adversarial tests;
- property-based tests;
- fuzz tests;
- replay tests.

---

# 5. Development Loop and Antifragility

The project should become stronger as it encounters failures.

The development loop should therefore deliberately convert every failure into permanent system knowledge.

---

## 5.1 Core loop

```text
Observe
  ↓
Propose
  ↓
Model
  ↓
Threat-model
  ↓
Implement
  ↓
Test
  ↓
Attack
  ↓
Deploy experimentally
  ↓
Observe failure
  ↓
Encode lesson
  ↓
Improve protocol / tests / tooling
  ↓
Repeat
```

---

## 5.2 Every failure must leave a scar

When something fails, the response should not stop at fixing the bug.

The system should determine:

1. Why was this failure possible?
2. Which assumption was wrong?
3. Which invariant was missing?
4. Which test would have caught it?
5. Which monitoring signal would have exposed it sooner?
6. Which documentation allowed misunderstanding?
7. Which permission was too broad?
8. Which dependency was too trusted?
9. Which protocol ambiguity enabled the failure?

Then add permanent artifacts:

```text
regression test
new invariant
new ADR
new lint rule
new security rule
new fixture
new monitoring condition
new documentation
new compatibility case
```

A repaired failure should make the same class of failure harder across the entire ecosystem.

---

## 5.3 Diversity produces resilience

The system should encourage:

- multiple provider implementations;
- multiple clients;
- multiple RPC providers;
- multiple pricing sources;
- multiple execution solvers;
- multiple identity providers;
- multiple maintainers;
- multiple jurisdictions.

Redundancy is not waste.

In an open financial system, diversity prevents single points of failure.

---

## 5.4 Forkability is a resilience mechanism

The project should remain easy to fork.

This means:

- reproducible builds;
- clear dependencies;
- portable configuration;
- open schemas;
- migration tools;
- documented deployment;
- no secret server dependency in the core protocol.

The credible ability to fork constrains bad governance.

---

## 5.5 Progressive decentralization

Do not decentralize prematurely.

Early phases may need strong technical leadership.

Over time, decentralize:

```text
implementation
maintenance
registries
governance
testing
security review
roadmap
funding
regional integrations
```

Decentralization should follow demonstrated capability.

---

## 5.6 Antifragility score

Every major subsystem can be evaluated against questions such as:

```text
Can one provider failure break it?
Can one maintainer disappearance break it?
Can one cloud outage break it?
Can one jurisdiction break it?
Can one compromised plugin steal everything?
Can one bad protocol upgrade corrupt state?
Can users migrate away?
Can another team reimplement it?
Can agents verify it independently?
```

The goal is not zero failure.

The goal is making failures increasingly local, visible, recoverable, and informative.

---

# 6. Agent-Driven Buildout

Agents should not merely write code.

Agents should become structured participants in the project's architecture, testing, governance, security, documentation, and maintenance.

The project should be designed from the beginning as a collaboration between:

- humans;
- autonomous agents;
- specialized development agents;
- security agents;
- research agents;
- test agents;
- documentation agents;
- governance agents.

---

## 6.1 Agent Constitution

Every project agent must inherit the principles defined in:

1. the Manifesto;
2. the Architecture;
3. the Iterative Growth Plan;
4. the Systematic Building Approach;
5. the Antifragile Development Loop.

Agents must not optimize locally in ways that violate these higher-level constraints.

Decision hierarchy:

```text
Manifesto
  ↓
Protocol invariants
  ↓
Architecture
  ↓
RFCs / ADRs
  ↓
Current milestone
  ↓
Issue
  ↓
Implementation
```

When a lower-level instruction conflicts with a higher-level principle, the agent should escalate rather than silently violate the architecture.

---

## 6.2 Agent Roles

### Research Agent

Responsibilities:

- monitor new financial infrastructure;
- track relevant protocols;
- identify new rails;
- identify new wallet standards;
- identify new open banking systems;
- track regulations;
- compare provider capabilities;
- draft research notes;
- propose RFCs.

Output:

```text
research/
```

---

### Protocol Architect Agent

Responsibilities:

- maintain protocol coherence;
- review new interfaces;
- detect abstraction leaks;
- review compatibility;
- propose schemas;
- maintain ADRs;
- ensure new features fit the system model.

Output:

```text
spec/
adr/
rfc/
```

---

### Implementation Agent

Responsibilities:

- implement approved specifications;
- maintain reference code;
- create adapters;
- fix defects;
- improve developer experience.

Output:

```text
core/
sdk/
plugins/
client/
```

---

### Test Agent

Responsibilities:

- derive tests from protocol invariants;
- generate compatibility tests;
- create edge cases;
- fuzz interfaces;
- test migration;
- replay incidents.

Output:

```text
tests/
conformance/
fixtures/
```

---

### Adversarial Security Agent

Responsibilities:

- assume components are malicious;
- test permission boundaries;
- search for privilege escalation;
- test compromised providers;
- simulate malicious plugins;
- simulate dependency compromise;
- attack agent workflows;
- inspect key-management surfaces.

Output:

```text
security/
threat-models/
incidents/
```

---

### Documentation Agent

Responsibilities:

- keep specifications readable;
- update documentation from code changes;
- produce examples;
- produce contributor guides;
- create migration notes;
- detect stale documentation.

Output:

```text
docs/
examples/
```

---

### Maintainer Agent

Responsibilities:

- triage issues;
- detect duplicates;
- identify abandoned modules;
- track version drift;
- suggest dependency upgrades;
- monitor compatibility failures.

---

### Governance Agent

Responsibilities:

- summarize RFC debates;
- identify unresolved tradeoffs;
- detect contradictions;
- maintain proposal status;
- generate neutral decision summaries;
- preserve institutional memory.

Agents should not unilaterally decide governance outcomes.

---

## 6.3 Agent Issue Loop

Every issue can follow:

```text
Issue submitted
    ↓
Research Agent enriches context
    ↓
Architect Agent classifies affected layer
    ↓
Security Agent identifies threat implications
    ↓
Implementation Agent proposes patch
    ↓
Test Agent produces failure cases
    ↓
Implementation Agent iterates
    ↓
Documentation Agent updates docs
    ↓
Independent Review Agent evaluates
    ↓
Human/community merge or reject
```

This loop can become increasingly automated.

---

## 6.4 Agents should consume project memory

Agents must read structured project context before acting.

Recommended structure:

```text
/constitution
  MANIFESTO.md

/spec
  RESOURCE.md
  ASSET.md
  INTENT.md
  PERMISSIONS.md
  IDENTITY.md

/adr
/rfc
/security
/incidents
/conformance
/roadmap
```

This repository is the shared memory of the project.

Agents should not rely on hidden conversational memory for architectural truth.

---

## 6.5 Machine-readable project rules

Key principles should also exist in structured form.

Example:

```yaml
invariants:
  private_keys_exposed_to_plugins: false
  permissions_default: deny
  provider_lock_in: prohibited
  protocol_requires_reference_client: false
  alternative_clients_allowed: true
  user_data_exportable: true
  provider_specific_logic_in_core: prohibited
```

Agents can validate pull requests against these invariants.

---

## 6.6 Agent-generated RFCs

When an agent identifies a potentially valuable feature, it should not immediately implement it.

It should create:

```text
RFC
↓
problem
↓
proposed abstraction
↓
compatibility impact
↓
security impact
↓
alternatives
↓
prototype
```

This keeps agents from turning the codebase into a collection of locally sensible but globally incoherent features.

---

## 6.7 Agent swarm model

Larger tasks can be decomposed.

Example: "Add Pix"

```text
Research Agent
→ studies Pix

Protocol Agent
→ maps Pix to existing capabilities

Adapter Agent
→ builds provider implementation

Security Agent
→ evaluates authentication and fraud surfaces

Test Agent
→ creates conformance suite

Documentation Agent
→ creates integration guide

Review Agent
→ checks architectural compliance
```

No single agent needs to own the entire task.

---

## 6.8 Agents improve agents

The agent system itself should use the antifragile loop.

When an agent makes a mistake:

```text
bad code
bad assumption
security miss
incorrect architecture
stale dependency
weak test
```

the system should update:

```text
agent instructions
evaluation dataset
lint rules
test suites
project memory
prompt templates
review checklists
```

Agent failures therefore improve future agent behavior.

---

## 6.9 Reputation for agents and contributors

Over time, maintain structured reputation around:

- protocol accuracy;
- security findings;
- adapter reliability;
- code quality;
- review quality;
- documentation quality;
- incident response;
- conformance.

Reputation should inform review requirements, not create permanent authority.

---

## 6.10 Human role

Humans remain essential for:

- values;
- legitimacy;
- governance;
- legal judgment;
- difficult tradeoffs;
- social trust;
- resolving ambiguity;
- assigning responsibility;
- choosing long-term direction.

Agents expand the project's ability to think, test, build, and maintain.

They should not become an unaccountable governing class.

---

# 7. Repository Structure

A possible initial repository:

```text
open-financial-system/
│
├── MANIFESTO.md
├── ROADMAP.md
├── CONTRIBUTING.md
├── SECURITY.md
│
├── constitution/
│   ├── principles.yaml
│   └── governance.md
│
├── spec/
│   ├── resource.md
│   ├── asset.md
│   ├── capability.md
│   ├── identity.md
│   ├── intent.md
│   ├── permissions.md
│   └── registry.md
│
├── adr/
├── rfc/
├── incidents/
├── security/
│
├── core/
├── sdk/
├── client/
│
├── plugins/
│   ├── bitcoin/
│   ├── evm/
│   ├── base/
│   └── open-banking/
│
├── apps/
├── agents/
│
├── conformance/
├── fixtures/
├── examples/
└── docs/
```

---

# 8. First Six Milestones

## Milestone 1 — Make the constitution real

Deliver:

- manifesto;
- architecture;
- glossary;
- invariants;
- repository;
- contribution process.

---

## Milestone 2 — Define the protocol core

Deliver:

- resource spec;
- asset spec;
- capability spec;
- provider interface;
- reference test fixtures.

---

## Milestone 3 — Prove the financial graph

Deliver adapters for:

- Bitcoin;
- EVM/Base;
- bank connection.

Read-only is enough.

---

## Milestone 4 — Prove external extensibility

Publish SDK.

Have someone outside the core team build a fourth adapter.

This is the first critical project milestone.

---

## Milestone 5 — Execute one universal intent

Implement:

```text
SEND(value, recipient)
```

Support at least two independent routes.

---

## Milestone 6 — Introduce safe autonomous action

Implement:

- permissions;
- application sandbox;
- agent identity;
- policy engine;
- audit log.

Then allow the first agent-managed financial workflow.

---

# 9. Long-Term Direction

The endpoint is not a single application.

The endpoint is a shared financial protocol ecosystem.

A mature version of the system could allow:

```text
USER
│
├── chooses client
├── chooses custody
├── chooses providers
├── chooses networks
├── chooses assets
├── chooses applications
├── chooses agents
└── chooses policies
```

while the protocol ensures these components can work together.

The system succeeds when the sentence:

> "Which bank do you use?"

becomes less important than:

> "Which financial environment do you run?"

---

# 10. Final Principle

The project should continuously move power upward:

```text
from provider
to protocol

from protocol
to user

from central team
to ecosystem

from hidden infrastructure
to open interfaces

from one implementation
to many

from fragile dependencies
to replaceable components

from manual maintenance
to agent-assisted maintenance

from closed financial products
to composable financial capabilities
```

The project is not trying to predict the future financial system.

It is trying to build an architecture capable of **absorbing whatever the future financial system becomes**.

That is the standard against which every major decision should be judged.
