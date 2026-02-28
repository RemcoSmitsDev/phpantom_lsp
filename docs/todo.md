# PHPantom — Remaining Work

> Last updated: 2026-02-28

Items are ordered by **impact** (descending), then **effort** (ascending)
within the same impact tier — so high-impact quick wins come first. The
Critical tier tracks internal refactors that must land before new features
because multiple subsystems depend on their infrastructure.

Each item carries two ratings:

| Label | Scale |
|---|---|
| **Impact** | How much this improves the user experience or closes competitive gaps: **Critical**, **High**, **Medium-High**, **Medium**, **Low-Medium**, **Low** |
| **Effort** | Expected implementation effort: **Low** (≤ 1 day), **Medium** (2-5 days), **Medium-High** (1-2 weeks), **High** (2-4 weeks), **Very High** (> 1 month) |

---

<!-- ============================================================ -->
<!--  TIER 0 — CRITICAL (infrastructure refactors)                -->
<!-- ============================================================ -->

## Critical

### 1. Named-argument completion: migrate to symbol map
**Impact: Critical · Effort: Low**

`detect_named_arg_context` in `named_args.rs` uses `find_enclosing_open_paren`
and `extract_call_expression` (character-level backward scanning) to detect
which function or method call the cursor is inside. This suffers from chain-
resolution failures on `$this->prop->method(`, `$obj->first()->second(`,
`(new Foo())->bar(`, and strings containing parentheses.

The `CallSite` vector and `find_enclosing_call_site` are now available in
`SymbolMap` (added during the signature help refactor). Named-arg completion
can use `find_enclosing_call_site` as its primary detection path and fall
back to the text scanner for broken ASTs, mirroring the approach already
working in signature help.

`resolve_named_arg_params` also has an `else { vec![] }` bug: when the
subject of a `->` call doesn't start with `$` (e.g. a chain result or
bare class name), it returns no parameters. The fix is the same one
applied to signature help's `resolve_callable`: route all subjects
through `resolve_target_classes` instead of guarding on `starts_with('$')`.

---

### 2. Completion member access: add symbol-map primary path
**Impact: Critical · Effort: Medium**

`try_member_access_completion` calls `extract_completion_target` (in
`target.rs`), which uses `collapse_continuation_lines`,
`extract_arrow_subject`, and `extract_double_colon_subject` from
`subject_extraction.rs`. This is pure character-level backward scanning
with no symbol-map involvement (confirmed: zero references to
`symbol_map` in the entire `completion/` directory).

Hover and go-to-definition already use `SymbolMap::lookup` to get a
pre-extracted `MemberAccess { subject_text, member_name, is_static,
is_method_call }` from the AST. Completion should adopt the same
"symbol map primary, text fallback" pattern so it benefits from the
AST's correct handling of `(new Foo())->`, call-result chains, array
access chains, and null-safe chains.

The main challenge is that completion fires while the user is actively
typing, so the AST is frequently broken at the cursor. The text scanner
is more resilient in that scenario, which is why it should remain as a
fallback rather than being removed. The symbol-map path would improve
accuracy for all cases where the parser successfully recovers.

---

<!-- ============================================================ -->
<!--  TIER 1 — HIGH IMPACT                                        -->
<!-- ============================================================ -->

## High Impact

### 3. Pipe operator (PHP 8.5)
**Impact: High · Effort: Low**

PHP 8.5 introduced the pipe operator (`|>`):

```php
$result = $input
    |> htmlspecialchars(...)
    |> strtoupper(...)
    |> fn($s) => "<b>$s</b>";
```

The mago parser already produces `Expression::Pipe` nodes, and the
closure resolution module walks into pipe sub-expressions to find
closures. However, **no type resolution** is performed for the pipe
result — the RHS callable's return type is never resolved, so
`$result->` after a pipe chain produces no completions.

**Fix:** In `resolve_rhs_expression`, add a `Expression::Pipe` arm
that resolves the RHS callable (function reference, closure, or
arrow function) and returns its return type. For first-class
callable syntax (`htmlspecialchars(...)`), reuse the existing
`extract_first_class_callable_return_type` logic.

---

### 4. Function-level `@template` generic resolution
**Impact: High · Effort: Medium**

`MethodInfo` has `template_params` and `template_bindings` fields that
enable method-level generic resolution at call sites (e.g.
`@template T` + `@param class-string<T> $class` + `@return T`).
`FunctionInfo` has **neither** field, so standalone functions that use
`@template` lose their generic type information entirely.

The only function-level template handling today is
`synthesize_template_conditional` in `parser/functions.rs`, which
converts the narrow `@return T` + `@param class-string<T>` pattern
into a `ConditionalReturnType`.  This does not cover the general case
where template params appear inside a generic return type:

```php
/**
 * @template TKey of array-key
 * @template TValue
 * @param  array<TKey, TValue>  $value
 * @return \Illuminate\Support\Collection<TKey, TValue>
 */
function collect($value = []) { ... }

/**
 * @template TValue
 * @param  TValue  $value
 * @param  callable(TValue): TValue  $callback
 * @return TValue
 */
function tap($value, $callback = null) { ... }
```

