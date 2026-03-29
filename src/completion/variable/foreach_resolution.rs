use mago_span::HasSpan;
use mago_syntax::ast::*;
/// Foreach and destructuring variable type resolution.
///
/// This submodule handles resolving types for variables that appear as:
///
///   - **Foreach value/key variables:** `foreach ($items as $key => $item)`
///     where the iterated expression has a generic iterable type annotation.
///   - **Array/list destructuring:** `[$a, $b] = getUsers()` or
///     `['name' => $name] = $data` where the RHS has a generic iterable
///     or array shape type annotation.
///
/// These functions are self-contained: they receive a [`VarResolutionCtx`]
/// and push resolved [`ResolvedType`] values into a results vector.  They were
/// extracted from `variable_resolution.rs` to improve navigability.
use std::sync::Arc;

use crate::docblock;
use crate::types::{ClassInfo, ResolvedType};
use crate::util::short_name;

use crate::completion::resolver::{Loaders, VarResolutionCtx};

/// Resolve an expression's type string via the unified pipeline.
///
/// Wraps `resolve_rhs_expression` + `type_strings_joined` into a single
/// `Option<String>` suitable for callers that previously used
/// `extract_rhs_iterable_raw_type`.  Returns `None` when the unified
/// pipeline produces no results or an empty type string.
pub(in crate::completion) fn resolve_expression_type_string<'b>(
    expr: &'b mago_syntax::ast::Expression<'b>,
    ctx: &VarResolutionCtx<'_>,
) -> Option<String> {
    let resolved = super::rhs_resolution::resolve_rhs_expression(expr, ctx);
    if resolved.is_empty() {
        return None;
    }
    let ts = ResolvedType::type_strings_joined(&resolved);
    if ts.is_empty() { None } else { Some(ts) }
}

// ─── Foreach Resolution ─────────────────────────────────────────────

