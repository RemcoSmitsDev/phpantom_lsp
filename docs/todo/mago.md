# PHPantom — Mago Crate Migration

This document describes the migration from hand-rolled PHP parsing
subsystems to upstream Mago crates. The goal is to replace fragile,
maintenance-heavy internal code with well-tested, upstream-maintained
libraries — improving correctness and robustness while reducing the
long-term maintenance burden.

> **Guiding principle:** Correctness and robustness win over raw
> performance. We accept modest overhead from structured
> representations in exchange for eliminating entire classes of
> edge-case bugs in string-based type manipulation.

## Crates to adopt

| Crate              | Replaces                                               | Effort      | Status |
| ------------------ | ------------------------------------------------------ | ----------- | ------ |
| `mago-docblock`    | Manual docblock parsing scattered across the codebase   | Medium-High | ✅ Done |
| `mago-names`       | `src/parser/use_statements.rs` + `use_map` resolution   | Medium-High | ✅ Done |
| `mago-type-syntax` | `src/docblock/{type_strings,generics,shapes,callable_types,conditional}.rs` + string-based type pipeline | Very High | M4 |

`mago-docblock` is fully integrated — all modules that benefit from
structured parsing use `DocblockInfo` / `TagInfo`. The remaining
raw-text docblock code in the codebase operates on individual lines
for surgical text editing and is better served by direct string
manipulation.

A fifth crate, `mago-reporting`, comes in as a transitive dependency
of `mago-semantics` and `mago-names`. It does not replace any
PHPantom code but will appear in `Cargo.toml`.

### Crates explicitly ruled out

| Crate              | Reason                                                                   |
| ------------------ | ------------------------------------------------------------------------ |
| `mago-codex`       | Replaces `ClassInfo` model with one that cannot carry `LaravelMetadata`. |
| `mago-semantics`   | 12K false positives on Laravel; no way to inject our type context.       |
| `mago-linter`      | Same problem; `Integration::Laravel` is surface-level only.              |
| `mago-fingerprint` | Requires `mago-names` for limited value; `signature_eq` already works.   |

---

## M3. Migrate to `mago-names`

No outstanding items.

---

## M4. Migrate to `mago-type-syntax`

**What it replaces:** The string-based type pipeline — approximately
4,700 lines across:

- `src/docblock/type_strings.rs` (~630 lines — `split_type_token`,
  `split_union_depth0`, `clean_type`, `base_class_name`,
  `replace_self_in_type`, etc.)
- `src/docblock/generics.rs` (~230 lines — `parse_generic_args`,
  `extract_generic_value_type`, etc.)
- `src/docblock/shapes.rs` (~340 lines — `parse_array_shape`,
  `parse_object_shape`, etc.)
- `src/docblock/callable_types.rs` (~290 lines —
  `extract_callable_return_type`, `extract_callable_param_types`, etc.)
- `src/docblock/conditional.rs` (~215 lines —
  `extract_conditional_return_type`, `parse_conditional_expr`)
- Scattered `split_type_token` / `split_union_depth0` calls
  throughout `src/hover/`, `src/completion/`, `src/resolution.rs`,
  and `src/symbol_map/docblock.rs`.

**Why:** Every type in the system is `Option<String>`. Consumers
decompose these strings with hand-written depth-tracking parsers
(counting `<>`, `{}`, `()` nesting) at every use site. This is
fragile, repetitive, and makes it impossible to add features like
conditional-type evaluation, generic type substitution, or type
compatibility checks without yet more string surgery.

`mago-type-syntax` provides `PhpType` — a structured enum that
represents unions, intersections, generics, callables, shapes,
conditionals, etc. as a tree. One parse at extraction time; pattern
matching everywhere else.

**Risk:** Very high blast radius. Every struct that carries a type
field (`ParameterInfo::type_hint`, `MethodInfo::return_type`,
`PropertyInfo::type_hint`, `ConditionalReturnType::Concrete`, etc.)
is affected. The phased approach below is designed to make this
manageable.

### Phase 3: Migrate consumers to structured types

**Goal:** Replace string-based type manipulation with `PhpType`
pattern matching, one module at a time. After each module is
migrated, its string-field reads are removed.

Modules in recommended migration order (least dependencies first):