After `$users = collect($userArray)`, the result is an unparameterised
`Collection` — element type information is lost, and
`$users->first()->` produces no completions.

This affects Laravel helpers (`collect`, `value`, `retry`, `tap`,
`with`, `transform`, `data_get`), PHPStan/Psalm helper libraries, and
any userland function using `@template`.

#### Implementation plan

1. **Add fields to `FunctionInfo`** (in `types.rs`):

   ```text
   pub template_params: Vec<String>,
   pub template_bindings: Vec<(String, String)>,
   ```

   Mirror the existing `MethodInfo` fields.

2. **Populate during parsing** (in `parser/functions.rs`):

   Extract `@template` tags via `extract_template_params` and
   `@param`-based bindings via `extract_template_param_bindings`,
   the same functions already used for method-level templates in
   `parser/classes.rs`.

3. **Resolve at call sites** (in `variable_resolution.rs`,
   `resolve_rhs_function_call`):

   After loading the `FunctionInfo` and before falling through to
   `type_hint_to_classes`, check whether the function has
   `template_params`.  If so:

   a. For each template param, try to infer the concrete type from
      the call-site arguments using `template_bindings` (positional
      match between `$paramName` and the `ArgumentList`).  Reuse
      the existing `resolve_rhs_expression` / `type_hint_to_classes`
      to get the argument's type.

   b. Build a substitution map `{T => ConcreteType, ...}`.

   c. Apply the substitution to `return_type` via
      `apply_substitution` before passing it to
      `type_hint_to_classes`.

   This mirrors what `apply_generic_args` does for class-level
   templates, but applied to a function call.

4. **Text-based path** (in `text_resolution.rs`):

   The inline chain resolver (`resolve_raw_type_from_call_chain`)
   also needs the same treatment for chains like
   `collect($arr)->first()->`.  After resolving the function's
   return type, apply template substitution before continuing the
   chain.

**Note:** The existing `synthesize_template_conditional` path for
`@return T` + `@param class-string<T>` should be kept as-is — it
handles the special case where the return type is the template param
itself, which is common for container-style `resolve()`/`app()`
functions.  The new general path handles the remaining cases.

See also: `todo-laravel.md` gap 11 (`collect()` and other helper
functions lose generic type info), which is the Laravel-specific
manifestation of this gap.

---

### 5. Parse and resolve `($param is T ? A : B)` return types
**Impact: High · Effort: Medium**

PHPStan's stubs use conditional return type syntax in docblocks:

```php
/**
 * @return ($value is string ? true : false)
 */
function is_string(mixed $value): bool {}
```

This is the mechanism behind all `is_*` function type narrowing in
PHPStan — the stubs declare the conditional, and the type specifier
reads it.  If we parse this syntax from stubs and `@return` tags, we
get type narrowing for `is_string`, `is_int`, `is_array`,
`is_object`, `is_null`, `is_bool`, `is_float`, `is_numeric`,
`is_scalar`, `is_callable`, `is_iterable`, `is_countable`, and
`is_resource` from annotations alone, without any hard-coded function
list.

The syntax also appears in userland code (PHPStan and Psalm both
support it), and in array function stubs:

```php
/**
 * @return ($array is non-empty-array ? non-empty-list<T> : list<T>)
 */
function array_values(array $array): array {}
```

**Implementation:** Extend the return type parser in
`docblock/types.rs` to recognise the `($paramName is Type ? A : B)`
pattern.  At call sites, check the argument's type against the
condition and select the appropriate branch.  This could reuse or
extend the existing `ConditionalReturnType` infrastructure.

---

### 6. Warn when composer.json is missing or classmap is not optimized
**Impact: High · Effort: Medium**

PHPantom relies on Composer artifacts (`vendor/composer/autoload_classmap.php`,
`autoload_psr4.php`, `autoload_files.php`) for class discovery. When these
are missing or incomplete, completions silently degrade. The user should be
told what's wrong and offered help fixing it.

#### Detection (during `initialized`)

| Condition | Severity | Message |
|---|---|---|
| No `composer.json` in workspace root | Warning | "No composer.json found. Class completions will be limited to open files and stubs." |

For the no-composer.json case, offer to generate a minimal one via
`window/showMessageRequest`:

1. **"Generate composer.json"** — create a `composer.json` that maps
   the entire project root as a classmap (`"autoload": {"classmap": ["./"]}`).
   Then run `composer dump-autoload -o` to build the classmap. This
   covers legacy projects and single-directory setups that don't follow
   PSR-4 conventions.
2. **"Dismiss"** — do nothing.

| `composer.json` exists but `vendor/` directory is missing | Warning | "No vendor directory found. Run `composer install` to enable full completions." |
| PSR-4 prefixes exist but no user classes in classmap | Info | "Composer classmap does not contain your project classes. Run `composer dump-autoload -o` for full class completions." |