/// Try to resolve the foreach value variable's type from a generic
/// iterable annotation on the iterated expression.
///
/// When the variable being resolved (`ctx.var_name`) matches the
/// foreach value variable and the iterated expression is a simple
/// `$variable` whose type is annotated as a generic iterable (via
/// `@var list<User> $var` or `@param list<User> $var`), this function
/// extracts the element type and pushes the resolved `ResolvedType` into
/// `results`.
pub(in crate::completion) fn try_resolve_foreach_value_type<'b>(
    foreach: &'b Foreach<'b>,
    ctx: &VarResolutionCtx<'_>,
    results: &mut Vec<ResolvedType>,
    conditional: bool,
) {
    // Check if the foreach value variable is the one we're resolving.
    let value_expr = foreach.target.value();
    let value_var_name = match value_expr {
        Expression::Variable(Variable::Direct(dv)) => dv.name.to_string(),
        _ => return,
    };
    if value_var_name != ctx.var_name {
        return;
    }

    // ── Check for a `/** @var Type $var */` docblock directly on the
    //    foreach value variable ──────────────────────────────────────
    //
    // Example:
    //   /** @var Foobar $foobar */
    //   foreach ($collection as $foobar) { $foobar-> }
    //
    // The docblock annotates the value variable itself, overriding
    // whatever the collection's element type would be.
    let foreach_offset = foreach.foreach.span().start.offset as usize;
    if let Some((var_type, var_name)) =
        crate::docblock::find_inline_var_docblock(ctx.content, foreach_offset)
    {
        // The docblock must either have no variable name (applies to the
        // next statement) or name the foreach value variable explicitly.
        // Both `var_name` (from the docblock) and `value_var_name` (from
        // the AST) include the `$` prefix, so compare them directly.
        let name_matches = var_name.as_ref().is_none_or(|n| *n == value_var_name);
        if name_matches {
            let resolved = crate::completion::type_resolution::type_hint_to_classes(
                &var_type,
                &ctx.current_class.name,
                ctx.all_classes,
                ctx.class_loader,
            );
            if !resolved.is_empty() {
                let resolved_types = ResolvedType::from_classes_with_hint(resolved, &var_type);
                if conditional {
                    ResolvedType::extend_unique(results, resolved_types);
                } else {
                    results.clear();
                    ResolvedType::extend_unique(results, resolved_types);
                }
                return;
            }
        }
    }

    // Try to resolve the iterable type from the foreach expression
    // via the unified pipeline.  Filter out bare class names (no
    // generics, array suffix, or shape) so that the class-based
    // fallback can resolve generics through @implements / @extends
    // annotations (e.g. `Collection` → `Collection<int, Customer>`
    // via closure parameter inference).
    let raw_type = resolve_expression_type_string(foreach.expression, ctx)
        .filter(|ts| ts.contains('<') || ts.contains('[') || ts.contains('{'))
        .or_else(|| {
            // Fallback 1: for simple `$variable` expressions, search backward
            // from the foreach for @var or @param annotations.
            let expr_span = foreach.expression.span();
            let expr_start = expr_span.start.offset as usize;
            let expr_end = expr_span.end.offset as usize;
            let expr_text = ctx.content.get(expr_start..expr_end)?.trim();

            if !expr_text.starts_with('$') || expr_text.contains("->") || expr_text.contains("::") {
                return None;
            }

            let foreach_offset = foreach.foreach.span().start.offset as usize;
            docblock::find_iterable_raw_type_in_source(ctx.content, foreach_offset, expr_text)
        })
        .or_else(|| {
            // Fallback 2: for simple `$variable` expressions, resolve the
            // variable's type from its assignment (e.g.
            // `$items = Country::cases();` → `Country[]`).
            // This covers cases where the iterable type comes from a method
            // return type or other expression rather than a docblock.
            let expr_span = foreach.expression.span();
            let expr_start = expr_span.start.offset as usize;
            let expr_end = expr_span.end.offset as usize;
            let expr_text = ctx.content.get(expr_start..expr_end)?.trim();

            if !expr_text.starts_with('$') || expr_text.contains("->") || expr_text.contains("::") {
                return None;
            }

            let foreach_offset = foreach.foreach.span().start.offset;
            let resolved = super::resolution::resolve_variable_types(
                expr_text,
                ctx.current_class,
                ctx.all_classes,
                ctx.content,
                foreach_offset,
                ctx.class_loader,
                Loaders::with_function(ctx.function_loader()),
            );
            if resolved.is_empty() {
                None
            } else {
                let ts = ResolvedType::type_strings_joined(&resolved);
                // If the resolved type is a bare class name (no generics,
                // array suffix, or shape), return None so that the
                // class-based fallback can resolve generics through
                // @implements / @extends annotations.
                if !ts.contains('<') && !ts.contains('[') && !ts.contains('{') {
                    None
                } else {
                    Some(ts)
                }
            }
        });

    // ── Expand type aliases before extracting generic element type ──
    // When the raw type is a type alias (e.g. `UserList` defined via
    // `@phpstan-type UserList array<int, User>`), expand it so that
    // `extract_generic_value_type` can see the underlying generic type.
    let raw_type = raw_type.map(|rt| {
        crate::completion::type_resolution::resolve_type_alias(
            &rt,
            &ctx.current_class.name,
            ctx.all_classes,
            ctx.class_loader,
        )
        .unwrap_or(rt)
    });

    // Extract the generic element type (e.g. `list<User>` → `User`).
    if let Some(ref rt) = raw_type {
        let parsed = crate::php_type::PhpType::parse(rt);
        if let Some(element_type) = parsed.extract_value_type(true) {
            let element_str = element_type.to_string();
            push_foreach_resolved_types(&element_str, ctx, results, conditional);
            return;
        }
    }

    // ── Fallback: resolve the iterated expression to ClassInfo and
    //    extract the value type from its generic annotations ─────────
    //
    // This handles cases where the iterated expression resolves to a
    // concrete collection class (e.g. `$items = new UserCollection()`)
    // whose `@extends` or `@implements` annotations carry the generic
    // type parameters, but no inline `@var` annotation is present.
    //
    // Also handles the case where a method/property returns a class
    // name like `PaymentOptionLocaleCollection` without generic syntax
    // in the return type string.
    let iterable_classes = if let Some(ref rt) = raw_type {
        // raw_type is a class name like "PaymentOptionLocaleCollection"
        // (extract_generic_value_type returned None above).
        crate::completion::type_resolution::type_hint_to_classes(
            rt,
            &ctx.current_class.name,
            ctx.all_classes,
            ctx.class_loader,
        )
        .into_iter()
        .map(Arc::new)
        .collect()
    } else {
        // No raw type at all — resolve the foreach expression as a
        // subject string via variable / assignment scanning.
        resolve_foreach_expression_to_classes(foreach.expression, ctx)
    };

    for cls in &iterable_classes {
        let merged = crate::virtual_members::resolve_class_fully_maybe_cached(
            cls,
            ctx.class_loader,
            ctx.resolved_class_cache,
        );
        if let Some(value_type) =
            extract_iterable_element_type_from_class(&merged, ctx.class_loader)
        {
            push_foreach_resolved_types(&value_type, ctx, results, conditional);
            return;
        }
    }
}

