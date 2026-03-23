# Changelog

All notable changes to PHPantom will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **Null-coalesce (`??`) type refinement.** When the left-hand side of `??` is provably non-nullable (e.g. `new Foo()`, `clone $x`, a literal), the right-hand side is recognized as dead code and the result resolves to the LHS type only. When the LHS is nullable (e.g. a `?Foo` return type), `null` is stripped from the LHS and the result is the union of the non-null LHS with the RHS.
- **`@mixin` generic substitution.** When a class declares `@mixin Builder<TRelatedModel>`, the generic arguments are now preserved and substituted into the mixin's members. Previously, `@mixin Builder<TRelatedModel>` was stored as just `Builder` (the generic argument was stripped), so methods like `firstOrFail(): TModel` returned the raw template parameter name instead of the concrete type. This resolves through inheritance chains: `BelongsTo @extends Relation<Product>` with `Relation @mixin Builder<TRelatedModel>` now correctly substitutes `TModel` to `Product` on all Builder methods.

### Fixed

- **Hover and completion type parity.** The hover type-string engine now handles pipe expressions (`|>`), closure/arrow-function literals, and generator yield-assignment (`$var = yield`) expressions, matching the completion engine. Previously these expression kinds were only handled by the completion pipeline, so hover could show a different (or missing) type for variables assigned from them.
- **Generic substitution through transitive interface chains.** When a class implements an interface that itself extends another generic interface (e.g. `CommandCollection @implements TypedCollection<string, Command>` where `TypedCollection @extends IteratorAggregate<TKey, TValue>`), template parameters are now substituted at each level. Previously the intermediate interface's `@extends` generics were propagated with raw template parameter names (e.g. `T` instead of `Command`), breaking foreach element resolution and other generic-dependent features for multi-level interface hierarchies.
- **Guard clause narrowing across instanceof branches.** After `if ($x instanceof Y) { return; }`, subsequent `instanceof` checks on the same variable no longer incorrectly resolve to `Y`. Previously the diagnostic cache reused the narrowed type from inside the first branch for all later accesses to the same variable, producing false-positive "not found" warnings (e.g. "Property 'value' not found on class 'Stringable'" inside a `BackedEnum` branch).
- **`instanceof self/static/parent` narrowing.** Type narrowing with `instanceof self`, `instanceof static`, and `instanceof parent` now works correctly in all contexts (assert, if-blocks, guard clauses, compound conditions). Previously these keywords were ignored because the AST parser produces dedicated node types rather than identifiers, and `parent` was not recognized as a type hint during resolution.
- **Interface constants through multi-extends chains.** Constants defined on parent interfaces are now found when an interface extends multiple other interfaces. Previously only the first parent interface was walked for member merging, so constants like `Carbon::JANUARY` (defined on the `UnitValue` interface, sixth in `CarbonInterface`'s extends list) were reported as unknown.
- **Variable type inside self-referencing assignment RHS.** In `$request = new Foo(arg: $request->uuid)`, the `$request` reference inside the constructor arguments now correctly resolves to the original type instead of the type being assigned. PHP evaluates all RHS arguments before performing the assignment, but PHPantom previously applied the new definition too early.
- **Variable assignments inside foreach loops.** Variables conditionally reassigned inside a `foreach` body are now visible after the loop. Previously, `$x = null; foreach (...) { $x = new Foo(); } $x->method();` lost the `Foo` type because the foreach body was only scanned when the cursor was inside it. The `while`, `for`, and `do-while` loops were not affected.
- **Variable-to-variable type propagation.** Assignments like `$found = $pen` now resolve `$found` to the type of `$pen`. Previously, bare variable references on the right-hand side of an assignment were ignored, so patterns like `foreach ($pens as $pen) { $found = $pen; }` left `$found` untyped. This also eliminates false-positive `scalar_member_access` diagnostics when the initial assignment was `$found = null` — the type from the later reassignment now takes precedence.
- **Variable type after reassignment.** When a method parameter is reassigned mid-body (e.g. `$file = $result->getFile()`), subsequent member accesses now resolve against the new type instead of the original parameter type. Previously the diagnostic subject cache reused the first resolution for the entire method scope, producing false-positive "not found" warnings on members that exist only on the reassigned type.
- **Null-safe method chain resolution.** Null-safe method calls (`$obj?->method()`) now resolve the return type correctly for variable type inference, including cross-file chains. Previously `?->` calls were ignored by the RHS resolution pipeline, losing the type for any variable assigned from a null-safe chain.
- **Nullable and generic types in class lookup.** Variables typed as `?ClassName` or `Collection<Item>` now resolve correctly across all code paths. Previously the `?` prefix and generic parameters were not stripped before class lookup, causing the type engine to treat them as unknown types. This fixes completion, hover, go-to-definition, and false-positive diagnostics for any variable whose type uses the nullable shorthand or carries generic arguments.
- **`@see` references with qualified class names.** Docblock `@see Fully\Qualified\ClassName` references no longer have the file's namespace prepended, which previously produced doubled names like `App\Models\App\Models\User`. Qualified names in `@see` tags are now treated as fully-qualified.
- **Ternary and null-coalesce member access.** Accessing a member on a ternary or null-coalesce expression (e.g. `($a ?: $b)->property`, `($x ?? $y)->method()`) now resolves correctly for hover, go-to-definition, and diagnostics. Previously the subject extraction produced an empty string, causing a confusing "Cannot resolve type of ''" hint and no hover information.
- **Inherited methods missing through deep stub chains.** Methods like `getCode()` and `getMessage()` are now found on classes that inherit through multi-level chains where intermediate classes live in stubs (e.g. `QueryException` → `PDOException` → `RuntimeException` → `Exception`). Previously the inheritance chain broke when a stub file contained multiple namespace blocks, causing parent class names to be resolved against the wrong namespace.
- **False-positive diagnostics for same-named variables in different methods.** When two methods in the same class both used a variable like `$order`, the diagnostic cache resolved it once and reused that type for every other method in the class. The second method saw the wrong type and flagged valid member accesses as unknown. Both the per-file subject cache and the cross-collector resolution cache are now scoped to the enclosing function/method/closure body, so each method resolves variables independently.
- **False-positive diagnostics for `$this` inside traits.** Accessing host-class members via `$this->`, `self::`, `static::`, or `parent::` inside a trait method no longer produces "not found" warnings, including chain expressions like `static::where(...)->update(...)` and accesses inside closures or arrow functions nested within trait methods. Traits are incomplete by nature and expect the host class to provide these members.
- **False-positive argument count errors on overloaded built-in functions.** Functions like `array_keys`, `mt_rand`, and `rand` accept multiple valid argument counts that phpstorm-stubs cannot express with a single declaration. An overload map derived from PHPStan's `functionMap.php` provides the correct minimum argument count for these genuine overloads. The AST parser's `#[PhpStormStubsElementAvailable]` version filtering now handles parameter variants that differ by PHP version, eliminating ~70 entries that previously required workarounds.
- **Type narrowing inside `return` statements.** `instanceof` checks in `&&` chains and ternary conditions now narrow the variable type when the expression is the operand of a `return` statement. Previously, `return $e instanceof QueryException && $e->errorInfo;` would flag `errorInfo` as unknown because narrowing only applied inside standalone expression statements and `if` conditions.
- **CLI analyze performance (pathological files).** Single-file analysis of Eloquent-heavy services dropped from ~6 min to ~63 s (5.8× faster). The shared package (~2 500 files) dropped from 14 m 28 s to 1 m 25 s (10× faster, now ~30 % faster than PHPStan on the same machine). Key changes:
  - *Negative class cache.* `find_or_load_class` now caches "not found" results so repeated references to unknown types skip the four-phase lookup (fqn_index → classmap → PSR-4 → stubs). Invalidated when new classes are discovered.
  - *`SharedVec<T>` cheap-clone wrapper.* `ClassInfo.methods`, `.properties`, and `.constants` are now `SharedVec<T>` (an `Arc<Vec<T>>` newtype). Cloning a `ClassInfo` bumps three refcounts instead of deep-copying hundreds of method/property/constant structs. Copy-on-write mutation via `Arc::make_mut` preserves correctness.
  - *Zero-copy parent chain walks.* A `ClassRef` enum (`Borrowed(&ClassInfo)` | `Owned(Arc<ClassInfo>)`) replaces `Arc::new(class.clone())` in `resolve_class_with_inheritance` and the interface-generics collection loop, eliminating a full `ClassInfo` clone per parent level.
  - *O(1) method dedup in virtual member and interface merging.* `merge_virtual_members` and `merge_interface_members_into` now use `HashMap` indexes instead of linear `.position()` / `.find()` scans, reducing O(N×M) string comparisons to O(M).
  - *Thread-local resolved-class cache.* `type_hint_to_classes_depth` and `inject_model_virtual_methods` called `resolve_class_fully` without access to the cache. A thread-local guard (same pattern as the parse cache) now makes the `resolved_class_cache` implicitly available, so every `Builder<Model>` type hint hits the cache instead of triggering a full uncached resolution.
  - *Two-phase analyze.* The CLI analyze command now parses all user files first (Phase 1), then collects diagnostics (Phase 2). Cross-file class references resolve via O(1) `fqn_index` lookup instead of falling through to classmap/PSR-4 lazy loading.
  - *Per-collector parse cache.* A single `with_parse_cache` guard wraps all diagnostic collectors per file, so the AST is parsed once and shared.
- **Inline array access on method returns.** Expressions like `$c->items()[0]->getLabel()` now resolve the element type correctly for both completion and diagnostics. Previously, indexing into a method's array return type inline produced false "cannot verify" warnings and no completions. Assigning to an intermediate variable first was the only workaround.
- **Diagnostic deduplication.** Multiple diagnostics on the same span or line are no longer collapsed into one. If PHPStan reports five issues on a line, all five are shown. If PHPantom reports two issues on the same call, both are shown. Cross-source overlap is still handled by suppressing full-line PHPStan diagnostics when a precise native diagnostic exists on that line.
- **Diagnostic performance on large files.** Unknown-member diagnostics on files with many member accesses are up to 7× faster. The diagnostic pass now caches subject resolution per unique (subject, access kind, scope) tuple, eliminating hundreds of redundant file re-parses that occurred when the same variable or `$this` appeared in many member accesses within a single class.
- **Redundant file re-parsing in unknown-member diagnostics.** Each unique variable subject that went through the resolution pipeline triggered up to 6 full re-parses of the file via `with_parsed_program`. A new thread-local parse cache (`with_parse_cache`) now parses the file once at the start of the diagnostic pass and reuses the AST for all subsequent `with_parsed_program` calls on the same content. This eliminates all redundant parsing for distinct variable subjects (e.g. `$var1->`, `$var2->`, `$var3->`) within a single pass, complementing the earlier subject-deduplication cache which only helped identical subjects.
- **Progress notifications.** The server no longer sends `window/workDoneProgress/create` requests to clients that do not advertise support for it. Previously this could block clients indefinitely while they waited for a response that never came.
- **Clone expressions.** `(clone $var)->` now resolves to the same type as `$var`, providing correct completion, hover, and diagnostics. Previously the type was lost, producing false "Cannot resolve type of ''" hints and no completions.
- **Change visibility.** The code action no longer appears when the cursor is inside a method body. It now only triggers on the method signature (modifiers, name, parameters, return type).
- **Update docblock.** The code action no longer appears when the cursor is inside a function or method body. It now only triggers on the signature or the preceding docblock.
- **Update docblock.** No longer suggests adding redundant `@param` tags when the docblock has no `@param` tags and all parameters already have sufficient native type hints. This matches the generate-docblock behaviour, which intentionally omits `@param` for fully-typed non-templated parameters.
- **PHPStan diagnostics.** PHPStan cache pruning after deduplication now unconditionally writes the pruned set back, fixing a theoretically possible stale-entry reappearance when pruning changed diagnostics without changing the count.
- **Function return type resolution across files.** Standalone functions (e.g. Laravel's `now()`) that declare return types using short names from their own `use` imports now resolve correctly in consuming files. Previously, the return type was stored as the unqualified name (e.g. `CarbonInterface` instead of `Carbon\CarbonInterface`), causing false "subject type could not be resolved" warnings and broken completion when the consuming file did not import the same class. Function parameter types and `@throws` types are also resolved.
- **Incorrect argument count warnings for built-in functions.** Several built-in PHP functions had wrong parameter counts in the upstream stubs (e.g. optional parameters marked as required), causing false "Expected N arguments, got M" diagnostics. Switched to a fork with corrected signatures until the fixes are merged upstream.

### Added

- **Phar archive class resolution.** Classes inside `.phar` archives (e.g. PHPStan's `phpstan.phar`) are now discovered and indexed automatically. During Composer autoload scanning, bootstrap files that reference a phar are detected, the archive is parsed in-process (no PHP runtime needed), and all PHP classes inside are registered for completion, hover, go-to-definition, and diagnostics. Anyone writing PHPStan extensions, custom rules, or dynamic return type extensions now gets full IDE support for the PHPStan API. Only uncompressed phars are supported (the format used by PHPStan and most other phar-distributed tools).
- **PSR-0 autoload support.** Packages that use the legacy PSR-0 autoloading standard (e.g. HTMLPurifier) are now discovered automatically. Composer's `autoload_namespaces.php` is parsed at startup and the listed directories are scanned for classes. Previously these packages were invisible unless the user ran `composer install -o` to dump an optimised classmap.
- **Analyze command.** `phpantom_lsp analyze` scans a Composer project and reports PHPantom's own diagnostics in a PHPStan-like table format. Useful for measuring type coverage across an entire codebase without opening files one by one. Accepts an optional path argument to limit the scan to a single file or directory. Only native diagnostics are reported (no PHPStan, no external tools). Output includes diagnostic identifiers and supports `--severity` filtering and `--no-colour` for CI.
- **Add @throws.** New code action triggered by PHPStan's `missingType.checkedException` diagnostic. When PHPStan reports that a method or function throws a checked exception not documented in `@throws`, the quick-fix inserts a `@throws ShortName` tag into the existing docblock (or creates a new docblock) and adds a `use` import for the exception class when needed. Handles methods, standalone functions, and property hooks. Skips the action when the exception is already documented, already imported, or in the same namespace.
- **Remove @throws.** New code action triggered by PHPStan's `throws.unusedType` (a `@throws` tag for a type that is never thrown) and `throws.notThrowable` (a `@throws` tag for a type that is not a subtype of `Throwable`). The quick-fix removes the offending `@throws` line from the docblock, cleans up orphaned blank separator lines, and removes the entire docblock when it would be empty after removal. Handles FQN, short-name, and leading-backslash variants, as well as single-line docblocks.
- **Instant feedback for @throws actions.** PHPStan `throws.*` diagnostics are eagerly pruned from the cache when the file content changes and the condition that triggered them no longer holds. After applying an "Add @throws" or "Remove @throws" code action, the diagnostic disappears on the next keystroke without waiting for the next PHPStan run.
- **Semantic Tokens.** Type-aware syntax highlighting that goes beyond what a TextMate grammar can achieve. Classes, interfaces, enums, traits, methods, properties, parameters, variables, functions, constants, and template parameters all get distinct token types. Modifiers convey declaration sites, static access, readonly, deprecated, and abstract status.
- **Inlay hints.** Parameter name and by-reference indicators appear at call sites (`textDocument/inlayHint`). Hints are suppressed when the argument already makes the parameter obvious: variable names matching the parameter, property accesses with a matching trailing identifier, string literals whose content matches, well-known single-parameter functions like `count` and `strlen`, and spread arguments. Named arguments never receive a redundant hint. Mixed positional and named argument ordering is handled correctly.
- **PHPStan diagnostics.** PHPStan errors appear inline as you edit, using PHPStan's editor mode (`--tmp-file` / `--instead-of`). Auto-detects `vendor/bin/phpstan` or `$PATH`. Runs in a dedicated background worker with a 2-second debounce and at most one process at a time, so native diagnostics are never blocked. Configurable via `[phpstan]` in `.phpantom.toml` (`command`, `memory-limit`, `timeout`). "Ignore PHPStan error" and "Remove unnecessary @phpstan-ignore" code actions manage inline ignore comments.
- **Formatting.** Built-in PHP formatting via mago-formatter (PER-CS 2.0 style). Formatting works out of the box without any external tools. Projects that depend on php-cs-fixer or PHP_CodeSniffer in their `composer.json` `require-dev` automatically use those tools instead (both can run in sequence). Per-tool command overrides and disable switches in `[formatting]` in `.phpantom.toml`. External formatters run without blocking completions or other requests.
- **Document Symbols.** The outline sidebar and breadcrumbs now show classes, interfaces, traits, enums, methods, properties, constants, and standalone functions with correct nesting, icons, visibility detail, and deprecation tags.
- **Workspace Symbols.** "Go to Symbol in Workspace" (Ctrl+T / Cmd+T) searches across all indexed files including vendor classes. Results include namespace context and deprecation markers, sorted by relevance.
- **Type Hierarchy.** "Show Type Hierarchy" on any class, interface, trait, or enum reveals its supertypes and subtypes with full up-and-down navigation through the inheritance tree, including cross-file resolution and transitive relationships.
- **Code Lens.** Clickable annotations above methods that override a parent class method or implement an interface method. Clicking navigates to the prototype declaration.
- **Folding Ranges.** AST-aware code folding for class bodies, method/function bodies, closures, arrays, argument/parameter lists, control flow blocks, doc comments, and consecutive single-line comment groups.
- **Selection Ranges.** Smart select / expand selection returns AST-aware nested ranges from innermost to outermost.
- **Document Links.** `require`/`include` paths are now Ctrl+Clickable. Path resolution supports string literals, `__DIR__` concatenation, `dirname(__DIR__)`, `dirname(__FILE__)`, and nested `dirname` with levels.
- **Syntax error diagnostic.** Parse errors from the Mago parser now appear as Error-severity diagnostics instantly as you type.
- **Implementation error diagnostic.** Concrete classes that fail to implement all required methods from their interfaces or abstract parents are now flagged with an Error-severity diagnostic on the class name. The existing "Implement missing methods" quick-fix appears inline alongside the error. Cyclic hierarchies are handled gracefully.
- **Argument count diagnostic.** Flags function and method calls that pass too few arguments. The "too many arguments" check is off by default (PHP silently ignores extra arguments) and can be enabled with `extra-arguments = true` in the `[diagnostics]` section of `.phpantom.toml`. Variadic parameters and argument unpacking are handled correctly.
- **Change visibility.** Code action on any method, property, constant, or promoted constructor parameter offers to change its visibility (`public`, `protected`, `private`).
- **Update docblock.** Code action on a function or method whose existing docblock is out of sync with its signature. Adds missing `@param` tags, removes stale ones, reorders to match the signature, fixes contradicted types, and removes redundant `@return void`. Refinement types and unrelated tags are preserved. Handles `@param $name` (no type) correctly.
- **PHPDoc block generation.** Typing `/**` above any declaration generates a docblock skeleton. Tags are only emitted when the native type hint needs enrichment. Properties and constants always get `@var`. Class-likes with templated parents or interfaces get `@extends`/`@implements` tags. Uncaught exceptions get `@throws` with auto-import. Works both via completion and on-type formatting.
- **PHPDoc `@var` completion.** Inline `@var` above variable assignments sorts first and pre-fills the inferred type when available. Template parameters from `@template` enrich `@param`, `@return`, and `@var` type hints.
- **File rename on class rename.** Renaming a class whose file follows PSR-4 naming now also renames the file to match. The file is only renamed when it contains a single class-like declaration and the editor supports file rename operations.
- **`@see` and `@link` improvements.** `@see` references in docblocks now work with go-to-definition (class, member, and function forms). Hover popups show all `@link` and `@see` URLs as clickable links. Deprecation diagnostics include `@see` targets when the `@deprecated` docblock references them.
- **Progress indicators.** Go to Implementation and Find References now show a progress indicator in the editor while scanning.

### Changed

- **Embedded stubs track upstream master.** The bundled phpstorm-stubs are now pulled from the `master` branch instead of the latest GitHub release, matching what PHPStan does. This brings in upstream fixes and new PHP version annotations weeks or months before a formal release.
- **Version-aware stub types.** `#[LanguageLevelTypeAware]` attributes in phpstorm-stubs are now resolved against the project's PHP version. Functions, methods, parameters, and properties whose types changed across PHP versions (e.g. `int|false` in 7.x becoming `int` in 8.0) now show the correct type for your version. This eliminates false-positive diagnostics and incorrect completions from stale type annotations.

- **Pull diagnostics.** Diagnostics are now delivered via the LSP 3.17 pull model (`textDocument/diagnostic`) when the editor supports it. The editor requests diagnostics only for visible files, and cross-file invalidation uses `workspace/diagnostic/refresh` instead of recomputing every open tab. Clients without pull support fall back to the previous push model automatically.
- **Class name completion ranking.** Completions now rank by match quality first (exact match, then starts-with, then substring), so typing `Order` puts `Order` above `OrderLine` above `CheckOrderFlowJob` regardless of where the class comes from. Within each match quality group, use-imported and same-namespace classes appear first, followed by everything else sorted by namespace affinity (classes from heavily-imported namespaces rank higher).
- **Use-import completion.** Same-namespace classes no longer appear in `use` statement completions (PHP auto-resolves them without an import). Classes that are already imported are filtered out. Namespace affinity still ranks the remaining candidates.
- **Import class code action ordering.** The "Import Class" code action now sorts candidates by namespace affinity (derived from existing imports) instead of alphabetically, so the most likely namespace appears first.
- **Cross-file resolution.** Fully-qualified class names are now stored in a single canonical form, eliminating cases where completion, hover, or go-to-definition failed because one side had a leading backslash and the other did not.

### Fixed

- **Generic shape substitution.** Template parameters inside array shapes (`array{data: T}`) and object shapes (`object{name: T}`) are now correctly substituted when inherited through `@extends`. Previously only template parameters inside angle brackets were resolved, leaving bare references like `T` unsubstituted in shape bodies.
- **Array shape bracket access.** Variables assigned from string-key bracket access on array shapes (`$name = $data['name']`) now resolve to the correct value type. Chained access (`$first = $result['items'][0]`) walks through shape keys and generic element types in sequence. This fixes completion, hover, and go-to-definition for variables derived from array shape fields. Previously only direct `$data['key']->` subjects resolved; intermediate variable assignments lost the type.
- **Hover on array shape types.** Hovering over a variable whose type is an array shape (e.g. `array{data: User}`) no longer produces a corrupted `namespace array{...` line in the popup.
- **Eloquent `morphedByMany` relationships.** The inverse side of polymorphic many-to-many relationships (`$this->morphedByMany(...)`) is now recognised. Virtual properties and `_count` properties are synthesized for models using this relationship type.
- **Hover.** Hovering over unresolved function calls, unknown constants, or unresolvable `self`/`static`/`parent`/`$this` keywords no longer shows a bare placeholder. If the symbol cannot be found, no hover is shown.
- **Add @throws.** The code action no longer double-indents the closing `*/` when inserting a `@throws` tag into an existing multi-line docblock.
- **PHPStan stale-diagnostic clearing.** The `@throws`-based staleness check now scopes to the enclosing function's docblock instead of searching the entire file. A `@throws` tag on a different function no longer causes an unrelated diagnostic to be incorrectly cleared.
- **Closure and arrow function variable scope.** Variable name completion now correctly respects PHP scoping rules for anonymous functions and arrow functions. Parameters of a closure are visible inside its body, `use`-captured variables appear alongside them, and `$this` is available when the closure is defined in an instance method. Outer method locals that were not captured do not leak in. Arrow function parameters are now visible inside the arrow body while the enclosing scope's variables remain accessible, matching PHP's implicit capture behaviour.
- **Namespace alias completion.** Typing a class name through a namespace alias (e.g. `OA\Re` with `use OpenApi\Attributes as OA`) now correctly suggests classes under the aliased namespace such as `OA\Response` and `OA\RequestBody`. Previously only unrelated classes matched because the alias was not expanded before prefix matching.
- **Virtual property merging.** Native type hints are now considered when determining virtual property specificity. Previously only docblock types were compared, causing properties with native PHP type declarations (e.g., `public string $name`) to be incorrectly overridden by less specific virtual properties.
- **Native type override compatibility.** A docblock type only overrides a native type hint when it is a compatible refinement. For example, `class-string<Foo>` can refine `string` and `positive-int` can refine `int`, but `array<int>` no longer incorrectly overrides `string`. Previously any docblock type with generic parameters was accepted regardless of compatibility.
- **PHPStan pseudo-type recognition.** Types like `non-positive-int`, `non-negative-int`, `non-zero-int`, `lowercase-string`, `truthy-string`, `callable-object`, and many other PHPStan pseudo-types are now recognized across the entire pipeline. Previously they could be misresolved as class names, flagged as contradictions in docblock updates, or missing from PHPDoc completion suggestions.
- **Rename updates imports.** Renaming a class now updates `use` statement FQNs (last segment only), preserves explicit aliases, and introduces an alias when the new name collides with an existing import in the same file. Previously, `use` statements were left unchanged, breaking the file.
- **Trait alias go-to-definition.** Clicking a trait alias (e.g. `$this->__foo()` from `use Foo { foo as __foo; }`) now jumps to the trait method instead of the class's own same-named method.
- **Diagnostics.** Enums that implement interfaces are now checked for missing methods, matching the existing behaviour for concrete classes. Scalar member access errors now detect method-return chains where an intermediate call returns a scalar type. By-reference `@param` annotations no longer produce a false "unknown class" diagnostic. Duplicate diagnostics from different analysis phases are now reliably collapsed into a single entry per range. Deprecated-usage checks no longer block the instant diagnostic push.
- **Hover on empty arrays.** `[]` and `array()` literals now show `array` on hover instead of nothing.
- **Catch clause completion.** Throwable interfaces and abstract exception classes now appear in catch clause completions. Previously only concrete, non-abstract classes were offered.
- **Type-hint and PHPDoc completion.** Traits are now excluded from completions in parameter types, return types, property types, and PHPDoc type tags. `@throws` continues to use Throwable-filtered completion.
- **Position encoding.** All LSP position conversions now correctly count UTF-16 code units, matching the LSP specification. Files containing emoji or supplementary Unicode characters no longer produce incorrect positions for completions, hover, go-to-definition, references, highlights, or code actions.

## [0.5.0] - 2026-03-12

### Added

- **Find References.** Locate every usage of a symbol across the project. Supports classes, methods, properties, constants, functions, and variables. Variable references are scoped to the enclosing function or closure. Member references are scoped to the class hierarchy, so unrelated classes sharing a method name are excluded.
- **Rename.** Rename variables, classes, methods, properties, functions, and constants across the workspace. Variable renames are scoped to their enclosing function or closure. Symbols in vendor files are rejected. Non-renameable tokens (`$this`, `self`, `static`, `parent`) are rejected at the prepare step.
- **Document highlighting.** Placing the cursor on a symbol highlights all occurrences in the current file. Variables are scoped to their enclosing function or closure with write vs. read distinction.
- **Implement missing methods.** Code action that generates method stubs when a class is missing required interface or abstract method implementations. Handles deep inheritance chains, cross-file resolution, correct visibility and types, and respects the file's indentation style.
- **Reverse go-to-implementation.** Go-to-implementation on a concrete method jumps to the interface or abstract class that declares the prototype, and vice versa.
- **Go to Type Definition.** Jump from a variable, property, method call, or function call to the class declaration of its resolved type. Union types produce multiple locations.
- **Diagnostics.** Unknown classes, unknown members, and unknown functions are flagged with appropriate severity. Duplicate diagnostics on the same span are suppressed. An opt-in unresolved member access diagnostic is available via `.phpantom.toml`.
- **Deprecation support.** `@deprecated` tags and `#[Deprecated]` attributes surface in hover, completion strikethrough, and diagnostics. A quick-fix code action rewrites deprecated calls when a `replacement` template is available.
- **Project configuration.** `.phpantom.toml` for per-project settings: PHP version override, diagnostic toggles, and indexing strategy. Run `phpantom --init` to generate a default config.
- **Self-generated classmap.** PHPantom works without `composer dump-autoload -o`. Missing or incomplete classmaps are supplemented by scanning autoload directories. Non-Composer projects are supported by scanning all PHP files.
- **Non-Composer function and constant discovery.** Cross-file function completion, go-to-definition, and constant resolution for projects without `composer.json`.
- **Monorepo support.** Discovers subdirectories that are independent Composer projects and processes each through the full pipeline.
- **Indexing progress indicator.** The editor shows a progress bar during workspace initialization, including per-subproject progress in monorepos.
- **PHP version-aware stubs.** Detects the target PHP version from `composer.json` and filters built-in stub signatures accordingly.
- **`@param-closure-this`.** `$this` inside a closure resolves to the type declared by `@param-closure-this` on the receiving parameter.
- **Function-level `@template` with generic return types.** Functions like `collect()` that use `@template` parameters inside generic return types now resolve concrete types from call-site arguments.
- **`@implements` generic resolution.** `@implements Interface<ConcreteType>` substitutes template parameters on the interface's methods and properties. Foreach iteration on generic iterable interfaces resolves value and key types.
- **Interface template inheritance.** Implementing classes inherit `@template` parameters, bindings, conditional return types, and type assertions from their interfaces.
- **Generic `@phpstan-assert` with `class-string<T>`.** Assertion methods like `Assert::instanceOf($value, Foo::class)` resolve the narrowed type from the call-site argument.
- **Property-level narrowing.** `if ($this->prop instanceof Foo)` narrows `$this->prop` in then/else bodies and after guard clauses.
- **Inline `&&` short-circuit narrowing.** The right-hand side of `&&` now sees the narrowed type from the left-hand side.
- **Compound negated guard clause narrowing.** `if (!$x instanceof A && !$x instanceof B) { return; }` narrows `$x` to `A|B` in the surviving code.
- **Invoked closure and arrow function return types.** `(fn(): Foo => ...)()` and `(function(): Bar { ... })()` resolve to their return type.
- **`new $classStringVar` and `$classStringVar::method()`.** Class-string variables resolve for `new` and static member access.
- **`iterator_to_array()` element type.** Resolves the element type from the iterator's generic annotation.
- **Enum case properties.** `$case->name` and `$case->value` resolve on enum case variables.
- **Pass-by-reference parameter type inference.** After calling a function with a typed `&$var` parameter, the variable acquires that type.
- **Pipe operator (PHP 8.5).** `$input |> trim(...) |> createDate(...)` resolves through the chain.
- **Closure variable scope isolation.** Variables outside a closure are no longer offered as completions unless captured via `use()`.
- **AST-based array type inference.** Array shape keys, element access, spread elements, and push-style assignments all resolve through an AST walker.
- **Docblock navigation.** Go-to-definition and hover work on class names inside callable types, array/object shape value types, and object shape properties.
- **GTD from parameter and property variables.** Clicking a parameter or property at its definition site jumps to the type hint class.
- **Inline `@var` on promoted constructor properties.** Overrides the native type hint, matching existing `@param` support.
- **`--version` and `--help` CLI flags.** Contributed by [@calebdw](https://github.com/calebdw) in [#7](https://github.com/AJenbo/phpantom_lsp/pull/7).

### Changed

- **Resolution engine rewritten on AST.** Variable type inference, subject dispatch, call return types, and go-to-definition all run through the AST walker. The text-based scanner has been removed entirely.
- **Hover redesigned.** Short names with `namespace` line, actual default values, `@link` URLs, precise token highlighting, constructor signatures on `new`, `@template` details, enum case listing, trait member listing, origin indicators, and deprecated explanations.
- **Signature help enriched.** Compact parameter list with native types, per-parameter `@param` descriptions, default values, and attribute parenthesis support.
- **Faster resolution and lower memory usage.** O(1) class resolution, per-request caching, hash-set deduplication, reference-counted file content, async diagnostics with 500 ms debounce, and signature-aware cache invalidation.
- **Two-phase diagnostic publishing.** Cheap diagnostics (unused imports, deprecation) publish immediately; expensive diagnostics (unknown classes/members/functions) arrive in a second pass.
- **Concurrent read access.** All read-heavy maps use `parking_lot::RwLock` for parallel request handling.
- **Parallel workspace indexing.** File parsing, PSR-4 scanning, and vendor scanning run across all CPU cores. `.gitignore` rules are respected. `memchr` SIMD acceleration for the byte-level scanner.
- **Merged classmap + self-scan pipeline.** Composer classmaps and self-scanning work together instead of being mutually exclusive. Stale classmaps are supplemented automatically.
- **Automatic stub fetching.** The build script downloads phpstorm-stubs automatically when missing. Composer is no longer needed to build PHPantom. Contributed by [@calebdw](https://github.com/calebdw) in [#16](https://github.com/AJenbo/phpantom_lsp/pull/16).
- **Feature comparison table corrected.** Phactor capabilities updated in the README. Contributed by [@dantleech](https://github.com/dantleech) in [#10](https://github.com/AJenbo/phpantom_lsp/pull/10).

### Fixed

- **Go-to-definition on trait `as` alias and `insteadof` declarations.** Method names, alias names, and trait names inside trait use adaptation blocks now resolve correctly.
- **Parallel file scanner panics no longer crash the server.**
- **Type alias array shape diagnostics no longer fire on object values.**
- **Inline array-element function calls resolve correctly in diagnostics.** `end($obj->items)->method()` no longer produces a false "unknown member" diagnostic.
- **Eloquent Builder scope chain diagnostics no longer flicker.**
- **Diagnostics refresh across open files when a class signature changes.**
- **Unknown member diagnostics on property and method return chains.**
- **Variable types resolve through ternary, elvis, null-coalesce, and match assignments.**
- **Parameter types resolve inside `function_exists` guards.**
- **Virtual property merging picks the most specific type.**
- **Custom cast classes declared as string literals resolve correctly.**
- **`@implements CastsAttributes<T>` takes priority over `get()` return type.**
- **Editing a cast class now updates model property types.**
- **Go-to-definition for variables captured via `use`.**
- **Closure parameter inference inside namespaces and across files.**
- **Signature help no longer fires inside closure/arrow function bodies or function definitions.**
- **Signature help parameter type display with parenthesized callable unions.**
- **`__invoke()` return type resolution.** Works with chaining, foreach, and parenthesized invocations.
- **Enum `from()` and `tryFrom()` chaining.**
- **Nested closures with reused parameter names no longer crash.**
- **Scope methods on Builder variables.**
- **`static`/`self`/`$this` in method return types used as iterable expressions.**
- **`instanceof` narrowing no longer widens specific types.**
- **Closure parameter with bare type hint inherits inferred generics.**
- **Closure parameter with parent type hint narrows to inferred subclass.**
- **Cross-file inheritance from global-scope classes imported via `use`.**
- **Model `@method` tags available on Builder instances.**
- **Arrow function outer-scope variable resolution and parameter completion.**
- **Inherited `@method` and `@property` tags.**
- **Elseif chain narrowing and sequential assert narrowing.**
- **First-open performance.** Diagnostics on `did_open` run asynchronously.
- **Variadic `@param` template bindings.**
- **Laravel relationship classification with non-Eloquent namespaces.**
- **Trait `use` no longer triggers false-positive unused import.**
- **PHPDoc types on constructor-promoted properties now recognised.**
- **PHPDoc type tags no longer skipped by unused-import safety net.**
- **`@phpstan-type` aliases in foreach, `list()`, and key types.**
- **Mixed `->` then `::` accessor chains.**
- **Inline `(new Foo)->method()` chaining.**
- **Literal string conditional return types.**
- **Class constant and enum case assignment resolution.**
- **False-positive unknown-class warnings on PHPStan type syntax.**
- **Go-to-implementation no longer produces false positives across namespaces.**
- **Named-argument resolution for non-variable subjects.**
- **"Remove all unused imports" only offered on `use` import lines.**
- **GTD for `@method`/`@property` on interfaces.**
- **`?->` null-safe chain resolution.**
- **`(new Canvas())->easel` property access resolution.**
- **Array function resolution for `array_pop`, `array_filter`, `array_values`, `end`, `array_map`.**
- **Hover on variable definition sites no longer shows redundant popups.**
- **Inline `@var` annotations no longer leak across scopes.**
- **Docblock tag parsing in description text.**
- **Double-negated `instanceof` narrowing.**
- **Accessor on new line with whitespace.**
- **Partial static property completion.**
- **Hover respects `instanceof`, `assert`, and inline `@var` narrowing.**
- **`instanceof` narrowing with same-named classes in different namespaces.**
- **Self-referential array key assignments no longer crash the LSP.**
- **Cross-file `@property` and `@method` type resolution.**
- **Editing a `@property` docblock now invalidates hover in other files.**
- **Vendor class resolution simplified.** Composer classmap is the sole source of truth for vendor code.

## [0.4.0] - 2026-03-01

### Added

- **Signature help.** Parameter hints in function/method calls with active parameter highlighting.
- **Hover.** Type, signature, and docblock in a Markdown popup for all symbol kinds.
- **Closure and callable inference.** Untyped closure parameters inferred from the callable signature. First-class callable syntax resolves return types.
- **Laravel Eloquent.** Relationships, scopes, Builder forwarding, factories, custom collections, casts, accessors, mutators, `$attributes`, and `$visible`.
- **Type narrowing.** `in_array()` with strict mode, early return guards, `instanceof` in ternaries and with interfaces.
- **Anonymous class support.** `$this->` resolves inside anonymous classes with full inheritance support.
- **Context-aware completions.** `extends`, `implements`, `use` inside class body, union member sorting, namespace segments, string literal suppression.
- **Additional resolution.** Multi-line chains, nested array keys, generator yield types, conditional return types with template substitution, switch/unset variable tracking.
- **Transitive interface go-to-implementation.**

### Fixed

- Visibility filtering, scope isolation, static call chains, `static` return type, trait resolution, mixin fluent chains, go-to-definition accuracy, import handling, UTF-8 boundaries, and parenthesized RHS expressions.

## [0.3.0] - 2026-02-21

### Added

- **Go-to-implementation.** Interface/abstract class to all concrete implementations.
- **Method-level `@template`.** Infers `T` from the call-site argument.
- **`@phpstan-type` / `@psalm-type` aliases** and `@phpstan-import-type`.
- **Array function type preservation.** `array_filter`, `array_map`, `array_pop`, `current`, etc.
- **Early return narrowing.** Guard clauses narrow types for subsequent code.
- **Callable variable invocation.** `$fn()->` resolves return types.
- **Additional resolution.** Spread operators, trait `insteadof`/`as`, chained assignments, destructuring, foreach on function returns, type hint completion, try-catch suggestions.

### Fixed

- PHPDoc type parsing and internal stability fixes.

## [0.2.0] - 2026-02-18

### Added

- **Generics.** Class-level `@template` with `@extends` substitution. Method-level `class-string<T>`. Generic trait substitution.
- **Array shapes and object shapes.** Key completion from literals, incremental assignments, destructuring, element access.
- **Foreach type resolution.** Generic iterables, array shapes, `Collection<User>`, `Generator<int, Item>`, `IteratorAggregate`.
- **Expression type inference.** Ternary, null-coalescing, and match expressions.
- **Additional completions.** Named arguments, variable name suggestions, standalone functions, `define()` constants, PHPDoc tags, deprecated members, promoted property types, property chaining, `require_once` discovery, go-to type definition.

### Fixed

- `@mixin` context for return types, global class imports, namespace resolution, and aliased class go-to-definition.

## [0.1.0] - 2026-02-16

Initial release.

### Added

- **Completion.** Methods, properties, and constants via `->`, `?->`, and `::` with visibility filtering.
- **Type resolution.** Inheritance merging, `self`/`static`/`parent`, union types, nullsafe chains.
- **PHPDoc support.** `@return`, `@property`, `@method`, `@mixin`, conditional return types, inline `@var`.
- **Type narrowing.** `instanceof`, `is_a()`, `@phpstan-assert`.
- **Enum support.** Case completion and `UnitEnum`/`BackedEnum` interface members.
- **Go-to-definition.** Classes, methods, properties, constants, functions, `new` expressions, variables.
- **Class name completion with auto-import.**
- **PSR-4 lazy loading and Composer classmap support.**
- **Embedded phpstorm-stubs.**
- **Zed editor extension.**

[Unreleased]: https://github.com/AJenbo/phpantom_lsp/compare/0.5.0...HEAD
[0.5.0]: https://github.com/AJenbo/phpantom_lsp/compare/0.4.0...0.5.0
[0.4.0]: https://github.com/AJenbo/phpantom_lsp/compare/0.3.0...0.4.0
[0.3.0]: https://github.com/AJenbo/phpantom_lsp/compare/0.2.0...0.3.0
[0.2.0]: https://github.com/AJenbo/phpantom_lsp/compare/0.1.0...0.2.0
[0.1.0]: https://github.com/AJenbo/phpantom_lsp/commits/0.1.0