1. ✅ **`src/hover/`** — Type display and structural operations.

   **Status:** Complete. Structural type operations migrated to
   `PhpType`; display formatting kept on `shorten_type_string` to
   preserve callable parameter names and source-level
   parenthesization. All 236 hover integration tests pass.

   **What changed:**

   - `build_variable_hover_body` uses `PhpType::parse()` +
     `union_members()` instead of `split_top_level_union` (deleted).
   - `build_variable_hover_body` uses `PhpType::is_scalar()` instead
     of `docblock::type_strings::is_scalar`.
   - `resolve_type_namespace` replaced by
     `resolve_type_namespace_structured` which uses
     `PhpType::base_name()` instead of string surgery.
   - `build_var_annotation` and `build_param_return_section` use
     `PhpType::equivalent()` instead of `types_equivalent` for
     type comparison.
   - Template bound display (3 sites) uses
     `PhpType::parse(bound).shorten()` instead of
     `shorten_type_string(bound)`.
   - `shorten_type_string` and `types_equivalent` kept as exports
     for `completion/builder.rs` and other modules not yet migrated.

   **Design decision:** `PhpType::shorten().to_string()` drops
   callable parameter names (`$item`) and changes union spacing
   (`|` → ` | `). For display in hover popups, the old
   `shorten_type_string` is kept because it preserves the original
   format character-by-character. `PhpType` is used only for
   structural operations (union splitting, equivalence checks,
   scalar detection, base-name extraction).

   **New `PhpType` helper methods** added in this step:
   - `shorten()` — produce a new `PhpType` with all FQNs shortened
   - `is_scalar()` — whether a type is a built-in / non-class type
   - `base_name()` — extract the base class name (if any)
   - `union_members()` — return top-level union members as a vec
   - `equivalent()` — compare two types ignoring namespace differences