/// Try to resolve the foreach **key** variable's type from a generic
/// iterable annotation on the iterated expression.
///
/// When the variable being resolved (`ctx.var_name`) matches the
/// foreach key variable and the iterated expression is a simple
/// `$variable` whose type is annotated as a two-parameter generic
/// iterable (via `@var array<Request, Response> $var` or similar),
/// this function extracts the key type and pushes the resolved
/// `ResolvedType` into `results`.
///
/// For common scalar key types (`int`, `string`), no `ClassInfo` is
/// produced — which is correct because scalars have no members to
/// complete on.
pub(in crate::completion) fn try_resolve_foreach_key_type<'b>(
    foreach: &'b Foreach<'b>,
    ctx: &VarResolutionCtx<'_>,
    results: &mut Vec<ResolvedType>,
    conditional: bool,
) {
    // Check if the foreach has a key variable and if it matches what
    // we're resolving.
    let key_expr = match foreach.target.key() {
        Some(expr) => expr,
        None => return,
    };
    let key_var_name = match key_expr {
        Expression::Variable(Variable::Direct(dv)) => dv.name.to_string(),
        _ => return,
    };
    if key_var_name != ctx.var_name {
        return;
    }

    // Try to resolve the iterable type from the foreach expression
    // via the unified pipeline.  Same bare-class-name filter as the
    // value-type path above.
    let raw_type = resolve_expression_type_string(foreach.expression, ctx)
        .filter(|ts| ts.contains('<') || ts.contains('[') || ts.contains('{'))
        .or_else(|| {
            // Fallback 1: for simple `$variable` expressions, search backward
            // from the foreach for @var or @param annotations.
            let expr_span = foreach.expression.span();
            let expr_start = expr_span.start.offset as usize;
            let expr_end = expr_span.end.offset as usize;
            let expr_text = ctx.content.get(expr_start..expr_end)?.trim();

            if !expr_text.starts_with('$') || expr_text.contains("->") || expr_text.contains("::") {
                return None;
            }

            let foreach_offset = foreach.foreach.span().start.offset as usize;
            docblock::find_iterable_raw_type_in_source(ctx.content, foreach_offset, expr_text)
        })
        .or_else(|| {
            // Fallback 2: for simple `$variable` expressions, resolve the
            // variable's type from its assignment (e.g.
            // `$items = Country::cases();` → `Country[]`).
            // This covers cases where the iterable type comes from a method
            // return type or other expression rather than a docblock.
            let expr_span = foreach.expression.span();
            let expr_start = expr_span.start.offset as usize;
            let expr_end = expr_span.end.offset as usize;
            let expr_text = ctx.content.get(expr_start..expr_end)?.trim();

            if !expr_text.starts_with('$') || expr_text.contains("->") || expr_text.contains("::") {
                return None;
            }

            let foreach_offset = foreach.foreach.span().start.offset;
            let resolved = super::resolution::resolve_variable_types(
                expr_text,
                ctx.current_class,
                ctx.all_classes,
                ctx.content,
                foreach_offset,
                ctx.class_loader,
                Loaders::with_function(ctx.function_loader()),
            );
            if resolved.is_empty() {
                None
            } else {
                let ts = ResolvedType::type_strings_joined(&resolved);
                // If the resolved type is a bare class name (no generics,
                // array suffix, or shape), return None so that the
                // class-based fallback can resolve generics through
                // @implements / @extends annotations.
                if !ts.contains('<') && !ts.contains('[') && !ts.contains('{') {
                    None
                } else {
                    Some(ts)
                }
            }
        });

    // ── Expand type aliases before extracting generic key type ──
    // Same as the value-type path: when the raw type is a type alias
    // (e.g. `UserList` defined via `@phpstan-type UserList array<int, User>`),
    // expand it so that `extract_generic_key_type` can see the underlying
    // generic type.
    let raw_type = raw_type.map(|rt| {
        crate::completion::type_resolution::resolve_type_alias(
            &rt,
            &ctx.current_class.name,
            ctx.all_classes,
            ctx.class_loader,
        )
        .unwrap_or(rt)
    });

    // Extract the generic key type (e.g. `array<Request, Response>` → `Request`).
    if let Some(ref rt) = raw_type {
        let parsed = crate::php_type::PhpType::parse(rt);
        if let Some(key_type) = parsed.extract_key_type(true) {
            let key_str = key_type.to_string();
            push_foreach_resolved_types(&key_str, ctx, results, conditional);
            return;
        }
    }

    // ── Fallback: resolve the iterated expression to ClassInfo and
    //    extract the key type from its generic annotations ───────────
    let iterable_classes = if let Some(ref rt) = raw_type {
        crate::completion::type_resolution::type_hint_to_classes(
            rt,
            &ctx.current_class.name,
            ctx.all_classes,
            ctx.class_loader,
        )
        .into_iter()
        .map(Arc::new)
        .collect()
    } else {
        resolve_foreach_expression_to_classes(foreach.expression, ctx)
    };

    for cls in &iterable_classes {
        let merged = crate::virtual_members::resolve_class_fully_maybe_cached(
            cls,
            ctx.class_loader,
            ctx.resolved_class_cache,
        );
        if let Some(key_type) = extract_iterable_key_type_from_class(&merged, ctx.class_loader) {
            push_foreach_resolved_types(&key_type, ctx, results, conditional);
            return;
        }
    }
}

