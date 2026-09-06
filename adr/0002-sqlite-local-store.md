# ADR 0002 — SQLite for the local store

- **Status:** accepted
- **Date:** 2026-09-06
- **Decision by:** Linus, ratifying founding plan §9

## Context

Sumer is local-first (plan §2.5, §8). The first release stores resources,
observations, revisions, and export state on one machine for one user. Plan §4
requires "a relational local store initially" with "durable observations and
revisions sufficient to explain the current view," and warns against designing a
universal ontology in advance.

## Decision

**SQLite**, embedded in the core process, as the local structured store.

Schema grows from demonstrated need. Observations and their revisions are durable
and append-oriented, because §5.4 requires explaining how a pending record became a
posted one, and §5.5 requires recording discrepancies rather than overwriting them.

## Alternatives considered

- **Postgres.** A server process, a daemon to supervise, and a backup story the user
  did not ask for, in exchange for concurrency one local user does not need. Plan §9:
  introduce a server database only when a hosted deployment has an established need.
- **Embedded key-value store (sled, RocksDB).** Loses relational queries and ad-hoc
  inspection. Reconciliation is inherently relational — it compares observations
  across providers, resources, and time.
- **Flat files / JSON.** No transactions. Interrupted writes during an ingest would
  corrupt exactly the durability this design exists to provide.

## Consequences

- Users can inspect their own data with any `sqlite3` binary. That is a continuity
  property (§2.8), not a convenience.
- Migrations must be tested against export/restore, per §8.
- Concurrent writers are a non-goal for the first release. A single-writer core
  process owns the file.

## Security implications

**SQLite provides no encryption.** Plan §9 is explicit: "database choice does not
supply encryption automatically." The encryption and key-management approach is a
separate, explicit decision that must be selected and tested before the encrypted-state
milestone (Milestone 3) can be considered complete. Candidates — SQLCipher, an
encrypted container, or application-level field encryption — are deliberately NOT
chosen here, because the choice depends on a threat model that is not yet written.

Encryption at rest protects stored data under a defined threat model. It does not
protect an unlocked process from a compromised host (§8).

## Reversibility

High. The store is behind the core's persistence boundary, the data is exportable by
requirement (§2.1), and export/restore is itself a tested path (§8). Migrating to
another engine is a documented export followed by an import.
