# Changelog

All notable changes to PHPantom will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **Virtual member provider abstraction.** Introduced the `VirtualMemberProvider` trait and `VirtualMembers` struct in a new `virtual_members` module. This provides a priority-ordered pipeline for synthesizing members from `@method`/`@property` tags, `@mixin` classes, and framework-specific patterns (e.g. Laravel relationships, scopes, Builder forwarding). All completion and go-to-definition call sites now use the new `resolve_class_fully` entry point, which applies base inheritance resolution followed by virtual member providers. No providers are registered yet, so behavior is unchanged. This is the foundation for upcoming Laravel support.
- **Union completion sorting.** When a variable has a union type (e.g. `Dog|Cat` from a match or ternary), members shared by all types in the union (the intersection) now sort above members that only exist on a subset of types. Branch-only members display their originating class name as a label detail suffix, giving an at-a-glance visual hint in the completion popup. No completions are removed. Single-type completions are unaffected.
- **Context-aware class name filtering.** Class name completions are now filtered by syntactic context. `extends` in a class declaration only offers non-final classes, `extends` in an interface only offers interfaces, `implements` offers interfaces only, `use` inside a class body offers traits only, and `instanceof` excludes traits. Constants and functions are also suppressed in these positions. Multi-line declarations and comma-separated lists are handled. Built-in (stub) classes are filtered correctly even before they are fully parsed, using a lightweight source scanner that detects the declaration keyword from the raw PHP source already in memory. Classmap entries whose source is not yet loaded pass through the filter since their kind is unknown.
- **First-class callable syntax.** PHP 8.1's `strlen(...)`, `$obj->method(...)`, and `ClassName::method(...)` syntax now resolves correctly. The variable holding the callable is typed as `Closure` (for `$fn->bindTo()` etc.), and invoking it with `$fn()` resolves to the underlying function or method's return type. Works with function references, instance methods, static methods, `$this->method(...)`, `self::method(...)`, chained calls on the result, and cross-file resolution.
- **Multi-line method chain completion.** Fluent chains spanning multiple lines now produce completions and support go-to-definition. Continuation lines starting with `->` or `?->` are joined with preceding lines before subject extraction, so builder patterns, query chains, and collection pipelines work seamlessly.
- **Template parameter bound resolution.** When a property or variable type is a `@template` parameter (e.g. `TNode`), the resolver falls back to the upper bound declared via `of` (e.g. `@template TNode of SomeClass`) for completion and go-to-definition.
- **Transitive interface inheritance in go-to-implementation.** If `InterfaceB extends InterfaceA` and `ClassC implements InterfaceB`, go-to-implementation on `InterfaceA` now finds `ClassC`. Works through arbitrary depth and with interfaces that extend multiple parents.
- **Switch statement variable type tracking.** Variables assigned inside `switch` case bodies now resolve their types. Both brace-delimited and colon-delimited (`switch(): … endswitch;`) forms are supported, and all cases contribute to a union type.
- **`unset()` variable tracking.** After `unset($var)`, the variable no longer appears in name suggestions and `$var->` does not resolve to its previous type. Re-assignment after `unset` restores the variable with the new type. Conditional `unset` (inside `if` blocks) is handled conservatively, keeping the variable because it might still exist.
- **Class-string variable forwarding to conditional return types.** When a variable holds `::class` values from a `match` expression, ternary, or simple assignment (e.g. `$cls = match (...) { 'a' => A::class, 'b' => B::class }`) and is passed to a function or method with `@template T` + `@param class-string<T>` + `@return T`, the resolver traces the class-string back through the variable and produces a union of all possible return types. Works for instance methods, static methods, standalone functions, and inline chains.
- **Trait property @var docblock type resolution.** When a trait defines a property with a `@var` docblock type, and a class uses that trait (possibly through inheritance), chaining on `$this->prop` resolves to the docblock type and offers its members.
- **Anonymous class completion and go-to-definition.** `$this->` inside anonymous class bodies (`new class { ... }`) now resolves to the anonymous class's own members, properties, and constants. Supports `extends`, `implements`, trait `use`, constructor-promoted properties, and `@var` docblock types. Anonymous classes nested inside named class methods, closures, control flow blocks, function arguments, and return statements are all detected. `find_class_at_offset` now picks the innermost (most specific) class when scopes overlap.
- **Alphabetical `use` statement insertion.** Auto-imported `use` statements are now inserted at the correct alphabetical position among existing imports instead of being appended at the bottom of the use block. This keeps the import list sorted, matching the convention expected by PSR-12, Symfony, and Laravel coding standards.
- **Namespace segment completion.** When typing a namespace-qualified reference (`use App\`, `new \Illuminate\`, `\App\` in a type hint, etc.), completion now shows the next-level namespace segments as navigable MODULE-kind items alongside matching classes. Segments sort above class items so the user can drill into deep namespace trees incrementally instead of being overwhelmed by hundreds of flat class names. Works in `use` statements, `new` expressions, type hints, docblocks, `instanceof`, `extends`, `implements`, and trait `use` contexts. Segments respect the typed partial (e.g. `App\M` shows `App\Models` but not `App\Services`), simplify within the current namespace, and carry correct leading-backslash handling. In `use` import context, segments do not receive a trailing semicolon.
- **String-aware completion suppression.** Completion is now suppressed inside string literals (single-quoted strings, nowdocs, and plain text in double-quoted strings and heredocs) to reduce noise from accidental triggers. Interpolation contexts are detected and allowed through: `{$expr->}` brace interpolation and simple `"$var->"` property access both produce completions as expected. Array shape key completion (`$arr['`) is unaffected.
- **Nested key completion for literal arrays.** When a variable is assigned a literal array with nested associative arrays (e.g. `$cfg = ['db' => ['host' => 'x', 'port' => 3306]]`), completing `$cfg['db']['` now offers the nested keys. Previously only a `@var array{...}` docblock produced nested completions. Works at arbitrary nesting depth, inside class methods, and at the top level.
- **Generator yield type inference inside generator bodies.** When a function or method declares `@return Generator<TKey, TValue, TSend, TReturn>`, variables inside the generator body are now typed from the annotation. Variables that appear as the operand of a `yield` statement are inferred as TValue (the 2nd generic parameter), and variables assigned from a yield expression (`$var = yield $expr`) are inferred as TSend (the 3rd generic parameter). Works in class methods, top-level functions, key-value yields (`yield $k => $v`), cross-file resolution via PSR-4, and all four Generator type parameter arities. Explicit assignments still take priority over yield inference.

### Fixed

- **Namespaced functions treated as global.** Functions declared inside a namespace (e.g. `Illuminate\Support\enum_value`) previously appeared in completion as bare `enum_value`, and accepting them produced `use function enum_value;` instead of `use function Illuminate\Support\enum_value;`. The completion builder is now namespace-aware. In `use function` context the insert text is the FQN. In inline code the short name is inserted and a `use function FQN;` auto-import is added via `additional_text_edits`, mirroring how class auto-import works. The detail field shows the namespace (e.g. `function (Illuminate\Support)`) so users can distinguish same-named functions from different namespaces. Functions in the same namespace as the current file do not receive an auto-import. Short-name collisions between namespaces are eliminated: deduplication now uses the FQN, and the redundant short-name fallback entry in `global_functions` has been removed.
- **Docblock annotation scope leak across sibling methods.** `find_iterable_raw_type_in_source` could pick up a `@param` or `@var` annotation from a sibling method when the parameter or variable shared the same name. For example, if `fromParam(array $config)` had a `@param array{host: string} $config` annotation and a sibling `nestedLiteral()` method also used a local `$config`, the annotation from `fromParam` would shadow the local assignment. The backward scanner now tracks when it has exited the current scope and re-entered a sibling scope, skipping all annotations from that point on.
- **Variable property access in text-based resolution.** `$var->propName` on the RHS of an assignment now resolves correctly in the text-based fallback path. Previously only `$this->propName` was handled, so intermediate assignments like `$addr = $user->address; $addr->` would fail to produce completions when the AST-based path did not cover the case. The resolver now recursively resolves the variable's type and looks up the property on the resulting class. Works with `new ClassName()`, `$this->prop`, chained intermediate variables, cross-file types, and top-level code.
- **Parameter type resolution scoped to the correct method.** When multiple methods in the same class had a parameter with the same name but different types (e.g. `handleGsmp(IShoppingCart $cart)` and `updateCartData(ShoppingCart $cart)`), the resolver would return the type from whichever method appeared first in source order, ignoring which method the cursor was actually inside. The resolver now checks whether the cursor falls within each method's body and skips methods that do not contain the cursor.
- **Array element access from assignments.** `$var[0]->` now resolves the element type when `$var` was assigned from a method call returning a typed array (e.g. `@return User[]` or `@return array<int, User>`). Both direct access (`$items[0]->`) and intermediate assignments (`$first = $items[0]; $first->`) work. Multi-line call chains like `(new Foo())\n->getItems()` no longer fail due to trailing whitespace in the text-based resolution path.
- **Go-to-definition for static properties and typed constants via `::`.** `ClassName::$staticProp` now jumps to the property declaration. Previously the `$` prefix caused it to be misidentified as a local variable, bypassing member access resolution. Also fixed `find_member_position` failing on PHP 8.3 typed constants (`const string NAME = ...`) where a type between `const` and the name broke the pattern match. Works same-file, cross-file, and with `self::`/`static::`.
- **`static` return type resolved to concrete class at call sites.** When a method declares `@return static` and is called on a subclass variable, the resolver now returns the caller's concrete class rather than the declaring (parent) class. Chained fluent calls preserve the subclass through multiple `static` returns.
- **Namespaced FQN return types no longer break chain resolution.** `clean_type` now preserves the leading `\` on fully-qualified names so that `resolve_type_string` does not incorrectly prepend the current file's namespace. Cross-file FQN return types (e.g. `@return \Illuminate\Database\Eloquent\Builder`) resolve correctly regardless of the caller's namespace.
- **Parenthesized RHS expressions now resolved.** Assignments like `$var = (new Foo())` and `$var = ($cond ? $a : $b)` now resolve correctly through the AST path. Previously the `Expression::Parenthesized` wrapper was not unwrapped in `resolve_rhs_expression`.
- **`$var::` completion for class-string variables.** When a variable holds a class-string (e.g. `$cls = User::class`), using `$cls::` now offers the referenced class's static members, constants, and static properties. Handles `self::class`, `static::class`, `parent::class`, and unions from match/ternary/null-coalescing expressions.
- **`?->` chaining fallback now recurses correctly.** The `?->` fallback branch in subject extraction called `extract_simple_variable` instead of `extract_arrow_subject`. The primary `->` branch already handled `?->` chains correctly via a `?` skip, so this was not user-visible, but the fallback is now consistent.
- **Go-to-definition for inherited members when child and parent share the same short name.** When a child class and its parent have the same simple name in different namespaces (e.g. `App\Console\Kernel extends Illuminate\Foundation\Console\Kernel`), go-to-definition on inherited members like `$this->load()` now correctly jumps to the parent file. Previously `find_class_file_content` matched by short name only, found the child class first, and returned the wrong file. The function is now namespace-aware and `find_declaring_class` propagates the FQN used to load each ancestor.
- **Go-to-definition on foreach variable no longer jumps to a previous loop.** When the same variable name appeared in consecutive `foreach` loops, clicking `$b` in the second loop's `as $b` clause incorrectly jumped to the first loop. The resolver now checks whether the cursor line itself defines the variable and returns `None` immediately, letting the caller fall through to type-hint resolution instead of scanning backwards past the current definition site.
- **Multi-extends interfaces now fully stored.** Interfaces extending multiple parents (e.g. `interface C extends A, B`) now store all parent names, not just the first one.
- **Conflicting use-import resolution.** When auto-importing a class whose short name collides with an existing `use` statement (e.g. accepting `App\Exception` while `use Cassandra\Exception;` is already present), the LSP now inserts a fully-qualified reference (`\App\Exception`) at the usage site instead of adding a duplicate `use` statement that would cause a compile error. Applies to all completion sources (class index, classmap, stubs) and to catch clause completions.

### Changed

- **Mixin resolution extracted to `MixinProvider`.** The `@mixin` merge logic (`merge_mixins_into` / `merge_mixins_into_recursive`) has been moved from `inheritance.rs` into a new `MixinProvider` in `virtual_members/mixin.rs`, implementing the `VirtualMemberProvider` trait. `resolve_class_with_inheritance` now performs base resolution only (own + traits + parent chain), and mixin members are applied lazily through the provider pipeline in `resolve_class_fully`. No user-facing behavior changes. This is groundwork for the upcoming PHPDoc and Laravel providers.
- **`@method`/`@property` tags extracted to `PHPDocProvider`.** `@method`, `@property`, `@property-read`, and `@property-write` tags are no longer parsed eagerly during AST extraction. Instead, the raw class-level docblock is preserved on `ClassInfo.class_docblock` and parsed lazily by a new `PHPDocProvider` in `virtual_members/phpdoc.rs`. This places virtual PHPDoc members at the correct precedence (below real declared members, traits, and parents; above `@mixin`) and allows future framework providers to override them with richer type information. No user-facing behavior changes.

## [0.3.0] - 2026-02-21

### Added

- **Go-to-implementation.** Jump from an interface or abstract class (or a method call typed as one) to all concrete implementations. Scans open files, class index, classmap, embedded stubs, and PSR-4 directories in five phases.
- **Method-level `@template` (general case).** When a method declares `@template T` with `@param T $model` and `@return Collection<T>`, the resolver infers `T` from the actual argument at the call site. Works with inline chains, static methods, `new` expressions, and cross-file resolution.
- **`@phpstan-type` / `@psalm-type` aliases.** Local type aliases defined on a class are expanded during resolution, including `@phpstan-import-type` for importing aliases from other classes.
- **Array function type preservation.** `array_filter`, `array_map`, `array_pop`, `current`, and similar functions preserve the element type instead of losing it to `array`.
- **Spread operator type tracking.** `$all = [...$users, ...$admins]` resolves to the union of element types from all spread sources.
- **Callable/closure variable invocation.** `$fn()->` resolves the return type when `$fn` holds a closure, arrow function, or a variable annotated as `Closure(...): T`.
- **Early return narrowing.** Guard clauses (`if (!$x instanceof Foo) return;`) narrow the type for subsequent code. Multiple guards stack. Works in ternaries and `match(true)`.
- **`instanceof` narrowing to interface and abstract types.**
- **`instanceof` narrowing inside ternary expressions.**
- **Trait `insteadof` / `as` conflict resolution.** Visibility changes and method aliasing via `as`, exclusion via `insteadof`.
- **Generics tracked through loop iterations.**
- **Yield type resolution.**
- **Chained method calls in variable assignment.** `$x = $this->foo()->bar()` resolves through the full chain.
- **Named key destructuring from array shapes.** `['name' => $name] = $shape` resolves `$name` to the correct type.
- **Type hint completion in function/method signatures.**
- **Variable and clone assignment type tracking.**
- **Iterate directly on function return values.** `foreach (getUsers() as $user)` resolves `$user`.
- **User constant completion.**
- **Required argument completion.**
- **Contextual try-catch completion.** Exception suggestions are scoped to what the `try` block can actually throw.
- **Void detection for `@return` PHPDoc suggestions.**

### Fixed

- More robust PHPDoc type parsing.
- Fixed false positive type lookups for internal stubs.
- Fixed crash in variable resolver.
- Fixed incorrect method resolution.
- Fixed finding definitions inside comments.
- Fixed use of incorrect import map for name resolution.
- Fixed completion suggestions being too aggressive or appearing in comments.

## [0.2.0] - 2026-02-18

### Added

- **Generics.** Class-level `@template` with `@extends` substitution through inheritance chains. Method-level `class-string<T>` pattern. Generic trait substitution.
- **Array shapes.** `['key' => Type]` literals offer key completion with no annotation needed. Incremental assignments extend the shape.
- **Object shapes.**
- **Array growth tracking.** `$arr[] = new Foo()` and `$arr['key'] = $value` build up the shape incrementally.
- **Array destructuring.** `[$a, $b] = $pair` resolves element types.
- **Array element access.** `$arr[0]->` resolves the element type.
- **Foreach key type resolution.** Keys from generic iterables and array shapes.
- **Iterable value type resolution.** Foreach on `Collection<User>`, `Generator<int, Item>`, and `@implements IteratorAggregate<int, User>`.
- **Ternary and null-coalescing type resolution.**
- **Match expression type inference.**
- **Named argument completion.**
- **Variable name suggestions.**
- **Standalone function completion.**
- **`define()` constant completion.**
- **Smart PHPDoc tag completion.** Tags filtered to context (`@var` only in property docblocks, `@param` only when there are undocumented parameters). `@throws` detects uncaught exceptions. `@param` pre-fills name and type. Already-documented tags are not suggested again.
- **Deprecated member detection.**
- **Promoted property type via `@param`.**
- **Property chaining.**
- **`require_once` function discovery.**
- **Go-to type definition from property.**

### Fixed

- Fixed `@mixin` context for return types.
- Fixed import of global classes and namespace context.
- Fixed go-to-definition for aliased classes.

## [0.1.0] - 2026-02-16

Initial release.

### Added

- **Completion.** Methods, properties, and constants via `->`, `?->`, and `::`. Context-aware visibility filtering. Alphabetically ordered results.
- **Class inheritance.** Parent classes, interfaces, and traits with correct member merging.
- **`self::`, `static::`, `parent::` resolution.**
- **PHPDoc support.** `@return` type resolution, `@property` virtual properties, `@method` virtual methods, `@mixin` class merging.
- **Conditional return types.** `@return ($param is class-string<T> ? T : mixed)` and similar PHPStan-style conditional types.
- **Inline `@var` annotations.** `/** @var User $user */` resolves the variable type.
- **Enum support.** Case completion, implicit `UnitEnum`/`BackedEnum` interface members.
- **Type narrowing.** `instanceof`, `is_a()`, and `@phpstan-assert` annotations.
- **Nullsafe operator.** `?->` completion.
- **Class name completion with auto-import.** Suggests class names and inserts the `use` statement.
- **Union type inference.**
- **Go-to-definition.** Classes, interfaces, traits, enums, methods, properties, constants, standalone functions, `new` expressions, and variable assignments.
- **PSR-4 lazy loading** via Composer.
- **Composer classmap support.**
- **Embedded phpstorm-stubs.** Standard library type information bundled in the binary.
- **Namespace aliasing and prefix imports.**
- **Zed editor extension.**

[Unreleased]: https://github.com/AJenbo/phpantom_lsp/compare/0.3.0...HEAD
[0.3.0]: https://github.com/AJenbo/phpantom_lsp/compare/0.2.0...0.3.0
[0.2.0]: https://github.com/AJenbo/phpantom_lsp/compare/0.1.0...0.2.0
[0.1.0]: https://github.com/AJenbo/phpantom_lsp/commits/0.1.0