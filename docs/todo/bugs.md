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

#### B1. Dual type-resolution engines cause hover / completion divergence

| | |
|---|---|
| **Impact** | Medium |
| **Effort** | Medium-High |

Variable type resolution has two parallel RHS expression resolvers that
must be kept in sync manually:

1. **`resolve_rhs_expression`** in `completion/variable/rhs_resolution.rs`
   — returns `Vec<ClassInfo>`, used by the completion pipeline.
2. **`resolve_rhs_raw_type`** in `completion/variable/raw_type_inference.rs`
   — returns `Option<String>`, used by hover's type-string path
   (`resolve_variable_type_string` → step 5).

Hover tries the type-string path first.  When it returns `Some(…)` the
ClassInfo fallback never fires, so hover shows whatever the raw-type
engine inferred — even if it is wrong.  The completion pipeline uses the
ClassInfo engine directly and gets the correct answer.

The two resolvers handle **different sets of expression types**:

| Expression kind          | ClassInfo engine | Raw-type engine |
| ------------------------ | :--------------: | :-------------: |
| `clone`                  | ✓                | ✗               |
| `\|>` (pipe)             | ✓                | ✗               |
| `Closure` / arrow fn     | ✓ (→ `\Closure`) | ✗               |
| `yield` (send type)      | ✓                | ✗               |
| `Call` (full resolution) | ✓                | partial¹        |
| `Access` (property)      | ✓                | partial¹        |
| Scalar literals          | ✗                | ✓               |
| Array literal inference  | ✗                | ✓               |

¹ The raw-type engine's `_ =>` catch-all delegates to
`extract_rhs_iterable_raw_type`, which covers some calls and accesses
but not all.

The `??` null-coalesce handling also diverges: the ClassInfo engine
checks whether the LHS *AST node* is syntactically non-nullable (pattern
match on `Clone`, `Literal`, etc.), while the raw-type engine checks
whether the resolved *type string* is non-nullable.  When the raw-type
engine cannot resolve the LHS at all (e.g. `clone $x` → `None`), it
falls through to returning only the RHS type, which is how
`clone $pen ?? new Marker()` shows `Marker` on hover but `Pen` on
completion.

**Possible approaches:**

- **Unify into one engine** — make `resolve_rhs_expression` the single
  source of truth and derive the type string from its result.  Hover
  would call the ClassInfo path and format the names into a union
  string, falling back to `resolve_variable_type_string` only for
  scalar / generic types that ClassInfo cannot represent.  This
  eliminates the synchronisation burden entirely.
- **Exhaustiveness enforcement** — if keeping both engines, add a shared
  enum of "RHS expression kinds" and a compile-time check (or test)
  that both match arms cover the same set, so new expression types
  cannot be added to one without the other.

---

#### B12. Interface-extends-interface constants (and other members) not merged

| | |
|---|---|
| **Impact** | Low-Medium |
| **Effort** | Low-Medium |

When an interface extends multiple parent interfaces (e.g.
`interface CarbonInterface extends DateTimeInterface, JsonSerializable, UnitValue`),
`resolve_class_with_inheritance` only walks the `parent_class` field (the
first extended interface). The remaining parent interfaces stored in the
`interfaces` list are not traversed for member merging.

Then `resolve_class_fully_inner` calls `resolve_class_with_inheritance` on
each interface collected from the class, but that inner call has the same
limitation — it does not recurse into the interface's own `interfaces` list.

**Reproducer:** `Illuminate\Support\Carbon::JANUARY` — the `JANUARY` constant
lives on `Carbon\Constants\UnitValue`, which `CarbonInterface` extends (6th
in the extends list). PHPantom reports "Member 'JANUARY' not found on class
'Illuminate\Support\Carbon'".

**Fix:** In `resolve_class_with_inheritance`, when processing an interface
(or always), also merge members from all entries in `self.interfaces`, not
just `parent_class`. Alternatively, make `resolve_class_fully_inner`
recursively collect parent interfaces when resolving each interface.

Affects 4 diagnostics in shared (Carbon month constants) and likely more in
other projects that use interfaces with multi-extends chains.

---

#### B13. Variable type resolved from reassignment target inside RHS expression

| | |
|---|---|
| **Impact** | Low |
| **Effort** | Medium |

When a variable is reassigned with an expression that references itself in
the RHS arguments, PHPantom resolves the variable to the NEW type inside
those arguments instead of the original type.

**Reproducer:**
```php
public function requestToken(PaymentTokenRequest $request, ...): ... {
    // $request is PaymentTokenRequest here
    $request = new CreateRecurringSessionRequest(
        paymentMethodReference: $request->uuid,  // ← PHPantom resolves $request as CreateRecurringSessionRequest
    );
}
```

PHP evaluates all arguments before performing the assignment, so `$request->uuid`
should resolve against `PaymentTokenRequest`. PHPantom's variable definition
offset tracking considers the new definition active too early — it should
only take effect after the full RHS expression is evaluated.

Affects 1 diagnostic in shared. Edge case but could appear in code that
reuses variable names across reassignments with self-referencing expressions.