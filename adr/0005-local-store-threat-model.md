# ADR 0005 — The local store's threat model, and why it ships unencrypted

- **Status:** accepted
- **Date:** 2026-09-07
- **Decision by:** Linus, Milestone 1

## Context

ADR 0002 chose SQLite and deliberately refused to pick an encryption approach,
on the grounds that the choice depends on a threat model that was not yet
written. PR 4 is where the store stops being a schema sketch and starts holding
a real user's data on a real disk, so the threat model is now owed. This is it.

`constitution/FOUNDING_PLAN.md` §8 requires that encryption key storage,
locked-state behaviour, backup encryption, recovery and deletion be
*specified* — and §9 requires that the encryption and key-management approach
be selected and tested before the encrypted-state milestone (Milestone 3) is
considered complete. Neither says the first release must be encrypted. What
they forbid is shipping without having thought about it, and shipping a
reassuring claim in place of a real one.

**What the profile directory holds after Milestone 1.**

| Asset | Present | What it discloses |
|---|---|---|
| Full transaction history | yes | Every amount, date, counterparty description and provider-side identifier the host has ever read |
| Address sets | yes | Which scriptPubKeys belong to one wallet — the same clustering `adapters/bitcoin/PRIVACY.md` says is permanent and unrevocable |
| Balances | yes | Current and historical figures per category |
| Credentials | **no** | Milestone 1 is watch-only. A watch-only Bitcoin wallet has no credential, no session, and nothing that can be revoked (ADR 0004) |

The absence of credentials is the fact that makes this decision defensible and
is also the fact that expires it. Milestone 2 is the bank vertical slice, and a
bank connection carries a bearer credential (plan §6 requires documenting that
exposure rather than claiming no adapter ever handles a secret). That is the
first secret in this system worth a keyring.

**The two realistic losses**, stated as losses rather than as attacker
archetypes:

1. **A stolen or discarded disk.** A laptop is taken, or a drive is sold,
   returned under warranty, or thrown away. The attacker has the platter and
   unlimited time; no process of ours is running.
2. **A profile directory synced to a cloud backup.** The user's backup tool,
   or their home-directory sync, copies `<profile>/` to a third party. Nothing
   was stolen and nothing misbehaved — the user's own tooling did exactly what
   they configured it to do, and the history is now on somebody else's
   infrastructure.

## Decision

**Ship Milestone 1's local store unencrypted, with the file modes set, and say
precisely what that does and does not buy.**

1. **Full-disk encryption is the correct tool for loss 1, and `sumer init`
   recommends it by name in its first output.** Not in a manual, not in a
   README section a user reaches after their history is already on the disk —
   in the first thing the command that creates the profile prints. Loss 1 is a
   whole-device loss, and the whole-device answer to it already exists on every
   platform this runs on, is maintained by people who do nothing else, and
   protects the user's other files at the same time. An application-level
   scheme layered over an unencrypted disk would protect this one directory
   while the swap file, the temp directory, the shell history and every other
   application's data stayed in the clear.

2. **`0700` on the profile directory and `0600` on its files buy the
   other-local-user case only, and they do not survive a backup sync.** `init`
   says both. The modes are worth setting: a multi-user machine is real, and
   the default umask on several platforms would otherwise leave the store
   world-readable. But a sync client runs as the same user, reads the files
   because it is allowed to, and writes them somewhere whose permissions are
   the remote service's business and not this project's. Loss 2 is therefore
   **not** mitigated by anything in this decision, and the honest thing is to
   tell the user that at the moment they create the profile, when excluding the
   directory from their backup set is still a one-line change.

3. **Application-level encryption ships at Milestone 3, triggered by Milestone
   2's first bank credential.** That is the point at which the store holds
   something whose disclosure is not merely a privacy loss but an access
   grant — and it is also the point at which the questions plan §8 asks
   (key storage, locked-state behaviour, backup encryption, recovery,
   deletion) have concrete answers instead of hypothetical ones, because there
   is a secret whose lifecycle defines them.

