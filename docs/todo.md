# PHPantom — Roadmap

This document tracks planned work for PHPantom. Each item links to a
domain document with full context. Items are grouped into time-boxed
sprints (roughly 1-2 weeks each) and a backlog of ideas not yet
scheduled.

**Guiding priorities:** Completion accuracy → Type intelligence →
Cross-file navigation → Diagnostics → Code actions → Performance.

Items inside each sprint are ordered by priority (top = do first).
The backlog is ordered by impact (descending), then effort (ascending)
within the same impact tier.

| Label      | Scale                                                                                                                  |
| ---------- | ---------------------------------------------------------------------------------------------------------------------- |
| **Impact** | **Critical**, **High**, **Medium-High**, **Medium**, **Low-Medium**, **Low**                                           |
| **Effort** | **Low** (≤ 1 day), **Medium** (2-5 days), **Medium-High** (1-2 weeks), **High** (2-4 weeks), **Very High** (> 1 month) |

# Scheduled Sprints

## Sprint 4 — Refactoring toolkit

| #   | Item                                                                               | Impact | Effort |
| --- | ---------------------------------------------------------------------------------- | ------ | ------ |
| R1  | [Extract cursor-context AST helper](todo/refactor.md#r1-cursor-context-ast-helper) | —      | Low    |
| A2  | [Extract function](todo/actions.md#a2-extract-function-refactoring)                | High   | High   |
| A5  | [Extract variable](todo/actions.md#a5-extract-variable)                            | Medium | Medium |
| A4  | [Inline variable](todo/actions.md#a4-inline-variable)                              | Medium | Medium |
| A6  | [Generate constructor](todo/actions.md#a6-generate-constructor)                    | Medium | Medium |
| A7  | [Promote constructor parameter](todo/actions.md#a7-promote-constructor-parameter)  | Medium | Low    |
|     | **Release 0.7.0**                                                                  |        |        |

## Sprint 4a — Mago foundation: Composer + Docblock + Names

| #   | Item                                                        | Impact | Effort      |
| --- | ----------------------------------------------------------- | ------ | ----------- |
|     | Clear [refactoring gate](todo/refactor.md)                  | —      | —           |
| M1  | [Migrate to mago-composer](todo/mago.md#m1-mago-composer)   | Low    | Low         |
| M2  | [Migrate to mago-docblock](todo/mago.md#m2-mago-docblock)   | High   | Medium      |
| M3  | [Migrate to mago-names](todo/mago.md#m3-mago-names)         | High   | Medium-High |

## Sprint 4b — Mago foundation: Type Syntax

| #   | Item                                                              | Impact   | Effort    |
| --- | ----------------------------------------------------------------- | -------- | --------- |
|     | Clear [refactoring gate](todo/refactor.md)                        | —        | —         |
| M4  | [Migrate to mago-type-syntax](todo/mago.md#m4-mago-type-syntax)  | Critical | Very High |

## Sprint 5 — Polish for office adoption

| #   | Item                                                       | Impact | Effort |
| --- | ---------------------------------------------------------- | ------ | ------ |
|     | Clear [refactoring gate](todo/refactor.md)                 | —      | —      |
| D4  | Unused variable warning                                    | Medium | Medium |
|     | **Release 0.8.0**                                          |        |        |

> **Note:** F1 (Workspace symbol search), F2 (Document symbols), A8
> (Implement interface methods), A9 (Auto import), D1 (Unknown class
> diagnostic), and D3 (Unknown member diagnostic) were originally
> planned here but have already shipped.

## Sprint 6 — Type intelligence depth

| #   | Item                                                                                                                       | Impact      | Effort |
| --- | -------------------------------------------------------------------------------------------------------------------------- | ----------- | ------ |
|     | Clear [refactoring gate](todo/refactor.md)                                                                                 | —           | —      |
| C2  | [`LanguageLevelTypeAware` version-aware type hints](todo/completion.md#c2-languageleveltypeaware-version-aware-type-hints) | Medium-High | Medium |
| C3  | [`#[ArrayShape]` return shapes on stub functions](todo/completion.md#c3-arrayshape-return-shapes-on-stub-functions)        | Medium      | Medium |
| T1  | [First-class callable resolution](todo/type-inference.md#t1-first-class-callables)                                         | Medium      | Medium |
| T2  | [`@phpstan-type` / `@psalm-type` local type aliases](todo/type-inference.md#t2-local-type-aliases)                         | Medium      | Medium |
| T3  | [`@phpstan-import-type` cross-file type imports](todo/type-inference.md#t3-cross-file-type-imports)                        | Medium      | Medium |

## Sprint 7 — Laravel excellence & stub accuracy

| #   | Item                                                                                          | Impact      | Effort      |
| --- | --------------------------------------------------------------------------------------------- | ----------- | ----------- |
|     | Clear [refactoring gate](todo/refactor.md)                                                    | —           | —           |
| L1  | [Eloquent model attribute completion](todo/laravel.md#l1-eloquent-model-attribute-completion) | High        | High        |
| L5  | [Blade component tag completion](todo/laravel.md#l5-blade-component-tags)                     | Medium      | Medium-High |
| E1  | [External stub packages (ide-helper, etc.)](todo/external-stubs.md#e1-external-stub-packages) | Medium-High | Medium      |
| E4  | [Stub version alignment with target PHP](todo/external-stubs.md#e4-stub-version-alignment)    | Medium      | Medium      |
| E5  | [Extension stub coverage audit](todo/external-stubs.md#e5-extension-stub-audit)               | Medium      | Low         |

## Sprint 8 — Blade support

| #   | Item                                             | Impact | Effort    |
| --- | ------------------------------------------------ | ------ | --------- |
|     | Clear [refactoring gate](todo/refactor.md)       | —      | —         |
| BL1 | [Blade template language support](todo/blade.md) | High   | Very High |

The Blade sprint is a placeholder. Scope will be refined after Sprint 7
ships. The goal is to provide a single-binary PHP + Blade language
server with Blade intelligence.

# Backlog

Items not yet assigned to a sprint. Worth doing eventually but
unlikely to move the needle for most users.

| #   | Item                                                                                                                                                         | Impact      | Effort      |
| --- | ------------------------------------------------------------------------------------------------------------------------------------------------------------ | ----------- | ----------- |
|     | **[Bugs](todo/bugs.md)**                                                                                                                                     |             |             |
| B4  | [Variable reassignment loses type when parameter name is reused](todo/bugs.md#b4-variable-reassignment-loses-type-when-parameter-name-is-reused)             | Medium      | Medium      |
| B7  | [Overloaded built-in function signatures in stubs](todo/bugs.md#b7-overloaded-built-in-function-signatures-in-stubs)                                         | Low         | Low         |
|     | **[Completion](todo/completion.md)**                                                                                                                         |             |             |
| C1  | Array functions needing new code paths                                                                                                                       | Medium     | High        |
| C10 | [Lazy documentation via `completionItem/resolve`](todo/completion.md#c10-lazy-documentation-via-completionitemresolve)                                        | Medium     | Medium      |
| C12 | [Smarter member ordering after `->` / `::`](todo/completion.md#c12-smarter-member-ordering-after----)                                                        | Medium     | Medium      |
| C4  | Go-to-definition for array shape keys via bracket access                                                                                                     | Low-Medium | Medium      |
| C8  | `class_alias()` support                                                                                                                                      | Low-Medium | Medium      |
| C9  | [Filesystem proximity as an affinity tiebreaker](todo/completion.md#c9-filesystem-proximity-as-an-affinity-tiebreaker)                                        | Low-Medium | Low         |
| C5  | Non-array functions with dynamic return types                                                                                                                | Low        | High        |
| C6  | `#[ReturnTypeContract]` parameter-dependent return types                                                                                                     | Low        | Low         |
| C7  | `#[ExpectedValues]` parameter value suggestions                                                                                                              | Low        | Medium      |
| C11 | [Deprecation markers on class-name completions from all sources](todo/completion.md#c11-deprecation-markers-on-class-name-completions-from-all-sources)       | Low        | Low         |
|     | **[Type Inference](todo/type-inference.md)**                                                                                                                 |            |             |
| T7  | [`key-of<T>` and `value-of<T>` resolution](todo/type-inference.md#t7-key-oft-and-value-oft-resolution)                                                       | Medium     | Medium      |
| T6  | `Closure::bind()` / `Closure::fromCallable()` return type preservation                                                                                       | Low-Medium | Low-Medium  |
| T8  | [Null coalesce (`??`) type refinement](todo/type-inference.md#t8-null-coalesce--type-refinement)                                                              | Low-Medium | Low         |
| T4  | Non-empty-\* type narrowing and propagation                                                                                                                  | Low        | Low         |
| T5  | Fiber type resolution                                                                                                                                        | Low        | Low         |
| T9  | [Dead-code elimination after `never`-returning calls](todo/type-inference.md#t9-dead-code-elimination-after-never-returning-calls)                            | Low        | Low-Medium  |
| T10 | [Ternary expression as RHS of list destructuring](todo/type-inference.md#t10-ternary-expression-as-rhs-of-list-destructuring)                                 | Low        | Low-Medium  |
| T11 | [Nested list destructuring](todo/type-inference.md#t11-nested-list-destructuring)                                                                             | Low        | Low-Medium  |
|     | **[Diagnostics](todo/diagnostics.md)**                                                                                                                       |            |             |
| D8  | [Undefined variable diagnostic](todo/diagnostics.md#d8-undefined-variable-diagnostic)                                                                        | High       | Medium      |
| D2  | Chain error propagation (flag only the first broken link)                                                                                                    | Medium     | Medium      |
| D5  | Diagnostic suppression intelligence                                                                                                                          | Medium     | Medium      |
| D11 | [Invalid class-like kind in context](todo/diagnostics.md#d11-invalid-class-like-kind-in-context)                                                             | Medium     | Low         |
| D6  | [Unreachable code diagnostic](todo/diagnostics.md#d6-unreachable-code-diagnostic)                                                                            | Low-Medium | Low         |
| D10 | PHPMD diagnostic proxy                                                                                                                                       | Low        | Medium      |
|     | **[Code Actions](todo/actions.md)**                                                                                                                          |            |             |
| A1  | [Simplify with null coalescing / null-safe operator](todo/actions.md#a1-simplify-with-null-coalescing--null-safe-operator)                                    | Medium     | Medium      |
| A5  | [Extract variable](todo/actions.md#a5-extract-variable)                                                                                                      | Medium     | Medium      |
| A7  | [Extract constant](todo/actions.md#a7-extract-constant)                                                                                                      | Medium     | Medium      |
| A8  | [Update docblock to match signature](todo/actions.md#a8-update-docblock-to-match-signature)                                                                  | Medium     | Medium      |
| A6  | [Inline function/method](todo/actions.md#a6-inline-functionmethod)                                                                                           | Medium     | High        |
| A11 | [ScopeCollector infrastructure](todo/actions.md#a11-scopecollector-infrastructure)                                                                            | Medium     | Medium      |
| A10 | Generate interface from class                                                                                                                                | Low-Medium | Medium      |
| A3  | Switch → match conversion                                                                                                                                    | Low        | Medium      |
|     | **[PHPStan Code Actions](todo/phpstan-actions.md)**                                                                                                          |            |             |
| P1  | `new.static` — add `final` or `@phpstan-consistent-constructor`                                                                                              | Medium     | Low         |
| P2  | `method.missingOverride` — add `#[Override]`                                                                                                                 | Medium     | Low         |
| P3  | `method.override` / `property.override` — remove `#[Override]`                                                                                               | Medium     | Low         |
| P5  | `method.tentativeReturnType` — add `#[\ReturnTypeWillChange]`                                                                                                | Medium     | Low         |
| P7  | `return.phpDocType` — fix `@return` to match native type                                                                                                     | Medium     | Low         |
| P8  | `parameter.phpDocType` — fix `@param` to match native type                                                                                                   | Medium     | Low         |
| P9  | `property.phpDocType` — fix property docblock type                                                                                                           | Medium     | Low         |
| P11 | `method.visibility` / `property.visibility` — fix overriding visibility                                                                                      | Medium     | Low         |
| P12 | `class.prefixed` — fix prefixed class name                                                                                                                   | Medium     | Low         |
| P4  | `assign.byRefForeachExpr` — unset by-reference foreach variable                                                                                              | Medium     | Medium      |
| P6  | `return.type` — update return type to match actual returns                                                                                                   | Medium     | Medium      |
| P10 | `return.unusedType` — remove unused type from return union                                                                                                   | Medium     | Medium      |
| P13 | `property.notFound` — declare missing property (same-class)                                                                                                  | Medium     | Medium      |
| P14 | `throws.unusedType` — narrow `@throws` to actual thrown types                                                                                                | Medium     | Medium      |
| P15 | Template bound from tip — add `@template T of X`                                                                                                             | Medium     | Medium      |
| P16 | `match.unhandled` — add missing match arms                                                                                                                   | Medium     | Medium      |
| P17 | `missingType.iterableValue` — add `@return` with inferred element type                                                                                       | Medium     | High        |
| P18 | `deadCode.unreachable` — remove unreachable code                                                                                                             | Low        | Low         |
| P19 | `property.unused` / `method.unused` — remove unused member                                                                                                   | Low        | Low         |
| P20 | `generics.callSiteVarianceRedundant` — remove redundant variance annotation                                                                                  | Low        | Low         |
| P21 | `return.void` — remove return value from void function                                                                                                       | Low        | Low         |
| P22 | `return.empty` — add return value or change return type to void                                                                                              | Low        | Low         |
| P23 | `instanceof.alwaysTrue` — remove redundant instanceof check                                                                                                  | Low        | Low         |
| P24 | `catch.neverThrown` — remove unnecessary catch clause                                                                                                        | Low        | Low         |
|     | **[LSP Features](todo/lsp-features.md)**                                                                                                                     |            |             |
| F2  | [Partial result streaming via `$/progress`](todo/lsp-features.md#f2-partial-result-streaming-via-progress)                                                   | Medium     | Medium-High |
| F3  | Incremental text sync                                                                                                                                        | Low-Medium | Medium      |
|     | **[Signature Help](todo/signature-help.md)**                                                                                                                 |            |             |
| S1  | [Attribute constructor signature help](todo/signature-help.md#s1-attribute-constructor-signature-help)                                                        | Medium     | Medium      |
| S2  | [Closure / arrow function parameter signature help](todo/signature-help.md#s2-closure--arrow-function-parameter-signature-help)                               | Medium     | Medium      |
| S3  | Multiple overloaded signatures                                                                                                                               | Medium     | Medium-High |
| S4  | Named argument awareness in active parameter                                                                                                                 | Low-Medium | Medium      |
| S5  | Language construct signature help and hover                                                                                                                  | Low        | Low         |
|     | **[Laravel](todo/laravel.md)**                                                                                                                               |            |             |
| L4  | Custom Eloquent builders (`HasBuilder` / `#[UseEloquentBuilder]`)                                                                                            | Medium     | Medium      |
| L2  | [`morphedByMany` missing from relationship method map](todo/laravel.md#l2-morphedbymany-missing-from-relationship-method-map)                                | Low-Medium | Low         |
| L3  | `$dates` array (deprecated)                                                                                                                                  | Low-Medium | Low         |
| L6  | Factory `has*`/`for*` relationship methods                                                                                                                   | Low-Medium | Medium      |
| L7  | `$pivot` property on BelongsToMany                                                                                                                           | Medium     | Medium-High |
| L8  | `withSum`/`withAvg`/`withMin`/`withMax` aggregate properties                                                                                                 | Low-Medium | Medium-High |
| L9  | Higher-order collection proxies                                                                                                                              | Low-Medium | Medium-High |
| L10 | `View::withX()` / `RedirectResponse::withX()` dynamic methods                                                                                                | Low        | Low         |
| L11 | `$appends` array                                                                                                                                             | Low        | Low         |
| L12 | [`auth()` helper return type](todo/laravel.md#l12-auth-helper-return-type)                                                                                   | Low-Medium | Low         |
| L13 | [`app()->make()` container return type](todo/laravel.md#l13-appmake-container-return-type)                                                                   | Low        | Medium      |
|     | **[External Stubs](todo/external-stubs.md)**                                                                                                                 |            |             |
| E2  | Project-level stubs as type resolution source                                                                                                                | Medium     | Medium      |
| E3  | IDE-provided and `.phpantom.toml` stub paths                                                                                                                 | Low-Medium | Low         |
| E6  | Stub install prompt for non-Composer projects                                                                                                                | Low        | Low         |
|     | **[Performance](todo/performance.md)**                                                                                                                       |            |             |
| P1.5 | [Layered class resolution (zero-copy inheritance)](todo/performance.md#p15-layered-class-resolution-zero-copy-inheritance)                                   | High       | Very High   |
| P13 | [Tiered storage: drop per-file maps for non-open files](todo/performance.md#p13-tiered-storage-drop-per-file-maps-for-non-open-files)                        | Medium-High| Medium-High |
| P9  | [`resolved_class_cache` generic-arg specialisation](todo/performance.md#p9-resolved_class_cache-generic-arg-specialisation)                                  | Medium     | Medium      |
| P14 | [Eager docblock parsing into structured fields](todo/performance.md#p14-eager-docblock-parsing-into-structured-fields)                                       | Medium     | Medium      |
| P10 | [Redundant `parse_and_cache_file` from multiple threads](todo/performance.md#p10-redundant-parse_and_cache_file-from-multiple-threads)                       | Medium     | Low         |
| P2  | Type AST for `apply_substitution` (full refactor)                                                                                                            | Medium     | High        |
| P11 | [Uncached base-resolution in `build_scope_methods_for_builder`](todo/performance.md#p11-uncached-base-resolution-in-build_scope_methods_for_builder)          | Low-Medium | Low         |
| P3  | Parallel pre-filter in `find_implementors`                                                                                                                   | Low-Medium | Medium      |
| P1a | `type_hint_to_classes` returns `Vec<Arc<ClassInfo>>`                                                                                                         | Low        | Low         |
| P1b | Propagate `Arc<ClassInfo>` through variable-resolution pipeline                                                                                              | Low        | Medium      |
| P4  | `memmem` for block comment terminator search                                                                                                                 | Low        | Low         |
| P5  | `memmap2` for file reads during scanning                                                                                                                     | Low        | Low         |
| P6  | O(n²) transitive eviction in `evict_fqn`                                                                                                                     | Low        | Low         |
| P7  | `diag_pending_uris` uses `Vec::contains` for dedup                                                                                                           | Low        | Low         |
| P8  | `find_class_in_ast_map` linear fallback scan                                                                                                                 | Low        | Low         |
| P12 | [`find_or_load_function` Phase 1.75 serial bottleneck](todo/performance.md#p12-find_or_load_function-phase-175-serial-bottleneck)                            | Low        | Low         |
|     | **[Indexing](todo/indexing.md)**                                                                                                                             |            |             |
| X1  | Staleness detection and auto-refresh                                                                                                                         | Medium     | Medium      |
| X3  | Completion item detail on demand (`completionItem/resolve`)                                                                                                  | Medium     | Medium      |
| X7  | [Recency tracking](todo/indexing.md#x7-recency-tracking)                                                                                                     | Medium     | Medium      |
| X2  | Parallel file processing — remaining work                                                                                                                    | Low-Medium | Medium      |
| X5  | Granular progress reporting for indexing, GTI, and Find References                                                                                           | Low-Medium | Medium      |
| X4  | Full background indexing (`strategy = "full"`)                                                                                                               | Medium     | High        |
| X8  | [Phar archive class resolution](todo/indexing.md#x8-phar-archive-class-resolution)                                                                           | Medium     | Medium      |
| X6  | Disk cache (evaluate later)                                                                                                                                  | Medium     | High        |
|     | **[Inline Completion](todo/inline-completion.md)**                                                                                                           |            |             |
| N1  | Template engine (type-aware snippets)                                                                                                                        | Medium     | High        |
| N2  | N-gram prediction from PHP corpus                                                                                                                            | Medium     | Very High   |
| N3  | Fine-tuned GGUF sidecar model                                                                                                                                | Medium     | Very High   |
|     | **[Phpactor Test Parity](todo/phpactor-test-parity.md)**                                                                                                     |            |             |
| Q1  | `is_callable()` / `is_float()` narrowing fixtures                                                                                                            | Low        | Low         |
| Q2  | `in_array()` with class constants in haystack                                                                                                                | Low        | Low         |
| Q3  | Elseif with terminating statement (`die`/`throw`)                                                                                                            | Low        | Low         |
| Q4  | Else-branch assignment union merging                                                                                                                         | Low        | Low         |
| Q5  | Combined negated type guards (`!is_string && !instanceof`)                                                                                                   | Low        | Low         |
| Q6  | Namespace-qualified instanceof in or-chains                                                                                                                  | Low        | Low         |
| Q7  | Ternary assignment producing union type                                                                                                                      | Low        | Low         |
| Q8  | Cast expression type resolution (`(string)`, `(int)`, …)                                                                                                     | Low        | Low         |
| Q9  | Variadic parameter type inside function body                                                                                                                 | Low        | Low         |
| Q10 | `list<T>` type alias resolution                                                                                                                              | Low        | Low         |
| Q11 | `string\|false` return type                                                                                                                                   | Low        | Low         |
| Q12 | Union from relative docblock class names                                                                                                                     | Low        | Low         |
| Q13 | Callable / Closure docblock param types                                                                                                                      | Low        | Low-Medium  |
| Q14 | `int<min,max>` range types                                                                                                                                   | Low        | Low-Medium  |
| Q15 | Parenthesized union types with narrowing                                                                                                                     | Low        | Low-Medium  |
| Q16 | Variable-variable `${$bar}` resolution                                                                                                                       | Low        | Low-Medium  |
| Q17 | `global` keyword variable access                                                                                                                             | Low        | Low-Medium  |
| Q18 | `define()` constant type resolution                                                                                                                          | Low        | Low-Medium  |
| Q19 | Array mutation tracking (`$arr[] = …`)                                                                                                                       | Low        | Medium      |
| Q20 | Return statement body type inference                                                                                                                         | Low        | Medium      |
| Q21 | Binary expression type inference (arithmetic, concat, …)                                                                                                     | Low        | High        |
| Q22 | Postfix increment / decrement literal types                                                                                                                  | Low        | Low         |
