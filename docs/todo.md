# PHPantom — Remaining Work

> Last updated: 2026-02-27

Items are ordered by recommended implementation sequence: quick wins
first, then high-impact items, then competitive-parity features, then
long-tail polish.

---

## Completion & Go-to-Definition Gaps

### Competitive parity (close the gap with PHPStorm / Intelephense)

#### 21. No reverse jump: implementation → interface method declaration

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

### Remaining by user need

#### 34 / 36. No go-to-definition for built-in (stub) functions and constants

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

#### 33a. Generator yield inference gaps inside generator bodies

The basic generator yield inference (item 33) is implemented, but
several edge cases do not resolve yet:

**Yield inside control flow.** When `yield $var` appears inside an `if`,
`foreach`, or other block within the generator body, the variable is not
inferred outside that block. The text-based body scan finds the yield,
but the variable resolution pipeline does not connect it back to the
outer scope.

```php
/** @return \Generator<int, User> */
public function filteredUsers(): \Generator {
    if (true) {
        yield $user;
    }
    $user->  // ← no completion
}
```

**Multiple yields with different variable names.** When two separate
variables are yielded in the same generator, neither resolves. The
scan finds both yield statements but the inference does not fire for
either variable.

```php
/** @return \Generator<int, User> */
public function allUsers(): \Generator {
    yield $first;
    $first->   // ← no completion

    yield $second;
    $second->  // ← no completion
}
```

**Chaining through a yield-inferred variable.** When a variable gets
its type from reverse yield inference, chaining a method call on it
does not resolve the next link. The yield fallback returns the type
directly from `resolve_variable_in_members` instead of feeding it
back into the normal subject resolution chain.

```php
/** @return \Generator<int, User> */
public function withProfiles(): \Generator {
    yield $user;
    $user->getProfile()->  // ← no completion after getProfile()
}
```

**TSend inference not consistent across classes.** The `$var = yield`
pattern resolves in some classes but not others with identical
structure. Likely a position-dependent issue in
`find_enclosing_return_type` where the backward scan crosses the
wrong opening brace.

---

## Go-to-Implementation Gaps

### 5b. Short-name collisions in `find_implementors`
**Priority: Low**

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

## Composer Environment Detection & Warnings

### 37. Warn when composer.json is missing or classmap is not optimized

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

### 38. File system watching for vendor and project changes

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

## Missing LSP Features

### 6. Hover (`textDocument/hover`)
**Priority: High**

No hover support at all. Users can't see inferred types, docblock descriptions,
or method signatures by hovering. Most of the infrastructure already exists
(type resolution, class loading, docblocks) — wiring it into a hover handler
would be relatively straightforward and high-impact.

---

### 7. Signature Help (`textDocument/signatureHelp`)
**Priority: Medium**

No parameter hints shown while typing function/method arguments. Named arg
completion partially fills this role, but proper signature help is more
ergonomic.

---

### 8. Document Symbols (`textDocument/documentSymbol`)
**Priority: Medium**

No outline view. Editors can't show a file's class/method/property structure.

---

### 9. Find References (`textDocument/references`)
**Priority: Medium**

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

### 10. Rename (`textDocument/rename`)
**Priority: Low**

No rename refactoring support. Rename builds on find-references (§9) —
once all occurrences of a symbol are known, the rename handler produces
a `WorkspaceEdit` replacing each occurrence. The `SymbolMap`'s byte
ranges translate directly to LSP `Range`s via `offset_to_position`,
which makes generating the text edits straightforward.

For member renames, the stored `name_offset` on `MethodInfo`,
`PropertyInfo`, and `ConstantInfo` provides the declaration-site edit
position without text scanning.

---

### 11. Workspace Symbols (`workspace/symbol`)
**Priority: Low**

Can't search for classes/functions across the project. The `ast_map`
already contains `ClassInfo` records (with `keyword_offset`) and
`global_functions` contains `FunctionInfo` records (with `name_offset`)
for every parsed file. A workspace symbol handler would iterate these
maps, filter by the query string, and convert stored byte offsets to
LSP `Location`s.

---

### 12. Diagnostics
**Priority: Low** (large scope)

No error reporting (undefined methods, type mismatches, etc.).

---

### 13. Code Actions
**Priority: Low**

No quick fixes or refactoring suggestions. No `codeActionProvider` in
`ServerCapabilities`, no `textDocument/codeAction` handler, and no
`WorkspaceEdit` generation infrastructure beyond trivial `TextEdit`s for
use-statement insertion.

#### 13a. Extract Function refactoring

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
| Hover (§6) | "Resolve type at arbitrary position" — needed to type params |
| Document Symbols (§8) | AST range → symbol mapping — needed to find enclosing function and valid insertion points |
| Find References (§9) | Variable usage tracking across a scope — the same "which variables are used where" analysis |
| Simple code actions (add use stmt, implement interface) | Builds the code action + `WorkspaceEdit` plumbing |

---

## Infrastructure Cleanup

### Remove deprecated text-search fallbacks

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

## Performance / UX Ideas

### 14. Partial result streaming via `$/progress`
**Priority: Medium** (cross-cutting optimisation)

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
| `textDocument/references` (§9) | Will need full-project scanning; streaming is essential |
| `workspace/symbol` (§11) | Searches every known class/function; early batches feel instant |
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
