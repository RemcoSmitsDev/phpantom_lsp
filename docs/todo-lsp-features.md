# PHPantom — New LSP Features

Items are ordered by **impact** (descending), then **effort** (ascending)
within the same impact tier.

| Label | Scale |
|---|---|
| **Impact** | **Critical**, **High**, **Medium-High**, **Medium**, **Low-Medium**, **Low** |
| **Effort** | **Low** (≤ 1 day), **Medium** (2-5 days), **Medium-High** (1-2 weeks), **High** (2-4 weeks), **Very High** (> 1 month) |

---

## 2. Document Highlighting (`textDocument/documentHighlight`)
**Impact: Medium-High · Effort: Low**

When the cursor lands on a symbol, highlight all other occurrences of that
symbol in the current file.  This is a cheap, high-visibility UX
improvement — users expect it from any modern language server and notice
its absence immediately.

### Existing infrastructure

The `SymbolMap` already records every navigable symbol occurrence in
a file during `update_ast`.  Each `SymbolSpan` carries a `SymbolKind`
discriminant (`ClassReference`, `ClassDeclaration`, `MemberAccess`,
`Variable`, `FunctionCall`, `SelfStaticParent`, `ConstantReference`).
The highlight handler can reuse these spans directly — no additional
parsing or AST walking is needed.

### Implementation plan

1. **Register the capability** — set `document_highlight_provider: Some(OneOf::Left(true))`
   in `ServerCapabilities` inside `server.rs`.

2. **Add a handler method** on `Backend`:
   - Look up the `SymbolMap` for the file URI from `self.symbol_maps`.
   - Call `symbol_map.lookup(offset)` to find the `SymbolSpan` under the
     cursor.
   - Based on the `SymbolKind`, determine matching criteria:
     - `Variable { name }` → match all `Variable` spans with the same
       `name` that share the same enclosing scope (use
       `symbol_map.find_enclosing_scope(offset)`).
     - `ClassReference { name, .. }` / `ClassDeclaration { name }` →
       match all `ClassReference` and `ClassDeclaration` spans whose
       resolved name is the same FQN.
     - `MemberAccess { member_name, is_static, .. }` → match all
       `MemberAccess` spans with the same `member_name` and `is_static`
       flag.  Optionally resolve the subject type to avoid false positives
       across unrelated classes, but a name-only match is acceptable for v1.
     - `FunctionCall { name }` → match all `FunctionCall` spans with the
       same name.
     - `ConstantReference { name }` → match all `ConstantReference` spans
       with the same name.
     - `SelfStaticParent { keyword }` → match all `SelfStaticParent`
       spans with the same keyword within the same class body.
   - Iterate `symbol_map.spans` and collect every span that matches.
   - Convert each matching `SymbolSpan` to a `DocumentHighlight` with
     `range` computed from the byte offsets and `kind` set to
     `DocumentHighlightKind::Read` (or `Write` for assignment LHS
     variables, detectable via `symbol_map.var_def_kind_at`).

3. **Wire the LSP method** — implement `async fn document_highlight` in the
   `LanguageServer` impl in `server.rs`, delegating to the handler above.

### Highlight kind assignment

- Variable on an assignment LHS, parameter definition, foreach binding,
  or catch binding → `DocumentHighlightKind::Write` (check
  `var_def_kind_at` for the offset).
- Everything else → `DocumentHighlightKind::Read`.

### Scope rules

- **Variables** should be scoped to their enclosing function/method/closure
  body via `find_enclosing_scope`.  A `$user` in method A must not
  highlight `$user` in method B.
- **Class names, member names, function names, constants** are file-global —
  highlight all occurrences in the file regardless of scope.

---

## 3. PHPDoc block generation on `/**`
**Impact: Medium-High · Effort: Medium**

Typing `/**` above a function, class, property, or constant should
generate a complete doc block skeleton — not just offer tag completions
(which we already do via `@`-triggered completion inside existing doc
blocks). This is a distinct feature: the trigger is `/**` outside any
doc block, and the result is a multi-line snippet with all tags
pre-filled.

### What to generate