The third condition needs care. The classmap is rarely empty because
vendor packages like PHPUnit use `classmap` autoloading (not PSR-4), so
there will be vendor entries even without `-o`. The real signal is:
the project's `composer.json` declares PSR-4 prefixes (e.g. `App\`,
`Tests\`), but none of the classmap FQNs start with any of those
project prefixes. This means the user's own classes were not dumped
into the classmap, which is exactly what `-o` fixes.

Detection logic:
1. Collect non-vendor PSR-4 prefixes from `psr4_mappings` (already
   tagged with `is_vendor`).
2. After loading the classmap, check whether any classmap FQN starts
   with one of those prefixes.
3. If there are project PSR-4 prefixes but zero matching classmap
   entries, the autoloader is not optimized.

#### Actions (via `window/showMessageRequest`)

For the non-optimized classmap case, offer action buttons:

1. **"Run composer dump-autoload -o"** — spawn the command in the
   workspace root, reload the classmap on success, show a progress
   notification.
2. **"Add to composer.json & run"** — add
   `"config": {"optimize-autoloader": true}` to `composer.json` so
   future `composer install` / `composer update` always produce an
   optimized classmap, then run `composer dump-autoload`.
3. **"Dismiss"** — do nothing.

#### UX guidelines

- The no-composer.json and no-vendor warnings are safe to show via
  `window/showMessage` (informational, no action taken).
- The classmap warning should use `window/showMessageRequest` with
  action buttons so the user explicitly opts in before we touch files
  or run commands.
- Only show once per session. Do not re-trigger on every `didOpen`.
- Never modify `composer.json` or run commands without explicit user
  confirmation via an action button.
- If the spawned `composer` command fails (e.g. PHP not installed
  locally, Docker-only setup), catch the error gracefully and show
  "Composer command failed. You may need to run it manually."
- Log the detection result to the output panel regardless (already done
  for the "Loaded N classmap entries" message, just add context when
  zero user classes are found).

---

### 7. Find References (`textDocument/references`)
**Impact: High · Effort: Medium-High**

Can't find all usages of a symbol. The precomputed `SymbolMap` (built
during `update_ast` for every open file) already records every navigable
symbol occurrence with byte offsets and a typed `SymbolKind` — class
references, member accesses, variables, function calls, etc. This is
exactly the index a find-references implementation needs for the current
file. The main work is cross-file scanning: iterating `ast_map` entries
(and lazily parsing uncached files) to collect matching symbol spans
across the project.

The `SymbolMap` also stores variable definition sites (`var_defs`) with
scope boundaries, which directly supports "find all references to this
variable within its scope" without re-parsing.

---

<!-- ============================================================ -->
<!--  TIER 2 — MEDIUM-HIGH IMPACT                                 -->
<!-- ============================================================ -->

## Medium-High Impact

### 8. File system watching for vendor and project changes
**Impact: Medium-High · Effort: Medium**

PHPantom loads Composer artifacts (classmap, PSR-4 mappings, autoload
files) once during `initialized` and caches them for the session. If
the user runs `composer update`, `composer require`, or `composer remove`
while the editor is open, the cached data goes stale. The user gets
completions and go-to-definition based on the old package versions
until they restart the editor.

#### What to watch

| Path | Trigger | Action |
|---|---|---|
| `vendor/composer/autoload_classmap.php` | Changed | Reload classmap |
| `vendor/composer/autoload_psr4.php` | Changed | Reload PSR-4 mappings |
| `vendor/composer/autoload_files.php` | Changed | Re-scan autoload files for global functions/constants |
| `composer.json` | Changed | Reload project PSR-4 prefixes, re-check vendor dir |
| `composer.lock` | Changed | Good secondary signal that packages changed |

All three `autoload_*.php` files are rewritten atomically by Composer
on every `install`, `update`, `require`, `remove`, and `dump-autoload`.
Watching these is sufficient to catch any package change.

#### Implementation options

1. **LSP `workspace/didChangeWatchedFiles`** — register file watchers
   via `client/registerCapability` during `initialized`. The editor
   handles the OS-level watching and sends notifications. This is the
   cleanest approach and works cross-platform. Register glob patterns
   for the vendor Composer files and `composer.json`.

2. **Server-side `notify` crate** — use the `notify` Rust crate to
   watch the file system directly. More control but adds a dependency
   and duplicates what the editor already provides.

Option 1 is preferred. The LSP spec's `DidChangeWatchedFilesRegistrationOptions`
supports glob patterns like `**/vendor/composer/autoload_*.php`.

#### Reload strategy

- On change notification, re-run the same parsing logic from
  `initialized` for the affected artifact.
- Invalidate `class_index` entries that came from vendor files (their
  parsed AST may have changed).
- Clear and re-populate `classmap` from the new `autoload_classmap.php`.
- Log the reload to the output panel so the user knows it happened.
- Debounce rapid changes (Composer writes multiple files in sequence)
  with a short delay (e.g. 500ms) to avoid redundant reloads.

---

<!-- ============================================================ -->
<!--  TIER 3 — MEDIUM IMPACT                                      -->
<!-- ============================================================ -->

## Medium Impact

### 9. No reverse jump: implementation → interface method declaration
**Impact: Medium · Effort: Low**

Go-to-implementation lets you jump from an interface method to its concrete
implementations, but there is no way to jump from a concrete implementation
*back* to the interface or abstract method it satisfies.  For example,
clicking `handle()` in a class that `implements Handler` cannot jump to
`Handler::handle()`.

This would be a natural extension of `find_declaring_class` in
`definition/member.rs`: when the cursor is on a method *definition* (not
a call), check whether any implemented interface or parent abstract class
declares a method with the same name, and offer that as a definition
target.

---

### 10. `BackedEnum::from()` / `::tryFrom()` return type refinement
**Impact: Medium · Effort: Low**

When calling `MyEnum::from($value)` or `MyEnum::tryFrom($value)`,
PHPStan resolves the return type to `MyEnum` (or `MyEnum|null` for
`tryFrom`) rather than the generic `BackedEnum` base type.  This is a
static method return type that depends on the calling class — the
same pattern as `static` return types on static methods.

We already handle `new static()` and `static` return types for
instance methods, but static method calls on enums may not flow
through the same path.  Verify and fix if needed.

See `BackedEnumFromMethodDynamicReturnTypeExtension` in PHPStan.

---

### 11. Document Symbols (`textDocument/documentSymbol`)
**Impact: Medium · Effort: Low**

No outline view. Editors can't show a file's class/method/property structure.

---

### 12. Workspace Symbols (`workspace/symbol`)
**Impact: Medium · Effort: Low-Medium**

Can't search for classes/functions across the project. The `ast_map`
already contains `ClassInfo` records (with `keyword_offset`) and
`global_functions` contains `FunctionInfo` records (with `name_offset`)
for every parsed file. A workspace symbol handler would iterate these
maps, filter by the query string, and convert stored byte offsets to
LSP `Location`s.

---

### 13. No go-to-definition for built-in (stub) functions and constants
**Impact: Medium · Effort: Medium**

Clicking on a built-in function name like `array_map`, `strlen`, or
`json_decode` does not navigate anywhere. `resolve_function_definition`
finds the function in `stub_function_index` and caches it under a
synthetic `phpantom-stub-fn://` URI, but then explicitly skips navigation
because the URI is not a real file path. The same applies to built-in
constants like `PHP_EOL`, `SORT_ASC`, `PHP_INT_MAX` — they exist in
`stub_constant_index` for completion but `resolve_constant_definition`
only checks `global_defines`.