/// Push resolved foreach element types into the results list.
///
/// Shared by both value and key foreach resolution paths: resolves a
/// type string to `ResolvedType`(s) and merges them into `results`.
fn push_foreach_resolved_types(
    type_str: &str,
    ctx: &VarResolutionCtx<'_>,
    results: &mut Vec<ResolvedType>,
    conditional: bool,
) {
    let resolved = crate::completion::type_resolution::type_hint_to_classes(
        type_str,
        &ctx.current_class.name,
        ctx.all_classes,
        ctx.class_loader,
    );

    if resolved.is_empty() {
        return;
    }

    let resolved_types = ResolvedType::from_classes_with_hint(resolved, type_str);
    if !conditional {
        results.clear();
    }
    ResolvedType::extend_unique(results, resolved_types);
}

/// Resolve the foreach iterated expression to `ClassInfo`(s).
///
/// Extracts the source text of the expression and resolves it using
/// `resolve_target_classes`, which handles `$variable`, `$this->prop`,
/// method calls, etc.
fn resolve_foreach_expression_to_classes<'b>(
    expression: &'b Expression<'b>,
    ctx: &VarResolutionCtx<'_>,
) -> Vec<Arc<ClassInfo>> {
    let expr_span = expression.span();
    let expr_start = expr_span.start.offset as usize;
    let expr_end = expr_span.end.offset as usize;
    let expr_text = match ctx.content.get(expr_start..expr_end) {
        Some(t) => t.trim(),
        None => return vec![],
    };

    if expr_text.is_empty() {
        return vec![];
    }

    crate::completion::resolver::resolve_target_classes(
        expr_text,
        crate::types::AccessKind::Arrow,
        &ctx.as_resolution_ctx(),
    )
}

