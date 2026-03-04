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
**Impact: Low · Effort: Low**

`class_implements_or_extends` matches interfaces by both short name and
FQN (`iface_short == target_short || iface == target_fqn`).  Two
interfaces in different namespaces with the same short name (e.g.
`App\Logger` and `Vendor\Logger`) could produce false positives.
Similarly, `seen_names` in `find_implementors` deduplicates by short
name, so two classes with the same short name in different namespaces
could shadow each other.

**Fix:** always compare fully-qualified names by resolving both sides
before comparison.

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
**Impact: Medium · Effort: Medium**

`find_implementors` only finds classes that directly implement or extend
the target interface/abstract class (plus one level of interface-extends
and parent-class chains). It does not discover classes that extend a
non-final concrete class which itself implements the target.

**Example:**

```php
interface Renderable {}
class BaseView implements Renderable {}  // found ✓
class HtmlView extends BaseView {}       // missed ✗
class JsonView extends HtmlView {}       // missed ✗
```

PhpStorm finds all three. PHPantom only finds `BaseView`.

**Fix:** After the initial scan, collect all non-final concrete classes
in the result set and re-scan for classes that extend them. Repeat until
no new implementors are discovered (fixed-point iteration). The
`seen_names` set prevents infinite loops. This only affects the
`class_implements_or_extends` check — the five-phase file discovery
pipeline stays the same.

### Scanning strategy

Only Go-to-implementation and Find References do multi-file scanning.
All other features (completion, go-to-definition, hover, diagnostics)
use maps or known file names and never walk directories.

For both GTI and Find References, the scanning should follow these
principles:

- **Vendor code:** the classmap is the sole source of truth. Never walk
  vendor directories. Vendor PSR-4 mappings are not loaded at all (see
  §7). If the classmap is missing or stale, vendor classes fail to
  resolve visibly (fix: run `composer dump-autoload`).
- **User code:** walk user PSR-4 roots from `composer.json`. User files
  may have been created since the last `dump-autoload`, so a filesystem
  walk is appropriate.

### Shared pre-filter for file scanning

Both GTI (Phases 3 and 5) and Find References read raw file contents
and use `raw.contains(target_short)` to skip files cheaply before
parsing. This produces many false positives because `target_short` can
appear in comments, strings, variable names, or as a substring of
unrelated identifiers.

A tighter regex-based pre-filter could eliminate most false positives.
For a class name like `Renderable`, searching for a pattern like
`\bRenderable\b` followed by a likely PHP context character (`;`, `(`,
`,`, `{`, or preceded by `\`, `implements`, `extends`, `new`, `use`)
would reject files that only mention the name in a comment or string.

This should be extracted into a shared utility (e.g.
`source_likely_references_name(raw: &str, name: &str) -> bool`) that
both GTI and Find References can use. The exact pattern can be tuned
over time without touching the callers.

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