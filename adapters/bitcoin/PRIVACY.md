# Privacy: what this adapter discloses, to whom, and forever

This is a **Milestone 1 exit criterion**, not a footnote. A Bitcoin
adapter's privacy properties are decided by which addresses it asks about
and who it asks — decisions made in code, in this crate, and not visible to
the person running it unless they are written down. This is the writing
down.

Read this before pointing `--source` at anything you do not run yourself.

## What an address query discloses

Every read this adapter performs is an HTTP request naming one of your
addresses in the URL path:

```
GET /address/bc1q…            balances
GET /address/bc1q…/txs/chain  history
GET /address/bc1q…/txs/mempool
GET /tx/<txid>                the tombstone probe
```

The operator of the Esplora instance therefore learns, for every sync:

- **which addresses you are watching** — the whole set, not a sample;
- **that someone is watching them at all**, which is not otherwise public:
  the blockchain is public, but *interest* in a particular address is not;
- **your IP address**, and with it a coarse location and network;
- **when you sync, and how often** — a timing pattern that reveals when you
  are awake, when you are away, and when something prompted you to check;
- **the txids you probe**, which are transactions you previously saw and no
  longer do — that is, your wallet's own history, restated.

Their reverse proxy, their CDN, their hosting provider, and anyone with
access to their logs learn the same. So does any network observer able to
see your DNS lookups or TLS SNI, though not the paths inside the encrypted
connection.

## Querying a wallet's addresses in one burst CLUSTERS them

This is the important one, and it is not avoidable by intent.

A wallet is a set of addresses that, on the blockchain alone, may have no
visible link to each other. **Asking one server for all of them, from one
IP address, within one sync, links them.** The server does not need to
infer anything clever: it is handed the set. Every address you watch
together is thereafter known to belong together, and known to belong to
whoever was at that IP at that moment.

That linkage is exactly what coin-control, fresh receive addresses, and
avoiding address reuse exist to prevent. This adapter's normal, correct
operation defeats those practices against the party it queries.

**It is permanent and unrevocable.** There is no delete request that
un-learns a set membership, no rotation that undoes it, and no future
version of this software that can take it back. Logs are copied, retained,
subpoenaed, sold, and breached. Treat every sync against a third-party
instance as a permanent, public-in-principle disclosure of your wallet's
address set.

Running your own `esplora`/`electrs` is the only configuration in which
this disclosure does not happen. It is one line:

```
--source http://localhost:3000
```

Everything else about the adapter is identical: same protocol, same code
path, no feature is lost.

## We never transmit an xpub

An extended public key is not one address, it is **every address you will
ever derive from that branch** — past, present, and future — in a single
string. Handing one to a third-party service is a categorically larger
disclosure than any number of address queries, and this adapter has no code
path that does it. Services that accept an xpub are excluded by design, not
by configuration.

**When xpub support lands, it will leak future, unused addresses, and this
sentence is a commitment binding that PR.** Deriving from an xpub locally
does not by itself disclose anything — but *finding* your used addresses
requires gap-limit lookahead: querying the next N unused addresses to
discover whether any of them has activity. Those queries name addresses you
have never used and may never use. The observer learns forward derivation
output: a bounded window of your future receive addresses, linked to the
set already known.

It is a smaller leak than transmitting the xpub itself — bounded by the gap
limit rather than infinite — but it is **the same kind of leak** the
xpub-service rejection was about, and it must not be shipped quietly on the
grounds that "the xpub never left the machine." The xpub PR must:

1. state the gap limit it queries and make it configurable;
2. repeat this disclosure in its own documentation and in this file; and
3. default to warning, at startup, when lookahead runs against a
   third-party `--source`.

## User-Agent

Every request carries:

```
User-Agent: sumer-bitcoin/0.1
```

Honest, and deliberately not a browser string. Forging one would be a lie
told to a free service that is doing us a favour, and it would buy nothing:
the disclosure that matters is the address set, which no User-Agent hides.
A public instance that wants to know which clients are costing it bandwidth
is entitled to a truthful answer.

## The three deployments, and what each costs you

| Deployment | Privacy | Cost |
|---|---|---|
| `https://blockstream.info/api` (default) | Blockstream learns your address set, IP, and sync timing. Permanent. | Free; rate-limited; you are a guest. |
| `https://mempool.space/api` | Same, with mempool.space as the observer. | Free; rate-limited; you are a guest. |
| your own `esplora`/`electrs` | Nothing leaves your machine. Your node still connects to the Bitcoin p2p network, which is a separate and much weaker disclosure. | Your hardware, an initial block download, and the disk to keep it. |

Being a guest also carries an obligation. This adapter makes one request
per address per history page, per sync. A large wallet polled aggressively
against a free public instance is a cost someone else is paying. Run your
own instance, or sync sparingly.

## What this adapter does not protect you from

Stated plainly rather than reassuringly:

- **No Tor, no proxy, no per-request IP rotation.** `ureq` connects
  directly. If you need network-level unlinkability, this adapter does not
  provide it.
- **No query padding or decoy addresses.** The set queried is exactly the
  set configured.
- **No protection from the host.** The Sumer host runs this adapter as the
  same uid, on the same filesystem, with inherited file descriptors
  (`spec/wire.md` §9). Your `wallets.json` and `--state-dir` are readable
  by anything else running as you.
- **`--record` writes provider responses to disk in the clear**, including
  your full transaction history. It exists for building test corpora.
  Recorded corpora that are committed anywhere public must use addresses
  you are content to publish forever.
