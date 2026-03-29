/// Right-hand-side expression resolution for variable assignments.
///
/// This module resolves the type of the right-hand side of an assignment
/// (`$var = <expr>`) to zero or more [`ResolvedType`] values.  It handles:
///
///   - Scalar literals: `1` → `int`, `'hello'` → `string`, etc.
///   - Array literals: `[new Foo()]` → `list<Foo>`,
///     `['a' => 1]` → `array{a: int}`
///   - `new ClassName(…)` → the instantiated class
///   - Array access: `$arr[0]` → generic element type,
///     `$arr['key']` → array shape value type,
///     `$arr['key'][0]` → chained bracket access
///   - Function calls: `someFunc()` → return type
///   - Method calls: `$this->method()`, `$obj->method()` → return type
///   - Static calls: `ClassName::method()` → return type
///   - Property access: `$this->prop`, `$obj->prop` → property type
///   - Match expressions: union of all arm types
///   - Ternary / null-coalescing: union of both branches
///   - Clone: `clone $expr` → preserves the cloned expression's type
///
/// The entry point is [`resolve_rhs_expression`], which dispatches to
/// specialised helpers based on the AST node kind.
/// The only caller is
/// [`check_expression_for_assignment`](super::resolution::check_expression_for_assignment)
/// in `variable_resolution.rs`.
use std::collections::HashMap;
use std::sync::Arc;

use mago_span::HasSpan;
use mago_syntax::ast::*;

use crate::Backend;
use crate::docblock;
use crate::parser::extract_hint_string;
use crate::types::{ClassInfo, ResolvedType};

use super::resolution::build_var_resolver_from_ctx;
use crate::completion::call_resolution::MethodReturnCtx;
use crate::completion::conditional_resolution::resolve_conditional_with_args;
use crate::completion::resolver::{Loaders, VarResolutionCtx};

