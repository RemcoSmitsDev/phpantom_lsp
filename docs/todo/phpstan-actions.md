# PHPStan Code Actions

Code actions that respond to PHPStan diagnostics. Each action parses the PHPStan
error message, extracts the relevant information, and offers a quickfix that
modifies the source code to resolve the issue.


## Prerequisites — Infrastructure improvements

No outstanding items.

---

## Tier 1 — Trivial (no message parsing or simple static message)

No outstanding items.

---

## Tier 2 — Simple message parsing

### H6. `return.type` — Update return type to match actual returns

**Identifier:** `return.type`
**Messages:**
- `Method Foo::bar() should return {expected} but returns {actual}.`
- `Function foo() should return {expected} but returns {actual}.`
- `Anonymous function should return {expected} but returns {actual}.`

Parse `{actual}` from the message with regex:
`should return (.+) but returns (.+)\.$`

Offer two quickfixes:

1. **Update native return type** — find the `: Type` after the parameter list
   and replace `Type` with the actual type. Only offer this when the actual
   type is a valid native PHP type (scalars, class names, `null`, simple
   unions on PHP 8.0+, intersection types on PHP 8.1+).
2. **Update `@return` tag** — if a docblock with `@return` exists, replace the
   type. If no docblock exists, create one with `@return {actual}`.

Mark neither as `is_preferred` since the right fix might be to change the code
rather than the signature.

**Stale detection:** difficult to do precisely because PHPStan's type syntax
differs from source syntax. As a heuristic, check whether the return type
declaration or `@return` tag was modified since the diagnostic was issued
(i.e. the text on the relevant line no longer matches the `{expected}` type).
Or skip stale detection for this one — the `@phpstan-ignore` stale check
already covers the suppress-with-comment path.

---

### H10. `return.unusedType` — Remove unused type from return union

**Identifier:** `return.unusedType`
**Messages:**
- `Method Foo::bar() never returns {type} so it can be removed from the return type.`
- `Function foo() never returns {type} so it can be removed from the return type.`

Parse `{type}` from the message. Find the return type (native or `@return`),
parse the union/intersection, remove the unused member, and rewrite.

For native types: `string|null` with unused `null` becomes `string`.
For docblock types: same logic on the `@return` tag.
Handle intersection types too: `Foo&Bar` with unused `Bar` becomes `Foo`.

If removing the type would leave a single-member union, simplify
(e.g. `string|null` minus `null` becomes `string`).

**Stale detection:** the return type no longer contains `{type}` as a union
or intersection member.

---

### H4. `assign.byRefForeachExpr` — Unset by-reference foreach variable

**Identifier:** `assign.byRefForeachExpr`
**Tip (in message):** `Unset it right after foreach to avoid this problem.`

The diagnostic is on a line that uses a variable that was previously bound as a
by-reference foreach variable. The fix is to insert `unset($var);` after the
foreach loop that created the binding.

**Implementation steps:**