2. ✅ **`src/completion/`** — Type matching for member access.

   **Status:** Complete. All `extract_generic_value_type`,
   `extract_generic_key_type`, and several `clean_type` call sites
   migrated to `PhpType` methods. All 3,400+ tests pass.

   **What changed:**

   - `src/hover/variable_type.rs` — foreach value/key extraction
     uses `PhpType::extract_value_type(true)` /
     `PhpType::extract_key_type(true)` instead of
     `docblock::types::extract_generic_value_type` /
     `extract_generic_key_type`.
   - `src/completion/variable/foreach_resolution.rs` — 4 call sites
     migrated from `extract_generic_value_type` /
     `extract_generic_key_type` to `PhpType::extract_value_type` /
     `extract_key_type`.
   - `src/completion/variable/raw_type_inference.rs` — 8 call sites
     migrated: all `extract_generic_value_type` calls, plus
     `clean_type`/`is_scalar` in `extract_array_map_element_type`
     replaced with `PhpType::parse().base_name()`.
   - `src/completion/variable/rhs_resolution.rs` — 4 call sites:
     `classify_template_binding` and `resolve_rhs_array_access`
     use `PhpType::base_name()` and `extract_value_type`.
     `resolve_rhs_property_access` uses `PhpType::base_name()`.
   - `src/completion/variable/resolution.rs` — 1 call site migrated
     (`resolve_arg_raw_type` uses `PhpType::extract_value_type`).
   - `src/completion/call_resolution.rs` — 1 call site migrated.
   - `src/completion/source/helpers.rs` — `walk_array_segments_and_resolve`
     uses `PhpType::extract_value_type` for element access and
     `PhpType::is_scalar()` for the final type check. Two
     `resolve_lhs_to_class` sites kept on `clean_type` for now
     (they handle unions/nullable types that `base_name()` can't
     collapse).

   **New `PhpType` helper methods** added in this step:
   - `extract_value_type(skip_scalar)` — extract the value type from
     generics/arrays (last param, or 2nd for Generator)
   - `extract_key_type(skip_scalar)` — extract the key type from
     2+-param generics
   - `extract_element_type()` — convenience for
     `extract_value_type(false)`
   - `intersection_members()` — return top-level intersection members

   **Design decision:** `clean_type` is a Swiss-army-knife function
   that strips `?`, leading `\`, trailing punctuation, extracts
   non-null from unions, and strips generics. It cannot be replaced
   by a single `PhpType` method. Call sites where `clean_type` is
   used purely for base-name extraction were migrated to
   `PhpType::base_name()`. Call sites where `clean_type` handles
   union collapsing (e.g. `User|null` → `User`) were kept on
   `clean_type` since `base_name()` returns `None` for unions.

3. ✅ **`src/parser/ast_update.rs`** — `resolve_type_string`.

   **Status:** Complete. The ~120-line character-by-character
   `resolve_type_string` function has been replaced with
   `PhpType::resolve_names()`. Call sites that operate on `PhpType`
   fields (return types, parameter type hints, property type hints)
   now call `resolve_names` directly on the structured type,
   eliminating the `to_string()` → string surgery →
   `PhpType::parse()` round-trip. String-typed fields (`throws`,
   `native_return_type`, type alias definitions, docblock tag
   types) use a thin `resolve_type_string_via_php_type` wrapper
   that parses, resolves, and converts back. A shared
   `build_type_resolver` method constructs the resolver closure
   that skips template params and type alias names. The callable
   `kind` field (e.g. `\Closure`) is now resolved through the
   resolver in `PhpType::resolve_names`, fixing FQN callable
   references that previously retained their leading backslash.
   All 7,732 tests pass.

   **New `PhpType` helper methods** added in this step:
   - `resolve_names(resolver)` — produce a new `PhpType` with all
     class-like names resolved through a callback (now also
     resolves callable `kind` names like `\Closure`)
   - `is_keyword_type()` (module-level) — superset of `is_scalar_name`
     that covers all PHPDoc pseudo-types and special keywords

4. **`src/docblock/types.rs` and sub-modules** — The old string
   parsers. Once all consumers use `PhpType`, these become dead code
   and can be deleted.

5. ✅ **`src/symbol_map/docblock.rs`** — `emit_type_spans`.

   **Status:** Complete. The 423-line recursive string decomposer
   replaced with `mago-type-syntax` AST tree walker. All 3,001
   library tests pass.

   **What changed:**

   - `emit_type_spans` now parses the type token with
     `mago_type_syntax::parse_str` and delegates to
     `emit_type_spans_from_ast` which walks the mago AST using
     `HasSpan` for accurate byte offsets.
   - New `emit_type_spans_from_ast` function handles all 69 mago
     `Type` variants: `Reference` → `ClassReference` span on the
     identifier, `Variable("$this")` → `SelfStaticParent`,
     keyword types (`static`/`self`/`parent`) →
     `SelfStaticParent`, composite types (Union, Intersection,
     Nullable, Parenthesized) → recurse, generics → recurse into
     parameters, callables → emit keyword span + recurse into
     params and return type, shapes → recurse into field values,
     conditionals → recurse into target/then/otherwise.
   - New `emit_identifier_span` helper emits `ClassReference` or
     `SelfStaticParent` based on navigability.
   - New `emit_generic_params` helper recurses into generic
     parameter entries.
   - New `strip_variance_annotations` pre-processor strips
     PHPStan `covariant `/ `contravariant ` prefixes from generic
     arguments (which mago doesn't recognise) and builds an offset
     map to translate cleaned-string positions back to original
     positions.
   - Parse-failure fallback emits a single `ClassReference` span
     for the whole token if it looks navigable.
   - Deleted `find_callable_paren` helper (54 lines).
   - Deleted `find_keyword_depth0` helper (24 lines).
   - Removed imports of `split_union_depth0`,
     `split_intersection_depth0` from the file.
   - Removed dead `split_union_depth0` re-export from
     `docblock/types.rs`.
   - Added `use mago_type_syntax::ast as type_ast` import.
   - Updated module doc comment to reflect structured approach.

6. ✅ **`src/diagnostics/`** — Type compatibility checks.

   **Status:** Complete. All `clean_type` + `is_scalar` +
   `strip_generics` + `PHPDOC_TYPE_KEYWORDS` patterns migrated to
   `PhpType` methods. All 269 diagnostics tests pass.

   **What changed:**

   - `resolve_scalar_subject_type` (5 sites) — replaced
     `clean_type(&hint)` + `is_scalar(&cleaned)` with
     `PhpType::all_members_primitive_scalar()` +
     `PhpType::non_null_type()` for the display string.
   - `resolve_unresolvable_class_subject` — replaced
     `clean_type` + `strip_generics` + `is_scalar` +
     `PHPDOC_TYPE_KEYWORDS` check with
     `PhpType::all_members_scalar()` early exit +
     `PhpType::non_null_type()` + `PhpType::base_name()`.
   - Removed imports of `PHPDOC_TYPE_KEYWORDS`, `is_scalar`,
     `strip_generics` from the file.

   **New `PhpType` helper methods** added in this step:
   - `non_null_type()` — extract the non-null part of a type
     (`User|null` → `User`, `?User` → `User`)
   - `all_members_scalar()` — whether all non-null members are
     scalar (replaces `clean_type` + `is_scalar`)
   - `is_primitive_scalar()` / `all_members_primitive_scalar()` —
     narrower scalar check excluding `mixed`, `object`, etc.
     (for diagnostics that should only flag primitive scalars)

7. ✅ **`src/code_actions/update_docblock.rs`** — Type comparison.

   **Status:** Complete. `split_union_depth0` calls in
   `is_type_contradiction` and `normalize_type_for_comparison`
   migrated to `PhpType::parse()` + `union_members()`. The
   `split_type_token` calls in `parse_doc_params_from_info` and
   `parse_doc_return_from_info` are kept — they tokenize raw
   docblock text (separating the type from the parameter
   name/description), not type structure. All 44 update_docblock
   tests pass.

   **What changed:**

   - `is_type_contradiction` uses `PhpType::parse()` +
     `union_members()` instead of `split_union_depth0`.
   - `normalize_type_for_comparison` uses `PhpType::parse()` +
     `union_members()` instead of `split_union_depth0`.
   - Removed import of `split_union_depth0`.

8. ✅ **`src/definition/type_definition.rs`** — Go-to-type-definition.

   **Status:** Complete. All string-based type extraction replaced
   with `PhpType` methods. All 11 unit tests + 22 integration
   tests pass.

   **What changed:**

   - `resolve_member_type_names` uses `PhpType::parse().replace_self()`
     + `top_level_class_names()` instead of
     `replace_self_in_type` + `extract_class_names_from_type_string`.
   - `resolve_function_return_type_names` uses
     `PhpType::parse().top_level_class_names()`.
   - `resolve_type_names_to_locations` uses `PhpType::parse().is_scalar()`
     instead of `is_scalar()`.
   - `resolve_variable_type_names` uses `PhpType::parse().top_level_class_names()`
     and `PhpType::parse().is_scalar()`.
   - Deleted local `extract_class_names_from_type_string` and
     `split_top_level_union` functions (77 lines removed).
   - Removed imports of `is_scalar`, `replace_self_in_type`.

   **New `PhpType` helper methods** added in this step:
   - `replace_self(class_name)` — produce a new `PhpType` with
     `self`/`static`/`$this` replaced by the given class name
   - `extract_class_names()` — recursively collect all class-like
     names from the type tree
   - `top_level_class_names()` — collect only outermost class names
     (does not recurse into generics, callables, shapes, etc.)

9. ✅ **`src/completion/types/resolution.rs`** — Core type-hint-to-class
   resolution.

   **Status:** Complete. Union/intersection splitting and generic
   arg parsing migrated to `PhpType`. All 3,400+ tests pass.

   **What changed:**

   - Union splitting uses `PhpType::parse()` + pattern match on
     `PhpType::Union` instead of `split_union_depth0`.
   - Intersection splitting uses pattern match on
     `PhpType::Intersection` instead of `split_intersection_depth0`.
   - Object shape detection uses pattern match on
     `PhpType::ObjectShape` instead of `is_object_shape` +
     `parse_object_shape`.
   - Generic arg extraction uses pattern match on
     `PhpType::Generic` instead of `parse_generic_args`.
   - `self<…>` / `static<…>` detection uses the same
     `PhpType::Generic` match instead of `parse_generic_args`.
   - Base class name extraction uses `base_hint` from the
     `PhpType::Generic` match instead of `strip_generics`.
   - Removed imports of `split_union_depth0`,
     `split_intersection_depth0`, `parse_generic_args`,
     `strip_generics`, and `crate::docblock`.

10. ✅ **`src/resolution.rs`** — Class loading normalisation.

    **Status:** Complete. `find_or_load_class` uses
    `PhpType::parse().base_name()` instead of `strip_nullable` +
    `strip_generics` for defensive type normalisation. All tests pass.

11. ✅ **`src/completion/source/helpers.rs`** — LHS-to-class resolution.

    **Status:** Complete. Two `clean_type` call sites in
    `resolve_lhs_to_class` migrated to `PhpType::parse()` +
    `non_null_type()` + `base_name()`. All tests pass.

12. ✅ **`src/completion/variable/rhs_resolution.rs`** —
    `extract_array_type_at_position`.

    **Status:** Complete. Replaced `clean_type` +
    `parse_generic_args` with `PhpType::parse()` + pattern matching
    on `PhpType::Array` and `PhpType::Generic`. All tests pass.

13. ✅ **`src/definition/member/mod.rs`** — Eloquent Builder model
    extraction.

    **Status:** Complete. `extract_model_from_builder_ret` uses
    `PhpType::parse()` + pattern match on `PhpType::Generic`
    instead of `parse_generic_args`. All tests pass.

14. ✅ **`src/parser/classes.rs`** — Custom collection extraction.

    **Status:** Complete. `extract_custom_collection_from_new_collection`
    uses `PhpType::parse().base_name()` instead of
    `parse_generic_args`. All tests pass.

15. ✅ **`src/virtual_members/laravel/accessors.rs`** — Modern
    accessor type extraction.

    **Status:** Complete. `extract_modern_accessor_type` uses
    `PhpType::parse()` + pattern match on `PhpType::Generic`
    instead of `parse_generic_args`. Test updated for
    `PhpType::Display` union spacing (`"string | null"`). All tests
    pass.

16. ✅ **`src/virtual_members/laravel/relationships.rs`** —
    Relationship classification and related type extraction.

    **Status:** Complete. `classify_relationship` uses
    `PhpType::parse().base_name()` instead of `parse_generic_args`.
    `extract_related_type` uses pattern match on
    `PhpType::Generic` instead of `parse_generic_args`. All tests
    pass.

   **Dead code removed in this batch:**
   - `parse_generic_args` in `docblock/generics.rs` — all external
     callers migrated; function deleted.
   - `parse_generic_args` re-export in `docblock/types.rs` — removed.
   - `strip_generics` re-export in `docblock/types.rs` — removed.

### Phase 4: Migrate the Laravel provider ✅

The Laravel provider (`src/virtual_members/laravel/`) has its own
type manipulation for Eloquent models, relationships, collections,
and facades.

1. ✅ **`src/virtual_members/laravel/accessors.rs`** — Modern
   accessor detection.

   **Status:** Complete. Ad-hoc `split('<')` generic stripping
   replaced with `PhpType::parse().base_name()`. All tests pass.

   **What changed:**

   - `is_modern_accessor` uses `PhpType::parse(rt).base_name()`
     instead of `rt.split('<').next().unwrap_or(rt).trim()`.

2. ✅ **`src/virtual_members/laravel/relationships.rs`** —
   Relationship property type construction and body inference.

   **Status:** Complete. All `format!`-based type string
   construction replaced with `PhpType` construction +
   `.to_string()`. All 70 relationship tests pass.

   **What changed:**

   - `build_property_type` — Collection branch uses
     `PhpType::Generic(collection_class, [PhpType::Named(inner)])`
     instead of `format!("{collection_class}<{inner}>")`.
   - `infer_relationship_from_body` — morphTo returns
     `PhpType::Named(format!("\\{fqn}")).to_string()`,
     generic relationships return
     `PhpType::Generic(format!("\\{fqn}"), [PhpType::Named(class_arg)]).to_string()`,
     bare relationships return
     `PhpType::Named(format!("\\{fqn}")).to_string()`.

3. ✅ **`src/virtual_members/laravel/builder.rs`** — Builder
   self-type construction and collection replacement.

   **Status:** Complete. Builder self-type uses `PhpType::Generic`
   construction. `replace_eloquent_collection` uses a recursive
   `PhpType` tree walk instead of naive `.replace()`. All builder
   tests pass.

   **What changed:**

   - `builder_self_type` uses
     `PhpType::Generic(ELOQUENT_BUILDER_FQN, [PhpType::Named(class.name)])`
     instead of `format!("{ELOQUENT_BUILDER_FQN}<{}>", class.name)`.
   - `replace_eloquent_collection` parses the type string into a
     `PhpType` tree and recursively replaces any `Generic` node
     whose base name matches the Eloquent Collection FQN.
   - New `replace_collection_in_type` helper recursively walks
     `PhpType` trees (Union, Intersection, Nullable, Generic,
     Array) to find and replace collection references.

4. ✅ **`src/virtual_members/laravel/scopes.rs`** — Default scope
   return type.

   **Status:** Complete. String constant replaced with
   `PhpType::Generic` construction. All scope tests pass.

   **What changed:**

   - `DEFAULT_SCOPE_RETURN_TYPE` string constant replaced with
     `default_scope_return_type()` function that constructs
     `PhpType::Generic("…Builder", [PhpType::Named("static")])`.

5. **Eloquent attribute types** — `cast_type_to_php_type` in
   `casts.rs` returns simple type name strings (`"bool"`,
   `"Carbon\\Carbon"`, etc.).  These are plain class/keyword
   names, not composite type expressions.  They flow into
   `PropertyInfo::virtual_property(column, Some(&php_type))`
   which already parses them into `type_hint_parsed`.  No
   migration needed — these are identity round-trips.

6. **Facade accessor resolution** — The `getFacadeAccessor` →
   class lookup produces a class name string.  This stays as a
   string (it's a class name, not a type expression).

   **`PhpType::Display` fix:** Changed union separator from
   ` | ` to `|` and intersection separator from ` & ` to `&` in
   `PhpType`'s `Display` implementation to match PHP convention.
   Updated 9 tests across `php_type.rs`, `accessors_tests.rs`,
   `builder_tests.rs`, `hover/tests.rs`, and
   `tests/completion_laravel.rs` that had been adapted for the
   spaced format.

### Phase 5: Remove string type fields ✅

All steps complete. The dual `String` / `PhpType` representation
has been eliminated from the core data model:

- `MethodInfo::return_type` is now `Option<PhpType>` (was
  `Option<String>`). The redundant `return_type_parsed` field has
  been removed.
- `FunctionInfo::return_type` is now `Option<PhpType>`.
- `ParameterInfo::type_hint` is now `Option<PhpType>` (was
  `Option<String>`). The redundant `type_hint_parsed` field has
  been removed.
- `PropertyInfo::type_hint` is now `Option<PhpType>` (same).
- `ConstantInfo::type_hint` is now `Option<PhpType>` (same).
- `ResolvedCallableTarget::return_type` is now `Option<PhpType>`.

Convenience methods `return_type_str()` and `type_hint_str()`
(returning `Option<String>`) are provided on each struct for
display sites that need a string.

`apply_substitution_to_method` and `apply_substitution_to_property`
now call `PhpType::substitute()` directly on the structured type
tree, eliminating the parse/to_string round-trip.

The `split_intersection_depth0` function (test-only, no production
callers) has been deleted along with its re-exports and tests.

Deleted types and functions: `ConditionalReturnType`,
`ParamCondition`, `replace_self_in_type`, `is_scalar`,
`SCALAR_TYPES`, `raw_type_is_nullable`, `strip_null_from_union`,
`strip_generics`, `strip_nullable`, `normalize_nullable`,
`find_matching_close`, `parse_conditional_expr`,
`parse_type_or_conditional`, `find_token_at_depth`,
`parse_condition`, `split_intersection_depth0`, and 5 local
helpers in `inheritance.rs`.

Remaining intra-docblock string helpers (`clean_type`,
`split_union_depth0`, `split_generic_args`, `split_type_token`,
`PHPDOC_TYPE_KEYWORDS`) are kept: they operate on raw docblock
text (not struct fields) and have no `PhpType` equivalent.

See `docs/todo/m4-phase5-scratch.md` for detailed step tracking.

---

## Testing strategy

Each migration step (M3, M4) must pass the **full existing test
suite** before merging. This is the primary safety net.

Additional testing per migration:

| Migration | Extra tests |
| --------- | ----------- |
| M3 | Snapshot tests comparing `use_map`-based resolution with `OwnedResolvedNames`-based resolution across the fixture corpus. |
| M4 | Round-trip tests (`PhpType::parse(s).to_string() == s`) for every type string in the test suite. Per-module migration tests comparing old string-based output with new `PhpType`-based output. |

For M4 specifically, the dual-representation phase (Phase 2) enables
**shadow testing**: compute the result both ways and assert they
match, before removing the old path. This catches regressions without
blocking progress.

---

## Version alignment

All Mago crates should be pinned to the same release. At the time of
writing, the latest version is **1.15.x**. When upgrading, update all
Mago crates in a single commit and run the test suite.

The `mago-docblock` crate is already present in `Cargo.toml`. When
adding `mago-type-syntax` and `mago-names`, align them to the same
version.

---

## What this enables

Once M3 + M4 are complete:

- **Structured types everywhere.** No more string surgery for type
  manipulation. Generic substitution, conditional evaluation, and
  type compatibility checks become tree operations.

- **Correct name resolution.** Every identifier resolves to its FQN
  in a single pass. Auto-import and unused-import diagnostics become
  straightforward.

- **Foundation for advanced features.** Laravel Eloquent attribute
  completion, Blade template support, and PHPStan-level type
  inference all require structured types and correct name resolution.
  Building them on the new foundation avoids double work.

- **Reduced maintenance burden.** ~6,500+ lines of hand-written
  parsers replaced by well-tested upstream crates.