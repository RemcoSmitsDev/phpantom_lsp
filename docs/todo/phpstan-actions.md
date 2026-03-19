# PHPStan Code Actions

Code actions that respond to PHPStan diagnostics. Each action parses the PHPStan
error message, extracts the relevant information, and offers a quickfix that
modifies the source code to resolve the issue.

**Already implemented:**

- `missingType.checkedException` — Add `@throws` tag
- `throws.unusedType` / `throws.notThrowable` — Remove `@throws` tag
- `ignore.unmatched*` — Remove unnecessary `@phpstan-ignore`
- Any identifier — Add `@phpstan-ignore <identifier>`

---

## Tier 1 — Trivial (no message parsing or simple static message)

### P1. `new.static` — Unsafe usage of `new static()`

**Identifier:** `new.static`
**Message:** `Unsafe usage of new static().`

No data to extract from the message. The diagnostic line is inside the class
that needs fixing. Offer three quickfixes:

1. **Add `final` to class** — insert `final ` before the `class` keyword.
2. **Add `final` to constructor** — find `__construct` in the same class and
   insert `final ` before the visibility modifier.
3. **Add `@phpstan-consistent-constructor`** — add the tag to the class-level
   docblock (or create one). This is the least invasive option and should be
   marked `is_preferred`.

Walk backward from the diagnostic line to find the enclosing class declaration.
The same `find_enclosing_docblock` pattern from `add_throws.rs` works here.

**Stale detection:** the diagnostic is stale when the class has `final` keyword,
the constructor has `final` keyword, or the class docblock contains
`@phpstan-consistent-constructor`.

**Reference:** https://phpstan.org/blog/solving-phpstan-error-unsafe-usage-of-new-static

---

### P2. `method.missingOverride` — Add `#[Override]` attribute

**Identifier:** `method.missingOverride`
**Message:** `Method Foo::bar() overrides method Parent::bar() but is missing the #[\Override] attribute.`

The diagnostic points to the method declaration line. Insert `#[\Override]`
on the line before the method (after the docblock, before modifiers). Use the
same indentation as the method line.

No message parsing needed beyond confirming the identifier. The fix is always
the same: insert the attribute.

**Stale detection:** the line above the method contains `#[Override]` or
`#[\Override]`.

---

### P3. `method.override` / `property.override` — Remove `#[Override]` attribute

**Identifiers:** `method.override`, `property.override`
**Messages:**
- `Method Foo::bar() has #[\Override] attribute but does not override any method.`
- `Property Foo::$baz has #[\Override] attribute but does not override any property.`

Find and remove the `#[Override]` or `#[\Override]` attribute line above the
declaration. If the attribute is on its own line, remove the entire line. If it
shares a line with other attributes, remove just the `#[Override]` token.

**Stale detection:** no `#[Override]` attribute found near the diagnostic line.

---

### P4. `assign.byRefForeachExpr` — Unset by-reference foreach variable

**Identifier:** `assign.byRefForeachExpr`
**Tip:** `Unset it right after foreach to avoid this problem.`

The diagnostic is on a line that assigns to a variable that was used as a
by-reference foreach binding. Find the closing `}` or `endforeach;` of the
relevant foreach loop and insert `unset($var);` on the next line.

Extract the variable name from the foreach statement. The variable is the
`&$var` in `foreach ($items as &$var)`.

---

### P5. `method.tentativeReturnType` — Add `#[\ReturnTypeWillChange]`

**Identifier:** `method.tentativeReturnType`
**Tip:** `Make it covariant, or use the #[\ReturnTypeWillChange] attribute to temporarily suppress the error.`

Same insertion pattern as P2: add `#[\ReturnTypeWillChange]` on the line
before the method declaration.

---

## Tier 2 — Simple message parsing

### P6. `return.type` — Update return type to match actual returns

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
   type is a valid native PHP type (scalars, class names, unions on PHP 8+).
2. **Update `@return` tag** — if a docblock with `@return` exists, replace the
   type. If no docblock exists, create one with `@return {actual}`.

Mark neither as `is_preferred` since the right fix might be to change the code
rather than the signature.

**Stale detection:** the return type or `@return` tag now matches `{actual}`.

---

### P7. `return.phpDocType` — Fix `@return` to match native type

**Identifier:** `return.phpDocType`
**Messages:**
- `PHPDoc tag @return with type {phpdoc} is incompatible with native type {native}.`
- `PHPDoc tag @return with type {phpdoc} is not subtype of native type {native}.`

Parse both types. Offer two quickfixes:

1. **Update `@return` to `{native}`** — replace the `@return` type in the
   docblock.
2. **Remove `@return` tag** — the native type is authoritative, so just
   remove the redundant/wrong docblock tag.

Reuse docblock editing from `update_docblock.rs`.

---

### P8. `parameter.phpDocType` — Fix `@param` to match native type