User-defined functions and `define()` constants work correctly. Only
built-in PHP symbols from stubs are affected.

**Fix:** either embed the stub source files as navigable resources (e.g.
write them to a temporary directory and use real file URIs), or accept
that stub go-to-definition is out of scope and document it as a known
limitation.

---

### 14. Property hooks (PHP 8.4)
**Impact: Medium · Effort: Medium**

PHP 8.4 introduced property hooks (`get` / `set`):

```php
class User {
    public string $name {
        get => strtoupper($this->name);
        set => trim($value);
    }
}
```

The mago parser (v1.8) already produces `Property::Hooked` and
`PropertyHook` AST nodes, and the generic `.modifiers()`, `.hint()`,
`.variables()` methods mean hooked properties are extracted for basic
completion. However:

- **Hook bodies are never walked.** Variables and anonymous classes
  inside `get`/`set` bodies are invisible to resolution.
- **`$value` parameter** inside `set` hooks is not offered for
  variable completion.
- **Asymmetric visibility** (`public private(set) string $name`) is
  not recognised — the `set` visibility is ignored, so filtering
  may incorrectly allow setting a property that should be
  write-restricted.

**Fix:** In `extract_class_like_members`, match `Property::Hooked`
explicitly, walk hook bodies for anonymous classes and variable
scopes, and parse the set-visibility modifier into a new
`set_visibility` field on `PropertyInfo`.

---

### 15. Narrow types of `&$var` parameters after function calls
**Impact: Medium · Effort: Medium**

When a function takes a parameter by reference, the variable's type
after the call should reflect what the function writes to it.  PHPStan
has `FunctionParameterOutTypeExtension` for this.

Key examples:

| Function | Parameter | Type after call |
|---|---|---|
| `preg_match($pattern, $subject, &$matches)` | `$matches` | Typed array shape based on the regex |
| `preg_match_all($pattern, $subject, &$matches)` | `$matches` | Nested typed array based on the regex |
| `parse_str($string, &$result)` | `$result` | `array<string, string>` |
| `sscanf($string, $format, &...$vars)` | `$vars` | Types based on format specifiers |

The most impactful case is `preg_match` — PHPStan's
`RegexArrayShapeMatcher` parses the regex pattern to produce a precise
array shape for `$matches`, including named capture groups.  A simpler
first step would be to just type `$matches` as `array<int|string,
string>` when passed to `preg_match`.

**Implementation:** When resolving a variable's type after a function
call where the variable was passed by reference, look up the
function's parameter annotations for `@param-out` tags (PHPStan/Psalm
extension) or use a built-in map for known functions.

---

### 16. SPL iterator generic stubs
**Impact: Medium · Effort: Medium**