## Alternatives considered

- **SQLCipher, or an encrypted container, in Milestone 1.** Rejected as
  premature, not as wrong. It buys nothing against loss 1 that full-disk
  encryption does not buy better and more broadly, and against loss 2 it buys
  something real only once the key is not sitting beside the database — which
  is the key-management design plan §8 requires and Milestone 3 owes. Shipping
  the cipher first and the key management later is the version of this that
  produces a reassuring claim with nothing behind it: a database file that
  cannot be opened without a key stored in the same synced directory is
  encrypted in name only. ADR 0002 declined to pick between the candidates for
  exactly this reason and that stands.

- **A passphrase prompt on every command in Milestone 1.** Rejected: it is
  locked-state behaviour without a locked-state design (plan §8), it makes
  `refresh` unrunnable from cron — which is the workflow PR 4's single-writer
  lock exists to support — and the thing it would be protecting is public
  blockchain data plus an address set. The cost lands on the user every day and
  the benefit arrives at Milestone 2.

- **The argument this ADR originally accepted, and had to correct.** The first
  draft justified shipping unencrypted on the grounds that *adapters run under
  the same uid as the host, so an adapter can read the database anyway, so
  application-level encryption is theatre.* **That is wrong, and it is wrong in
  a way worth recording, because it sounds correct.** It conflates the
  **running host** with the **disk at rest**. `spec/wire.md` §9 and ADR 0001
  (Revision 2) are about a live process's exposure to a same-uid peer; both of
  the losses above happen when no process of ours is running at all, and an
  adapter is not a participant in either one. A stolen drive does not contain a
  running adapter. Whether adapters are isolated has no bearing on whether the
  bytes on that platter are readable — the two questions do not touch. The real
  argument for deferring is the one in the Decision: the right tool for loss 1
  is full-disk encryption, and the trigger for the second layer is the first
  secret, which arrives in Milestone 2.

- **Doing nothing and saying nothing.** Rejected. It is the only option here
  that fails plan §2.7 outright — a claim requires evidence, and silence about
  a threat model is a claim that there isn't one.

## Consequences

- **A user who does not turn on full-disk encryption is unprotected against
  loss 1, and we told them so once.** `init` prints the recommendation; it does
  not verify that the recommendation was taken, and it does not refuse to run.
  A check that FDE is enabled is platform-specific and easy to get wrong in the
  reassuring direction (reporting "encrypted" for a suspended volume with the
  key in memory), so we do not make a claim we cannot stand behind.
- **A user whose home directory is synced has put their financial history on a
  third party's infrastructure, and the modes did not stop it.** This is stated
  at `init` time and remains true until Milestone 3.
- **Milestone 3's encryption work now has a written threat model to be tested
  against**, which is what ADR 0002 said it was waiting for and what plan §9
  requires before that milestone can be considered complete.
- **The store remains inspectable with any `sqlite3` binary**, which ADR 0002
  called a continuity property (plan §2.8) rather than a convenience. Milestone
  3 will have to decide what it does to that property; this ADR does not
  pre-empt it.

## Security implications

This ADR makes exactly one security claim: **the file modes prevent another
local user, on the same machine, without root, from reading the store.**
Nothing else. It does not defend against a stolen disk (full-disk encryption
does), a backup sync (nothing here does), a compromised account (nothing here
does), or a same-uid process — which is the boundary `spec/wire.md` §9 already
states verbatim and does not soften.

Encryption at rest protects stored data under a defined threat model; it does
not protect an unlocked process from a compromised host. That is plan §8's
sentence and ADR 0002 repeated it. This ADR is where the "defined threat model"
half stops being a placeholder.

## Reversibility

High, and deliberately so. Nothing in the schema, the CLI surface, or the wire
depends on the store being plaintext. Adding encryption at Milestone 3 is a
migration of the store file, which plan §8 already requires to be tested
against export and restore, plus the key-management design that milestone owes
anyway. Nothing decided here has to be undone first — the file modes stay, the
`init` recommendation stays, and the second layer lands on top.