- **Functions/methods:** `@param` tags for every parameter (with type
  from native hint or inference), `@return` with the declared/inferred
  return type, `@throws` for uncaught exception types in the body.
- **Method overrides:** insert `{@inheritDoc}` instead of repeating the
  parent's documentation, unless the override changes the signature.
- **Classes/interfaces/enums:** `@package` or a blank summary line.
- **Properties:** `@var` with the declared/inferred type.
- **Constants:** `@var` with the value's type.

### Implementation

1. Register `/**` as a completion trigger (it already fires on
   individual characters; the trigger is the `/` after `**`).
2. In the completion handler, detect that the cursor is immediately
   after `/**` with only whitespace before the next declaration.
3. Find the declaration below the cursor (function, class, property,
   constant) by scanning forward in the AST or raw text.
4. Build a `CompletionItem` with `insertTextFormat: Snippet` containing
   the full doc block with tab stops for summary and description fields.
5. For functions, resolve parameter types using the same paths as
   signature help. For `@throws`, reuse the exception-detection logic
   from existing PHPDoc `@throws` completion.

**Note:** This is different from the existing `@`-triggered PHPDoc
completion which suggests individual tags inside an already-open doc
block. This generates the entire block from scratch.

---

## 4. Document Symbols (`textDocument/documentSymbol`)
**Impact: Medium · Effort: Low**

No outline view. Editors can't show a file's class/method/property structure.

---

## 5. Workspace Symbols (`workspace/symbol`)
**Impact: Medium · Effort: Low-Medium**

Can't search for classes/functions across the project. The `ast_map`
already contains `ClassInfo` records (with `keyword_offset`) and
`global_functions` contains `FunctionInfo` records (with `name_offset`)
for every parsed file. A workspace symbol handler would iterate these
maps, filter by the query string, and convert stored byte offsets to
LSP `Location`s.

---

## 6. Partial result streaming via `$/progress`
**Impact: Medium · Effort: Medium-High**

The LSP spec (3.17) allows requests that return arrays — such as
`textDocument/implementation`, `textDocument/references`,
`workspace/symbol`, and even `textDocument/completion` — to stream
incremental batches of results via `$/progress` notifications when both
sides negotiate a `partialResultToken`.  The final RPC response then
carries `null` (all items were already sent through progress).

This would let PHPantom deliver the *first* useful results almost
instantly instead of blocking until every source has been scanned.

### Streaming between existing phases

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

### Prioritising user code within Phase 3

Phase 3 iterates the Composer classmap, which contains both user and
vendor entries.  Currently they are processed in arbitrary order.  A
simple optimisation: partition classmap file paths into user paths
(under PSR-4 roots from `composer.json` `autoload` / `autoload-dev`)
and vendor paths (everything else, typically under `vendor/`), then
process user paths first.  This way the results most relevant to the
developer arrive before vendor matches, even within a single phase.

### Granularity options

- **Per-phase batches** (simplest) — one `$/progress` notification at
  each of the five phase boundaries listed above.
- **Per-file streaming** — within Phases 3 and 5, emit results as each
  file is parsed from disk instead of waiting for the entire phase to
  finish.  Phase 3 can iterate hundreds of classmap files and Phase 5
  recursively walks PSR-4 directories, so per-file flushing would
  significantly improve perceived latency for large projects.
- **Adaptive batching** — collect results for a short window (e.g. 50 ms)
  then flush, balancing notification overhead against latency.

### Applicable requests

| Request | Benefit |
|---|---|
| `textDocument/implementation` | Already scans five phases; each phase's matches can be streamed |
| `textDocument/references` (§1) | Will need full-project scanning; streaming is essential |
| `workspace/symbol` (§5) | Searches every known class/function; early batches feel instant |
| `textDocument/completion` | Less critical (usually fast), but long chains through vendor code could benefit |

### Implementation sketch

1. Check whether the client sent a `partialResultToken` in the request
   params.
2. If yes, create a `$/progress` sender.  After each scan phase (or
   per-file, depending on granularity), send a
   `ProgressParams { token, value: [items...] }` notification.
