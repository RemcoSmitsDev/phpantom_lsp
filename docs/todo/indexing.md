# PHPantom — Indexing & File Discovery

This document covers how PHPantom discovers, parses, and caches class
definitions across the workspace. The goal is to remain fast and
lightweight by default while offering progressively richer modes for
users who want exhaustive workspace intelligence.

---

## Current state

PHPantom has three byte-level scanners (no AST) for early-stage file
discovery:

1. **composer-classmap** — parses Composer's `autoload_classmap.php`
   into an in-memory `HashMap<String, PathBuf>`.
2. **PSR-4 scanner** (`find_classes`) — walks PSR-4 directories from
   `composer.json` and extracts class FQNs with namespace compliance
   filtering.
3. **full-scan** (`find_symbols`) — walks files and extracts classes,
   standalone functions, `define()` constants, and top-level `const`
   declarations in a single pass.

These scanners serve three scenarios at startup:

| Scenario | Class discovery | Function & constant discovery |
|---|---|---|
| **Optimized Composer** (classmap present and complete) | composer-classmap | `autoload_files.php` byte-level scan + lazy parse |
| **Composer self-scan** (missing/incomplete classmap or `"self"` strategy) | PSR-4 scanner + vendor packages | `autoload_files.php` byte-level scan + lazy parse |
| **No `composer.json`** | full-scan on all workspace files | full-scan on all workspace files |
| **Monorepo** (no root `composer.json`, subprojects found) | Per-subproject: composer-classmap + PSR-4 + vendor packages. Loose files: full-scan with skip set | Per-subproject: `autoload_files.php` byte-level scan + lazy parse. Loose files: full-scan |

The "no `composer.json`" path is fully lightweight: `find_symbols`
populates classmap, `autoload_function_index`, and
`autoload_constant_index` in one pass, and lazy `update_ast` on first
access provides complete `FunctionInfo`/`DefineInfo`. All directory
walkers (full-scan, PSR-4 scanner, vendor package scanner, and
go-to-implementation file collector) use the `ignore` crate for
gitignore-aware traversal instead of hardcoded directory name
filtering. Hidden directories are skipped automatically.

The monorepo path activates when there is no root `composer.json` but
`discover_subproject_roots` finds subdirectories with their own
`composer.json` files. Each subproject is processed through the full
Composer pipeline (PSR-4, classmap, vendor packages, autoload files)
and results are merged into the shared backend state. Loose PHP files
outside subproject trees are picked up by the full-scan walker with a
skip set that prevents double-scanning subproject directories. See
the ARCHITECTURE.md Composer Integration section for full details.

Find References parses files in parallel via `std::thread::scope`.
Go-to-Implementation walks classmap files sequentially.

---

## Strategy modes

Four indexing strategies, selectable via `.phpantom.toml`:

```toml
[indexing]
# "composer" (default) - use composer classmap, self-scan on fallback
# "self"    - always self-scan, ignore composer classmap
# "full"    - background-parse all project files for rich intelligence
# "none"    - no proactive scanning
strategy = "composer"
```

### `"composer"` (default)

Use Composer's classmap when available and complete. Fall back to
self-scan when the classmap is missing or incomplete. This is the
zero-config experience.

### `"self"`

Always build the classmap ourselves. Ignores `autoload_classmap.php`
entirely. For users who refuse to run `composer dump-autoload -o` or
who are actively editing `composer.json` dependencies.

### `"full"`

Background-parse every PHP file in the project. Uses Composer data to
guide file discovery when available, falls back to scanning all PHP
files in the workspace when it is not. Populates the ast_map,
symbol_maps, and all derived indices. Enables workspace symbols, fast
find-references without on-demand scanning, and rich hover on
completion items. Memory usage grows proportionally to project size.

### `"none"`

No proactive file scanning. Still uses Composer's classmap if present,
still resolves classes on demand when the user triggers completion or
hover, still has embedded stubs. The only difference from `"composer"`
is that it never falls back to self-scan when the classmap is missing
or incomplete. This is essentially what PHPantom does today.

