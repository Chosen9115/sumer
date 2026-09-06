# Sumer

An open financial operating system.

Your financial environment should belong to you. Banks, wallets, brokers, payment
providers, and applications should connect to it through open interfaces, with
authority you can inspect and constrain.

## Status: pre-alpha. Nothing here is usable yet.

There is no working software in this repository. What exists is the founding plan,
the accepted architecture decisions, and the development method. Code begins at
Milestone 0.

Do not connect real accounts to anything here. There is nothing to connect them to.

## The first product

Deliberately small, and read-only:

> Connect a Bitcoin wallet and a bank account, see accurate balances and history
> together, understand the source and freshness of every observation, and export
> the complete record.

No agent, no token, no marketplace, no new identity standard, and no execution
engine are required for that to be useful. Execution comes later, behind an
authorization boundary that does not exist yet.

## What this is not

- Not a neobank, wallet, brokerage, or payment app.
- Not Bitcoin-only, Ethereum-first, stablecoin-first, or bank-first. Assets and
  providers keep their real technical, legal, custody, and settlement differences.
- Not a promise that your bank approvals, legal agreements, or account identifiers
  are portable. Software and records are portable; relationships are not.

## Reading order

| File | What it is |
|---|---|
| [`constitution/FOUNDING_PLAN.md`](constitution/FOUNDING_PLAN.md) | Direction, architecture, and the delivery sequence. Start here. |
| [`adr/`](adr/) | Consequential decisions, including why the core is Rust. |
| [`CLAUDE.md`](CLAUDE.md) | How this project is built and what gets a change rejected. |
| [`SECURITY.md`](SECURITY.md) | Reporting a vulnerability. |
| [`CONTRIBUTING.md`](CONTRIBUTING.md) | How to propose work. |

## Design commitments worth knowing before you read the code

- **Exact money.** Amounts are exact coefficient-and-scale values. Wire amounts are
  validated decimal strings. JSON floating-point numbers are never money.
- **Unknown is not zero.** A failed refresh marks data stale or unavailable. It does
  not erase the last observation and it never renders as `0`.
- **Provenance on every observation.** Source, observation time, and freshness travel
  with the data. Conflicts are represented, not silently resolved toward the newest
  response.
- **Conformance proves conformance.** An adapter passing the suite follows a contract
  under those tests. It does not prove the provider is honest or the adapter is
  uncompromised.
- **Process separation is a crash boundary, not a security boundary.** Until OS-level
  isolation is defined and tested, only explicitly trusted adapters run, and that
  limitation is visible.

## Adapters

Adapters are separate processes speaking a versioned JSON Lines protocol over stdin
and stdout. The core is Rust; **an adapter can be written in any language**.
Compatibility belongs to the wire contract and the conformance suite, not to a
language SDK.

## License

Apache-2.0. See [LICENSE](LICENSE).