/// Known interface/class names whose generic parameters describe
/// iteration types in PHP's `foreach`.
const ITERABLE_IFACE_NAMES: &[&str] = &[
    "Iterator",
    "IteratorAggregate",
    "Traversable",
    "ArrayAccess",
    "Enumerable",
];

/// Extract the iterable **value** (element) type from a class's generic
/// annotations.
///
/// When a collection class like `PaymentOptionLocaleCollection` has
/// `@extends Collection<int, PaymentOptionLocale>` or
/// `@implements IteratorAggregate<int, PaymentOptionLocale>`, this
/// function returns `Some("PaymentOptionLocale")`.
///
/// Checks (in order of priority):
/// 1. `implements_generics` for known iterable interfaces
/// 2. `extends_generics` for any parent with generic type args
///
/// Returns `None` when no generic iterable annotation is found or
/// when the element type is a scalar (scalars have no completable
/// members).
fn extract_iterable_element_type_from_class(
    class: &ClassInfo,
    class_loader: &dyn Fn(&str) -> Option<Arc<ClassInfo>>,
) -> Option<String> {
    // 1. Check implements_generics for known iterable interfaces.
    for (name, args) in &class.implements_generics {
        let short = short_name(name);
        if ITERABLE_IFACE_NAMES.contains(&short) && !args.is_empty() {
            let value = args.last().unwrap();
            if !docblock::types::is_scalar(value) {
                return Some(value.clone());
            }
        }
    }

    // 1b. Check implements_generics for interfaces that transitively
    //     extend a known iterable interface (e.g. `TypedCollection`
    //     extends `IteratorAggregate`).
    for (name, args) in &class.implements_generics {
        let short = short_name(name);
        if !ITERABLE_IFACE_NAMES.contains(&short)
            && !args.is_empty()
            && let Some(iface) = class_loader(name)
            && is_transitive_iterable(&iface, class_loader)
        {
            let value = args.last().unwrap();
            if !docblock::types::is_scalar(value) {
                return Some(value.clone());
            }
        }
    }

    // 2. Check extends_generics — common for collection subclasses
    //    like `@extends Collection<int, User>`.
    for (_, args) in &class.extends_generics {
        if !args.is_empty() {
            let value = args.last().unwrap();
            if !docblock::types::is_scalar(value) {
                return Some(value.clone());
            }
        }
    }

    None
}

/// Extract the iterable **key** type from a class's generic annotations.
///
/// For two-parameter generics (e.g. `@implements ArrayAccess<int, User>`),
/// returns the first parameter (`"int"`).
///
/// Returns `None` when no suitable annotation is found or when only a
/// single type parameter is present (single-param generics have an
/// implicit `int` key which is scalar).
fn extract_iterable_key_type_from_class(
    class: &ClassInfo,
    class_loader: &dyn Fn(&str) -> Option<Arc<ClassInfo>>,
) -> Option<String> {
    // 1. Check implements_generics for known iterable interfaces.
    for (name, args) in &class.implements_generics {
        let short = short_name(name);
        if ITERABLE_IFACE_NAMES.contains(&short) && args.len() >= 2 {
            let key = &args[0];
            if !docblock::types::is_scalar(key) {
                return Some(key.clone());
            }
        }
    }

    // 1b. Check implements_generics for interfaces that transitively
    //     extend a known iterable interface.
    for (name, args) in &class.implements_generics {
        let short = short_name(name);
        if !ITERABLE_IFACE_NAMES.contains(&short)
            && args.len() >= 2
            && let Some(iface) = class_loader(name)
            && is_transitive_iterable(&iface, class_loader)
        {
            let key = &args[0];
            if !docblock::types::is_scalar(key) {
                return Some(key.clone());
            }
        }
    }

    // 2. Check extends_generics.
    for (_, args) in &class.extends_generics {
        if args.len() >= 2 {
            let key = &args[0];
            if !docblock::types::is_scalar(key) {
                return Some(key.clone());
            }
        }
    }

    None
}