---

## Phase 1: Self-generated classmap

No outstanding items. Implemented: byte-level classmap scanner,
PSR-4 compliance filtering, vendor package scanning via
`installed.json`, non-Composer workspace fallback, strategy
configuration, and user feedback messages.

---

## Phase 2: Staleness detection and auto-refresh

**Goal:** Keep the classmap fresh without user intervention.

### Trigger points

- On `workspace/didChangeWatchedFiles`: if `composer.json` or
  `composer.lock` changed, schedule a rescan of vendor directories
  (the user likely ran `composer install` or `composer update`).
- On `did_save` of a PHP file: if the file is in a PSR-4 directory,
  do a targeted single-file rescan (read the file, extract class
  names, update the classmap entry). This is cheap enough to do
  synchronously.

### Targeted refresh

For single-file changes, re-scan only that file and update/remove its
classmap entries. No need to rescan the entire workspace.

For dependency changes (vendor rescan), this is the expensive case but
happens rarely (a few times per day at most).

---

## Phase 2.5: Lazy autoload file indexing

No outstanding items. Implemented: the autoload file loop in
`server.rs` (`initialized`) now uses `find_symbols` (the byte-level
full-scan) instead of `update_ast` to process files from Composer's
`autoload_files.php` and their `require_once` chains. This populates
`autoload_function_index`, `autoload_constant_index`, and
`class_index` without building full AST data. Full parsing is deferred
to the moment a symbol is first accessed via `find_or_load_function`
(Phase 1.5), `resolve_constant_definition` (Phase 1.5), or
`find_or_load_class` (through `class_index`).

Functions inside `if (! function_exists(...))` guards are not
discovered by the byte-level scanner (they are at brace depth > 0).
As a safety net, `autoload_file_paths` stores all visited autoload
file paths. When a function or constant is not found in any index or
stubs, `find_or_load_function` and `resolve_constant_definition`
lazily parse each known autoload file via `update_ast` as a last
resort (Phase 1.75). Each file is parsed at most once.

---

## Phase 2.6: Non-Composer function and constant discovery

No outstanding items. Implemented: `find_symbols` byte-level scanner
extracts classes, functions, `define()` constants, and top-level
`const` in a single pass. `scan_workspace_fallback_full` populates
`autoload_function_index` and `autoload_constant_index` for
non-Composer projects. Lazy resolution in `find_or_load_function`
and `resolve_constant_definition` parses files on demand. Completion
shows autoload index entries before they are lazily parsed.

---

## Phase 3: Parallel file processing

**Goal:** Speed up workspace-wide operations (find references,
go-to-implementation, self-scan, diagnostics) by processing files in
parallel with priority awareness.

All prerequisites (§3 `RwLock`, §5 `Arc<String>`, §6 `Arc<SymbolMap>`)
are complete.

### Current state (partial)

`ensure_workspace_indexed` (used by find references) now parses files
in parallel via two helpers in `references/mod.rs`:

- **`parse_files_parallel`** — takes `(uri, Option<content>)` pairs,
  loads content via `get_file_content` when not provided, splits work
  into chunks, and parses each chunk in a separate OS thread.
- **`parse_paths_parallel`** — takes `(uri, PathBuf)` pairs, reads
  files from disk and parses them in parallel.

Both use `std::thread::scope` for structured concurrency (all threads
join before the function returns). The thread count is capped at
`std::thread::available_parallelism()` (typically the number of CPU
cores). Batches of 2 or fewer files skip threading overhead.

Transient entry eviction after GTI and find references has been
removed. Parsed files stay cached in `ast_map`, `symbol_maps`,
`use_map`, and `namespace_map` so that subsequent operations benefit
from the work already done. This trades a small amount of memory for
faster repeat queries and simpler code.

### Remaining work

The following are deferred to a later sprint:

- **Priority-aware scheduling.** Interactive requests (completion,
  hover, go-to-definition) should preempt batch work. Currently all
  threads run at equal priority.
- **Parallel classmap scanning in `find_implementors`.** Phase 3 of
  `find_implementors` reads and parses many classmap files
  sequentially. Parallelizing this requires care because it
  interleaves reads and writes through `class_loader` callbacks.
- **`memmap2` for file reads.** Avoids copying file contents into
  userspace when the OS page cache already has them.

### Why not rayon?

`rayon` is the obvious choice for "process N files in parallel" and
Libretto uses it successfully. But it runs its own thread pool
separate from tokio's runtime. When rayon saturates all cores on a
batch scan, tokio's async tasks (completion, hover, signature help)
get starved for CPU time. There is no clean way to pause a rayon
batch when a high-priority LSP request arrives.

### Why the classmap is not a prerequisite

The classmap is a convenience for O(1) class lookup and class name
completion. But most resolution already works on demand via PSR-4
(derive path from namespace, check if file exists). Class name
completion is a minor subset of what users actually trigger. This
means classmap generation can run at normal priority without blocking
the user. They can start writing code immediately while the classmap
builds in the background.

---

## Phase 4: Completion item detail on demand

**Goal:** Show type signatures, docblock descriptions, and
deprecation info in completion item hover without parsing every
possible class up front.

### Current limitation

When completion shows `SomeClass::doThing()`, hovering over that item
in the completion menu shows nothing because we haven't parsed
`SomeClass`'s file yet. Parsing it on demand would be fine for one
item, but the editor may request resolve for dozens of items as the
user scrolls.

### Approach: "what's already discovered"

Use `completionItem/resolve` to populate `detail` and
`documentation` fields. If the class is already in the ast_map (parsed
during a prior resolution), return the full signature and docblock.
If not, return just the item label with no extra detail.

In `"full"` mode, everything is already parsed, so every completion
item gets rich hover for free. In `"composer"` / `"self"` mode, items
that happen to have been resolved earlier in the session get rich
detail; others don't. This is a graceful degradation that never blocks
the completion response.

### Future: speculative background parsing

When a completion list is generated, queue the unresolved classes for
background parsing at low priority. If the user lingers on the
completion menu, resolved items will progressively gain detail. This
is a nice-to-have, not a requirement.

---

## Phase 5: Full background indexing

**Goal:** Parse every PHP file in the project in the background,
enabling workspace symbols, fast find-references without on-demand
scanning, and complete completion item detail.

**Prerequisites (from [performance.md](performance.md)):**

- **§1 FQN secondary index.** Done. `fqn_index` provides O(1)
  lookups by fully-qualified name, so the second pass populating
  `ast_map` with thousands of entries no longer causes linear scans.
- **§2 `Arc<ClassInfo>`.** Full indexing stores a `ClassInfo` for every
  class in the project. Without `Arc`, every resolution clones the
  entire struct out of the map. With `Arc`, retrieval is a
  reference-count increment. This is the difference between full
  indexing using ~200 MB vs. ~500 MB for a large project.
- **§3 `RwLock`.** Done. The second pass writes to `ast_map` at Low priority
  while High-priority LSP requests read from it. `Mutex` would force
  every completion/hover request to wait for the current background
  parse to finish its map insertion. `RwLock` lets reads proceed
  concurrently with other reads; only the brief write window blocks.
- **§4 `HashSet` dedup.** Done. All member deduplication during
  inheritance merging now uses `HashSet` lookups, bringing the
  per-resolution cost from O(N²) to O(N).

### Trigger

When `strategy = "full"` is set in `.phpantom.toml`.

### Design: self + second pass

Full mode is not a separate discovery system. It works exactly like
`"self"` mode (Phase 1) and then schedules a second pass:

1. **First pass (same as self):** Build the classmap via byte-level
   scanning. This completes in about a second and gives us class
   name completion and O(1) file lookup.