**Identifier:** `parameter.phpDocType`
**Messages:**
- `PHPDoc tag @param for parameter $name with type {phpdoc} is incompatible with native type {native}.`
- `PHPDoc tag @param for parameter $name with type {phpdoc} is not subtype of native type {native}.`

Parse the parameter name and both types. Offer:

1. **Update `@param $name` to `{native}`**
2. **Remove `@param $name` tag**

---

### P9. `property.phpDocType` — Fix property docblock type

**Identifier:** `property.phpDocType`
**Messages:**
- `{desc} for property Foo::$bar with type {phpdoc} is incompatible with native type {native}.`
- `{desc} for property Foo::$bar with type {phpdoc} is not subtype of native type {native}.`

Parse property name and both types. Offer to update or remove the `@var` tag
on the property docblock.

---

### P10. `return.unusedType` — Remove unused type from return union

**Identifier:** `return.unusedType`
**Messages:**
- `Method Foo::bar() never returns {type} so it can be removed from the return type.`
- `Function foo() never returns {type} so it can be removed from the return type.`

Parse `{type}` from the message. Find the return type (native or `@return`),
parse the union, remove the unused member, and rewrite.

For native types: `string|null` with unused `null` becomes `string`.
For docblock types: same logic on the `@return` tag.

If removing the type would leave a single-member union, simplify
(e.g. `string|null` minus `null` becomes `string`).

---

### P11. `method.visibility` / `property.visibility` — Fix overriding visibility

**Identifiers:** `method.visibility`, `property.visibility`
**Messages:**
- `{Private|Protected} method Foo::bar() overriding public method Parent::bar() should also be public.`
- `Private method Foo::bar() overriding protected method Parent::bar() should be protected or public.`
- (equivalent patterns for properties)

Parse the required visibility from the message. Use the existing
`change_visibility.rs` infrastructure to change the visibility modifier,
but pre-select the correct target visibility instead of offering all three.

Mark as `is_preferred` since there is only one correct answer.

Note: these errors are `.nonIgnorable()` in PHPStan, so the `@phpstan-ignore`
action will not apply. The visibility fix is the only option.

---

### P12. `class.prefixed` — Fix prefixed class name

**Identifier:** `class.prefixed`
**Tip:** `This is most likely unintentional. Did you mean to type {corrected}?`

The corrected class name is in the tip. Replace the prefixed name with the
unprefixed one. This is a simple text replacement at the diagnostic location.

---

## Tier 3 — Requires locating related code

### P13. `property.notFound` (same-class) — Declare missing property

**Identifier:** `property.notFound`
**Message:** `Access to an undefined property Foo::$bar.`

Parse class name and property name from the message. When the access is
`$this->bar` (same class), offer to declare the property:

1. **Declare property** — insert `private mixed $bar;` (or infer type from
   the assignment context) at the top of the class body, after existing
   property declarations.
2. **Add `@property` PHPDoc** — add `@property mixed $bar` to the class
   docblock. Better for classes that use `__get`/`__set`.

For cross-file access (the class is defined elsewhere), this requires a
workspace edit targeting a different file. Start with same-file only.

**Type inference:** if the diagnostic is on an assignment like
`$this->bar = $someString`, infer the type from the RHS. If it is on a read,
fall back to `mixed`.

**Reference:** https://phpstan.org/blog/solving-phpstan-access-to-undefined-property

---

### P14. `throws.unusedType` (narrow) — Narrow `@throws` to actual thrown types

**Identifier:** `throws.unusedType`
**Tip:** `You can narrow the thrown type with PHPDoc tag @throws {narrowed_type}.`

When the tip contains a specific narrowed type, offer to replace the existing
`@throws` tag with the narrowed union. The replacement type is computed by
PHPStan and included in the tip text.

This is different from the existing "Remove @throws" action: instead of
removing the tag entirely, it replaces it with a more precise type.

Note: we need to check whether PHPStan diagnostics include tips in the data
sent to the LSP. If tips are not available, we can skip this one.

---

### P15. Template bound from tip — Add `@template T of X`

**Identifiers:** errors in `IncompatiblePhpDocTypeCheck`, `IncompatiblePropertyPhpDocTypeRule`
**Tip:** `Write @template T of X to fix this.`

The exact PHPDoc text to insert is computed by PHPStan and included in the tip.
Parse `@template {name} of {bound}` from the tip and insert it into the class
or function docblock.

Same caveat as P14: requires tips to be available in the diagnostic data.

---

### P16. `match.unhandled` — Add missing match arms

**Identifier:** `match.unhandled`
**Message:** `Match expression does not handle remaining value(s): {types}`

Parse the remaining value(s) from the message. Find the match expression at
the diagnostic line. Insert new arms before the closing `}`:

```
'value' => throw new \LogicException('Unexpected value'),
```