PHPStan's `iterable.stub` provides full `@template TKey` /
`@template TValue` annotations for the entire SPL iterator hierarchy:
`ArrayIterator`, `FilterIterator`, `LimitIterator`,
`CachingIterator`, `RegexIterator`, `NoRewindIterator`,
`InfiniteIterator`, `AppendIterator`, `IteratorIterator`,
`RecursiveIteratorIterator`, `CallbackFilterIterator`, and more.

This means `foreach` over any SPL iterator properly resolves element
types.  If we rely on php-stubs for these classes, we are almost
certainly missing these generic annotations.  We should either:

- Ship our own stub overlays for the SPL iterator classes with
  `@template` annotations (like PHPStan does), or
- Detect and use PHPStan's stubs when present in the project's
  vendor directory.

---

### 17. Partial result streaming via `$/progress`
**Impact: Medium · Effort: Medium-High**

The LSP spec (3.17) allows requests that return arrays — such as
`textDocument/implementation`, `textDocument/references`,
`workspace/symbol`, and even `textDocument/completion` — to stream
incremental batches of results via `$/progress` notifications when both
sides negotiate a `partialResultToken`.  The final RPC response then
carries `null` (all items were already sent through progress).

This would let PHPantom deliver the *first* useful results almost
instantly instead of blocking until every source has been scanned.

#### Streaming between existing phases

`find_implementors` already runs five sequential phases (see
`docs/ARCHITECTURE.md` § Go-to-Implementation):

1. **Phase 1 — ast_map** (already-parsed classes in memory) — essentially
   free.  Flush results immediately.
2. **Phase 2 — class_index** (FQN → URI entries not yet in ast_map) —
   loads individual files.  Flush after each batch.
3. **Phase 3 — classmap files** (Composer classmap, user + vendor mixed)
   — iterates unique file paths, applies string pre-filter, parses
   matches.  This is the widest phase and the best candidate for
   within-phase streaming (see below).
4. **Phase 4 — embedded stubs** (string pre-filter → lazy parse) — flush
   after stubs are checked.
5. **Phase 5 — PSR-4 directory walk** (user code only, catches files not
   in the classmap) — disk I/O + parse per file, good candidate for
   per-file streaming.

Each phase boundary is a natural point to flush a `$/progress` batch,
so the editor starts populating the results list while heavier phases
are still running.

#### Prioritising user code within Phase 3

Phase 3 iterates the Composer classmap, which contains both user and
vendor entries.  Currently they are processed in arbitrary order.  A
simple optimisation: partition classmap file paths into user paths
(under PSR-4 roots from `composer.json` `autoload` / `autoload-dev`)
and vendor paths (everything else, typically under `vendor/`), then
process user paths first.  This way the results most relevant to the
developer arrive before vendor matches, even within a single phase.

#### Granularity options

- **Per-phase batches** (simplest) — one `$/progress` notification at
  each of the five phase boundaries listed above.
- **Per-file streaming** — within Phases 3 and 5, emit results as each
  file is parsed from disk instead of waiting for the entire phase to
  finish.  Phase 3 can iterate hundreds of classmap files and Phase 5
  recursively walks PSR-4 directories, so per-file flushing would
  significantly improve perceived latency for large projects.
- **Adaptive batching** — collect results for a short window (e.g. 50 ms)
  then flush, balancing notification overhead against latency.

#### Applicable requests

| Request | Benefit |
|---|---|
| `textDocument/implementation` | Already scans five phases; each phase's matches can be streamed |
| `textDocument/references` (§8) | Will need full-project scanning; streaming is essential |
| `workspace/symbol` (§13) | Searches every known class/function; early batches feel instant |
| `textDocument/completion` | Less critical (usually fast), but long chains through vendor code could benefit |

#### Implementation sketch

1. Check whether the client sent a `partialResultToken` in the request
   params.
2. If yes, create a `$/progress` sender.  After each scan phase (or
   per-file, depending on granularity), send a
   `ProgressParams { token, value: [items...] }` notification.
3. Return `null` as the final response.
4. If no token was provided, fall back to the current behaviour: collect
   everything, return once.

---

### 18. Rename (`textDocument/rename`)
**Impact: Medium · Effort: Medium-High**

No rename refactoring support. Rename builds on find-references (§8) —
once all occurrences of a symbol are known, the rename handler produces
a `WorkspaceEdit` replacing each occurrence. The `SymbolMap`'s byte
ranges translate directly to LSP `Range`s via `offset_to_position`,
which makes generating the text edits straightforward.

For member renames, the stored `name_offset` on `MethodInfo`,
`PropertyInfo`, and `ConstantInfo` provides the declaration-site edit
position without text scanning.

---

### 19. Array functions needing new code paths
**Impact: Medium · Effort: High**

These functions have return type semantics that don't fit into either
`ARRAY_PRESERVING_FUNCS` (same array type out) or `ARRAY_ELEMENT_FUNCS`
(single element out).  Each needs its own mini-resolver.

