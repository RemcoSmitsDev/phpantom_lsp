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

| Crate              | Replaces                                               | Effort      |
| ------------------ | ------------------------------------------------------ | ----------- |
| `mago-docblock`    | Manual docblock parsing scattered across the codebase   | Medium-High |
| `mago-type-syntax` | `src/docblock/{type_strings,generics,shapes,callable_types,conditional}.rs` + string-based type pipeline | Very High |
| `mago-names`       | `src/parser/use_statements.rs` + `use_map` resolution   | Medium-High |

`mago-docblock` is already a dependency — the core `src/docblock/`
module uses it for all tag extraction. The remaining work (M2) is
migrating the **rest** of the codebase that still does hand-rolled
docblock parsing.

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

## M2. Deeper `mago-docblock` integration

**What it replaces:** Manual `/** */` stripping, `*`-prefix scanning,
and `@tag`-matching code scattered across modules that have not yet
adopted the centralised `DocblockInfo` / `parse_docblock_for_tags`
infrastructure in `src/docblock/parser.rs`.

**Why it matters:** The core `src/docblock/` module is fully migrated,
and the majority of hand-rolled docblock parsers across the project
have been replaced with structured `DocblockInfo` access. The remaining
items are either high-effort (`symbol_map/docblock.rs`) or low-impact
(position-finding code in `file_lookup.rs`).

**Relationship to M3/M4:** Independent. Can be done before, after, or
in parallel with either. Items are ordered by impact.

### Remaining items

1. **Migrate `symbol_map/docblock.rs` (~1,360 lines).**
   Impact: **High**.  Effort: **High**.

   The single largest manual docblock parser in the project. It
   reimplements line-by-line tag scanning for ~26 tag names,
   multi-line type joining with offset maps, tag position validation,
   and a 423-line recursive type string decomposer (`emit_type_spans`)
   — all to produce `SymbolSpan` entries with file-level byte offsets
   for go-to-definition and semantic tokens within docblocks.

   `TagInfo` now has `span` and `description_span` fields (added as
   part of the prerequisites). This module could parse docblocks with
   `parse_docblock()` (which already calls
   `mago_docblock::parse_phpdoc_with_span`) and iterate the
   structured `TagInfo` entries instead of scanning raw lines.
   The `emit_type_spans` function would remain largely unchanged
   (it parses type *strings*, which is an M4 concern), but the
   ~400 lines of tag scanning, `is_tag_position`, and
   `join_multiline_type` would be eliminated.

2. **Consolidate docblock locators.**
   Impact: **Medium**.  Effort: **Medium**.

   There are **5 separate implementations** of "find the docblock
   for this function/node":

   1. `get_docblock_text_for_node` (`docblock/tags.rs`) — trivia-based, canonical
   2. `find_docblock_start_for_node` (`code_actions/update_docblock.rs`) — duplicate of #1
   3. `find_enclosing_docblock` (`code_actions/phpstan/add_throws.rs`) — raw-text backward scan
   4. `find_docblock_above_line` (`code_actions/phpstan/remove_throws.rs`) — raw-text backward scan
   5. `find_method_docblock` (`hover/variable_type.rs`) — raw-text backward scan

   Variants #3–5 exist because their callers start from a byte
   offset or diagnostic line rather than an AST node. A shared
   utility that can work from either starting point would eliminate
   this duplication.

3. **Migrate `definition/member/file_lookup.rs` — `@property`/`@method` position lookup.**
   Impact: **Low-Medium**.  Effort: **Low**.

   Manual `trim_start_matches('*')` + `starts_with("@property-read")`
   for finding member definition positions in docblocks. The line-scanning
   serves a dual purpose (finding positions AND verifying tag type), so
   the benefit from structured parsing is limited.

4. **Migrate `code_actions/update_docblock.rs` — `parse_docblock_lines` and rebuild.**
   Impact: **Low-Medium**.  Effort: **Medium**.

   The `parse_doc_params`, `parse_doc_return`, and `parse_doc_throws`
   functions have been migrated to `_from_info` variants. The remaining
   hand-rolled code is `parse_docblock_lines` (~80 lines, categorises
   lines into a `DocLine` enum for the rebuild pipeline) and
   `find_docblock_start_for_node` (duplicate of the canonical
   `get_docblock_text_for_node`). The rebuild pipeline operates on
   raw text lines for surgical editing, so it benefits less from
   structured parsing.