For enum cases, generate proper `Enum::Case => ...` arms. For literal values,
generate `'literal' => ...` arms.

---

## Tier 4 — Requires body analysis

### P17. `missingType.iterableValue` (return type) — Add `@return` with inferred element type

**Identifier:** `missingType.iterableValue`
**Messages:**
- `Method Foo::bar() return type has no value type specified in iterable type array.`
- `Function foo() return type has no value type specified in iterable type array.`

Only handle the "return type" variant (not parameter/property). Parse the
iterable type name (`array`, `Traversable`, etc.) from the message.

**Approach:**

1. Find the function/method at the diagnostic line.
2. Walk the function body looking for `return` statements.
3. For each return expression:
   - Array literals (`[expr1, expr2]`): infer element types from the values
     using existing `infer_element_type` / `infer_array_literal_raw_type` logic.
   - Variable returns: trace back to assignments where possible.
   - Function call returns: resolve the return type if we can.
4. Union all inferred element types.
5. Offer to add `@return array<InferredType>` (or `list<InferredType>` when
   all keys are sequential integers).

**Fallback:** if we cannot infer a specific type, offer `@return array<mixed>`.
This silences the PHPStan error while being explicit. The PHPStan blog
recommends this approach:
> "If you just want to make this error go away, replace array with mixed[]
> or array<mixed>."

**Stale detection:** a `@return` tag exists with a generic array type
(contains `<` or `[]`).

**Reference:** https://phpstan.org/blog/solving-phpstan-no-value-type-specified-in-iterable-type

---

## Tier 5 — Lower priority / more complex

### P18. `deadCode.unreachable` — Remove unreachable code

**Identifier:** `deadCode.unreachable`
**Message:** `Unreachable statement - code above always terminates.`

Delete the unreachable statement. Tricky because we need to determine the
extent of the dead code (could be a single statement or an entire block).
Start with single-statement removal.

---

### P19. `property.unused` / `method.unused` / `classConstant.unused` — Remove unused member

**Identifiers:** `property.unused`, `method.unused`, `classConstant.unused`
**Messages:**
- `Property Foo::$bar is unused.`
- `Method Foo::bar() is unused.`
- `Constant Foo::BAR is unused.`

Find and delete the entire member declaration. Destructive action, so mark
as non-preferred and only offer when the identifier is exact (not on
`property.onlyRead` etc. where the member is partially used).

---

### P20. `generics.callSiteVarianceRedundant` — Remove redundant variance annotation

**Identifier:** `generics.callSiteVarianceRedundant`
**Tip:** `You can safely remove the call-site variance annotation.`

Strip `covariant` or `contravariant` keywords from generic type arguments
in the docblock. Requires parsing PHPDoc generic syntax.

---

### P21. `return.void` — Remove return value from void function

**Identifier:** `return.void`
**Message:** `{desc} with return type void returns {type} but should not return anything.`

Replace `return {expr};` with `return;` (or remove the return statement).
Only offer when the return is at the end of the function body.

---

### P22. `return.empty` — Add return value or change return type to void

**Identifier:** `return.empty`
**Message:** `{desc} should return {type} but empty return statement found.`

Offer two quickfixes:
1. **Change return type to `void`** — if all returns in the function are empty.
2. **Add a placeholder return** — `return null;` or similar, less useful.

---

### P23. `instanceof.alwaysTrue` — Remove redundant instanceof check

**Identifier:** `instanceof.alwaysTrue`
**Message:** `Instanceof between {type} and {class} will always evaluate to true.`

Offer to simplify: remove the `instanceof` check and keep only the truthy
branch. Complex because it requires understanding the control flow (if/else,
ternary, match arm).

---

### P24. `catch.neverThrown` — Remove unnecessary catch clause

**Identifier:** `catch.neverThrown`
**Message:** `Dead catch - {exception} is never thrown in the try block.`

Remove the catch clause for the exception that is never thrown. If it is the
only catch clause, consider removing the entire try/catch structure.

---

## Implementation notes

### Message parsing

All message parsing should use regex with named capture groups for clarity.
Create a shared helper module (e.g. `code_actions/phpstan_message.rs`) for
common patterns like extracting class names, method names, types, and property
names from PHPStan messages.

### Stale diagnostic detection

Each new action should have a corresponding check in
`is_stale_phpstan_diagnostic()` in `diagnostics/mod.rs` so that the diagnostic
is eagerly cleared after the user applies the fix, without waiting for the
next PHPStan run.

### Testing

Each action needs integration tests following the existing pattern:
- Create a test backend
- Inject PHPStan diagnostics into the cache
- Request code actions
- Assert the edits produce the correct result

### Tip availability

Several actions (P14, P15) depend on PHPStan tips being available in the
diagnostic data sent to the LSP client. Investigate whether the PHPStan
integration preserves tip text. If not, those actions may need to compute
the fix independently rather than parsing it from the tip.