/// Check whether an interface transitively extends a known iterable
/// interface (e.g. `TypedCollection extends IteratorAggregate`).
fn is_transitive_iterable(
    iface: &ClassInfo,
    class_loader: &dyn Fn(&str) -> Option<Arc<ClassInfo>>,
) -> bool {
    // Check direct interfaces.
    for parent in &iface.interfaces {
        let s = short_name(parent);
        if ITERABLE_IFACE_NAMES.contains(&s) {
            return true;
        }
    }
    // Check extends_generics for the interface-extends-interface pattern.
    for (name, _) in &iface.extends_generics {
        let s = short_name(name);
        if ITERABLE_IFACE_NAMES.contains(&s) {
            return true;
        }
    }
    // Check parent class (interfaces use `parent_class` for extends).
    if let Some(ref parent_name) = iface.parent_class {
        let s = short_name(parent_name);
        if ITERABLE_IFACE_NAMES.contains(&s) {
            return true;
        }
        if let Some(parent) = class_loader(parent_name) {
            return is_transitive_iterable(&parent, class_loader);
        }
    }
    false
}

// ─── Destructuring Resolution ───────────────────────────────────────

/// Check whether the target variable appears inside an array/list
/// destructuring LHS and, if so, resolve its type from the RHS's
/// generic element type or array shape entry.
///
/// Supported patterns:
///   - `[$a, $b] = getUsers()`           — function call RHS (generic)
///   - `list($a, $b) = $users`           — variable RHS with `@var`/`@param`
///   - `[$a, $b] = $this->m()`           — method/static-method call RHS
///   - `['user' => $p] = $data`          — named key from array shape
///   - `[0 => $first, 1 => $second] = $data` — numeric key from array shape
///
/// When the RHS type is an array shape (`array{key: Type, …}`), the
/// destructured variable's key is matched against the shape entries.
/// For positional (value-only) elements, the 0-based index is used as
/// the key.  Falls back to `extract_generic_value_type` for generic
/// iterable types (`list<User>`, `array<int, User>`, `User[]`).
pub(in crate::completion) fn try_resolve_destructured_type<'b>(
    assignment: &'b Assignment<'b>,
    ctx: &VarResolutionCtx<'_>,
    results: &mut Vec<ResolvedType>,
    conditional: bool,
) {
    // ── 1. Collect the elements from the LHS ────────────────────────
    let elements = match assignment.lhs {
        Expression::Array(arr) => &arr.elements,
        Expression::List(list) => &list.elements,
        _ => return,
    };

    // ── 2. Find our target variable and extract its destructuring key
    //
    // For `KeyValue` elements like `'user' => $person`, extract the
    // string/integer key.  For positional `Value` elements, track
    // the 0-based index so we can look up positional shape entries.
    let var_name = ctx.var_name;
    let mut shape_key: Option<String> = None;
    let mut found = false;
    let mut positional_index: usize = 0;

    for elem in elements.iter() {
        match elem {
            ArrayElement::KeyValue(kv) => {
                if let Expression::Variable(Variable::Direct(dv)) = kv.value
                    && dv.name == var_name
                {
                    found = true;
                    // Extract the key from the LHS expression.
                    shape_key = extract_destructuring_key(kv.key);
                    break;
                }
            }
            ArrayElement::Value(val) => {
                if let Expression::Variable(Variable::Direct(dv)) = val.value
                    && dv.name == var_name
                {
                    found = true;
                    // Use the positional index as the shape key.
                    shape_key = Some(positional_index.to_string());
                    break;
                }
                positional_index += 1;
            }
            _ => {}
        }
    }
    if !found {
        return;
    }

    let current_class_name: &str = &ctx.current_class.name;
    let all_classes = ctx.all_classes;
    let content = ctx.content;
    let class_loader = ctx.class_loader;

    // ── 3. Try inline `/** @var … */` annotation ────────────────────
    // Handles both:
    //   `/** @var list<User> */`             (no variable name)
    //   `/** @var array{user: User} $data */` (with variable name)
    let stmt_offset = assignment.span().start.offset as usize;
    if let Some((var_type, _var_name_opt)) =
        docblock::find_inline_var_docblock(content, stmt_offset)
    {
        if let Some(ref key) = shape_key
            && let Some(entry_type) =
                docblock::types::extract_array_shape_value_type(&var_type, key)
        {
            let resolved = crate::completion::type_resolution::type_hint_to_classes(
                &entry_type,
                current_class_name,
                all_classes,
                class_loader,
            );
            if !resolved.is_empty() {
                let resolved_types = ResolvedType::from_classes_with_hint(resolved, &entry_type);
                if !conditional {
                    results.clear();
                }
                ResolvedType::extend_unique(results, resolved_types);
                return;
            }
        }

        let var_parsed = crate::php_type::PhpType::parse(&var_type);
        if let Some(element_type) = var_parsed.extract_value_type(true) {
            let element_str = element_type.to_string();
            let resolved = crate::completion::type_resolution::type_hint_to_classes(
                &element_str,
                current_class_name,
                all_classes,
                class_loader,
            );
            if !resolved.is_empty() {
                let resolved_types = ResolvedType::from_classes_with_hint(resolved, &element_str);
                if !conditional {
                    results.clear();
                }
                ResolvedType::extend_unique(results, resolved_types);
                return;
            }
        }
    }

    // ── 4. Try to resolve the iterable type from the RHS ────────────
    let raw_type: Option<String> = resolve_expression_type_string(assignment.rhs, ctx);

    // ── Expand type aliases before shape/generic extraction ─────────
    // Same as the foreach value/key paths: when the raw type is a type
    // alias (e.g. `UserData` defined via `@phpstan-type`), expand it so
    // that `extract_array_shape_value_type` and
    // `extract_generic_value_type` can see the underlying type.
    let raw_type = raw_type.map(|rt| {
        crate::completion::type_resolution::resolve_type_alias(
            &rt,
            current_class_name,
            all_classes,
            class_loader,
        )
        .unwrap_or(rt)
    });

    if let Some(ref raw) = raw_type {
        // First try array shape lookup with the destructured key.
        if let Some(ref key) = shape_key
            && let Some(entry_type) = docblock::types::extract_array_shape_value_type(raw, key)
        {
            let resolved = crate::completion::type_resolution::type_hint_to_classes(
                &entry_type,
                current_class_name,
                all_classes,
                class_loader,
            );
            if !resolved.is_empty() {
                let resolved_types = ResolvedType::from_classes_with_hint(resolved, &entry_type);
                if !conditional {
                    results.clear();
                }
                ResolvedType::extend_unique(results, resolved_types);
                return;
            }
        }

        // Fall back to generic element type extraction.
        let raw_parsed = crate::php_type::PhpType::parse(raw);
        if let Some(element_type) = raw_parsed.extract_value_type(true) {
            let element_str = element_type.to_string();
            let resolved = crate::completion::type_resolution::type_hint_to_classes(
                &element_str,
                current_class_name,
                all_classes,
                class_loader,
            );
            if !resolved.is_empty() {
                let resolved_types = ResolvedType::from_classes_with_hint(resolved, &element_str);
                if !conditional {
                    results.clear();
                }
                ResolvedType::extend_unique(results, resolved_types);
            }
        }
    }
}

/// Extract a string key from a destructuring key expression.
///
/// Handles string literals (`'user'`, `"user"`) and integer literals
/// (`0`, `1`).  Returns `None` for dynamic or unsupported key
/// expressions.
fn extract_destructuring_key(key_expr: &Expression<'_>) -> Option<String> {
    match key_expr {
        Expression::Literal(Literal::String(lit_str)) => {
            // `value` strips the quotes; fall back to `raw` trimmed.
            lit_str.value.map(|v| v.to_string()).or_else(|| {
                let raw = lit_str.raw;
                // Strip surrounding quotes from the raw representation.
                raw.strip_prefix('\'')
                    .and_then(|s| s.strip_suffix('\''))
                    .or_else(|| raw.strip_prefix('"').and_then(|s| s.strip_suffix('"')))
                    .map(|s| s.to_string())
            })
        }
        Expression::Literal(Literal::Integer(lit_int)) => Some(lit_int.raw.to_string()),
        _ => None,
    }
}
