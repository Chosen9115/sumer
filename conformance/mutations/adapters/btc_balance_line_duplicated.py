"""A second `unconfirmed` balance line, carrying zero.

The break: `balances.read` comes back with THREE lines -- `confirmed`,
`unconfirmed`, and a duplicate `unconfirmed` of 0. Nothing else changes.
The wallet's real figures are untouched, both categories are present, no
third category is invented, every line is an integer number of satoshis,
and the extra line adds zero to the total so the cross-endpoint
reconciliation in section 8 reconciles exactly as before.

Which is the point. `live_bitcoin.rs` section 3 compared a SET of the
categories it saw, and a set is blind to multiplicity: a duplicate line is
indistinguishable from the line it duplicates. That is the same shape as
the `fees` comparison that degraded silently rather than failing -- a
comparison that tolerates the very thing it is there to detect. "Two
balance lines" is a claim about COUNT, and it has to be asserted as one.

Two balance lines are not decoration: `confirmed` and `unconfirmed` are
never summed and never merged (`adapters/bitcoin/README.md`), and a host
that adds up whatever lines arrive double-counts a wallet the moment an
adapter emits one twice.

**This wrapper is driven by the LIVE check, not by the mutation battery**
(`tests/mutations.rs` runs recorded fixtures; the offline Bitcoin cases
compare balances line for line and would catch this by construction). See
`btc_resumed_pages_emptied.py` for why there is deliberately no
`mutations/*.json` manifest for a live wrapper.

DECLARED, exactly: the live check must fail at section 3 -- the balance
lines -- and at nothing else. Run it, and require that:

    SUMER_LIVE=1 \
    SUMER_LIVE_WRAPPER=mutations/adapters/btc_balance_line_duplicated.py \
    SUMER_LIVE_EXPECT='§3' \
      cargo test -p sumer-conformance --test live_bitcoin -- --ignored --nocapture

The run PASSES when the mutant is killed at section 3 and FAILS otherwise,
including when it survives.
"""
import _btc_rewrite as r


def rewrite(reply):
    lines = r.observations(reply)
    for line in list(lines):
        if line.get("category") == "unconfirmed":
            duplicate = dict(line)
            duplicate["amount"] = {"asset": "sat", "amount": "0"}
            lines.append(duplicate)
            break
    return reply


r.run(rewrite)
