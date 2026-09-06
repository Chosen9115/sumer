# FDX 6.4 field mapping

**Sumer is not FDX and makes no conformance claim.** This table exists to
document how FDX 6.4 concepts are represented in Sumer's wire model, for
implementers converting a provider that speaks FDX (or the `plaid/core-exchange`
implementation of it) into Sumer observations. It is not a certification
artifact, it does not imply Sumer implements the FDX API surface, and no Sumer
type anywhere is named after FDX.

Field names below are taken from the public `plaid/core-exchange` 6.4 schema
(branch `mj-add-corex-6.4`, `versions/6.4/corex.yaml`, `info.version: 6.4.0`,
commit `5b1c4c6daf7bb4bdb84d13e67917023278b8366c`) — the `AccountDescriptor`,
`DepositAccountDescriptor`, `DepositAccount`, `Transaction`, and
`DepositTransaction` schema objects, plus the shared `Currency` and
`PageMetadata` types they reference.

**`Transaction` carries no `accountId` and no `currency` field.** The
account is scoped by the request path (`/accounts/{accountId}/transactions`),
never repeated per-transaction; and there is no plain `currency` field on
`Transaction`/`DepositTransaction` at all — only `foreignCurrency`, paired
with `foreignAmount`, for the foreign-amount case. An earlier draft of
`conformance/cases/fdx_lossless.json`, built from Plaid's browsable 6.3
reference docs, had invented both fields (plus `DepositAccount.balanceAsOf`,
see the Balances section below); all three were removed once reconciled
against the real 6.4 schema above — 6.4 wins per this document's governing
ruling. See `fdx_field_map._removed_from_6.3_draft` in the fixture for the
same note in context.

**Why FDX's JSON schema itself is not adopted.** FDX encodes every monetary
amount as a bare JSON number (`currentBalance`, `availableBalance`, `amount`
are all `type: number`). A JSON number is an IEEE-754 double; doubles cannot
represent most decimal fractions exactly, and `0.1 + 0.2 != 0.3` under that
representation in every language whose JSON layer round-trips through
`float64` (`spec/money.md` §2). Sumer's wire format requires validated decimal
*strings* for every amount specifically to avoid this, so FDX's schema is not
usable as-is — an adapter converting FDX-shaped data into Sumer's wire must
parse the source JSON number with a decimal-preserving parser (e.g. Python's
`json.loads(..., parse_float=Decimal)`, then `format(d, 'f')` — never through
a native float) before it ever reaches Sumer's grammar. This is the same
discipline the `provider_json_number` conformance fixture exercises.

**Unmapped fields land in `provider_extra`.** This is the same idea FDX itself
uses under the name `fiAttributes` (an array of financial-institution-specific
`{name, value}` pairs, present on `InvestmentAccount` and
`InvestmentTransaction` in this schema version) — borrowed as a *pattern*, not
adopted as a Sumer type or name. Sumer's `provider_extra` is a plain
`{name: value}` map on every history observation (`spec/observation.md` §3),
independent of whether the source protocol happens to have its own version of
the idea.

The **Lossy** column below must be empty for every row. A non-empty Lossy
entry means either a mapping is missing or a Sumer field needs to grow — not
something to ship silently. `conformance/cases/fdx_lossless.json` exercises
this table against one sanitized `plaid/core-exchange` 6.4 payload and asserts
every field it contains is named in `expect` with where it landed.

## Accounts

FDX schema objects: `AccountDescriptor`, `DepositAccountDescriptor`.

| FDX 6.4 field | Sumer path | Lossy |
|---|---|---|
| `accountId` (`Identifier`, string) | `Provenance.provider_id` on every observation for that resource; keyed with `adapter_id` as `(adapter_id, resource_id)` (`spec/wire.md` §10) | |
| `accountCategory` (enum: `DEPOSIT_ACCOUNT`, `LOAN_ACCOUNT`, ...) | `provider_extra["accountCategory"]`, verbatim | |
| `accountType` (enum: `CHECKING`, `SAVINGS`, `CD`, ...) | `provider_extra["accountType"]`, verbatim | |
| `accountNumberDisplay` (string) | `provider_extra["accountNumberDisplay"]`, verbatim | |
| `productName` / `productId` (string / `Identifier`) | `provider_extra["productName"]` / `provider_extra["productId"]`, verbatim | |
| `nickname` (string) | `provider_extra["nickname"]`, verbatim | |
| `status` (enum: `OPEN`, `CLOSED`, `DELINQUENT`, ...) | `provider_extra["status"]`, verbatim | |
| `currency.currencyCode` (`Iso4217Code` enum, e.g. `"USD"`) | `AssetId` on every `AmountWire` for that resource's balances and history (`spec/money.md` §4 — an ISO 4217 code is a valid opaque `AssetId` by construction: 3 printable ASCII bytes, well under the 128-byte cap) | |
| `earnedInterest`, `underArbitration`, `overdraftOptIn`, `overdrafted`, `overdraftProtectionFunded`, `overdraftFundingSources` (deposit-specific fields) | `provider_extra`, verbatim, one key per field | |

## Balances

FDX schema object: `DepositAccount` (which extends `DepositAccountDescriptor`
with balance fields).

