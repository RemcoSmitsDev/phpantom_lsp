# PHPantom — Bug Fixes

Known bugs and incorrect behaviour. These are distinct from feature
requests — they represent cases where existing functionality produces
wrong results. Bugs should generally be fixed before new features at
the same impact tier.

Items are ordered by **impact** (descending), then **effort** (ascending)
within the same impact tier.

| Label      | Scale                                                                                                                  |
| ---------- | ---------------------------------------------------------------------------------------------------------------------- |
| **Impact** | **Critical**, **High**, **Medium-High**, **Medium**, **Low-Medium**, **Low**                                           |
| **Effort** | **Low** (≤ 1 day), **Medium** (2-5 days), **Medium-High** (1-2 weeks), **High** (2-4 weeks), **Very High** (> 1 month) |

---

## B7. Inlay hints: wrong parameter name with mixed named and positional arguments

**Impact: Medium · Effort: Medium**

`emit_parameter_hints` in `src/inlay_hints.rs` maps each argument to
a parameter by its positional index (`arg_idx`). This is wrong when
named arguments appear before positional arguments or consume
parameters out of order. For example, `greet(city: 'NYC', 'Alice')`
would label `'Alice'` as `age:` instead of `name:`.

**Fix:** Track which parameters are consumed by named arguments
first, then assign remaining positional arguments to the remaining
parameters in order.

**File:** `src/inlay_hints.rs` L115-121.

---

## B1. Native type hints not considered in virtual property specificity ranking

**Impact: Low-Medium · Effort: Medium**

The `type_specificity` function used during virtual member merging only
scores the `type_hint` field (the effective/docblock type). It does not
consider `native_type_hint` (the PHP-declared type on the property).

For example, a real property declared as `public string $name;` has
`native_type_hint = Some("string")` and `type_hint = Some("string")`.
If a docblock or virtual provider contributes `@property array<int> $name`,
the specificity comparison works correctly today because both values flow
through `type_hint`.

However, the broader issue is in `resolve_effective_type`: when a native
hint says `string` and a docblock says `array<int>`, the effective type
should be the docblock's version (it is more specific and deliberately
overrides the native hint). This is not specific to virtual member merging
but to the general type resolution pipeline. Fixing it here would not help
because the native vs docblock decision happens upstream in the parser.

This is out of scope for the virtual member specificity work but worth
tracking as a separate improvement to `resolve_effective_type`.
