# PHPantom — Bug Fixes

Known bugs and incorrect behaviour. These are distinct from feature
requests — they represent cases where existing functionality produces
wrong results. Bugs should generally be fixed before new features at
the same impact tier.

Items are ordered by **impact** (descending), then **effort** (ascending).

| Label | Scale |
|---|---|
| **Impact** | **Critical**, **High**, **Medium-High**, **Medium**, **Low-Medium**, **Low** |
| **Effort** | **Low** (≤ 1 day), **Medium** (2-5 days), **Medium-High** (1-2 weeks), **High** (2-4 weeks), **Very High** (> 1 month) |

---

## 1. Short-name collisions in `find_implementors`
**Impact: Low · Effort: Low (fixed)**

**Status:** Fixed. `class_implements_or_extends` now compares
fully-qualified names when a namespace is available. The short-name
fallback is only used when FQN information is absent. `seen_fqns` in
`find_implementors` deduplicates by FQN (built from `name` +
`file_namespace`) instead of by short name.

---

## 2. GTD fires on parameter variable names and class declaration names
**Impact: Medium · Effort: Low**

Go-to-definition fires on parameter variable names (`$supplier`, `$country`)
and class declaration names (`class Foo`), navigating to the same location —
the cursor is already at the definition. This is noisy and unexpected:
clicking a parameter name or a class declaration name should either do
nothing or offer a different action (e.g. find references).

### Current behaviour

- **Parameter names:** Ctrl+Click on `$supplier` in a method signature
  jumps to… `$supplier` in the same method signature. The `VarDefSite`
  with `kind: Parameter` is correctly recorded, and `find_var_definition`
  returns it — so the "definition" is the cursor's own position.

- **Class declarations:** Ctrl+Click on `Foo` in `class Foo {` jumps to
  the same `Foo` token. The `SymbolMap` records a `ClassDeclaration`
  span, and `resolve_definition` resolves it to the same file and offset.

### Fix

In the definition handler, after resolving the definition location, check
whether the target location is the same as (or within a few bytes of) the
cursor position. If so, return `None` — there is no useful jump to make.

Alternatively, suppress at the `SymbolKind` level:
- For `Variable` spans where `var_def_kind_at` returns `Some(Parameter)`,
  skip definition.
- For `ClassDeclaration` spans, skip definition.

### Tests to update

Several existing definition tests assert that parameter names and class
declarations produce a definition result pointing to themselves. These should
expect `None` instead.

---

## 3. Relationship classification matches short name only
**Impact: Low · Effort: Low**

`classify_relationship` in `virtual_members/laravel.rs` strips the
return type down to its short name (via `short_name`) and matches
against a hardcoded list (`HasMany`, `BelongsTo`, etc.). This means
any class whose short name collides with a Laravel relationship class
(e.g. a custom `App\Relations\HasMany` that does not extend
Eloquent's) would be incorrectly classified as a relationship.

The fix would be to resolve the return type to its FQN (using the
class loader or use-map) and verify it lives under
`Illuminate\Database\Eloquent\Relations\` (or extends a class that
does) before classifying. The short-name-only path could remain as a
fast-path fallback when the FQN is already in the
`Illuminate\Database\Eloquent\Relations` namespace.

---

## 4. Go-to-implementation misses transitive implementors
**Impact: Medium · Effort: Medium (fixed)**

**Status:** Fixed. `class_implements_or_extends` already walks the
parent class chain transitively (up to `MAX_INHERITANCE_DEPTH`) and
checks interface-extends chains recursively. Classes that extend a
concrete class which itself implements the target interface are found
correctly. Tested with `test_implementation_transitive_via_parent`,
`test_implementation_skips_abstract_subclasses`, and deep interface
inheritance chains.

---

## 5. Go-to-implementation Phase 5 should only walk user PSR-4 roots
**Impact: Low · Effort: Low (fixed)**

**Status:** Fixed. PSR-4 mappings now come exclusively from
`composer.json` (user code only). Vendor PSR-4 mappings are no longer
loaded (see §7), so Phase 5 inherently walks only user roots.

---

## 6. Go-to-definition does not check the classmap
**Impact: Medium · Effort: Low (fixed)**

**Status:** Fixed. `resolve_class_reference`, `resolve_self_static_parent`,
and `resolve_type_hint_string_to_location` now check the Composer classmap
(FQN → file path) between the class_index lookup and the PSR-4 fallback.
A cold Ctrl+Click on a vendor class resolves through the classmap without
needing vendor PSR-4 mappings.

---

## 7. Vendor PSR-4 mappings removed
**Impact: Low · Effort: Low (fixed)**

**Status:** Fixed. `parse_vendor_autoload_psr4` has been removed.
`parse_composer_json` no longer reads `vendor/composer/autoload_psr4.php`.
PSR-4 mappings come exclusively from the project's own `composer.json`
(`autoload.psr-4` and `autoload-dev.psr-4`). The `is_vendor` flag on
`Psr4Mapping` has been removed.

All resolution paths that could hit a vendor class now check the classmap
first (§6). If the classmap is missing or stale, vendor classes fail to
resolve visibly (fix: run `composer dump-autoload`). This reduces startup
time and memory for projects with large dependency trees.

**Note for Rename Symbol:** when rename support is implemented, the
handler should reject renames for symbols whose definition lives under
the vendor directory. The user cannot meaningfully rename third-party
code. Use `vendor_uri_prefix` to detect this and return an appropriate
error message.

---

## 13. Evict transiently-loaded files from ast_map after GTI and Find References
**Impact: Low · Effort: Low**

Go-to-implementation (Phases 3 and 5) and Find References
(`ensure_workspace_indexed`) parse files from disk and cache them in
`ast_map`. Most of these are false positives that passed the cheap
string pre-filter but don't actually contain matching symbols. Even the
true matches are rarely needed afterwards (the user will open the one
they care about through the editor, which triggers a fresh `did_open`).

Keeping these files in the ast_map wastes memory and pollutes subsequent
Phase 1 scans with classes from files that aren't part of the user's
working set.

**Fix:** After `find_implementors` returns, remove any `ast_map` entries
whose URI was not already present before the scan started. Same for
`ensure_workspace_indexed`. Collect the set of existing URIs before the
scan, then evict the difference afterwards. Files that are in
`open_files` must never be evicted.

**Discovered via:** fixture conversion (call_expression/static_factory_return_self).