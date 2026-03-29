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

3. **`src/resolution.rs`** — `resolve_type_string`. Replace the
   string-surgery approach (split on `|`, recurse, rejoin) with
   tree traversal on `PhpType`.

4. **`src/docblock/types.rs` and sub-modules** — The old string
   parsers. Once all consumers use `PhpType`, these become dead code
   and can be deleted.

5. **`src/symbol_map/docblock.rs`** — `emit_type_spans`. Replace the
   423-line recursive string decomposer with `PhpType` tree
   traversal + span emission. (The tag-level migration is already
   done; this is the type-level migration.)

6. **`src/diagnostics/`** — Type compatibility checks. Pattern match
   on `PhpType` variants instead of string prefix checks.

7. **`src/code_actions/`** — Type-aware refactorings. Use `PhpType`
   for type comparison, docblock generation, etc. Currently,
   `ResolvedType::type_strings_joined` joins all resolved types
   with `|`, which flattens intersection types (`A&B`) into unions
   (`A|B`). With `PhpType::Intersection` this is preserved.

### Phase 4: Migrate the Laravel provider

The Laravel provider (`src/laravel/`) has its own type manipulation
for Eloquent models, relationships, collections, and facades.

1. **Eloquent attribute types** — Replace string-based cast-type
   mapping with `PhpType` construction.

2. **Relationship return types** — Replace the string template
   `"HasMany<{model}>"` with `PhpType::Generic("HasMany",
   [PhpType::Named(model)])`.

3. **Collection generics** — Replace `format!("Collection<{},
   {}>", key, value)` with `PhpType::Generic` construction.

4. **Facade accessor resolution** — The `getFacadeAccessor` →
   class lookup produces a class name string. This stays as a string
   (it's a class name, not a type expression), but the *return type*
   it produces can be `PhpType::Named`.

### Phase 5: Remove string type fields

Once all consumers read `_parsed` fields:

1. Remove `return_type: Option<String>` from `MethodInfo` (rename
   `return_type_parsed` → `return_type`).
2. Remove `type_hint: Option<String>` from `ParameterInfo`,
   `PropertyInfo`, `ConstantInfo` (rename similarly).
3. Remove `native_return_type: Option<String>` /
   `native_type_hint: Option<String>` — these become
   `PhpType::Named` values populated from the AST hint.
4. Delete `src/docblock/type_strings.rs`, `generics.rs`, `shapes.rs`,
   `callable_types.rs` — the old string parsers.
5. Delete `ConditionalReturnType` enum (replaced by
   `PhpType::Conditional`).
6. Run the full test suite.

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