| Function | Return type logic | PHPStan extension |
|---|---|---|
| `array_keys` | Returns `list<TKey>` — extracts the *key* type, not value type | `ArrayKeysFunctionDynamicReturnTypeExtension` |
| `array_column` | Extracts a column from a 2D array, preserving types | `ArrayColumnFunctionReturnTypeExtension` |
| `array_combine` | Keys from first array arg, values from second | `ArrayCombineFunctionReturnTypeExtension` |
| `array_fill` | `array<int, TValue>` preserving the fill value type | `ArrayFillFunctionReturnTypeExtension` |
| `array_fill_keys` | Preserves key array type + value type | `ArrayFillKeysFunctionReturnTypeExtension` |
| `array_flip` | Swaps key↔value types | `ArrayFlipFunctionReturnTypeExtension` |
| `array_pad` | Union of existing value type + pad value type | `ArrayPadDynamicReturnTypeExtension` |
| `array_replace` | Merge-like, preserving types from all args | `ArrayReplaceFunctionReturnTypeExtension` |
| `array_change_key_case` | Preserves value type, transforms key type | `ArrayChangeKeyCaseFunctionReturnTypeExtension` |
| `array_intersect_key` | Preserves first array's types (dedicated extension) | `ArrayIntersectKeyFunctionReturnTypeExtension` |
| `array_reduce` | Returns the callback's return type (like `array_map`) | `ArrayReduceFunctionReturnTypeExtension` |
| `array_search` | Returns key type of the haystack array | `ArraySearchFunctionDynamicReturnTypeExtension` |
| `array_rand` | Returns key type of the input array | `ArrayRandFunctionReturnTypeExtension` |
| `array_sum` | Computes numeric return type from value types | `ArraySumFunctionDynamicReturnTypeExtension` |
| `array_count_values` | Returns `array<TValue, int>` | `ArrayCountValuesDynamicReturnTypeExtension` |
| `array_key_first` / `array_key_last` | Returns key type (usually scalar, low completion value) | `ArrayFirstLastDynamicReturnTypeExtension` |
| `array_find_key` | Returns key type (PHP 8.4) | `ArrayFindKeyFunctionReturnTypeExtension` |
| `iterator_to_array` | Preserves iterable key/value types into array | `IteratorToArrayFunctionReturnTypeExtension` |
| `compact` | Builds typed array from variable names | `CompactFunctionReturnTypeExtension` |
| `count` / `sizeof` | Returns precise int range based on array size | `CountFunctionReturnTypeExtension` |
| `min` / `max` | Returns union of argument types | `MinMaxFunctionReturnTypeExtension` |

---

<!-- ============================================================ -->
<!--  TIER 4 — LOW-MEDIUM IMPACT                                  -->
<!-- ============================================================ -->

## Low-Medium Impact

### 20. Asymmetric visibility (PHP 8.4)
**Impact: Low-Medium · Effort: Low**

Separate from property hooks, PHP 8.4 allows asymmetric visibility on
plain and promoted properties. PHP 8.5 extended this to static
properties as well.

```php
class Settings {
    public private(set) string $name;

    public function __construct(
        public protected(set) int $retries = 3,
    ) {}
}
```

PHPantom currently extracts a single `Visibility` per property.
Completion filtering uses this to decide whether a property should
appear. A `public private(set)` property should appear for reading
from outside the class but not for assignment contexts.

**Fix:** Add an optional `set_visibility: Option<Visibility>` to
`PropertyInfo`. Populate it from the AST modifier list (the parser
exposes the set-visibility keyword). Completion filtering does not
currently distinguish read vs write contexts, so the immediate fix
is just to store the value; context-aware filtering can follow later.

---

### 21. `str_contains` / `str_starts_with` / `str_ends_with` → non-empty-string narrowing
**Impact: Low-Medium · Effort: Low**

When `str_contains($haystack, $needle)` appears in a condition and
`$needle` is known to be a non-empty string, PHPStan narrows
`$haystack` to `non-empty-string`.  The same applies to
`str_starts_with`, `str_ends_with`, `strpos`, `strrpos`, `stripos`,
`strripos`, `strstr`, and the `mb_*` equivalents.

This is lower priority for an LSP because `non-empty-string` does
not directly enable class-based completion, but it would improve
hover type display and catch bugs if we ever add diagnostics.

See `StrContainingTypeSpecifyingExtension` in PHPStan.

---

### 22. `count` / `sizeof` comparison → non-empty-array narrowing
**Impact: Low-Medium · Effort: Low**

`if (count($arr) > 0)` or `if (count($arr) >= 1)` narrows `$arr` to
a non-empty-array.  PHPStan handles a full matrix of comparison
operators and integer range types against `count()` / `sizeof()` calls.

See `CountFunctionTypeSpecifyingExtension` and the count-related
branches in `TypeSpecifier::specifyTypesInCondition`.

---

<!-- ============================================================ -->
<!--  TIER 5 — LOW IMPACT                                         -->
<!-- ============================================================ -->

## Low Impact

### 23. Short-name collisions in `find_implementors`
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

### 24. Fiber type resolution
**Impact: Low · Effort: Low**

`Generator<TKey, TValue, TSend, TReturn>` has dedicated support for
extracting each type parameter (value type for foreach, send type
for `$var = yield`, return type for `getReturn()`). `Fiber` has no
equivalent handling — `Fiber::start()`, `Fiber::resume()`, and
`Fiber::getReturn()` don't resolve their generic types.