| FDX 6.4 field | Sumer path | Lossy |
|---|---|---|
| `currentBalance` (`number`) | A `Balance` list entry: `{category: "currentBalance", canonical_hint: "total", amount: AmountWire, provenance}` — parsed via a decimal-preserving JSON reader per the note above, never through a native float | |
| `availableBalance` (`number`) | A `Balance` list entry: `{category: "availableBalance", canonical_hint: "available", amount: AmountWire, provenance}` | |

**`DepositAccount` carries no balance timestamp field.** Unlike
`InvestmentAccount` (which has `balanceAsOf`), FDX 6.4's `DepositAccount`
schema has nothing analogous — no `asOf`, no `balanceDate`. `Provenance.observed_at`
is adapter-claimed evidence by definition (`spec/observation.md` §1), not
something every source protocol is required to supply, so an adapter
converting a `DepositAccount` balance stamps its own fetch-time clock reading
there. This is not a Lossy entry: there is no FDX field being dropped, only
one that never existed for this resource type in the first place.

Both fields are `required` in FDX's `DepositAccount` schema, but Sumer's
`Balance.amount` stays `Option<AmountWire>` regardless (`spec/observation.md`
§2) — an adapter reporting a provider that omits one under real-world failure
conditions represents that omission as `amount: null` (UNKNOWN), not as `0`.
FDX's own account schemas for other categories (`LoanAccount`,
`InvestmentAccount`) expose further balance-shaped fields (`principalBalance`,
`marketValue`, and others); each becomes its own `Balance` list entry with
`category` set to the FDX field name verbatim, following the same pattern —
Sumer's balances list has no fixed arity, so adding a category never requires
a schema change.

## Transactions

FDX schema objects: `Transaction` (base), `DepositTransaction`.

| FDX 6.4 field | Sumer path | Lossy |
|---|---|---|
| `transactionId` (`Identifier`) | `provider_id` on the history observation, verbatim; also one input to the adapter's documented `local_id` derivation (`spec/observation.md` §3) | |
| `referenceTransactionId` (`Identifier`, reverse-posting / correction link) | `supersedes_provider_id` | |
| `accountCategory` (fixed `DEPOSIT_ACCOUNT` on this variant) | Not carried per-observation — already expressed by the resource's `(adapter_id, resource_id)` key; also copied to `provider_extra["accountCategory"]` for traceability | |
| `status` (enum: `AUTHORIZATION`, `MEMO`, `PENDING`, `POSTED`) | `posting`: `PENDING` and `MEMO` and `AUTHORIZATION` -> `pending`; `POSTED` -> `posted`. (FDX's own text says Plaid treats `MEMO` and `AUTHORIZATION` as `PENDING`; Sumer's three-value `posting` enum has no `unknown` case exercised here because FDX's `status` is required and closed.) The original enum value is additionally kept at `provider_extra["status"]`, verbatim, so which of the three pending-shaped values it was is never actually lost | |
| `amount` (`number`, absolute value) + `debitCreditMemo` (enum: `CREDIT`, `DEBIT`, `MEMO`) | Combined into one signed `AmountWire.amount`: `DEBIT`/`MEMO` -> positive, `CREDIT` -> negative (FDX's own stated convention — see its `DebitCreditMemo` description), with `raw_sign` set to `provider_positive` or `provider_negative` to match the resulting sign, per `spec/money.md` §6 ("sign is not interpreted... Sumer does not pick a winner"). The original `debitCreditMemo` enum value is additionally kept at `provider_extra["debitCreditMemo"]`, verbatim | |
| `postedTimestamp` (`Timestamp`, RFC 3339; required when `status=POSTED`, omitted when pending) | `Provenance.effective_at` | |
| `transactionTimestamp` (`Timestamp`, RFC 3339; when the provider's backend recorded the transaction) | `Provenance.observed_at` (adapter-claimed evidence — `spec/observation.md` §1) | |
| `description` (string) | `description` (already required to be plain text on both sides — FDX's field is documented as user-facing merchant/place-of-business text, not markup) | |
| `category` / `subCategory` (string; MCC/SIC-preferred) | `provider_extra["category"]` / `provider_extra["subCategory"]`, verbatim | |
| `cardNumberDisplay` (string) | `provider_extra["cardNumberDisplay"]`, verbatim | |
| `foreignAmount` / `foreignCurrency` (`number` / `Iso4217Code`) | `provider_extra["foreignAmount"]` / `provider_extra["foreignCurrency"]`, verbatim (again via decimal-preserving parsing for `foreignAmount`, never a native float) | |
| `payee` (string, deprecated in this FDX version) | `provider_extra["payee"]`, verbatim | |
| `checkNumber` (`integer`, `DepositTransaction`-specific) | `provider_extra["checkNumber"]`, verbatim | |

## Pagination

FDX schema object: `PageMetadata`.

| FDX 6.4 field | Sumer path | Lossy |
|---|---|---|
| `nextOffset` (string, explicitly marked deprecated in this FDX version, "will be removed with a future major release") | Not adopted at all — Sumer's `PageRequest` has no numeric-offset variant (`spec/observation.md` §5). This mirrors FDX's own direction, not a divergence from it | |
| `nextPageKey` (string, opaque, "does not need to be numeric") | `PageRequest::Cursor { cursor }`, carried through as an opaque token per the resumption family the adapter declares (`spec/observation.md` §5) | |
| `totalElements` (integer) | `provider_extra["totalElements"]` on the page's status entry — informational, not load-bearing for resumption | |