2. **Second pass:** Iterate every file path in the now-populated
   in-memory classmap and call `update_ast` on each one at Low
   priority. This populates ast_map, symbol_maps, class_index,
   global_functions, and global_defines.

No new file discovery logic is needed. The classmap from the first
pass already contains every relevant file path. The second pass just
enriches it.

When `composer.json` does not exist (e.g. the user opened a monorepo
root or a non-Composer project), the first pass falls back to walking
all PHP files under the workspace root, so the second pass still has
a complete file list to work from.

### Progressive enrichment

The user experiences three stages:

1. **Immediate:** LSP requests are up and running. Completion, hover,
   and go-to-definition work via on-demand resolution and stubs.
2. **Seconds:** Classmap is ready. Class name completion covers the
   full project. Cross-file resolution is O(1).
3. **Under a minute:** Full AST parse complete. Workspace symbols,
   fast find-references (no on-demand scanning), rich hover on
   completion items.

Each stage improves on the last without blocking the previous one.

### Behaviour

1. Respect the priority system from Phase 3: pause the second pass
   when higher-priority work arrives.
2. Process user code first, then vendor.
3. Report progress via `$/progress` tokens so the editor can show
   "Indexing: 1,234 / 5,678 files".

### Memory

Currently we store `ClassInfo`, `FunctionInfo`, and `SymbolMap`
structs that are not as lean as they could be. For a 21K-file
codebase, full indexing will use meaningful RAM. This is acceptable
because it's an opt-in mode, but we should profile and trim struct
sizes over time. The aim is to stay under 512 MB for a full project.

The performance prerequisites above (§2 `Arc<ClassInfo>`, §5
`Arc<String>`, §6 `Arc<SymbolMap>`) directly reduce memory usage by
sharing data across the ast_map, caches, and snapshot copies instead
of deep-cloning each. These should be measured before and after to
validate the 512 MB target.

### Workspace symbols

With the full index populated, `workspace/symbol` becomes a simple
filter over the ast_map and global_functions maps. No additional
infrastructure needed.

In other modes, workspace symbols still works but only returns results
from already-parsed files (opened files, on-demand resolutions, stubs).
When the user invokes workspace symbols outside of full mode, show a
one-time hint suggesting they enable `strategy = "full"` in
`.phpantom.toml` for complete coverage.

---

## Phase 6: Disk cache (evaluate later)

**Goal:** Persist the full index to disk so that restarts don't
require a full rescan.

### When to consider

Only if Phase 5 background indexing is slow enough on cold start that
users complain. Given that:
- Mago can lint 45K files in 2 seconds.
- A regex classmap scan over 21K files should be sub-second.
- Full AST parsing of a few thousand user files should take single
  digit seconds.

...disk caching may never justify its complexity. The primary use
case would be memory savings (load from disk on demand instead of
holding everything in RAM), not startup speed.

### Format options

- `bincode` / `postcard`: simple, small dependency footprint, tolerant
  of struct changes (deserialization fails gracefully instead of
  reading garbage memory). The right default choice.
- SQLite: robust, queryable, but heavier than needed for a flat
  key-value store.

Zero-copy formats like `rkyv` are ruled out. They map serialized bytes
directly into memory as if they were the original structs, which means
any struct layout change between versions reads corrupt data. PHPantom's
internal types change frequently and will continue to do so. A cache
format that silently produces garbage after an update is worse than no
cache at all.

### Invalidation

Store file mtime + content hash per entry. On startup, walk the
directory, compare mtimes, re-parse only changed files. This is
Libretto's `IncrementalCache` approach and it works well.

### Decision criteria

Implement disk caching only if:
1. Full-mode cold start exceeds 10 seconds on a representative large
   codebase, AND
2. The memory overhead of holding the full index exceeds the 512 MB
   target, or users on constrained systems report issues.

If neither condition is met, skip this phase entirely. Simpler is
better.