5. **Migrate `code_actions/phpstan/remove_throws.rs` — `build_remove_throws_edit`.**
   Impact: **Low-Medium**.  Effort: **Low**.

   The `@throws` tag *detection* in `add_throws.rs` has been migrated.
   The remaining code in `remove_throws.rs` does line-level text editing
   (finding which raw line to delete, handling blank-line cleanup) which
   inherently needs to work with raw text.

### Not applicable

- **Source-scanning functions** in `docblock/tags.rs`
  (`find_var_raw_type_in_source`, `find_iterable_raw_type_in_source`)
  — complexity is in scope-aware backward scanning, not content
  parsing. Two already delegate to mago-docblock; the other two do
  minimal content parsing.

- **Cursor-position modules** (`completion/source/comment_position.rs`,
  `completion/phpdoc/context.rs`) — classify cursor position relative
  to comment boundaries; handle incomplete input during typing.

- **Consumer-only modules** (`signature_help.rs`, `document_symbols.rs`,
  `inlay_hints.rs`, `folding.rs`, `document_links.rs`,
  `diagnostics/deprecated.rs`) — read pre-extracted data from
  `ClassInfo`, `MethodInfo`, `ParameterInfo` etc. No direct docblock
  parsing.

### Effort estimate

| Item | Lines eliminated (approx.) | Effort |
| ---- | ------------------------- | ------ |
| 1. `symbol_map/docblock.rs` | ~800 | High |
| 2. Consolidate locators | ~150 | Medium |
| 3. `file_lookup.rs` | ~20 | Low |
| 4. `update_docblock.rs` rebuild | ~160 | Medium |
| 5. `remove_throws.rs` edit | ~30 | Low |
| **Total remaining** | **~1,160** | **Medium-High** |

### Implementation notes

- `collapse_newlines` in `parser.rs` normalises multi-line tag
  descriptions (`\n` + surrounding indentation → single space),
  skipping insertion next to structural delimiters `<>{}()`.
- `@phpstan-extends`, `@phpstan-implements`, `@phpstan-use`, and
  `@removed` are `TagKind::Other`; matched by `tag.name` fallback.
- The `_from_info` API eliminates redundant parsing by sharing a
  single `DocblockInfo` across multiple extractions from the same
  docblock.
- `parse_docblock_for_tags_lossy` appends `*/` and strips trailing
  bare `@` to handle partial docblocks during completion. Used by
  `completion/phpdoc/mod.rs` for existing-tag detection.

---

## M3. Migrate to `mago-names`

**What it replaces:** `src/parser/use_statements.rs` (130 lines),
the `use_map: DashMap<String, HashMap<String, String>>` on `Backend`,
and the lazy name-resolution helpers in `src/resolution.rs` that
manually look up the use map.

**Why:** `mago-names` resolves every identifier in a PHP file to its
fully-qualified name in a single pass. This is more correct than our
lazy approach for edge cases: names that resolve differently depending
on whether they appear in a type hint vs. a `new` expression vs. a
function call (PHP's different name resolution rules for classes,
functions, and constants). It also provides `is_imported()` which
tells us whether a name came from a `use` statement — useful for
auto-import code actions and unused-import diagnostics.

**What it does NOT replace:** Cross-file resolution
(`find_or_load_class`, PSR-4 resolution, classmap lookup, stub
loading). Those stay in `src/resolution.rs`. `mago-names` handles
only the within-file syntactic resolution (use statements + namespace
context → FQN).

**Risk:** Medium-high. The `use_map` is read from many places. The
arena lifetime for `ResolvedNames` must outlive the consumers.
Requires restructuring how we store per-file name resolution data.

### Steps

1. **Add `mago-names` to `Cargo.toml`.**
   This also brings in `foldhash` as a transitive dependency.

2. **Run the name resolver in `update_ast_inner`.**
   After parsing the `Program`, call
   `mago_names::resolver::NameResolver::new(&arena).resolve(program)`
   to produce a `ResolvedNames`. This happens in the same arena as
   the parse.

