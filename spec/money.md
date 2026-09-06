# Sumer money wire format

Normative. If your adapter (any language) emits or accepts amounts, this is the
contract. `sumer-money` (`core/money/`) is the reference implementation; this
document, not that code, is authoritative on disagreement.

## 1. Grammar

    -?(0|[1-9][0-9]*)(\.[0-9]+)?

Applied in this order, before any numeric interpretation:

- byte length of the input <= `MAX_INPUT_LEN` = 192, checked first
- significant digits of the coefficient (all digits, sign and `.` excluded) <= `MAX_DIGITS` = 128
- scale (digit count after `.`) <= `MAX_SCALE` = 38

No exponent notation, no digit grouping, no leading `+`, no leading zeros except
the single digit `0` itself, no whitespace, no non-ASCII digits, and no signed
zero (`-0`, `-0.00` are rejected — zero has no sign). `Display` output always
satisfies this grammar and always re-parses to the identical string: the
representation-to-string mapping is a bijection.

## 2. JSON shape

    {"asset": "<asset id>", "amount": "-10.25"}

Both fields are JSON **strings**. A JSON number in `amount` — `{"amount": -10.25}`
— MUST be rejected, at the deserializer, before any Sumer-specific validation
runs. Reason: a JSON number is defined as an IEEE-754 double. Doubles cannot
represent most decimal fractions exactly, and errors compound under arithmetic:
`0.1 + 0.2 != 0.3` in every language whose JSON layer round-trips through
`float64`. Money is exact coefficient-and-scale, never binary floating point,
full stop — see `CONTRIBUTING.md`. A conforming adapter emits and accepts
decimal strings only.

## 3. Emission recipes, per language

The grammar bans exponent notation. Several standard-library "canonical"
decimal-to-string paths silently emit exponent notation for very small
magnitudes and must not be used as-is. Verified for this document:

    $ python3 -c "from decimal import Decimal as D; print(str(D('0.0000001')))"
    1E-7
    $ python3 -c "from decimal import Decimal as D; print(format(D('0.0000001'), 'f'))"
    0.0000001

`str(Decimal(...))` reproduces the shortest form Python parsed the value from,
including exponent notation once the magnitude is small enough — confirmed above,
not assumed. The fix in every affected language forces plain notation explicitly:

| Language | Do NOT use | Use instead |
|---|---|---|
| Python | `str(Decimal(...))` | `format(d, 'f')` |
| Java | `BigDecimal.toString()` | `bigDecimal.toPlainString()` |
| Go | `big.Float` **entirely** | `shopspring/decimal`'s `.String()` — already plain |
| JavaScript | `Number`, `JSON.stringify(n)` | decimal.js: `d.toFixed()` (never round-trip through `Number`) |
| Rust | — | `sumer_money::Amount`'s `Display` is plain by construction |

### Go: `big.Float` is not an exact money type

Do not reach for `big.Float` at all, and do not use `Text('f', -1)` to "fix" it.
`big.Float` defaults to a 64-bit mantissa, so it rounds on the way *in*, before any
formatting happens:

```go
a, _ := new(big.Float).SetString("18446744073709551616")
b, _ := new(big.Float).SetString("18446744073709551617")
// a and b are now the same value. Every formatter prints them identically.
```

Plain notation cannot recover precision that parsing already discarded. Use
`shopspring/decimal`, or `big.Int` with an explicit scale you carry yourself.
This matters here: a `uint256` token balance is 78 digits and `big.Float` mangles
it silently.

`toString()`/`String()`/`str()` on an arbitrary-precision decimal type is a
"shortest round-trippable representation" function, not a "grammar-conformant
string" function — those coincide for most magnitudes and silently diverge at
the extremes. Always reach for the explicit plain-notation formatter.

## 4. `AssetId`

Opaque. Non-empty, <= `MAX_ASSET_ID_LEN` = 128 bytes, every byte a printable
ASCII character in `0x21..=0x7E` (no space, no control bytes, no bytes >= 0x80).

**No normalization of any kind.** Equality is exact byte equality. An adapter
MUST emit the byte-identical id for the same underlying asset on every call —
if adapter A emits `usdc` and adapter B emits `USDC` for the same token, they
are, as far as this layer is concerned, two different assets. Resolving that
divergence is a registry's job, and PR 1 deliberately does not build a registry.