1. The diagnostic line references the variable. Extract the variable name
   from the diagnostic line by finding the `$var` on that line (we can look
   for the first `$identifier` on the line, or parse the message — but the
   message doesn't include the variable name, so we must scan the source).
2. Search backward from the diagnostic line for a `foreach` statement
   containing `&$var` (the by-reference binding).
3. Find the closing `}` (or `endforeach;`) of that foreach.
4. Insert `unset($var);` on the line after the closing brace, with matching
   indentation.

This is trickier than the other Tier 1/2 items because of the need to locate
the foreach loop and its closing brace. Brace-matching is fragile without a
real parser, but a simple nesting-depth counter works for well-formatted code.

**Stale detection:** `unset($var)` appears between the foreach closing brace
and the diagnostic line.

---

## Tier 3 — Requires locating related code

### H13. `property.notFound` (same-class) — Declare missing property

**Identifier:** `property.notFound`
**Message:** `Access to an undefined property Foo::$bar.`

Parse class name and property name from the message:
`Access to an undefined property (.+)::\$(.+)\.$`

Scope to same-file only: when the diagnostic is on `$this->bar`, the fix
targets the current class. When it references a different class, skip.

Offer two quickfixes:

1. **Declare property** — insert a property declaration at the top of the
   class body, after existing property declarations. Use `private` visibility
   and `mixed` type by default. If the diagnostic is on an assignment like
   `$this->bar = expr;`, we might infer a better type later, but start with
   `mixed`.
2. **Add `@property` PHPDoc** — add `@property mixed $bar` to the class
   docblock. Better for classes that use `__get`/`__set`.

**Stale detection:** the class now declares `$bar` as a property, or the
class docblock contains `@property ... $bar`.

**Reference:** https://phpstan.org/blog/solving-phpstan-access-to-undefined-property

---

### H15. Template bound from tip — Add `@template T of X`

**Identifiers:** various (`generics.*`, `phpDoc.*` — needs investigation)
**Tip (in message):** `Write @template T of X to fix this.`

Parse the `@template` declaration from the tip using:
`Write (@template .+ of .+) to fix this\.`

Insert the `@template` tag into the class or function docblock (create one
if needed). Same docblock insertion pattern as `add_throws.rs`.

**Stale detection:** the docblock now contains the extracted `@template` tag.

---

### H16. `match.unhandled` — Add missing match arms

**Identifier:** `match.unhandled`
**Message:** `Match expression does not handle remaining value(s): {types}`

Parse the remaining value(s) from the message:
`does not handle remaining value\(s\): (.+)$`

The value list is comma-separated. Each value can be:
- An enum case: `Foo::Bar` — generate `Foo::Bar => TODO`
- A string literal: `'foo'` — generate `'foo' => TODO`
- An int literal: `42` — generate `42 => TODO`
- A type name: `int` — generate `default => TODO` (catch-all)

Find the match expression on the diagnostic line. Locate its closing `}`.
Insert new arms before the closing `}` with correct indentation.

Use `throw new \LogicException('Unexpected value')` as the arm body, or
a `TODO` comment — configurable later.

**Stale detection:** difficult without re-parsing the match. Skip for now.

---

## Tier 4 — Requires body analysis

### H17. `missingType.iterableValue` (return type) — Add `@return` with iterable type

**Identifier:** `missingType.iterableValue`
**Messages:**
- `Method Foo::bar() return type has no value type specified in iterable type array.`
- `Function foo() return type has no value type specified in iterable type array.`

Only handle the "return type" variant (not parameter/property). Parse the
iterable type name (`array`, `iterable`, `Traversable`, etc.) from the message.

**Simplest approach (start here):**

Offer to add `@return array<mixed>` (or `list<mixed>`, `iterable<mixed>`, etc.
matching the native type). This silences the PHPStan error while being explicit.
PHPStan's documentation recommends this as the quick fix:
> "If you just want to make this error go away, replace array with mixed[]
> or array<mixed>."

**Enhanced approach (later):**

Walk the function body for `return` statements and infer element types from
array literals using the existing `infer_element_type` /
`infer_array_literal_raw_type` logic. If all return expressions are array
literals with consistent value types, offer `@return array<ValueType>`.

**Stale detection:** a `@return` tag exists with a generic array type
(contains `<` or `[]`).

**Reference:** https://phpstan.org/blog/solving-phpstan-no-value-type-specified-in-iterable-type

---

## Tier 5 — Lower priority / more complex

### H19. `property.unused` / `method.unused` / `classConstant.unused` — Remove unused member

**Identifiers:** `property.unused`, `method.unused`, `classConstant.unused`
**Messages:**
- `Property Foo::$bar is unused.`
- `Method Foo::bar() is unused.`
- `Constant Foo::BAR is unused.`

Find and delete the entire member declaration. Destructive action, so mark
as non-preferred and only offer when the identifier exactly matches
(not on `property.onlyRead`, `property.onlyWritten`, etc. where the member
is partially used).

For methods, need to find the full extent (from docblock through closing `}`).
For properties/constants, just the line (plus docblock above).

**Stale detection:** the member name no longer appears as a declaration in the
class.

---

### H20. `generics.callSiteVarianceRedundant` — Remove redundant variance annotation

**Identifier:** `generics.callSiteVarianceRedundant`
**Tip (in message):** `You can safely remove the call-site variance annotation.`

Strip `covariant` or `contravariant` keywords from generic type arguments
in the docblock. Requires parsing PHPDoc generic syntax
(e.g. `Collection<covariant Foo>` becomes `Collection<Foo>`).

**Stale detection:** no `covariant`/`contravariant` in the PHPDoc on the
diagnostic line.

---

### H23. `instanceof.alwaysTrue` — Remove redundant instanceof check

**Identifier:** `instanceof.alwaysTrue`
**Message:** `Instanceof between {type} and {class} will always evaluate to true.`

Offer to simplify: remove the `instanceof` check and keep only the truthy
branch. Complex because it requires understanding the control flow (if/else,
ternary, match arm). Consider deferring this indefinitely — the user can just
`@phpstan-ignore` it.

---

### H24. `catch.neverThrown` — Remove unnecessary catch clause

**Identifier:** `catch.neverThrown`
**Message:** `Dead catch - {exception} is never thrown in the try block.`

Remove the catch clause for the exception that is never thrown. If it is the
only catch clause, the entire try/catch block should be unwrapped (keep just
the try body). This requires careful brace matching.

Start with the multi-catch case: if the catch has multiple exception types
(`catch (FooException | BarException $e)`), just remove the dead type from
the list.

---

## Suggested implementation order

Based on effort-to-value ratio and shared infrastructure:

1. **H6** — return type update
2. **H10** — remove unused union member
3. **H4** — unset by-ref foreach variable
4. Everything else based on user demand

---

## Implementation notes

### Message parsing

All message parsing should use regex with named capture groups for clarity.
Create a shared helper module (e.g. `code_actions/phpstan_message.rs`) for
common patterns like extracting class names, method names, types, and property
names from PHPStan messages. Example:

```rust
use regex::Regex;

/// Extract the "actual" type from a return.type diagnostic message.
pub fn extract_return_type_actual(message: &str) -> Option<&str> {
    let re = Regex::new(r"should return .+ but returns (?P<actual>.+)\.$").ok()?;
    re.captures(message)?.name("actual").map(|m| m.as_str())
}
```

### Tip extraction

Tips are appended to `Diagnostic.message` after a `\n` by
`parse_phpstan_message()` in `phpstan.rs`. To access the tip:

```rust
let (message, tip) = match diag.message.split_once('\n') {
    Some((m, t)) => (m, Some(t)),
    None => (diag.message.as_str(), None),
};
```

Actions that depend on tip text (H4, H12, H15, H20) should use this
pattern. The tip text has ANSI/HTML tags already stripped by `strip_ansi_tags`.

### Stale diagnostic detection

Each new action should have a corresponding check in
`is_stale_phpstan_diagnostic()` in `diagnostics/mod.rs` so that the diagnostic
is eagerly cleared after the user applies the fix, without waiting for the
next PHPStan run.

The function currently handles:
- `@phpstan-ignore` coverage (all identifiers)
- `method.override` / `property.override` / `property.overrideAttribute`
- `method.tentativeReturnType`
- `return.phpDocType` / `parameter.phpDocType` / `property.phpDocType`
- `new.static`
- `class.prefixed`
- `function.alreadyNarrowedType` (assert-only)
- `return.void` / `return.empty`
- `deadCode.unreachable`

Other identifiers (`throws.unusedType`, `throws.notThrowable`,
`missingType.checkedException`, `method.missingOverride`) are cleared
eagerly by `codeAction/resolve` rather than by content heuristics.

New actions should add branches to the `match identifier { ... }` block.

### Testing

Each action needs tests following the existing pattern:
- Unit tests for pure helper functions (regex extraction, edit building)
- Integration tests that construct `CodeActionParams` with mock diagnostics
  and call `collect_*_actions` directly
- Stale detection tests that construct `Diagnostic` objects and call
  `is_stale_phpstan_diagnostic`

### Attribute insertion pattern

`remove_override.rs` and `add_return_type_will_change.rs` each contain
their own `find_method_insertion_point` and attribute detection helpers,
following the same pattern as `add_override.rs`. Future attribute-related
actions can reference any of these three modules.

### PHPDoc type mismatch pattern

`fix_phpdoc_type.rs` provides a shared helper parameterised by tag name
(`@return`, `@param`, `@var`). Each diagnostic offers two quickfixes:
update the tag type to match the native type, or remove the tag entirely
(preferred). Stale detection checks whether the tag still contains the
original PHPDoc type.