PHP userland rarely annotates Fiber with generics (unlike Generator),
so this is low priority. If demand appears, the fix would mirror the
Generator extraction in `docblock/types.rs`.

---

### 25. Non-empty-string propagation through string functions
**Impact: Low · Effort: Low**

PHPStan tracks `non-empty-string` through string-manipulating
functions.  If you pass a `non-empty-string` to `addslashes()`,
`urlencode()`, `htmlspecialchars()`, `escapeshellarg()`,
`escapeshellcmd()`, `preg_quote()`, `rawurlencode()`, or
`rawurldecode()`, the return type is also `non-empty-string`.

This is low priority for an LSP because the narrower string type
does not directly enable class-based completion.  However, if we
ever add hover type display or diagnostics, this information
would improve accuracy.

See `NonEmptyStringFunctionsReturnTypeExtension` in PHPStan.

---

### 26. `Closure::bind()` / `Closure::fromCallable()` return type preservation
**Impact: Low · Effort: Low-Medium**

Variables holding closure literals, arrow functions, and first-class
callables now resolve to the `Closure` class, so `$fn->bindTo()`,
`$fn->call()`, etc. offer completions.  The remaining gap is
*preserving the closure's callable signature* through `Closure::bind()`
and resolving `Closure::fromCallable('functionName')` to the actual
function's signature as a typed `Closure`.  This is relevant for DI
containers and middleware patterns but is a niche use case.

See `ClosureBindDynamicReturnTypeExtension` and
`ClosureFromCallableDynamicReturnTypeExtension` in PHPStan.

---

### 27. Remove deprecated text-search fallbacks
**Impact: Low · Effort: Medium**

The go-to-definition subsystem now uses the precomputed `SymbolMap` as
its primary path and stored byte offsets (`name_offset`, `keyword_offset`)
for cross-file jumps. The original line-by-line text scanners are marked
`#[deprecated]` and retained only as fallbacks for:

- Stubs and synthetic members where `name_offset == 0`
- Files where the parser panicked and no symbol map exists
- The go-to-implementation subsystem (not yet migrated)

Once the AST-based paths have been stable for a release cycle, these
deprecated functions can be removed:

| Function | File | Replacement |
|---|---|---|
| `find_definition_position` | `definition/resolve.rs` | `ClassInfo::keyword_offset` + `offset_to_position` |
| `find_function_position` | `definition/resolve.rs` | `FunctionInfo::name_offset` + `offset_to_position` |
| `find_define_position` | `definition/resolve.rs` | Store `define()` offset during parsing |
| `extract_word_at_position` | `definition/resolve.rs` | `SymbolMap::lookup` |
| `resolve_variable_definition_text` | `definition/variable.rs` | `SymbolMap::find_var_definition` + AST walk |
| `line_defines_variable` | `definition/variable.rs` | (only used by `resolve_variable_definition_text`) |
| `find_member_position_in_range` text path | `definition/member.rs` | `name_offset` + `offset_to_position` |

The go-to-implementation subsystem (`resolve_implementation` in
`definition/implementation.rs`) still uses `extract_word_at_position`
for cursor context detection. Migrating it to use `SymbolMap::lookup`
would let that deprecated function be removed entirely.

---

### 28. Non-array functions with dynamic return types
**Impact: Low · Effort: High**

PHPStan also provides dynamic return type extensions for many non-array
functions.  These are lower priority because they mostly refine scalar
return types (less impactful for class-based completion).

| Function | Return type logic | PHPStan extension |
|---|---|---|
| `abs` | Preserves int/float return type | `AbsFunctionDynamicReturnTypeExtension` |
| `base64_decode` | `string\|false` based on strict param | `Base64DecodeDynamicFunctionReturnTypeExtension` |
| `explode` | `list<string>` / `non-empty-list<string>` / `false` | `ExplodeFunctionDynamicReturnTypeExtension` |
| `filter_var` | Return type depends on filter constant | `FilterVarDynamicReturnTypeExtension` |
| `filter_input` | Same as `filter_var` | `FilterInputDynamicReturnTypeExtension` |
| `filter_var_array` / `filter_input_array` | Typed array based on filter definitions | `FilterVarArrayDynamicReturnTypeExtension` |
| `get_class` | Returns `class-string<T>` | `GetClassDynamicReturnTypeExtension` |
| `get_called_class` | Returns `class-string<static>` | `GetCalledClassDynamicReturnTypeExtension` |
| `get_parent_class` | Returns parent class-string | `GetParentClassDynamicFunctionReturnTypeExtension` |
| `gettype` | Returns specific string literal for known types | `GettypeFunctionReturnTypeExtension` |
| `get_debug_type` | Returns specific string literal | `GetDebugTypeFunctionReturnTypeExtension` |
| `constant` | Resolves named constant to its type | `ConstantFunctionReturnTypeExtension` |
| `date` / `date_format` | Precise string return types | `DateFunctionReturnTypeExtension` |
| `date_create` / `date_create_immutable` | `DateTime\|false` | `DateTimeCreateDynamicReturnTypeExtension` |
| `hash` / `hash_file` / etc. | Precise return types | `HashFunctionsReturnTypeExtension` |
| `sprintf` / `vsprintf` | Non-empty-string preservation | `SprintfFunctionDynamicReturnTypeExtension` |
| `preg_split` | `list<string>\|false` based on flags | `PregSplitDynamicReturnTypeExtension` |
| `str_split` / `mb_str_split` | Non-empty-list | `StrSplitFunctionReturnTypeExtension` |
| `class_implements` / `class_uses` / `class_parents` | `array<string, string>\|false` | `ClassImplementsFunctionReturnTypeExtension` |