3. **Store resolved names per file.**
   `ResolvedNames<'arena>` borrows from the arena, but our arenas
   are dropped at the end of `update_ast_inner`. Two options:

   **Option A — Copy to owned storage.** Extract the resolved names
   into an owned `HashMap<u32, (String, bool)>` (offset → FQN +
   imported flag) and store that on `Backend` in a new
   `DashMap<String, Arc<OwnedResolvedNames>>`. This is the simpler
   approach and keeps the existing lifetime model.

   **Option B — Keep arenas alive.** Store the `Bump` arena
   alongside the `ResolvedNames` in an `Arc`-wrapped struct. This
   avoids the copy but requires more careful lifetime management.

   Start with Option A. It's simpler to reason about and the copy
   cost is bounded (one `HashMap` insert per identifier per file,
   done once per re-parse). Optimise to Option B later if profiling
   shows it matters.

4. **Build an `OwnedResolvedNames` wrapper.**
   Create a `src/names.rs` module with a struct that mirrors the
   `ResolvedNames` API but owns its data:

   ```
   pub struct OwnedResolvedNames {
       names: HashMap<u32, (String, bool)>,
   }

   impl OwnedResolvedNames {
       pub fn get(&self, offset: u32) -> Option<&str>;
       pub fn is_imported(&self, offset: u32) -> bool;
   }
   ```

   Populate it from `ResolvedNames` at the end of `update_ast_inner`.

5. **Replace `use_map` reads incrementally.**
   The `use_map` is read in:
   - `src/resolution.rs` — `resolve_class_name`, `resolve_function_name`
   - `src/diagnostics/unknown_classes.rs`
   - `src/diagnostics/unknown_functions.rs`
   - `src/diagnostics/unknown_members.rs`
   - `src/diagnostics/unused_imports.rs`
   - `src/completion/` (various modules)
   - `src/definition/` (various modules)
   - `src/references/`
   - `src/rename/`
   - `src/code_actions/import_class.rs`

   For each call site:
   - If the call site has access to the AST node's byte offset, use
     `resolved_names.get(offset)` to get the FQN directly. This
     eliminates the manual "look up short name in use_map, prepend
     namespace" dance.
   - If the call site only has a string name (no offset), keep the
     existing `resolve_class_name` / `resolve_function_name` helper
     but rewrite it to query `OwnedResolvedNames` instead of the raw
     use map.

   Do this incrementally — one module per commit.

6. **Deprecate and remove `use_map`.**
   Once all consumers use `OwnedResolvedNames`, remove the
   `use_map: DashMap<String, HashMap<String, String>>` from
   `Backend`. Also remove `extract_use_items` and
   `extract_use_statements_from_statements` from
   `src/parser/use_statements.rs`.

7. **Keep `namespace_map` for now.**
   The per-file namespace is still needed for PSR-4 resolution and
   class index construction. `mago-names` doesn't expose the file's
   namespace as a standalone value, so keep `namespace_map` or extract
   the namespace from the AST directly (it's trivial — first
   `Statement::Namespace` node).

8. **Update unused-import diagnostics.**
   `mago-names` provides `is_imported()` for each resolved name. An
   unused import is a `use` statement whose imported names never
   appear in `ResolvedNames` with `imported = true`. This may
   simplify the current `unused_imports.rs` logic.

9. **Run the full test suite.**

### Interaction with M2

M2 (deeper mago-docblock integration) and M3 (mago-names) are
independent — neither depends on the other. They can be done in
parallel or in either order. M2 items 1–3 (the highest-impact ones)
are recommended before M3 since they reduce manual docblock code that
M3's name-resolution changes would otherwise need to work around.

### Interaction with M4

M4 (mago-type-syntax) does NOT depend on mago-names. Type expression
parsing is purely syntactic — it takes a string and returns a type
AST. However, once both M3 and M4 are complete, the combination
enables a powerful pattern: resolve an identifier's FQN via
mago-names, then parse its docblock type via mago-type-syntax, and
work with fully-resolved structured types throughout. This is
especially valuable for the Laravel provider, where a relationship
return type like `HasMany<Post, $this>` needs both FQN resolution
(what is `Post`?) and type structure (what are the generic args?).

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

### Phase 1: Introduce the type representation

**Goal:** Define `PhpType`, implement `PhpType::parse()` and
`PhpType::to_string()`, and prove they round-trip correctly against
the existing string pipeline. No existing code changes.

1. Add `mago-type-syntax` to `Cargo.toml`.