3. Return `null` as the final response.
4. If no token was provided, fall back to the current behaviour: collect
   everything, return once.

---

## 7. Rename (`textDocument/rename`)
**Impact: Medium · Effort: Medium-High**

No rename refactoring support. Rename builds on find-references (§1) —
once all occurrences of a symbol are known, the rename handler produces
a `WorkspaceEdit` replacing each occurrence. The `SymbolMap`'s byte
ranges translate directly to LSP `Range`s via `offset_to_position`,
which makes generating the text edits straightforward.

For member renames, the stored `name_offset` on `MethodInfo`,
`PropertyInfo`, and `ConstantInfo` provides the declaration-site edit
position without text scanning.

**Vendor rejection:** The rename handler must reject renames for symbols
whose definition lives under the vendor directory. Users cannot
meaningfully rename third-party code. Use `vendor_uri_prefix` to detect
this and return an error via `prepareRename` (the LSP spec's
`textDocument/prepareRename` request exists specifically so the server
can reject a rename before the user types a new name).

---

## 8. Code Lens: jump to prototype method
**Impact: Low · Effort: Low**

When a method overrides a parent class method or implements an interface
method, show a code lens above the method declaration linking to the
prototype (base) method. Clicking the lens navigates to the parent/
interface declaration.

**Why only this one code lens:** Most code lens features (reference
counts, implementation counts, trait usage counts) require scanning the
entire workspace, which conflicts with our lazy-loading design. But
"jump to prototype" only needs the class hierarchy of the *current*
class — data we already fully resolve via `resolve_class_with_inheritance`.

### Implementation

1. **Register the capability** — set `code_lens_provider: Some(CodeLensOptions { resolve_provider: Some(false) })` in `ServerCapabilities`.

2. **Handler:** For each method in the file's AST:
   - Resolve the enclosing class via `find_or_load_class`.
   - Walk the inheritance chain (parent classes, then interfaces) to
     find a method with the same name.
   - If found, emit a `CodeLens` with the method's declaration range
     and a `Command` that triggers go-to-definition for the parent
     method's location.
   - Label: `↑ ParentClass::methodName` or `◆ InterfaceName::methodName`.

3. **Performance:** Only process the current file's methods. The class
   loader cache means parent lookups are fast. For a file with 20
   methods, this is 20 cache lookups — negligible.

**Future expansion:** If/when Find References ships and proves fast
enough for interactive use, reference count lenses could be added
behind a config flag. But the prototype lens stands alone without
that dependency.

---

## 9. Inlay hints (`textDocument/inlayHint`)
**Impact: Low · Effort: Medium**

Display inline type and parameter-name annotations in the editor without
the user having to hover or trigger completion. Since we already perform
deep type inference, this is primarily a presentation-layer feature —
surfacing data we already compute.

### Hint types (in priority order)

1. **Parameter name hints** — prepend the parameter name at call sites:
   `array_search(/*needle:*/ $x, /*haystack:*/ $arr)`. Skip when the
   argument is a variable whose name matches the parameter name (e.g.
   `foo($needle)` — the hint would be redundant).

2. **By-reference indicator** — annotate arguments passed by reference
   with `&`. This is a safety signal: the user may not realise a
   function mutates its argument.

3. **Inferred return type** — show the return type on functions/methods
   that lack an explicit return type declaration. Double-clicking (in
   editors that support it) could insert the type into the code.

4. **Variable assignment type** — show the inferred type after `$x =`
   assignments where the type isn't obvious from the RHS.

### Implementation

1. **Register the capability** — set `inlay_hint_provider: Some(OneOf::Left(true))` in `ServerCapabilities`.

2. **Handler:** Given a range, walk the AST within that range and emit
   `InlayHint` entries:
   - For call expressions: resolve the callable, zip arguments with
     parameters, emit parameter name hints.
   - For function/method declarations without return types: resolve the
     inferred return type, emit a hint after the closing `)`.
   - For variable assignments: resolve the RHS type, emit a hint after
     the `=`.

