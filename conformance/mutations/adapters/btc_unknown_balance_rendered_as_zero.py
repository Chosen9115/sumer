"""A balance the adapter reported as UNKNOWN (amount: null) leaves as "0".

Zero is a claim -- it says the wallet holds nothing -- and a wallet whose
provider could not be read is not a wallet holding nothing.
"""
import _btc_rewrite as r


def rewrite(reply):
    for o in r.observations(reply):
        if "category" in o and o.get("amount") is None:
            o["amount"] = {"asset": "sat", "amount": "0"}
    return reply


r.run(rewrite)
