"""Every figure this adapter ever emits leaves as zero satoshis.

The break: each balance line and each observation goes out carrying
`{"asset": "sat", "amount": "0"}`. Ids, categories, heights, `state`,
`posting`, order, counts, cursors and statuses are all untouched, so the
wallet reads as complete and healthy -- and completely empty. This is what
an adapter that has lost its mapping, or given up and answered the cheapest
thing that parses, looks like on the wire.

Before the anchor in §5 this survived the ENTIRE live check, and that is
the hollow assertion it exists to prove:

  * §3 sees two well-formed integer sat balance lines,
  * §5's floor counts transactions and looks for one txid -- neither is a
    figure,
  * §6 checks asset and scale, which `0 sat` satisfies,
  * §8 reconciles the balance counters against the summed history: 0 == 0,
  * §9a places observations relative to the cursor, and §9b compares the
    resumed money against the FIRST read's money -- zero equals zero, so a
    uniformly zeroed adapter is perfectly self-consistent,
  * §10 and §11 never look at money at all.

Every money check in the file was the adapter against itself. Internal
consistency is exactly what a broken implementation finds easiest to
provide, so the check needs ONE figure it did not learn from the adapter.
There is exactly one that can never go stale: the 2010 pizza payment moved
10,000 BTC to this address in block 57043, roughly 900,000 blocks deep.
`OLDEST_TX_SATS` is that number, and it is an ANCHOR -- one observation,
one figure -- not a test of the mapping.

**This wrapper is driven by the LIVE check, not by the mutation battery**
(`tests/mutations.rs` runs recorded fixtures, and the offline Bitcoin cases
compare amounts line for line by construction). As with the other live
wrappers there is deliberately no `mutations/*.json` manifest.

DECLARED, exactly: the live check must fail at §5 -- the anchor -- and at
nothing else. The handshake, discovery and the balance-line count all
precede it and must still pass:

    SUMER_LIVE=1 \
    SUMER_LIVE_WRAPPER=mutations/adapters/btc_all_amounts_zeroed.py \
    SUMER_LIVE_EXPECT='§5' \
      cargo test -p sumer-conformance --test live_bitcoin -- --ignored --nocapture

The run PASSES when the mutant is killed at §5 and FAILS otherwise --
when it survives, when something else catches it first, and (since a
skipped mutant is not a kill) when the provider rate-limits the run.
"""
import _btc_rewrite as r


def _zero(money):
    return {"asset": money.get("asset", "sat"), "amount": "0"}


def rewrite(reply):
    for o in r.observations(reply):
        if isinstance(o.get("amount"), dict):
            o["amount"] = _zero(o["amount"])
        if isinstance(o.get("fees"), dict):
            o["fees"] = _zero(o["fees"])
    return reply


r.run(rewrite)