3. **Configuration:** Respect editor-level `editor.inlayHints.enabled`
   (handled by the client). Consider per-hint-type flags if users find
   some hints noisy (e.g. `phpantom.inlayHints.parameterNames: bool`).

**Performance:** Inlay hints are requested for the visible viewport
range only (editors send the range). For a typical screen of ~50 lines,
the cost is resolving types for the call expressions and assignments
visible — well within our per-file performance budget.

---

## 10. Reverse jump: implementation → interface method declaration
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

## 11. No go-to-definition for built-in (stub) functions and constants
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

## 12. Folding Ranges (`textDocument/foldingRange`)
**Impact: Medium · Effort: Low**

Code folding for classes, methods, functions, arrays, comment blocks,
and multi-line argument lists. Most editors provide basic indentation-
based folding without LSP support, but an AST-aware provider is more
accurate (e.g. it won't break on blank lines inside a method body).

Mago's AST provides span information for every block-level construct.
Walk the AST once and emit `FoldingRange` entries for each compound
statement, array literal, argument list, and doc comment. Set the
`kind` field to `FoldingRangeKind::Comment` for doc blocks and
`FoldingRangeKind::Region` for code blocks.

**Implementation:**

1. **Register the capability** — set `folding_range_provider:
   Some(FoldingRangeProviderCapability::Simple(true))` in
   `ServerCapabilities`.

2. **Handler:** Walk the file's AST and emit a `FoldingRange` for each:
   - Class / interface / trait / enum body
   - Method / function body (compound statement)
   - Closure / arrow function body (if multi-line)
   - Array literals spanning multiple lines
   - Multi-line argument and parameter lists
   - Doc comments (`/** ... */`)
   - Multi-line single-line comment blocks (`// ...` sequences)
   - `if`/`else`/`switch`/`match`/`try`/`catch`/`finally` blocks

3. **Collapse text** — set `collapsed_text` to a summary where useful
   (e.g. `"/** ... */"` for doc comments, `"{ ... }"` for code blocks).

---

## 13. Selection Ranges (`textDocument/selectionRange`)
**Impact: Medium · Effort: Low**

"Smart select" / expand selection. Given a cursor position, returns a
nested chain of ranges from innermost to outermost (e.g. identifier,
expression, statement, block, function, class, file). Most editors have
basic word/line/block expansion, but AST-aware selection ranges produce
much tighter expansions (e.g. selecting just the condition of an `if`,
then the full `if` block, then the enclosing method).

**Implementation:**

1. **Register the capability** — set `selection_range_provider:
   Some(SelectionRangeProviderCapability::Simple(true))` in
   `ServerCapabilities`.

2. **Handler:** For each requested position:
   - Walk the AST to collect all nodes whose span contains the position,
     from root down to the deepest leaf.
   - Reverse the list (deepest first) and build a `SelectionRange`
     linked list where each entry's `parent` points to the next wider
     range.
   - Return the innermost `SelectionRange`.

---

## 14. Type Definition (`textDocument/typeDefinition`)
**Impact: Medium · Effort: Low**

"Go to Type Definition" jumps from a variable or expression to the
class of its resolved type, rather than to the definition site. For
example, if `$user` is typed as `User`, go-to-definition jumps to the
`$user = ...` assignment, while go-to-type-definition jumps to the
`User` class declaration.

The variable type resolution infrastructure already exists. The handler
resolves the variable's type (or the expression type for property
accesses and method calls), strips nullability and generic parameters,
and looks up the class definition location.

**Implementation:**

1. **Register the capability** — set `type_definition_provider:
   Some(TypeDefinitionProviderCapability::Simple(true))` in
   `ServerCapabilities`.

2. **Handler:** Given a position:
   - If the symbol under the cursor is a variable, resolve its type via
     the existing variable resolution pipeline.
   - If it's a property access or method call, resolve the subject type.
   - Strip `?`, `|null`, and generic parameters from the resolved type
     string to get the base class name.
   - Look up the class via `find_or_load_class` and return its
     definition location.
   - For union types, return multiple locations (one per class in the
     union).

---

## 15. Document Links (`textDocument/documentLink`)
**Impact: Low-Medium · Effort: Low**