Likely future structure: **CAIP-19** (`eip155:8453/erc20:0x833589f...`,
`slip44:60` for a chain-native asset). PR 1's grammar for `AssetId` is
intentionally *not* CAIP-19-shaped, and not merely CAIP-19-shaped-minus-features.
A near-miss of CAIP-19 is worse than either a full implementation or an honestly
opaque string: an on-chain-literate contributor who sees something that looks
almost like `eip155:1/erc20:0x...` will assume CAIP semantics (parseable
namespace, chain id, reference) that PR 1 does not provide, and be wrong in a
way a plainly opaque string never invites. When a registry lands, it is a new,
explicit, versioned contract, not a relaxation of this one.

## 5. Rejection table

| Input | Error |
|---|---|
| `""` | `Empty` |
| `"007.50"` | `LeadingZero { at }` |
| `"1e18"`, `"1E-7"` | `ExponentNotation { at }` |
| `".5"` | `MissingIntegerPart` |
| `"5."` | `MissingFractionDigits` — truncation only: the input ended after the point |
| `"100. 0"`, `"100.x"` | `InvalidByte { at }` at the offending byte, **not** `MissingFractionDigits` |
| `"5.e3"` | `ExponentNotation { at }` at the `e` |
| `"-0"`, `"-0.00"` | `SignOnZero` |
| `"+5"`, `"1,000"`, `" 5"`, `"NaN"`, `"Infinity"`, `"١٢٣"` | `InvalidByte { at }` |
| 129+ significant digits | `TooManyDigits { digits }` |
| scale 39+ | `ScaleTooLarge { scale }` |
| input > 192 bytes | `InputTooLong { len }` |
| JSON number `10.25` in `amount` | serde invalid-type error, before any of the above run |
| empty asset id | `AssetIdEmpty` |
| asset id > 128 bytes | `AssetIdTooLong { len }` |
| asset id with a byte outside `0x21..=0x7E` | `AssetIdInvalidByte { at }` |
| arithmetic result exceeding 128 digits | `CoefficientOverflow` |
| `add`/`sub`/`cmp_same_asset` across two different assets | `AssetMismatch` |

**Truncation errors versus wrong-byte errors.** `MissingIntegerPart` means no integer
digit was found where the grammar requires one — either the string ended (`""` after a
sign, `"-"`) or a decimal point appeared first (`"."`, `".5"`, `"-."`). A decimal point
is called out by name because it is the one non-digit byte that names a *specific*
mistake rather than a random one; any other non-digit there is `InvalidByte { at }`.

`MissingFractionDigits` is truncation only: it fires for a string ending in `.` and
nothing else. If a byte follows the point but is not a digit, the error names that
byte's offset instead, because an offset is what an implementer can act on.

**Bound precedence.** When an input violates more than one bound, the error reported is
the first violated in this order: byte length, then significant digits, then scale. An
implementation that reports them in a different order is non-conforming — two adapters
must reject the same input with the same error.

Every rejection is a named variant. No panic, ever, on any input to a public
function — malformed input is an ordinary `Err`, not an exceptional condition.

## 6. Sign is not interpreted

Negative amounts are legal and carry no meaning at this layer — `-10.25` is not
"a debit," it is just the number negative ten and a quarter. Upstream providers
disagree on convention: Plaid reports a positive amount for money leaving the
account; Teller reports a signed amount, negative for money leaving. Sumer does
not pick a winner here. Normalizing provider sign convention into a consistent
meaning is the adapter's responsibility, applied before or after this layer —
never inside it.

## 7. Stability of the bounds

`MAX_DIGITS = 128` and `MAX_SCALE = 38` are calibrated against `uint256` (the
largest integer type in common on-chain use) and against no real fixture yet —
no live provider has been observed needing anywhere near this range. These two
constants, of everything in this document, are the most likely to move as real
fixtures arrive.

They do not move by editing the constant. A bound change is a spec revision
with a version bump, communicated the same way any other breaking wire change
is: a peer built against the old bound rejects frames that a peer built against
a new, larger bound emits as valid. Silently widening (or narrowing) the
constant in the reference implementation without a spec revision breaks that
peer silently, which is precisely the failure mode a versioned wire contract
exists to prevent.
