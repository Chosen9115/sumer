"""The two balance lines keep their names and TRADE their amounts.

The break: `balances.read` still answers with exactly two lines, still
`confirmed` and `unconfirmed`, still integer satoshis, still one of each --
and the figure under `confirmed` is the mempool's, while the figure under
`unconfirmed` is the wallet's real settled balance. Nothing else changes,
and neither line is invented: both numbers came from this wallet.

Which is why it survived. `live_bitcoin.rs` validated the two lines by
NAME -- a sorted list of categories, which a swap leaves identical -- and
then folded both amounts into one total before comparing anything to the
history. Every quantity the check could see was preserved:

  * section 3 sees two lines, one per category, both integer sat,
  * section 5's floor and anchor are history and never touch a balance,
  * section 6 checks asset and scale, which both figures satisfy,
  * section 8 reconciled the SUM of the two lines against the sum of the
    history, and a swap is a permutation: the sum is unchanged,
  * sections 9 to 11 never look at a balance at all.

So a host would have been told this wallet holds `unconfirmed` money it
settled years ago, and `confirmed` money that is only in the mempool -- the
one distinction the two lines exist to draw (`adapters/bitcoin/README.md`:
never summed, never merged) -- and every invariant was green. The recorded
fixtures pin each amount to its category by construction, which is exactly
why this hole was live-check-shaped and nowhere else.

The fix reconciles PER CATEGORY: `confirmed` against the posted history,
`unconfirmed` against the pending history. Each line is the sum of one half
of the transactions, so the split is checkable against the same second
endpoint the total was already checked against, and no conforming adapter
is failed by it -- a wallet with no mempool activity has no pending
observation and a zero (or absent) unconfirmed sum, which is what the
check compares.

**This wrapper is driven by the LIVE check, not by the mutation battery**
(`tests/mutations.rs` runs recorded fixtures, whose expectations already
pin category-associated amounts). See `btc_resumed_pages_emptied.py` for
why there is deliberately no `mutations/*.json` manifest for a live
wrapper.

DECLARED, exactly: the live check must fail at section 8 -- the
cross-endpoint reconciliation -- and at nothing else. Everything before it
must still pass: the handshake, discovery, the two balance lines of
section 3, the floor and the anchor, and the shape checks. Run it, and
require that:

    SUMER_LIVE=1 \
    SUMER_LIVE_WRAPPER=mutations/adapters/btc_balance_amounts_swapped.py \
    SUMER_LIVE_EXPECT='§8' \
      cargo test -p sumer-conformance --test live_bitcoin -- --ignored --nocapture

The run PASSES when the mutant is killed at section 8 and FAILS otherwise,
including when it survives.

One thing it cannot prove: a wallet whose two balances are EQUAL (both
zero, most obviously) is unbroken by a swap, and the mutant would survive
for a reason that says nothing about the check. The address this check
watches has a settled balance and an empty mempool, so the swap moves a
real figure onto the wrong line.
"""
import _btc_rewrite as r

SWAP = ("confirmed", "unconfirmed")


def rewrite(reply):
    # Balance lines only: a history observation has no `category`.
    lines = [o for o in r.observations(reply) if "category" in o]
    by_resource = {}
    for line in lines:
        by_resource.setdefault(line.get("resource_id"), {})[line["category"]] = line
    for wallet in by_resource.values():
        confirmed, unconfirmed = (wallet.get(c) for c in SWAP)
        if confirmed is None or unconfirmed is None:
            continue
        confirmed["amount"], unconfirmed["amount"] = (
            unconfirmed["amount"],
            confirmed["amount"],
        )
    return reply


r.run(rewrite)