Makes `require`, `require_once`, `include`, and `include_once` paths
Ctrl+Clickable in the editor. Useful for legacy codebases that use
file-based includes rather than PSR-4 autoloading.

**Implementation:**

1. **Register the capability** — set `document_link_provider:
   Some(DocumentLinkOptions { resolve_provider: Some(false) })` in
   `ServerCapabilities`.

2. **Handler:** Walk the AST for include/require expressions. For each:
   - Extract the path string from the argument (only handle string
     literals and simple concatenations like `__DIR__ . '/file.php'`).
   - Resolve the path relative to the current file's directory and the
     workspace root.
   - If the resolved path exists on disk, emit a `DocumentLink` with
     the target URI and the range of the string literal.

---

## 16. Type Hierarchy (`textDocument/prepareTypeHierarchy`)
**Impact: Low-Medium · Effort: Medium**

Shows the class hierarchy (supertypes and subtypes) for a class under
the cursor. The LSP Type Hierarchy is a three-step protocol:

1. `textDocument/prepareTypeHierarchy` — returns a `TypeHierarchyItem`
   for the class/interface/trait under the cursor.
2. `typeHierarchy/supertypes` — returns parents. Walk the
   `extends`/`implements` chain, which is already resolved during
   inheritance. Essentially free.
3. `typeHierarchy/subtypes` — returns children. Calls
   `find_implementors`, same cost profile as go-to-implementation.

The supertypes direction is cheap (data already computed). The subtypes
direction has the same cost as go-to-implementation, but it's user-
initiated (triggered via a command, not automatic), so the latency is
acceptable.

*Depends on: go-to-implementation infrastructure (already shipped).*

**Implementation:**

1. **Register the capability** — set `type_hierarchy_provider:
   Some(TypeHierarchyServerCapabilities::Options(...))` in
   `ServerCapabilities`.

2. **Prepare handler:** Resolve the class under the cursor via
   `find_or_load_class`. Return a `TypeHierarchyItem` with the class
   name, kind, URI, range, and selection range.

3. **Supertypes handler:** Walk the class's `extends` and `implements`
   lists. For each parent/interface, resolve via `find_or_load_class`
   and return a `TypeHierarchyItem`.

4. **Subtypes handler:** Call `find_implementors` for the class name
   and return a `TypeHierarchyItem` for each result.

---

## 17. Incremental text sync
**Impact: Low-Medium · Effort: Medium**

PHPantom uses `TextDocumentSyncKind::FULL`, meaning every
`textDocument/didChange` notification sends the entire file content.
Switching to `TextDocumentSyncKind::INCREMENTAL` means the client sends
only the changed range (line/column start, line/column end, replacement
text), reducing IPC bandwidth for large files.

The practical benefit is bounded: Mago requires a full re-parse of the
file regardless of how the change was received, so the saving is purely
in the data transferred over the IPC channel. For files under ~1000
lines this is negligible. For very large files (5000+ lines, common in
legacy PHP), sending 200KB on every keystroke can become noticeable.

**Implementation:**

1. **Change the capability** — set `text_document_sync` to
   `TextDocumentSyncKind::INCREMENTAL` in `ServerCapabilities`.

2. **Apply diffs** — in the `did_change` handler, apply each
   `TextDocumentContentChangeEvent` to the stored file content string.
   The events contain a `range` (start/end position) and `text`
   (replacement). Convert positions to byte offsets and splice.

3. **Re-parse** — after applying all change events, re-parse the full
   file with Mago as today. No incremental parsing needed initially.

**Relationship with partial result streaming (§6):** These two features
address different performance axes. Incremental text sync reduces the
cost of *inbound* data (client to server per keystroke). Partial result
streaming (§6) reduces the *perceived latency* of *outbound* results
(server to client for large result sets). They are independent and can
be implemented in either order, but if both are planned, incremental
text sync is lower priority because full-file sync is rarely the
bottleneck in practice. Partial result streaming has a more immediate
user-visible impact for go-to-implementation, find references, and
workspace symbols on large codebases.