# Security policy

## Status

Sumer is pre-alpha and has no release. There is no deployed system, no user data,
and nothing holding real funds. Report anything you find anyway — the architecture
decisions being made now are the ones that will be expensive to change later.

## Reporting a vulnerability

Use **GitHub private vulnerability reporting**: the Security tab of this repository,
"Report a vulnerability". That channel is private to the maintainers.

Please do not open a public issue for a security problem.

Include what you would want if you were receiving it: what you did, what happened,
what you expected, and why it matters. A proof of concept helps. A working exploit
against a third party's production system does not — do not test against real
provider accounts that are not yours.

We will acknowledge, tell you what we think, and tell you when it is fixed. There is
no bounty program.

## Scope

In scope: the protocol design, the wire contract, the adapter isolation model, the
policy and authorization design, the money representation, and anything in this
repository.

Design-level reports are explicitly welcome. "This boundary does not hold" is a
valid finding even with no code to exploit — that is the most useful kind of report
at this stage, and the review that produced ADR 0001's revisions was exactly that.

Out of scope: vulnerabilities in third-party providers, banks, or chains. Report
those to the provider.

## What we already know

These are documented limitations, not undiscovered bugs. Reporting them is welcome
but they are not news:

- **Process separation is a crash boundary, not a security boundary.** Adapters run
  as separate processes, but OS-level restrictions on filesystem access, process
  inspection, inherited environment, credentials, and network destinations are not
  yet defined or tested. Until that gate passes, only explicitly trusted adapters
  may run.
- **Some providers force an adapter to hold a bearer credential.** We document that
  exposure and its upstream scopes rather than claiming no adapter ever handles a
  secret.
- **Local state encryption is not yet chosen.** SQLite provides none by default. See
  ADR 0002.
- **Conformance does not establish honesty.** A conformant adapter can lie.

## Handling of secrets

Adapters never receive wallet private keys. Adapter logs go to stderr with secret
redaction. If you find a path where a credential, key, or full account identifier
reaches a log, an error message, an export, or an adapter that should not have it,
that is a valid report and we want it.