/// Resolve a right-hand-side expression to zero or more
/// [`ResolvedType`] values.
///
/// This is the single place where an arbitrary PHP expression is
/// resolved to a type.  It handles scalars, array literals,
/// instantiations, calls, property access, match/ternary/null-coalesce,
/// clone, closures, generators, pipe, and bare variables.
///
/// Entries may have `class_info: None` (e.g. scalar literals, array
/// shapes).  Callers that need only class-backed results should
/// filter with [`ResolvedType::into_classes`].
///
/// Used by `check_expression_for_assignment` (for `$var = <expr>`),
/// `check_expression_for_raw_type` (for hover/diagnostics type strings),
/// and recursively by multi-branch constructs (match, ternary, `??`).
pub(in crate::completion) fn resolve_rhs_expression<'b>(
    expr: &'b Expression<'b>,
    ctx: &VarResolutionCtx<'_>,
) -> Vec<ResolvedType> {
    match expr {
        // ── Scalar literals ─────────────────────────────────────────
        Expression::Literal(Literal::Integer(_)) => {
            vec![ResolvedType::from_type_string("int".to_string())]
        }
        Expression::Literal(Literal::Float(_)) => {
            vec![ResolvedType::from_type_string("float".to_string())]
        }
        Expression::Literal(Literal::String(_)) => {
            vec![ResolvedType::from_type_string("string".to_string())]
        }
        Expression::Literal(Literal::True(_) | Literal::False(_)) => {
            vec![ResolvedType::from_type_string("bool".to_string())]
        }
        Expression::Literal(Literal::Null(_)) => {
            vec![ResolvedType::from_type_string("null".to_string())]
        }
        // ── Array literals ──────────────────────────────────────────
        Expression::Array(arr) => {
            let ts =
                super::raw_type_inference::infer_array_literal_raw_type(arr.elements.iter(), ctx)
                    .unwrap_or_else(|| "array".to_string());
            vec![ResolvedType::from_type_string(ts)]
        }
        Expression::LegacyArray(arr) => {
            let ts =
                super::raw_type_inference::infer_array_literal_raw_type(arr.elements.iter(), ctx)
                    .unwrap_or_else(|| "array".to_string());
            vec![ResolvedType::from_type_string(ts)]
        }
        Expression::Instantiation(inst) => resolve_rhs_instantiation(inst, ctx),
        Expression::ArrayAccess(array_access) => resolve_rhs_array_access(array_access, expr, ctx),
        Expression::Call(call) => resolve_rhs_call(call, expr, ctx),
        Expression::Access(access) => resolve_rhs_property_access(access, ctx),
        Expression::Parenthesized(p) => resolve_rhs_expression(p.expression, ctx),
        Expression::Match(match_expr) => {
            let mut combined = Vec::new();
            for arm in match_expr.arms.iter() {
                let arm_results = resolve_rhs_expression(arm.expression(), ctx);
                ResolvedType::extend_unique(&mut combined, arm_results);
            }
            combined
        }
        Expression::Conditional(cond_expr) => {
            let mut combined = Vec::new();
            let then_expr = cond_expr.then.unwrap_or(cond_expr.condition);
            ResolvedType::extend_unique(&mut combined, resolve_rhs_expression(then_expr, ctx));
            ResolvedType::extend_unique(
                &mut combined,
                resolve_rhs_expression(cond_expr.r#else, ctx),
            );
            combined
        }
        Expression::Binary(binary) if binary.operator.is_null_coalesce() => {
            // When the LHS is syntactically non-nullable (e.g. `new Foo()`,
            // a literal, `clone $x`), the RHS is dead code — return only
            // the LHS results.  Otherwise resolve both sides; if the LHS
            // type string is nullable, strip `null` before unioning.
            let lhs_non_nullable = matches!(
                binary.lhs,
                Expression::Instantiation(_)
                    | Expression::Literal(_)
                    | Expression::Array(_)
                    | Expression::LegacyArray(_)
                    | Expression::Clone(_)
            );
            let lhs_results = resolve_rhs_expression(binary.lhs, ctx);
            if !lhs_results.is_empty() && lhs_non_nullable {
                lhs_results
            } else if !lhs_results.is_empty() {
                // Strip `null` entries and nullable wrappers from the
                // LHS type strings before unioning with the RHS.
                // Example: `?Foo ?? Bar` → `Foo|Bar`.
                let mut combined: Vec<ResolvedType> = lhs_results
                    .into_iter()
                    .filter_map(|mut rt| {
                        if rt.type_string == "null" {
                            return None;
                        }
                        if docblock::type_strings::raw_type_is_nullable(&rt.type_string) {
                            rt.type_string =
                                docblock::type_strings::strip_null_from_union(&rt.type_string)
                                    .unwrap_or(rt.type_string);
                        }
                        Some(rt)
                    })
                    .collect();
                // Always union with the RHS.  Even when the LHS type
                // string looks non-nullable, the user wrote `??`
                // defensively and both branches are valid candidates.
                ResolvedType::extend_unique(&mut combined, resolve_rhs_expression(binary.rhs, ctx));
                combined
            } else {
                let mut combined = lhs_results;
                ResolvedType::extend_unique(&mut combined, resolve_rhs_expression(binary.rhs, ctx));
                combined
            }
        }
        Expression::Clone(clone_expr) => resolve_rhs_clone(clone_expr, ctx),
        // ── Pipe operator (PHP 8.5): `$expr |> callable(...)` ──
        // The result type is the return type of the callable.
        // The callable is typically a first-class callable reference
        // (PartialApplication) such as `trim(...)` or `createDate(...)`.
        Expression::Pipe(pipe) => resolve_rhs_pipe(pipe, ctx),
        Expression::PartialApplication(_)
        | Expression::Closure(_)
        | Expression::ArrowFunction(_) => {
            // First-class callable syntax (`strlen(...)`),
            // closure literals (`function() { … }`), and
            // arrow functions (`fn() => …`) all produce a
            // `Closure` instance at runtime.
            // Use the fully-qualified name so that resolution
            // succeeds even inside a namespace block (unqualified
            // class names are prefixed with the current namespace
            // and do NOT fall back to the global scope in PHP).
            ResolvedType::from_classes_with_hint(
                crate::completion::type_resolution::type_hint_to_classes(
                    "\\Closure",
                    &ctx.current_class.name,
                    ctx.all_classes,
                    ctx.class_loader,
                ),
                "Closure",
            )
        }
        // ── Generator yield-assignment: `$var = yield $expr` ──
        // The value of a yield expression is the TSend type from
        // the enclosing function's `@return Generator<K, V, TSend, R>`.
        Expression::Yield(_) => {
            if let Some(ref ret_type) = ctx.enclosing_return_type
                && let Some(send_type) = crate::docblock::extract_generator_send_type(ret_type)
            {
                return ResolvedType::from_classes_with_hint(
                    crate::completion::type_resolution::type_hint_to_classes(
                        &send_type,
                        &ctx.current_class.name,
                        ctx.all_classes,
                        ctx.class_loader,
                    ),
                    &send_type,
                );
            }
            vec![]
        }
        // ── Bare variable: `$a = $b` ────────────────────────────────
        // Resolve the RHS variable's type by walking assignments before
        // this point.  The caller (`check_expression_for_assignment`)
        // already set `ctx.cursor_offset` to the assignment's start
        // offset, so the recursive resolution only considers
        // assignments *before* the current one, preventing cycles.
        Expression::Variable(Variable::Direct(dv)) => {
            let rhs_var = dv.name.to_string();
            // Guard: never recurse into the same variable (self-assignment).
            if rhs_var == ctx.var_name {
                return vec![];
            }
            super::resolution::resolve_variable_types(
                &rhs_var,
                ctx.current_class,
                ctx.all_classes,
                ctx.content,
                ctx.cursor_offset,
                ctx.class_loader,
                Loaders::with_function(ctx.function_loader()),
            )
        }
        // ── Concatenation: `"prefix" . $var` → string ───────────────
        Expression::Binary(binary) if binary.operator.is_concatenation() => {
            vec![ResolvedType::from_type_string("string".to_string())]
        }
        // ── Global constant access: `PHP_EOL`, `SORT_ASC`, etc. ────
        Expression::ConstantAccess(ca) => {
            let name = ca.name.value().to_string();
            let name_clean = name.strip_prefix('\\').unwrap_or(&name);
            // `true`, `false`, `null` are parsed as ConstantAccess by
            // some AST variants — handle them the same as literals.
            match name_clean.to_lowercase().as_str() {
                "true" | "false" => {
                    return vec![ResolvedType::from_type_string("bool".to_string())];
                }
                "null" => {
                    return vec![ResolvedType::from_type_string("null".to_string())];
                }
                _ => {}
            }
            if let Some(loader) = ctx.constant_loader()
                && let Some(maybe_value) = loader(name_clean)
                && let Some(ref value) = maybe_value
                && let Some(ts) = infer_type_from_constant_value(value)
            {
                return vec![ResolvedType::from_type_string(ts)];
            }
            vec![]
        }
        // ── Arithmetic: `$a + $b`, `$a * $b` etc. → numeric ────────
        // We can't distinguish int vs float without deeper analysis,
        // so we don't emit a type here and let callers fall back.
        //
        // ── Catch-all: unrecognised expression types ────────────────
        // Return an empty vec — callers that need a type string for
        // expressions not handled above should use the raw-type
        // inference pipeline.
        _ => vec![],
    }
}

/// Infer a scalar type from a constant's initializer value string.
///
/// Recognises integer literals (`42`, `-1`, `0xFF`), float literals
/// (`3.14`, `1e10`), string literals (`'hello'`, `"world"`), boolean
/// keywords (`true`, `false`), `null`, and array literals (`[...]`,
/// `array(...)`).  Returns `None` for expressions that cannot be
/// trivially classified (e.g. concatenation, function calls).
fn infer_type_from_constant_value(value: &str) -> Option<String> {
    let v = value.trim();
    if v.is_empty() {
        return None;
    }

    // String literals: single or double quoted.
    if (v.starts_with('\'') && v.ends_with('\'')) || (v.starts_with('"') && v.ends_with('"')) {
        return Some("string".to_string());
    }

    // Array literals.
    if v.starts_with('[') || v.starts_with("array(") || v.starts_with("array (") {
        return Some("array".to_string());
    }

    let lower = v.to_lowercase();

    // Boolean / null keywords.
    if lower == "true" || lower == "false" {
        return Some("bool".to_string());
    }
    if lower == "null" {
        return Some("null".to_string());
    }

    // Numeric literals — try integer first, then float.
    // Strip optional leading sign for parsing.
    let numeric = v
        .strip_prefix('-')
        .or_else(|| v.strip_prefix('+'))
        .unwrap_or(v);
    if numeric.starts_with("0x") || numeric.starts_with("0X") {
        // Hex integer.
        if numeric[2..]
            .chars()
            .all(|c| c.is_ascii_hexdigit() || c == '_')
        {
            return Some("int".to_string());
        }
    }
    if numeric.starts_with("0b") || numeric.starts_with("0B") {
        // Binary integer.
        if numeric[2..]
            .chars()
            .all(|c| c == '0' || c == '1' || c == '_')
        {
            return Some("int".to_string());
        }
    }
    if numeric.starts_with("0o") || numeric.starts_with("0O") {
        // Octal integer (PHP 8.1+).
        if numeric[2..]
            .chars()
            .all(|c| ('0'..='7').contains(&c) || c == '_')
        {
            return Some("int".to_string());
        }
    }
    // Decimal integer (may contain underscores: 1_000_000).
    if !numeric.is_empty()
        && numeric.chars().all(|c| c.is_ascii_digit() || c == '_')
        && numeric.chars().next().is_some_and(|c| c.is_ascii_digit())
    {
        return Some("int".to_string());
    }
    // Float: contains `.` or `e`/`E` among digits.
    if !numeric.is_empty() {
        let has_dot = numeric.contains('.');
        let has_exp = numeric.contains('e') || numeric.contains('E');
        if (has_dot || has_exp)
            && numeric.chars().all(|c| {
                c.is_ascii_digit()
                    || c == '.'
                    || c == 'e'
                    || c == 'E'
                    || c == '+'
                    || c == '-'
                    || c == '_'
            })
        {
            return Some("float".to_string());
        }
    }

    None
}

/// Resolve a pipe expression `$input |> callable(...)` to the callable's
/// return type.
///
/// The pipe operator passes `$input` as the first argument to `callable`
/// and returns its result.  Chains like `$a |> f(...) |> g(...)` are
/// nested: the outer pipe's input is the inner pipe expression.
///
/// Currently handles function-level callables (e.g. `createDate(...)`).
/// Method and static method callables are not yet supported.
fn resolve_rhs_pipe(pipe: &Pipe<'_>, ctx: &VarResolutionCtx<'_>) -> Vec<ResolvedType> {
    // The callable determines the result type.
    // For `PartialApplication::Function`, extract the function name
    // and look up its return type.
    match pipe.callable {
        Expression::PartialApplication(PartialApplication::Function(fpa)) => {
            let func_name = match fpa.function {
                Expression::Identifier(ident) => ident.value().to_string(),
                _ => return vec![],
            };
            if let Some(fl) = ctx.function_loader()
                && let Some(func_info) = fl(&func_name)
                && let Some(ref ret) = func_info.return_type
            {
                return ResolvedType::from_classes_with_hint(
                    crate::completion::type_resolution::type_hint_to_classes(
                        ret,
                        &ctx.current_class.name,
                        ctx.all_classes,
                        ctx.class_loader,
                    ),
                    ret,
                );
            }
            vec![]
        }
        // Method callable: `$input |> $obj->method(...)`
        // Static callable: `$input |> Class::method(...)`
        // Not yet supported — fall back to empty.
        _ => vec![],
    }
}

/// Resolve `new ClassName(…)` to the instantiated class.
fn resolve_rhs_instantiation(
    inst: &Instantiation<'_>,
    ctx: &VarResolutionCtx<'_>,
) -> Vec<ResolvedType> {
    let class_name = match inst.class {
        Expression::Self_(_) => Some("self"),
        Expression::Static(_) => Some("static"),
        Expression::Identifier(ident) => Some(ident.value()),
        _ => None,
    };
    if let Some(name) = class_name {
        let classes = crate::completion::type_resolution::type_hint_to_classes(
            name,
            &ctx.current_class.name,
            ctx.all_classes,
            ctx.class_loader,
        );

        // ── Constructor template inference ──────────────────────
        // When the class has `@template` params and the constructor
        // has `@param` bindings for them, infer concrete types from
        // the constructor arguments and apply the substitution to
        // the class so that methods returning `T` resolve correctly.
        if classes.len() == 1 && !classes[0].template_params.is_empty() {
            let cls = &classes[0];
            if let Some(ctor) = cls.methods.iter().find(|m| m.name == "__construct")
                && !ctor.template_bindings.is_empty()
                && let Some(ref arg_list) = inst.argument_list
            {
                let text_args =
                    super::raw_type_inference::extract_argument_text(arg_list, ctx.content);
                if !text_args.is_empty() {
                    let rctx = ctx.as_resolution_ctx();
                    let subs = build_constructor_template_subs(cls, ctor, &text_args, &rctx, ctx);
                    if !subs.is_empty() {
                        let type_args: Vec<&str> = cls
                            .template_params
                            .iter()
                            .map(|p| subs.get(p).map(|s| s.as_str()).unwrap_or(p.as_str()))
                            .collect();
                        let resolved =
                            crate::virtual_members::resolve_class_fully(cls, ctx.class_loader);
                        let substituted =
                            crate::inheritance::apply_generic_args(&resolved, &type_args);
                        return vec![ResolvedType::from_class(substituted)];
                    }
                }
            }
        }

        return ResolvedType::from_classes(classes);
    }

    // ── `new $var` where `$var` holds a class-string ────────────
    // When the class expression is a variable, resolve it to check
    // if it holds a class-string value (e.g. `$f = Foo::class;
    // new $f`).  Extract the class name from the class-string and
    // use it to resolve the instantiated type.
    if let Expression::Variable(Variable::Direct(dv)) = inst.class {
        let var_name = dv.name.to_string();
        let resolved =
            crate::completion::variable::class_string_resolution::resolve_class_string_targets(
                &var_name,
                ctx.current_class,
                ctx.all_classes,
                ctx.content,
                ctx.cursor_offset,
                ctx.class_loader,
            );
        if !resolved.is_empty() {
            return ResolvedType::from_classes(resolved);
        }
    }

    vec![]
}

/// Build a template substitution map from constructor arguments.
///
/// Uses the constructor's `template_bindings` (from `@param T $name`
/// annotations) to match template parameters to their concrete types
/// inferred from the call-site arguments.  Handles:
///   - Direct type: `@param T $bar` + `new Foo(new Baz())` → `T = Baz`
///   - Array type: `@param T[] $items` + `new Foo([new X()])` → `T = X`
///   - Generic wrapper: `@param Wrapper<T> $w` + `new Foo(new Wrapper(new X()))` → `T = X`
///     (by resolving the wrapper's constructor template params recursively)
fn build_constructor_template_subs(
    _class: &ClassInfo,
    ctor: &crate::types::MethodInfo,
    text_args: &str,
    rctx: &crate::completion::resolver::ResolutionCtx<'_>,
    ctx: &VarResolutionCtx<'_>,
) -> HashMap<String, String> {
    let args = crate::completion::conditional_resolution::split_text_args(text_args);
    let mut subs = HashMap::new();

    for (tpl_name, param_name) in &ctor.template_bindings {
        // Find the parameter index for this binding.
        let param_idx = match ctor.parameters.iter().position(|p| p.name == *param_name) {
            Some(idx) => idx,
            None => continue,
        };

        // Get the corresponding argument text.
        let arg_text = match args.get(param_idx) {
            Some(text) => text.trim(),
            None => continue,
        };

        // Determine the binding mode by inspecting the parameter's
        // docblock type hint.  The type hint tells us how the template
        // param is embedded in the `@param` annotation.
        let param_hint = ctor
            .parameters
            .get(param_idx)
            .and_then(|p| p.type_hint.as_deref());
        let binding_mode = classify_template_binding(tpl_name, param_hint);

        match binding_mode {
            TemplateBindingMode::Direct => {
                // `@param T $bar` — the argument resolves directly to T.
                if let Some(type_name) = Backend::resolve_arg_text_to_type(arg_text, rctx) {
                    subs.insert(tpl_name.clone(), type_name);
                }
            }
            TemplateBindingMode::ArrayElement => {
                // `@param T[] $items` — resolve individual array elements.
                if arg_text.starts_with('[') && arg_text.ends_with(']') {
                    let inner = arg_text[1..arg_text.len() - 1].trim();
                    if !inner.is_empty() {
                        let first_elem =
                            crate::completion::conditional_resolution::split_text_args(inner);
                        if let Some(elem) = first_elem.first()
                            && let Some(type_name) =
                                Backend::resolve_arg_text_to_type(elem.trim(), rctx)
                        {
                            subs.insert(tpl_name.clone(), type_name);
                        }
                    }
                } else if let Some(type_name) = Backend::resolve_arg_text_to_type(arg_text, rctx) {
                    // Fallback: treat as direct if not an array literal.
                    subs.insert(tpl_name.clone(), type_name);
                }
            }
            TemplateBindingMode::GenericWrapper(wrapper_name, tpl_position) => {
                // `@param Wrapper<T> $a` — resolve the wrapper's constructor
                // template params to find the concrete type for T.
                if let Some(concrete) = resolve_generic_wrapper_template(
                    &wrapper_name,
                    tpl_position,
                    arg_text,
                    rctx,
                    ctx,
                ) {
                    subs.insert(tpl_name.clone(), concrete);
                }
            }
        }
    }

    subs
}

/// How a template parameter is referenced in a `@param` type annotation.
#[derive(Debug)]
pub(crate) enum TemplateBindingMode {
    /// `@param T $bar` — the whole type is the template param.
    Direct,
    /// `@param T[] $items` — the template param is the array element type.
    ArrayElement,
    /// `@param Wrapper<..., T, ...> $a` — the template param is a generic
    /// argument of the wrapper class at the given position.
    GenericWrapper(String, usize),
}

/// Classify how a template parameter name appears in a `@param` type hint.
///
/// Handles union types like `Arrayable<TKey, TValue>|iterable<TKey, TValue>|null`
/// by splitting at depth 0 first, then checking each union part individually.
pub(crate) fn classify_template_binding(
    tpl_name: &str,
    param_hint: Option<&str>,
) -> TemplateBindingMode {
    let hint = match param_hint {
        Some(h) => h,
        // No type hint — assume direct binding.
        None => return TemplateBindingMode::Direct,
    };

    // Strip nullable prefix.
    let hint = hint.strip_prefix('?').unwrap_or(hint);

    // Split the union at depth 0 (respecting `<…>` nesting) so that
    // `Arrayable<TKey, TValue>|iterable<TKey, TValue>|null` is split
    // into individual parts rather than treating the whole thing as one
    // type with a broken `<…>` span.
    let union_parts = split_union_at_depth0(hint);

    for part in &union_parts {
        let part = part.trim();
        if part == "null" || part.is_empty() {
            continue;
        }

        // Check for `T[]` pattern.
        if let Some(base) = part.strip_suffix("[]")
            && base == tpl_name
        {
            return TemplateBindingMode::ArrayElement;
        }

        // Check for direct `T`.
        if part == tpl_name {
            return TemplateBindingMode::Direct;
        }

        // Check for `Wrapper<..., T, ...>` pattern.
        if let Some(open) = part.find('<')
            && let Some(close) = part.rfind('>')
        {
            let wrapper_name = crate::php_type::PhpType::parse(&part[..open])
                .base_name()
                .unwrap_or("")
                .to_string();
            let generic_part = &part[open + 1..close];
            let hint_args: Vec<&str> = generic_part.split(',').map(|s| s.trim()).collect();
            for (i, arg) in hint_args.iter().enumerate() {
                if *arg == tpl_name {
                    return TemplateBindingMode::GenericWrapper(wrapper_name, i);
                }
            }
        }
    }

    // Fallback to direct.
    TemplateBindingMode::Direct
}

/// Split a type string on `|` at depth 0, respecting `<…>` nesting.
///
/// `"Arrayable<TKey, TValue>|iterable<TKey, TValue>|null"` →
/// `["Arrayable<TKey, TValue>", "iterable<TKey, TValue>", "null"]`
fn split_union_at_depth0(s: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0i32;
    let mut start = 0;
    for (i, c) in s.char_indices() {
        match c {
            '<' | '(' | '{' => depth += 1,
            '>' | ')' | '}' => depth -= 1,
            '|' if depth == 0 => {
                parts.push(&s[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(&s[start..]);
    parts
}

/// Resolve a template param that appears inside a generic wrapper type.
///
/// For `@param Wrapper<T> $a` with argument `new Wrapper(new X())`,
/// recursively resolve the wrapper's constructor template params to
/// find the concrete type for the template param at `tpl_position`.
fn resolve_generic_wrapper_template(
    wrapper_name: &str,
    tpl_position: usize,
    arg_text: &str,
    rctx: &crate::completion::resolver::ResolutionCtx<'_>,
    ctx: &VarResolutionCtx<'_>,
) -> Option<String> {
    // Load the wrapper class.
    let wrapper_cls = (ctx.class_loader)(wrapper_name)
        .map(Arc::unwrap_or_clone)
        .or_else(|| {
            ctx.all_classes
                .iter()
                .find(|c| crate::util::short_name(&c.name) == crate::util::short_name(wrapper_name))
                .map(|c| ClassInfo::clone(c))
        })?;

    // Find the wrapper's constructor and its template bindings.
    let wrapper_ctor = wrapper_cls
        .methods
        .iter()
        .find(|m| m.name == "__construct")?;
    if wrapper_ctor.template_bindings.is_empty() {
        return None;
    }

    // Extract the constructor arguments from the argument text.
    // e.g. from `new Foobar(new X())` extract `new X()`.
    let paren_start = arg_text.find('(')?;
    let paren_end = arg_text.rfind(')')?;
    let inner_args = arg_text[paren_start + 1..paren_end].trim();

    let wrapper_subs =
        build_constructor_template_subs(&wrapper_cls, wrapper_ctor, inner_args, rctx, ctx);

    // Find the wrapper's template param at the given position and
    // look it up in the substitution map.
    let wrapper_tpl = wrapper_cls.template_params.get(tpl_position)?;
    wrapper_subs.get(wrapper_tpl).cloned()
}

/// Resolve `$arr[0]` / `$arr[$key]` by extracting the generic element
/// type from the base array's annotation or assignment.
fn resolve_rhs_array_access<'b>(
    array_access: &ArrayAccess<'b>,
    expr: &'b Expression<'b>,
    ctx: &VarResolutionCtx<'_>,
) -> Vec<ResolvedType> {
    // Collect bracket segments and find the innermost base variable by
    // walking through nested ArrayAccess nodes.  This handles both
    // single access (`$result['data']`) and chained access
    // (`$result['items'][0]`).
    let mut segments: Vec<ArrayBracketSegment> = Vec::new();
    let mut current_expr: &Expression<'_> = array_access.array;

    // Classify the outermost (current) index first.
    segments.push(classify_array_index(array_access.index));

    // Walk inward through nested ArrayAccess nodes.
    while let Expression::ArrayAccess(inner) = current_expr {
        segments.push(classify_array_index(inner.index));
        current_expr = inner.array;
    }

    // Segments were collected innermost-last; reverse to left-to-right order.
    segments.reverse();

    // The innermost expression must be a variable.
    let Expression::Variable(Variable::Direct(base_dv)) = current_expr else {
        return vec![];
    };
    let base_var = base_dv.name.to_string();
    let access_offset = expr.span().start.offset as usize;

    // Resolve the base variable's raw type string from either a
    // docblock annotation or AST-based assignment scanning.
    let raw_type =
        docblock::find_iterable_raw_type_in_source(ctx.content, access_offset, &base_var).or_else(
            || {
                let resolved = super::resolution::resolve_variable_types(
                    &base_var,
                    ctx.current_class,
                    ctx.all_classes,
                    ctx.content,
                    access_offset as u32,
                    ctx.class_loader,
                    Loaders::with_function(ctx.function_loader()),
                );
                if resolved.is_empty() {
                    None
                } else {
                    Some(ResolvedType::type_strings_joined(&resolved))
                }
            },
        );

    let Some(mut current_type) = raw_type else {
        return vec![];
    };

    // Expand type aliases so that shape/generic extraction can see the
    // underlying type (e.g. a `@phpstan-type` alias).
    if let Some(expanded) = crate::completion::type_resolution::resolve_type_alias(
        &current_type,
        &ctx.current_class.name,
        ctx.all_classes,
        ctx.class_loader,
    ) {
        current_type = expanded;
    }

    // Walk each bracket segment, narrowing the type at each step.
    for seg in &segments {
        match seg {
            ArrayBracketSegment::StringKey(key) => {
                // String key → try array shape value extraction first.
                if let Some(value_type) =
                    docblock::types::extract_array_shape_value_type(&current_type, key)
                {
                    current_type = value_type;
                } else {
                    let parsed = crate::php_type::PhpType::parse(&current_type);
                    if let Some(element_type_ref) = parsed.extract_value_type(true) {
                        // Fallback: generic element type (e.g. `array<string, Foo>`
                        // accessed with a string key).
                        let element_type = element_type_ref.to_string();
                        current_type = element_type;
                    } else {
                        return vec![];
                    }
                }
            }
            ArrayBracketSegment::ElementAccess => {
                // Numeric / variable index → generic element type.
                let parsed = crate::php_type::PhpType::parse(&current_type);
                if let Some(element_type_ref) = parsed.extract_value_type(true) {
                    let element_type = element_type_ref.to_string();
                    current_type = element_type;
                } else {
                    return vec![];
                }
            }
        }
    }

    ResolvedType::from_classes_with_hint(
        crate::completion::type_resolution::type_hint_to_classes(
            &current_type,
            &ctx.current_class.name,
            ctx.all_classes,
            ctx.class_loader,
        ),
        &current_type,
    )
}

/// Classification of an array access index expression.
enum ArrayBracketSegment {
    /// A string-key access, e.g. `['items']`.
    StringKey(String),
    /// A numeric or variable index access, e.g. `[0]` or `[$i]`.
    ElementAccess,
}

/// Classify an array index expression as either a string key or generic
/// element access.
fn classify_array_index(index: &Expression<'_>) -> ArrayBracketSegment {
    if let Expression::Literal(Literal::String(s)) = index {
        let key = s.value.map(|v| v.to_string()).unwrap_or_else(|| {
            let raw = s.raw;
            raw.strip_prefix('\'')
                .and_then(|r| r.strip_suffix('\''))
                .or_else(|| raw.strip_prefix('"').and_then(|r| r.strip_suffix('"')))
                .unwrap_or(raw)
                .to_string()
        });
        ArrayBracketSegment::StringKey(key)
    } else {
        ArrayBracketSegment::ElementAccess
    }
}

/// Build a template substitution map for a function-level `@template` call.
///
/// Uses the function's `template_bindings` to match template parameters to
/// their concrete types inferred from the call-site arguments.  Handles:
///   - Direct type: `@param T $bar` + `func(new Baz())` → `T = Baz`
///   - Array type: `@param T[] $items` + `func([new X()])` → `T = X`
///   - Generic wrapper: `@param array<TKey, TValue> $v` + `func($users)` →
///     positional resolution through the wrapper's generic arguments.
pub(crate) fn build_function_template_subs(
    func_info: &crate::types::FunctionInfo,
    text_args: &str,
    rctx: &crate::completion::resolver::ResolutionCtx<'_>,
) -> HashMap<String, String> {
    let args = crate::completion::conditional_resolution::split_text_args(text_args);
    let mut subs = HashMap::new();

    for (tpl_name, param_name) in &func_info.template_bindings {
        let param_idx = match func_info
            .parameters
            .iter()
            .position(|p| p.name == *param_name)
        {
            Some(idx) => idx,
            None => continue,
        };

        let arg_text = match args.get(param_idx) {
            Some(text) => text.trim(),
            None => continue,
        };

        // Determine the binding mode by inspecting the parameter's
        // docblock type hint.  The type hint tells us how the template
        // param is embedded in the `@param` annotation.
        let param_hint = func_info
            .parameters
            .get(param_idx)
            .and_then(|p| p.type_hint.as_deref());
        let binding_mode = classify_template_binding(tpl_name, param_hint);

        match binding_mode {
            TemplateBindingMode::Direct => {
                if let Some(type_name) = Backend::resolve_arg_text_to_type(arg_text, rctx) {
                    subs.insert(tpl_name.clone(), type_name);
                }
            }
            TemplateBindingMode::ArrayElement => {
                // `@param T[] $items` — resolve individual array elements.
                if arg_text.starts_with('[') && arg_text.ends_with(']') {
                    let inner = arg_text[1..arg_text.len() - 1].trim();
                    if !inner.is_empty() {
                        let first_elem =
                            crate::completion::conditional_resolution::split_text_args(inner);
                        if let Some(elem) = first_elem.first()
                            && let Some(type_name) =
                                Backend::resolve_arg_text_to_type(elem.trim(), rctx)
                        {
                            subs.insert(tpl_name.clone(), type_name);
                        }
                    }
                } else if let Some(type_name) = Backend::resolve_arg_text_to_type(arg_text, rctx) {
                    // Fallback: treat as direct if not an array literal.
                    subs.insert(tpl_name.clone(), type_name);
                }
            }
            TemplateBindingMode::GenericWrapper(ref wrapper_name, tpl_position) => {
                // For `@param array<TKey, TValue> $value` with a variable
                // argument like `$users`, resolve the variable's raw type
                // string (e.g. `User[]`, `array<int, User>`) and extract
                // the positional generic argument.
                if is_array_like_wrapper(wrapper_name)
                    && arg_text.starts_with('$')
                    && let Some(resolved) = resolve_arg_variable_raw_type(arg_text, rctx)
                    && let Some(concrete) = extract_array_type_at_position(&resolved, tpl_position)
                {
                    subs.insert(tpl_name.clone(), concrete);
                    continue;
                }
                // Fall back to direct resolution for non-array wrappers
                // or when raw type extraction fails.
                if let Some(type_name) = Backend::resolve_arg_text_to_type(arg_text, rctx) {
                    subs.insert(tpl_name.clone(), type_name);
                }
            }
        }
    }

    subs
}

/// Resolve a variable argument to its raw type string.
///
/// For `$pens` with `/** @var Pen[] $pens */`, returns `Some("Pen[]")`.
/// For `$users` with `/** @var array<int, User> $users */`, returns
/// `Some("array<int, User>")`.
///
/// Tries docblock annotations first, then falls back to AST-based
/// raw type inference.
fn resolve_arg_variable_raw_type(
    arg_text: &str,
    rctx: &crate::completion::resolver::ResolutionCtx<'_>,
) -> Option<String> {
    let var_name = arg_text.trim();
    if !var_name.starts_with('$') {
        return None;
    }

    // 1. Try docblock annotation (@var).
    if let Some(raw) = crate::docblock::find_iterable_raw_type_in_source(
        rctx.content,
        rctx.cursor_offset as usize,
        var_name,
    ) {
        return Some(raw);
    }

    // 2. Fall back to unified variable resolution pipeline.
    let default_class = crate::types::ClassInfo::default();
    let current_class = rctx.current_class.unwrap_or(&default_class);
    let resolved = super::resolution::resolve_variable_types(
        var_name,
        current_class,
        rctx.all_classes,
        rctx.content,
        rctx.cursor_offset,
        rctx.class_loader,
        Loaders::with_function(rctx.function_loader),
    );
    if resolved.is_empty() {
        None
    } else {
        Some(ResolvedType::type_strings_joined(&resolved))
    }
}

/// Extract the concrete type at `position` from an array type string.
///
/// For array types with two generic parameters (key + value):
/// - `array<int, User>` at position 0 → `"int"`, position 1 → `"User"`
/// - `User[]` at position 0 → `"int"` (implicit key), position 1 → `"User"`
/// - `list<User>` at position 0 → `"int"`, position 1 → `"User"`
///
/// For single-param forms:
/// - `array<User>` at position 0 → `"User"`
fn extract_array_type_at_position(raw_type: &str, position: usize) -> Option<String> {
    let trimmed = raw_type.trim();

    // `T[]` shorthand → key is int (position 0), value is T (position 1).
    if let Some(base) = trimmed.strip_suffix("[]") {
        let element = crate::docblock::types::clean_type(base);
        return match position {
            0 => Some("int".to_string()),
            1 => Some(element),
            _ => None,
        };
    }

    // `list<T>` → key is int, value is T.
    if trimmed.starts_with("list<") || trimmed.starts_with("non-empty-list<") {
        let (_base, args) = crate::docblock::generics::parse_generic_args(trimmed);
        return match position {
            0 => Some("int".to_string()),
            1 => args
                .first()
                .map(|a| crate::docblock::types::clean_type(a.trim())),
            _ => None,
        };
    }

    // `array<K, V>` or `array<V>`.
    if trimmed.starts_with("array<")
        || trimmed.starts_with("non-empty-array<")
        || trimmed.starts_with("iterable<")
    {
        let (_base, args) = crate::docblock::generics::parse_generic_args(trimmed);
        if args.len() == 2 {
            // `array<K, V>` — position maps directly.
            return args
                .get(position)
                .map(|a| crate::docblock::types::clean_type(a.trim()));
        } else if args.len() == 1 {
            // `array<V>` — position 0 = int (key), position 1 = V.
            return match position {
                0 => Some("int".to_string()),
                1 => args
                    .first()
                    .map(|a| crate::docblock::types::clean_type(a.trim())),
                _ => None,
            };
        }
    }

    None
}

/// Whether a wrapper type name should be treated as array-like for
/// positional generic argument extraction.
///
/// When `@param Wrapper<TKey, TValue> $value` binds a template param
/// via `GenericWrapper`, and the wrapper is an array-like type, we can
/// resolve the argument variable's raw type (e.g. `User[]`) and extract
/// the positional generic component (key at 0, value at 1).
///
/// This covers `array`, `iterable`, `list`, and common Laravel/PHPStan
/// collection interfaces whose generic args follow `<TKey, TValue>`.
fn is_array_like_wrapper(name: &str) -> bool {
    // Compare the short name (last segment after `\`) case-insensitively.
    let short = crate::util::short_name(name);
    matches!(
        short.to_ascii_lowercase().as_str(),
        "array" | "iterable" | "list" | "non-empty-array" | "non-empty-list" | "arrayable"
    )
}

/// Resolve function, method, and static method calls to their return
/// types.
fn resolve_rhs_call<'b>(
    call: &'b Call<'b>,
    expr: &'b Expression<'b>,
    ctx: &VarResolutionCtx<'_>,
) -> Vec<ResolvedType> {
    match call {
        Call::Function(func_call) => resolve_rhs_function_call(func_call, expr, ctx),
        Call::Method(method_call) => resolve_rhs_method_call_inner(
            method_call.object,
            &method_call.method,
            &method_call.argument_list,
            ctx,
        ),
        Call::NullSafeMethod(method_call) => resolve_rhs_method_call_inner(
            method_call.object,
            &method_call.method,
            &method_call.argument_list,
            ctx,
        ),
        Call::StaticMethod(static_call) => resolve_rhs_static_call(static_call, ctx),
    }
}

/// Resolve a plain function call: `someFunc()`, array functions, variable
/// invocations (`$fn()`), and conditional return types.
fn resolve_rhs_function_call<'b>(
    func_call: &'b FunctionCall<'b>,
    expr: &'b Expression<'b>,
    ctx: &VarResolutionCtx<'_>,
) -> Vec<ResolvedType> {
    let current_class_name: &str = &ctx.current_class.name;
    let all_classes = ctx.all_classes;
    let content = ctx.content;
    let class_loader = ctx.class_loader;
    let function_loader = ctx.function_loader();

    let func_name = match func_call.function {
        Expression::Identifier(ident) => Some(ident.value().to_string()),
        _ => None,
    };

    // ── Known array functions ────────────────────────
    // For element-extracting functions (array_pop, etc.)
    // resolve to the element ClassInfo directly.
    if let Some(ref name) = func_name
        && let Some(element_type) = super::raw_type_inference::resolve_array_func_element_type(
            name,
            &func_call.argument_list,
            ctx,
        )
    {
        let resolved = crate::completion::type_resolution::type_hint_to_classes(
            &element_type,
            current_class_name,
            all_classes,
            class_loader,
        );
        if !resolved.is_empty() {
            return ResolvedType::from_classes_with_hint(resolved, &element_type);
        }
    }

    // For type-preserving functions (array_filter, array_values, etc.)
    // the output has the same iterable type as the input array.
    // Return the full type string (e.g. `list<User>`) so that
    // downstream consumers (foreach, array access, hover) see the
    // element type without needing the raw-type pipeline's fallback.
    if let Some(ref name) = func_name
        && let Some(raw_type) = super::raw_type_inference::resolve_array_func_raw_type(
            name,
            &func_call.argument_list,
            ctx,
        )
    {
        let resolved = crate::completion::type_resolution::type_hint_to_classes(
            &raw_type,
            current_class_name,
            all_classes,
            class_loader,
        );
        if !resolved.is_empty() {
            return ResolvedType::from_classes_with_hint(resolved, &raw_type);
        }
        // The type string is informative (e.g. `list<User>`) but
        // doesn't resolve to a class — return as type-string-only.
        return vec![ResolvedType::from_type_string(raw_type)];
    }

    if let Some(ref name) = func_name
        && let Some(fl) = function_loader
        && let Some(func_info) = fl(name)
    {
        // Try conditional return type first
        if let Some(ref cond) = func_info.conditional_return {
            let var_resolver = build_var_resolver_from_ctx(ctx);
            let resolved_type = resolve_conditional_with_args(
                cond,
                &func_info.parameters,
                &func_call.argument_list,
                Some(&var_resolver),
            );
            if let Some(ref ty) = resolved_type {
                let resolved = crate::completion::type_resolution::type_hint_to_classes(
                    ty,
                    current_class_name,
                    all_classes,
                    class_loader,
                );
                if !resolved.is_empty() {
                    return ResolvedType::from_classes_with_hint(resolved, ty);
                }
            }
        }

        // ── Function-level @template substitution ────────────
        // When the function has template params and bindings,
        // infer concrete types from the arguments and apply
        // substitution to the return type before resolving.
        if !func_info.template_params.is_empty()
            && !func_info.template_bindings.is_empty()
            && func_info.return_type.is_some()
        {
            let text_args =
                super::raw_type_inference::extract_argument_text(&func_call.argument_list, content);
            if !text_args.is_empty() {
                let rctx = ctx.as_resolution_ctx();
                let subs = build_function_template_subs(&func_info, &text_args, &rctx);
                if !subs.is_empty()
                    && let Some(ref ret) = func_info.return_type
                {
                    let substituted = crate::inheritance::apply_substitution(ret, &subs);
                    let resolved = crate::completion::type_resolution::type_hint_to_classes(
                        &substituted,
                        current_class_name,
                        all_classes,
                        class_loader,
                    );
                    if !resolved.is_empty() {
                        return ResolvedType::from_classes_with_hint(resolved, &substituted);
                    }
                }
            }
        }

        if let Some(ref ret) = func_info.return_type {
            let resolved = crate::completion::type_resolution::type_hint_to_classes(
                ret,
                current_class_name,
                all_classes,
                class_loader,
            );
            if !resolved.is_empty() {
                return ResolvedType::from_classes_with_hint(resolved, ret);
            }
            // The function has a return type string but
            // `type_hint_to_classes` found no matching class (e.g.
            // `list<Widget>`, `int`, `array{name: string}`).  Return a
            // type-string-only entry so that consumers reading
            // `.type_string` still get the information.
            //
            // Skip non-informative types like `array`, `mixed`, `void`
            // — returning these would mask more precise type info from
            // the raw-type pipeline's specialised handlers (e.g.
            // `resolve_array_func_raw_type` for `array_filter`).
            if is_informative_type_string(ret) {
                return vec![ResolvedType::from_type_string(ret.clone())];
            }
        }
    }

    // ── Source-scanning fallback for named function calls ────
    // When no function_loader is available (e.g. raw-type pipeline,
    // test backends without PSR-4), scan the source text for the
    // function's docblock @return annotation.  This covers standalone
    // function calls when no function_loader is available.
    if let Some(ref name) = func_name
        && function_loader.is_none()
        && let Some(ret) =
            crate::completion::source::helpers::extract_function_return_from_source(name, content)
    {
        let resolved = crate::completion::type_resolution::type_hint_to_classes(
            &ret,
            current_class_name,
            all_classes,
            class_loader,
        );
        if !resolved.is_empty() {
            return ResolvedType::from_classes_with_hint(resolved, &ret);
        }
        if is_informative_type_string(&ret) {
            return vec![ResolvedType::from_type_string(ret)];
        }
    }

    // ── Variable invocation: $fn() ──────────────────
    // When the callee is a variable (not a named function),
    // resolve the variable's type annotation for a
    // callable/Closure return type, or look for a
    // closure/arrow-function literal in the assignment.
    if let Expression::Variable(Variable::Direct(dv)) = func_call.function {
        let var_name = dv.name.to_string();
        let offset = expr.span().start.offset as usize;

        // 1. Try docblock annotation:
        //    `@var Closure(): User $fn` or
        //    `@param callable(int): Response $fn`
        if let Some(raw_type) =
            crate::docblock::find_iterable_raw_type_in_source(content, offset, &var_name)
            && let Some(ret) = crate::docblock::extract_callable_return_type(&raw_type)
        {
            let resolved = crate::completion::type_resolution::type_hint_to_classes(
                &ret,
                current_class_name,
                all_classes,
                class_loader,
            );
            if !resolved.is_empty() {
                return ResolvedType::from_classes_with_hint(resolved, &ret);
            }
        }

        // 2. Scan for closure literal assignment and
        //    extract native return type hint.
        if let Some(ret) =
            crate::completion::source::helpers::extract_closure_return_type_from_assignment(
                &var_name,
                content,
                ctx.cursor_offset,
            )
        {
            let resolved = crate::completion::type_resolution::type_hint_to_classes(
                &ret,
                current_class_name,
                all_classes,
                class_loader,
            );
            if !resolved.is_empty() {
                return ResolvedType::from_classes_with_hint(resolved, &ret);
            }
        }

        // 3. Scan backward for first-class callable assignment:
        //    `$fn = strlen(...)`, `$fn = $obj->method(...)`, or
        //    `$fn = ClassName::staticMethod(...)`.
        //    Resolve the underlying function/method's return type.
        let rctx = ctx.as_resolution_ctx();
        if let Some(ret) =
            crate::completion::source::helpers::extract_first_class_callable_return_type(
                &var_name, &rctx,
            )
        {
            let resolved = crate::completion::type_resolution::type_hint_to_classes(
                &ret,
                current_class_name,
                all_classes,
                class_loader,
            );
            if !resolved.is_empty() {
                return ResolvedType::from_classes_with_hint(resolved, &ret);
            }
        }

        // 4. Resolve the variable's type and check for __invoke().
        //    When $f holds an object with an __invoke() method,
        //    $f() should return __invoke()'s return type.
        let rctx = ctx.as_resolution_ctx();
        let var_classes = crate::completion::resolver::resolve_target_classes(
            &var_name,
            crate::types::AccessKind::Arrow,
            &rctx,
        );
        for owner in &var_classes {
            if let Some(invoke) = owner.methods.iter().find(|m| m.name == "__invoke")
                && let Some(ref ret) = invoke.return_type
            {
                let resolved = crate::completion::type_resolution::type_hint_to_classes(
                    ret,
                    current_class_name,
                    all_classes,
                    class_loader,
                );
                if !resolved.is_empty() {
                    return ResolvedType::from_classes_with_hint(resolved, ret);
                }
                // When type_hint_to_classes can't resolve the return
                // type (e.g. `Item[]` where the `[]` suffix prevents
                // class lookup), emit a type-string-only entry so that
                // callers like foreach resolution can still extract the
                // element type via `extract_generic_value_type`.
                if !ret.is_empty() {
                    return vec![ResolvedType::from_type_string(ret.clone())];
                }
            }
        }
    }

    // ── General expression invocation: ($expr)() ────
    // When the callee is an arbitrary expression (e.g.
    // `($this->foo)()`, `(getFactory())()`, etc.), resolve
    // the expression to classes and check for __invoke().
    let callee_expr = match func_call.function {
        Expression::Parenthesized(p) => p.expression,
        other => other,
    };
    // Skip if we already handled it as a variable above.
    if !matches!(callee_expr, Expression::Variable(Variable::Direct(_))) {
        // ── Directly invoked closure / arrow function ────
        // `(fn (): Foo => …)()` or `(function (): Foo { … })()`
        // Extract the return type from the literal instead of going
        // through `__invoke()` on the generic `Closure` stub.
        if let Some(ret_type) = extract_closure_or_arrow_return_type(callee_expr) {
            let resolved = crate::completion::type_resolution::type_hint_to_classes(
                &ret_type,
                current_class_name,
                all_classes,
                class_loader,
            );
            if !resolved.is_empty() {
                return ResolvedType::from_classes_with_hint(resolved, &ret_type);
            }
        }

        let callee_results = resolve_rhs_expression(callee_expr, ctx);
        for rt in &callee_results {
            if let Some(ref owner_cls) = rt.class_info
                && let Some(invoke) = owner_cls.methods.iter().find(|m| m.name == "__invoke")
                && let Some(ref ret) = invoke.return_type
            {
                let resolved = crate::completion::type_resolution::type_hint_to_classes(
                    ret,
                    current_class_name,
                    all_classes,
                    class_loader,
                );
                if !resolved.is_empty() {
                    return ResolvedType::from_classes_with_hint(resolved, ret);
                }
                if !ret.is_empty() {
                    return vec![ResolvedType::from_type_string(ret.clone())];
                }
            }
        }
    }

    vec![]
}

/// Resolve an instance method call: `$this->method()`, `$var->method()`,
/// chained calls, and other object expressions via AST-based resolution.
/// Resolve a method call (regular or null-safe) from its constituent parts.
///
/// Both `$obj->method()` and `$obj?->method()` share the same resolution
/// logic — the null-safe operator only affects whether `null` propagates
/// at runtime, not which class the method belongs to.
fn resolve_rhs_method_call_inner<'b>(
    object: &'b Expression<'b>,
    method: &'b ClassLikeMemberSelector<'b>,
    argument_list: &'b ArgumentList<'b>,
    ctx: &VarResolutionCtx<'_>,
) -> Vec<ResolvedType> {
    let method_name = match method {
        ClassLikeMemberSelector::Identifier(ident) => ident.value.to_string(),
        // Variable method name (`$obj->$method()`) — can't resolve statically.
        _ => return vec![],
    };
    // Resolve the object expression to candidate owner classes.
    let owner_classes: Vec<ClassInfo> = if let Expression::Variable(Variable::Direct(dv)) = object
        && dv.name == "$this"
    {
        ctx.all_classes
            .iter()
            .find(|c| c.name == ctx.current_class.name)
            .map(|c| ClassInfo::clone(c))
            .into_iter()
            .collect()
    } else if let Expression::Variable(Variable::Direct(dv)) = object {
        let var = dv.name.to_string();
        crate::completion::resolver::resolve_target_classes(
            &var,
            crate::types::AccessKind::Arrow,
            &ctx.as_resolution_ctx(),
        )
        .into_iter()
        .map(Arc::unwrap_or_clone)
        .collect()
    } else {
        // Handle non-variable object expressions like
        // `(new Factory())->create()`, `getService()->method()`,
        // or chained calls by recursively resolving the expression.
        ResolvedType::into_classes(resolve_rhs_expression(object, ctx))
    };

    let text_args = super::raw_type_inference::extract_argument_text(argument_list, ctx.content);
    let rctx = ctx.as_resolution_ctx();
    let var_resolver = build_var_resolver_from_ctx(ctx);

    for owner in &owner_classes {
        let template_subs = if !text_args.is_empty() {
            Backend::build_method_template_subs(owner, &method_name, &text_args, &rctx)
        } else {
            HashMap::new()
        };
        let mr_ctx = MethodReturnCtx {
            all_classes: ctx.all_classes,
            class_loader: ctx.class_loader,
            template_subs: &template_subs,
            var_resolver: Some(&var_resolver),
            cache: ctx.resolved_class_cache,
        };
        // Recover the effective return type string from the method.
        // Look up the method on the (possibly merged) owner and apply
        // the same template substitution that
        // `resolve_method_return_types_with_args` used internally,
        // then replace `static`/`self`/`$this` with the owner class
        // name so that e.g. `static[]` becomes `Country[]`.
        let merged = crate::virtual_members::resolve_class_fully(owner, ctx.class_loader);
        let ret_type_string = merged
            .methods
            .iter()
            .find(|m| m.name == method_name)
            .and_then(|m| m.return_type.as_ref())
            .map(|ret| {
                let substituted = if !template_subs.is_empty() {
                    crate::inheritance::apply_substitution(ret, &template_subs).into_owned()
                } else {
                    ret.clone()
                };
                docblock::type_strings::replace_self_in_type(&substituted, &owner.name)
            });

        let results = Backend::resolve_method_return_types_with_args(
            owner,
            &method_name,
            &text_args,
            &mr_ctx,
        );
        if !results.is_empty() {
            let classes: Vec<ClassInfo> = results.into_iter().map(Arc::unwrap_or_clone).collect();
            return match ret_type_string {
                Some(ref hint) => ResolvedType::from_classes_with_hint(classes, hint),
                None => ResolvedType::from_classes(classes),
            };
        }

        // The method has a return type string but `type_hint_to_classes`
        // found no matching class (e.g. `list<Widget>`, `int`,
        // `array{name: string}`).  Return a type-string-only entry so
        // that consumers reading `.type_string` (hover, foreach
        // resolution, null-coalesce stripping) still get the information.
        //
        // Skip non-informative types (`array`, `mixed`, `void`, etc.)
        // so that downstream handlers can provide more precise
        // information.  Also expand type aliases before the check so
        // that `@phpstan-type UserList array<int, User>` with
        // `@return UserList` is recognized as informative.
        if let Some(ref hint) = ret_type_string {
            // Try expanding type aliases (e.g. `UserList` → `array<int, User>`).
            let expanded = crate::completion::type_resolution::resolve_type_alias(
                hint,
                &owner.name,
                ctx.all_classes,
                ctx.class_loader,
            );
            let effective = expanded.as_deref().unwrap_or(hint);
            if is_informative_type_string(effective) {
                return vec![ResolvedType::from_type_string(effective.to_string())];
            }
        }
    }
    vec![]
}

/// Resolve a static method call: `ClassName::method()`, `self::method()`,
/// `static::method()`.
fn resolve_rhs_static_call(
    static_call: &StaticMethodCall<'_>,
    ctx: &VarResolutionCtx<'_>,
) -> Vec<ResolvedType> {
    let current_class_name: &str = &ctx.current_class.name;

    let class_name = match static_call.class {
        Expression::Self_(_) => Some(current_class_name.to_string()),
        Expression::Static(_) => Some(current_class_name.to_string()),
        Expression::Identifier(ident) => Some(ident.value().to_string()),
        // ── `$var::method()` where `$var` holds a class-string ──
        Expression::Variable(Variable::Direct(dv)) => {
            let var_name = dv.name.to_string();
            let targets =
                crate::completion::variable::class_string_resolution::resolve_class_string_targets(
                    &var_name,
                    ctx.current_class,
                    ctx.all_classes,
                    ctx.content,
                    ctx.cursor_offset,
                    ctx.class_loader,
                );
            targets.first().map(|c| c.name.clone())
        }
        _ => None,
    };
    if let Some(cls_name) = class_name
        && let ClassLikeMemberSelector::Identifier(ident) = &static_call.method
    {
        let method_name = ident.value.to_string();
        let owner = ctx
            .all_classes
            .iter()
            .find(|c| c.name == cls_name)
            .map(|c| ClassInfo::clone(c))
            .or_else(|| (ctx.class_loader)(&cls_name).map(Arc::unwrap_or_clone));
        if let Some(ref owner) = owner {
            let text_args = super::raw_type_inference::extract_argument_text(
                &static_call.argument_list,
                ctx.content,
            );
            let rctx = ctx.as_resolution_ctx();
            let template_subs = if !text_args.is_empty() {
                Backend::build_method_template_subs(owner, &method_name, &text_args, &rctx)
            } else {
                HashMap::new()
            };
            let var_resolver = build_var_resolver_from_ctx(ctx);
            let mr_ctx = MethodReturnCtx {
                all_classes: ctx.all_classes,
                class_loader: ctx.class_loader,
                template_subs: &template_subs,
                var_resolver: Some(&var_resolver),
                cache: ctx.resolved_class_cache,
            };
            // Recover the effective return type string from the method.
            // Look up the method on the (possibly merged) owner and apply
            // the same template substitution that
            // `resolve_method_return_types_with_args` used internally,
            // then replace `static`/`self`/`$this` with the owner class
            // name so that e.g. `static[]` becomes `Country[]`.
            let merged = crate::virtual_members::resolve_class_fully(owner, ctx.class_loader);
            let ret_type_string = merged
                .methods
                .iter()
                .find(|m| m.name == method_name)
                .and_then(|m| m.return_type.as_ref())
                .map(|ret| {
                    let substituted = if !template_subs.is_empty() {
                        crate::inheritance::apply_substitution(ret, &template_subs).into_owned()
                    } else {
                        ret.clone()
                    };
                    docblock::type_strings::replace_self_in_type(&substituted, &owner.name)
                });

            let results = Backend::resolve_method_return_types_with_args(
                owner,
                &method_name,
                &text_args,
                &mr_ctx,
            );
            if !results.is_empty() {
                let classes: Vec<ClassInfo> =
                    results.into_iter().map(Arc::unwrap_or_clone).collect();
                return match ret_type_string {
                    Some(ref hint) => ResolvedType::from_classes_with_hint(classes, hint),
                    None => ResolvedType::from_classes(classes),
                };
            }

            // The method has a return type string but `type_hint_to_classes`
            // found no matching class (e.g. `list<Widget>`, `int`,
            // `array{name: string}`).  Return a type-string-only entry so
            // that consumers reading `.type_string` (hover, raw-type
            // pipeline, null-coalesce stripping) still get the information.
            if let Some(ref hint) = ret_type_string
                && is_informative_type_string(hint)
            {
                return vec![ResolvedType::from_type_string(hint.clone())];
            }
        }
    }
    vec![]
}

/// Resolve property access: `$this->prop`, `$obj->prop`, `$obj?->prop`.
fn resolve_rhs_property_access(
    access: &Access<'_>,
    ctx: &VarResolutionCtx<'_>,
) -> Vec<ResolvedType> {
    let current_class_name: &str = &ctx.current_class.name;
    let all_classes = ctx.all_classes;
    let class_loader = ctx.class_loader;

    /// Resolve a property's type to `Vec<ResolvedType>`, preserving the
    /// property's type hint string in each result.
    ///
    /// When the property type is a scalar (e.g. `string`, `int`) and
    /// `type_hint_to_classes` returns no `ClassInfo`, a type-string-only
    /// `ResolvedType` is produced so that the type information is not lost.
    fn resolve_property_with_hint(
        prop_name: &str,
        owner: &ClassInfo,
        all_classes: &[Arc<ClassInfo>],
        class_loader: &dyn Fn(&str) -> Option<Arc<ClassInfo>>,
    ) -> Vec<ResolvedType> {
        // Get the type hint string before resolving to ClassInfo.
        let type_hint =
            crate::inheritance::resolve_property_type_hint(owner, prop_name, class_loader);
        let resolved = crate::completion::type_resolution::resolve_property_types(
            prop_name,
            owner,
            all_classes,
            class_loader,
        );
        if resolved.is_empty() {
            // The property has a type hint but `type_hint_to_classes`
            // found no matching class (e.g. `list<Widget>`, `int`,
            // `array{name: string}`).  Return a type-string-only
            // entry when the type is informative (carries generics,
            // shapes, or names a non-scalar class).
            return match type_hint {
                Some(hint) if is_informative_type_string(&hint) => {
                    vec![ResolvedType::from_type_string(hint)]
                }
                _ => vec![],
            };
        }
        match type_hint {
            Some(ref hint) => ResolvedType::from_classes_with_hint(resolved, hint),
            None => ResolvedType::from_classes(resolved),
        }
    }

    // ── Class constant / enum case access: `Foo::BAR` ──
    // When the RHS is a class constant access, resolve the class and
    // check whether the constant is an enum case (→ type is the enum
    // itself) or a typed constant (→ use its type_hint).
    if let Access::ClassConstant(cca) = access {
        let class_name = match cca.class {
            Expression::Identifier(ident) => Some(ident.value().to_string()),
            Expression::Self_(_) => Some(current_class_name.to_string()),
            Expression::Static(_) => Some(current_class_name.to_string()),
            _ => None,
        };
        if let Some(class_name) = class_name {
            let resolved_name = crate::php_type::PhpType::parse(&class_name)
                .base_name()
                .unwrap_or(&class_name)
                .to_string();
            let target_classes = crate::completion::type_resolution::type_hint_to_classes(
                &resolved_name,
                current_class_name,
                all_classes,
                class_loader,
            );

            let const_name = match &cca.constant {
                ClassLikeConstantSelector::Identifier(ident) => Some(ident.value.to_string()),
                _ => None,
            };

            if let Some(const_name) = const_name {
                for cls in &target_classes {
                    // Check if the constant is an enum case — the
                    // result type is the enum class itself.
                    if let Some(c) = cls.constants.iter().find(|c| c.name == const_name) {
                        if c.is_enum_case {
                            return ResolvedType::from_classes(target_classes);
                        }
                        // Typed class constant — resolve via type_hint.
                        if let Some(ref th) = c.type_hint {
                            let resolved = crate::completion::type_resolution::type_hint_to_classes(
                                th,
                                current_class_name,
                                all_classes,
                                class_loader,
                            );
                            if !resolved.is_empty() {
                                return ResolvedType::from_classes_with_hint(resolved, th);
                            }
                        }
                        // No type_hint — infer from the initializer value.
                        if let Some(ref val) = c.value
                            && let Some(ts) = infer_type_from_constant_value(val)
                        {
                            let resolved = crate::completion::type_resolution::type_hint_to_classes(
                                &ts,
                                current_class_name,
                                all_classes,
                                class_loader,
                            );
                            if !resolved.is_empty() {
                                return ResolvedType::from_classes_with_hint(resolved, &ts);
                            }
                            return vec![ResolvedType::from_type_string(ts)];
                        }
                    }
                }
            }
        }
        return vec![];
    }

    let (object_expr, prop_selector) = match access {
        Access::Property(pa) => (Some(pa.object), Some(&pa.property)),
        Access::NullSafeProperty(pa) => (Some(pa.object), Some(&pa.property)),
        _ => (None, None),
    };
    if let Some(obj) = object_expr
        && let Some(sel) = prop_selector
    {
        let prop_name = match sel {
            ClassLikeMemberSelector::Identifier(ident) => Some(ident.value.to_string()),
            _ => None,
        };
        if let Some(prop_name) = prop_name {
            let owner_classes: Vec<ClassInfo> = if let Expression::Variable(Variable::Direct(dv)) =
                obj
                && dv.name == "$this"
            {
                all_classes
                    .iter()
                    .find(|c| c.name == current_class_name)
                    .map(|c| ClassInfo::clone(c))
                    .into_iter()
                    .collect()
            } else if let Expression::Variable(Variable::Direct(dv)) = obj {
                let var = dv.name.to_string();
                crate::completion::resolver::resolve_target_classes(
                    &var,
                    crate::types::AccessKind::Arrow,
                    &ctx.as_resolution_ctx(),
                )
                .into_iter()
                .map(Arc::unwrap_or_clone)
                .collect()
            } else {
                // Handle non-variable object expressions like
                // `(new Canvas())->easel`, `getService()->prop`,
                // or `SomeClass::make()->prop` by recursively
                // resolving the expression type.
                ResolvedType::into_classes(resolve_rhs_expression(obj, ctx))
            };

            for owner in &owner_classes {
                let resolved =
                    resolve_property_with_hint(&prop_name, owner, all_classes, class_loader);
                if !resolved.is_empty() {
                    return resolved;
                }
            }
        }
    }
    vec![]
}

/// Whether a type string carries enough information to be worth
/// returning as a type-string-only `ResolvedType`.
///
/// Bare scalars (`int`, `string`, `bool`, etc.) and uninformative
/// base types (`array`, `mixed`, `void`, `object`, `iterable`) are
/// considered **not** informative — returning them from the unified
/// resolver would mask more precise results from the raw-type
/// pipeline's specialised handlers (e.g. `resolve_array_func_raw_type`
/// which tracks element types through `array_filter`).
///
/// Types that carry generic or shape information (`list<User>`,
/// `array{name: string}`, `Collection<int, Foo>`) ARE informative
/// because the raw-type pipeline cannot reconstruct the parametric
/// detail.
pub(in crate::completion) fn is_informative_type_string(ts: &str) -> bool {
    // Contains generic params or shape braces → informative.
    if ts.contains('<') || ts.contains('{') {
        return true;
    }
    // `Type[]` shorthand carries element type info.
    if ts.ends_with("[]") {
        return true;
    }
    // Union types that contain at least one non-informative part are
    // still informative if ANY part is informative.
    if ts.contains('|') {
        return ts
            .split('|')
            .any(|part| !part.trim().is_empty() && is_informative_type_string(part.trim()));
    }
    // Non-informative base types: these are either too vague for the
    // unified resolver to carry useful information, or have specialised
    // handlers in the raw-type pipeline that can produce more precise
    // results (e.g. `array_filter` returning `array` → the raw pipeline
    // can track the input element type).
    //
    // Concrete scalar types (`int`, `string`, `bool`, `float`, etc.)
    // ARE informative — there is no specialised handler that can
    // produce a more specific result for them.
    !matches!(
        ts,
        "array" | "mixed" | "object" | "void" | "null" | "self" | "static" | "$this"
    )
}

/// Resolve `clone $expr` — preserves the cloned expression's type.
///
/// First tries resolving the inner expression structurally (handles
/// `clone new Foo()`, `clone $this->getConfig()`, ternary, etc.).
/// If that yields nothing, falls back to text-based resolution by
/// extracting the source text of the cloned expression and resolving
/// it as a subject string via `resolve_target_classes`.
fn resolve_rhs_clone(clone_expr: &Clone<'_>, ctx: &VarResolutionCtx<'_>) -> Vec<ResolvedType> {
    let structural = resolve_rhs_expression(clone_expr.object, ctx);
    if !structural.is_empty() {
        return structural;
    }
    // Fallback: extract source text of the cloned expression
    // and resolve it as a subject.  This handles cases like
    // `clone $original` where `$original`'s type was set by a
    // prior assignment or parameter type hint.
    let obj_span = clone_expr.object.span();
    let start = obj_span.start.offset as usize;
    let end = obj_span.end.offset as usize;
    if end <= ctx.content.len() {
        let obj_text = ctx.content[start..end].trim();
        if !obj_text.is_empty() {
            let rctx = ctx.as_resolution_ctx();
            return crate::completion::resolver::resolve_target_classes(
                obj_text,
                crate::types::AccessKind::Arrow,
                &rctx,
            )
            .into_iter()
            .map(|arc| ResolvedType::from_class(Arc::unwrap_or_clone(arc)))
            .collect();
        }
    }
    vec![]
}

/// Extract the return type hint from a closure or arrow function expression.
///
/// Returns the type-hint string when the expression is a `Closure` or
/// `ArrowFunction` with an explicit return type annotation, e.g.
/// `fn (): Foo => …` yields `"Foo"`.  Returns `None` otherwise.
fn extract_closure_or_arrow_return_type(expr: &Expression<'_>) -> Option<String> {
    match expr {
        Expression::ArrowFunction(arrow) => arrow
            .return_type_hint
            .as_ref()
            .map(|rth| extract_hint_string(&rth.hint)),
        Expression::Closure(closure) => closure
            .return_type_hint
            .as_ref()
            .map(|rth| extract_hint_string(&rth.hint)),
        _ => None,
    }
}