---

### 29. Language construct signature help and hover
**Impact: Low · Effort: Low**

PHP language constructs that use parentheses (`unset()`, `isset()`, `empty()`,
`eval()`, `exit()`, `die()`, `print()`, `list()`) are not function calls in the
AST. Mago parses them as dedicated statement/expression nodes (e.g.
`Statement::Unset`) with no `ArgumentList`, so no `CallSite` is emitted and
neither signature help nor hover fires inside their parentheses. The phpstorm-stubs
don't define them either since they are keywords, not functions.

Supporting them requires emitting synthetic `CallSite` entries from the
statement-level extraction in `symbol_map.rs` and adding hardcoded parameter
metadata (e.g. `unset(mixed ...$vars): void`) in `resolve_callable`. Hover would
need a similar hardcoded lookup.

---

### 30. Diagnostics
**Impact: Low (large scope) · Effort: Very High**

No error reporting (undefined methods, type mismatches, etc.).

---

### 31. Code Actions
**Impact: Low · Effort: Very High**

No quick fixes or refactoring suggestions. No `codeActionProvider` in
`ServerCapabilities`, no `textDocument/codeAction` handler, and no
`WorkspaceEdit` generation infrastructure beyond trivial `TextEdit`s for
use-statement insertion.

#### 30a. Extract Function refactoring

Select a range of statements inside a method/function and extract them into a
new function. The LSP would need to:

1. **Scope analysis** — determine which variables are read in the selection but
   defined before it (→ parameters) and which are written in the selection but
   read after it (→ return values).
2. **Statement boundary validation** — reject selections that split an
   expression or cross control-flow boundaries in invalid ways.
3. **Type annotation** — use variable type resolution to generate parameter and
   return type hints on the new function.
4. **Code generation** — produce a `WorkspaceEdit` that replaces the selection
   with a call and inserts the new function definition nearby.

**Prerequisites (build these first):**

| Feature | What it contributes |
|---|---|
| Hover (§1) | "Resolve type at arbitrary position" — needed to type params |
| Document Symbols (§12) | AST range → symbol mapping — needed to find enclosing function and valid insertion points |
| Find References (§8) | Variable usage tracking across a scope — the same "which variables are used where" analysis |
| Simple code actions (add use stmt, implement interface) | Builds the code action + `WorkspaceEdit` plumbing |

---

<!-- ============================================================ -->
<!--  SUMMARY TABLE                                                -->
<!-- ============================================================ -->

## Summary

| # | Item | Impact | Effort |
|---|---|---|---|
| 1 | Named-arg completion: symbol-map migration | Critical | Low |
| 2 | Completion member access: symbol-map primary path | Critical | Medium |
| 3 | Pipe operator (PHP 8.5) | High | Low |
| 4 | Function-level `@template` generic resolution | High | Medium |
| 5 | Conditional return type syntax | High | Medium |
| 6 | Composer environment warnings | High | Medium |
| 7 | Find References | High | Medium-High |
| 8 | File system watching | Medium-High | Medium |
| 9 | Reverse jump: impl → interface | Medium | Low |
| 10 | `BackedEnum::from()` refinement | Medium | Low |
| 11 | Document Symbols | Medium | Low |
| 12 | Workspace Symbols | Medium | Low-Medium |
| 13 | Built-in stub go-to-definition | Medium | Medium |
| 14 | Property hooks (PHP 8.4) | Medium | Medium |
| 15 | Parameter out types (by-reference) | Medium | Medium |
| 16 | SPL iterator generic stubs | Medium | Medium |
| 17 | Partial result streaming | Medium | Medium-High |
| 18 | Rename | Medium | Medium-High |
| 19 | Array functions (new code paths) | Medium | High |
| 20 | Asymmetric visibility (PHP 8.4) | Low-Medium | Low |
| 21 | `str_contains` / `str_starts_with` narrowing | Low-Medium | Low |
| 22 | `count` / `sizeof` → non-empty-array | Low-Medium | Low |
| 23 | Short-name collisions | Low | Low |
| 24 | Fiber type resolution | Low | Low |
| 25 | Non-empty-string propagation | Low | Low |
| 26 | `Closure::bind()` preservation | Low | Low-Medium |
| 27 | Remove deprecated text-search fallbacks | Low | Medium |
| 28 | Non-array dynamic return types | Low | High |
| 29 | Language construct signature help / hover | Low | Low |
| 30 | Diagnostics | Low | Very High |
| 31 | Code Actions / Extract Function | Low | Very High |