2. Create `src/types/php_type.rs`:
   ```
   pub enum PhpType {
       Named(String),                  // e.g. "int", "App\\User"
       Nullable(Box<PhpType>),         // ?T
       Union(Vec<PhpType>),            // T|U
       Intersection(Vec<PhpType>),     // T&U
       Generic(String, Vec<PhpType>),  // Collection<int, User>
       Array(Box<PhpType>),            // T[]
       ArrayShape(Vec<ShapeEntry>),    // array{name: string, age: int}
       ObjectShape(Vec<ShapeEntry>),   // object{name: string}
       Callable {                      // callable(T): U
           params: Vec<PhpType>,
           return_type: Option<Box<PhpType>>,
       },
       Conditional {                   // ($x is T ? U : V)
           param: String,
           condition: Box<PhpType>,
           then_type: Box<PhpType>,
           else_type: Box<PhpType>,
       },
       ClassString(Option<Box<PhpType>>),  // class-string<T>
       KeyOf(Box<PhpType>),            // key-of<T>
       ValueOf(Box<PhpType>),          // value-of<T>
       Raw(String),                    // fallback for unparseable strings
   }
   ```

3. Implement `PhpType::parse(s: &str) -> PhpType` using
   `mago_type_syntax::parse_str()` to parse into the crate's AST,
   then convert to our `PhpType`. The `Raw(String)` variant is the
   fallback for anything the parser rejects — this guarantees the
   function never fails.

4. Implement `PhpType::to_string() -> String` so we can convert back
   to the string representation for display (hover, completion
   detail, etc.).

5. Write round-trip tests: for every type string in the existing test
   suite, assert `PhpType::parse(s).to_string() == s` (or a
   canonically equivalent form).

### Phase 2: Dual representation on core types

**Goal:** Add `_parsed: Option<PhpType>` fields alongside existing
string fields on the core types. Populate them at extraction time.
No consumers change yet.

1. Add `return_type_parsed: Option<PhpType>` to `MethodInfo`.
2. Add `type_hint_parsed: Option<PhpType>` to `ParameterInfo`.
3. Add `type_hint_parsed: Option<PhpType>` to `PropertyInfo`.
4. Add `type_hint_parsed: Option<PhpType>` to `ConstantInfo`.
5. Populate these fields in `src/parser/classes.rs` and
   `src/parser/functions.rs` by calling `PhpType::parse()` on the
   existing string value.

At this point, every extracted type has both representations. Old
code keeps reading the string field; new code can read the parsed
field.

### Phase 3: Migrate consumers to structured types

**Goal:** Replace string-based type manipulation with `PhpType`
pattern matching, one module at a time. After each module is
migrated, its string-field reads are removed.

Modules in recommended migration order (least dependencies first):

1. **`src/hover/`** — Type display. Replace `split_type_token` /
   `clean_type` chains with `PhpType::to_string()` formatting.

2. **`src/completion/`** — Type matching for member access. Replace
   `base_class_name` / `extract_generic_value_type` chains with
   `PhpType::Generic` / `PhpType::Named` pattern matching.

3. **`src/resolution.rs`** — `resolve_type_string`. Replace the
   string-surgery approach (split on `|`, recurse, rejoin) with
   tree traversal on `PhpType`.

4. **`src/docblock/types.rs` and sub-modules** — The old string
   parsers. Once all consumers use `PhpType`, these become dead code
   and can be deleted.

5. **`src/symbol_map/docblock.rs`** — `emit_type_spans`. Replace the
   423-line recursive string decomposer with `PhpType` tree
   traversal + span emission. (This depends on M2 item 1 for the
   tag-level migration; the type-level migration is M4.)

6. **`src/diagnostics/`** — Type compatibility checks. Pattern match
   on `PhpType` variants instead of string prefix checks.

7. **`src/code_actions/`** — Type-aware refactorings. Use `PhpType`
   for type comparison, docblock generation, etc.

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

Each migration step (M2, M3, M4) must pass the **full existing test
suite** before merging. This is the primary safety net.

Additional testing per migration:

| Migration | Extra tests |
| --------- | ----------- |
| M2 | Integration tests for each migrated module (code actions, completion, hover, diagnostics) to verify docblock tag extraction produces identical results. |
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

Once M2 + M3 + M4 are complete:

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