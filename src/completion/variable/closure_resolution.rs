/// Closure and arrow-function variable resolution.
///
/// When the cursor is inside a **closure** (`function (Type $p) { … }`),
/// variables are resolved from the closure's own parameter list rather
/// than the enclosing scope (closures have isolated scope in PHP).
///
/// **Arrow functions** (`fn(Type $p) => …`) automatically capture the
/// enclosing scope.  If the target variable matches an arrow function
/// parameter, it is resolved from that parameter list.  Otherwise the
/// walker returns `false` so the enclosing scope continues to resolve
/// the variable from prior assignments, just as PHP semantics require.
///
/// This module contains the recursive AST walkers that detect whether
/// the cursor falls inside such a construct and handle resolution
/// accordingly.
///
/// ## Callable parameter inference
///
/// When a closure or arrow function is passed as an argument to a method
/// or function call, and its parameters have no explicit type hints, the
/// resolver attempts to infer the parameter types from the called
/// method/function's signature.  For example, in
/// `$users->map(fn($u) => $u->name)`, the resolver looks up the `map`
/// method on the resolved type of `$users`, finds that its parameter is
/// typed as `callable(TValue): mixed` (with `TValue` already substituted
/// through generic resolution), and infers `$u` as the concrete element
/// type.
use std::cell::Cell;

use mago_span::HasSpan;
use mago_syntax::ast::sequence::TokenSeparatedSequence;
use mago_syntax::ast::*;

/// Maximum recursion depth for callable parameter inference.
///
/// When a closure parameter is untyped, the resolver infers its type
/// from the enclosing method's signature by resolving the receiver
/// object.  If that receiver text itself contains the same variable
/// (e.g. nested closures reusing `$q`), the resolution re-enters this
/// path, creating an infinite cycle.  This cap breaks the cycle.
const MAX_CLOSURE_INFER_DEPTH: u32 = 4;

thread_local! {
    /// Tracks the current recursion depth of callable parameter
    /// inference to prevent stack overflow from nested closures.
    static CLOSURE_INFER_DEPTH: Cell<u32> = const { Cell::new(0) };
}

use crate::docblock::replace_self_in_type;
use crate::parser::extract_hint_string;
use crate::types::{AccessKind, ClassInfo};

use crate::completion::resolver::VarResolutionCtx;

/// Check whether `stmt` contains a closure or arrow function whose
/// body encloses the cursor.  If so, resolve the variable from the
/// closure's parameter list and walk its body, then return `true`.
pub(in crate::completion) fn try_resolve_in_closure_stmt<'b>(
    stmt: &'b Statement<'b>,
    ctx: &VarResolutionCtx<'_>,
    results: &mut Vec<ClassInfo>,
) -> bool {
    match stmt {
        Statement::Expression(expr_stmt) => {
            try_resolve_in_closure_expr(expr_stmt.expression, ctx, results)
        }
        Statement::Return(ret) => {
            if let Some(val) = &ret.value {
                try_resolve_in_closure_expr(val, ctx, results)
            } else {
                false
            }
        }
        Statement::Block(block) => {
            for inner in block.statements.iter() {
                let s = inner.span();
                if ctx.cursor_offset >= s.start.offset
                    && ctx.cursor_offset <= s.end.offset
                    && try_resolve_in_closure_stmt(inner, ctx, results)
                {
                    return true;
                }
            }
            false
        }
        Statement::If(if_stmt) => match &if_stmt.body {
            IfBody::Statement(body) => try_resolve_in_closure_stmt(body.statement, ctx, results),
            IfBody::ColonDelimited(body) => {
                for inner in body.statements.iter() {
                    let s = inner.span();
                    if ctx.cursor_offset >= s.start.offset
                        && ctx.cursor_offset <= s.end.offset
                        && try_resolve_in_closure_stmt(inner, ctx, results)
                    {
                        return true;
                    }
                }
                false
            }
        },
        Statement::Foreach(foreach) => match &foreach.body {
            ForeachBody::Statement(inner) => try_resolve_in_closure_stmt(inner, ctx, results),
            ForeachBody::ColonDelimited(body) => {
                for inner in body.statements.iter() {
                    let s = inner.span();
                    if ctx.cursor_offset >= s.start.offset
                        && ctx.cursor_offset <= s.end.offset
                        && try_resolve_in_closure_stmt(inner, ctx, results)
                    {
                        return true;
                    }
                }
                false
            }
        },
        Statement::While(while_stmt) => match &while_stmt.body {
            WhileBody::Statement(inner) => try_resolve_in_closure_stmt(inner, ctx, results),
            WhileBody::ColonDelimited(body) => {
                for inner in body.statements.iter() {
                    let s = inner.span();
                    if ctx.cursor_offset >= s.start.offset
                        && ctx.cursor_offset <= s.end.offset
                        && try_resolve_in_closure_stmt(inner, ctx, results)
                    {
                        return true;
                    }
                }
                false
            }
        },
        Statement::For(for_stmt) => match &for_stmt.body {
            ForBody::Statement(inner) => try_resolve_in_closure_stmt(inner, ctx, results),
            ForBody::ColonDelimited(body) => {
                for inner in body.statements.iter() {
                    let s = inner.span();
                    if ctx.cursor_offset >= s.start.offset
                        && ctx.cursor_offset <= s.end.offset
                        && try_resolve_in_closure_stmt(inner, ctx, results)
                    {
                        return true;
                    }
                }
                false
            }
        },
        Statement::DoWhile(dw) => try_resolve_in_closure_stmt(dw.statement, ctx, results),
        Statement::Namespace(ns) => {
            for inner in ns.statements().iter() {
                let s = inner.span();
                if ctx.cursor_offset >= s.start.offset
                    && ctx.cursor_offset <= s.end.offset
                    && try_resolve_in_closure_stmt(inner, ctx, results)
                {
                    return true;
                }
            }
            false
        }
        Statement::Try(try_stmt) => {
            for inner in try_stmt.block.statements.iter() {
                let s = inner.span();
                if ctx.cursor_offset >= s.start.offset
                    && ctx.cursor_offset <= s.end.offset
                    && try_resolve_in_closure_stmt(inner, ctx, results)
                {
                    return true;
                }
            }
            for catch in try_stmt.catch_clauses.iter() {
                for inner in catch.block.statements.iter() {
                    let s = inner.span();
                    if ctx.cursor_offset >= s.start.offset
                        && ctx.cursor_offset <= s.end.offset
                        && try_resolve_in_closure_stmt(inner, ctx, results)
                    {
                        return true;
                    }
                }
            }
            if let Some(finally) = &try_stmt.finally_clause {
                for inner in finally.block.statements.iter() {
                    let s = inner.span();
                    if ctx.cursor_offset >= s.start.offset
                        && ctx.cursor_offset <= s.end.offset
                        && try_resolve_in_closure_stmt(inner, ctx, results)
                    {
                        return true;
                    }
                }
            }
            false
        }
        _ => false,
    }
}

/// Recursively walk an expression looking for a `Closure` or
/// `ArrowFunction` whose body contains the cursor.  When found,
/// resolve the target variable from the closure's parameters and
/// walk its body statements, returning `true`.
pub(in crate::completion) fn try_resolve_in_closure_expr<'b>(
    expr: &'b Expression<'b>,
    ctx: &VarResolutionCtx<'_>,
    results: &mut Vec<ClassInfo>,
) -> bool {
    // Quick span-based prune: if the cursor is not within this
    // expression at all, skip the entire sub-tree.
    let sp = expr.span();
    if ctx.cursor_offset < sp.start.offset || ctx.cursor_offset > sp.end.offset {
        return false;
    }

    match expr {
        // ── Closure: `function (Type $param) { … }` ──
        Expression::Closure(closure) => {
            let body_start = closure.body.left_brace.start.offset;
            let body_end = closure.body.right_brace.end.offset;
            if ctx.cursor_offset >= body_start && ctx.cursor_offset <= body_end {
                resolve_closure_params(&closure.parameter_list, ctx, results);
                super::resolution::walk_statements_for_assignments(
                    closure.body.statements.iter(),
                    ctx,
                    results,
                    false,
                );
                return true;
            }
            false
        }
        // ── Arrow function: `fn(Type $param) => expr` ──
        // Arrow functions capture the enclosing scope automatically
        // (unlike closures which require `use`).  Only claim the
        // variable if it matches one of the arrow function's own
        // parameters; otherwise return `false` so the outer scope
        // walk continues and can resolve variables like `$feature`
        // that were assigned before the arrow function.
        Expression::ArrowFunction(arrow) => {
            let arrow_body_span = arrow.expression.span();
            if ctx.cursor_offset >= arrow.arrow.start.offset
                && ctx.cursor_offset <= arrow_body_span.end.offset
            {
                let is_arrow_param = arrow
                    .parameter_list
                    .parameters
                    .iter()
                    .any(|p| *p.variable.name == *ctx.var_name);
                if is_arrow_param {
                    resolve_closure_params(&arrow.parameter_list, ctx, results);
                    return true;
                }
                // Variable is not a parameter of this arrow function —
                // it must come from the enclosing scope.  Return false
                // so the outer walk resolves it.
                return false;
            }
            false
        }
        Expression::Parenthesized(p) => try_resolve_in_closure_expr(p.expression, ctx, results),
        Expression::Assignment(a) => {
            try_resolve_in_closure_expr(a.lhs, ctx, results)
                || try_resolve_in_closure_expr(a.rhs, ctx, results)
        }
        Expression::Binary(bin) => {
            try_resolve_in_closure_expr(bin.lhs, ctx, results)
                || try_resolve_in_closure_expr(bin.rhs, ctx, results)
        }
        Expression::Conditional(cond) => {
            try_resolve_in_closure_expr(cond.condition, ctx, results)
                || cond
                    .then
                    .is_some_and(|e| try_resolve_in_closure_expr(e, ctx, results))
                || try_resolve_in_closure_expr(cond.r#else, ctx, results)
        }
        Expression::Call(call) => try_resolve_in_closure_call(call, ctx, results),
        Expression::Array(arr) => {
            for elem in arr.elements.iter() {
                let found = match elem {
                    ArrayElement::KeyValue(kv) => {
                        try_resolve_in_closure_expr(kv.key, ctx, results)
                            || try_resolve_in_closure_expr(kv.value, ctx, results)
                    }
                    ArrayElement::Value(v) => try_resolve_in_closure_expr(v.value, ctx, results),
                    ArrayElement::Variadic(v) => try_resolve_in_closure_expr(v.value, ctx, results),
                    _ => false,
                };
                if found {
                    return true;
                }
            }
            false
        }
        Expression::LegacyArray(arr) => {
            for elem in arr.elements.iter() {
                let found = match elem {
                    ArrayElement::KeyValue(kv) => {
                        try_resolve_in_closure_expr(kv.key, ctx, results)
                            || try_resolve_in_closure_expr(kv.value, ctx, results)
                    }
                    ArrayElement::Value(v) => try_resolve_in_closure_expr(v.value, ctx, results),
                    ArrayElement::Variadic(v) => try_resolve_in_closure_expr(v.value, ctx, results),
                    _ => false,
                };
                if found {
                    return true;
                }
            }
            false
        }
        Expression::Match(m) => {
            if try_resolve_in_closure_expr(m.expression, ctx, results) {
                return true;
            }
            for arm in m.arms.iter() {
                if try_resolve_in_closure_expr(arm.expression(), ctx, results) {
                    return true;
                }
            }
            false
        }
        Expression::Access(access) => match access {
            Access::Property(pa) => try_resolve_in_closure_expr(pa.object, ctx, results),
            Access::NullSafeProperty(pa) => try_resolve_in_closure_expr(pa.object, ctx, results),
            Access::StaticProperty(pa) => try_resolve_in_closure_expr(pa.class, ctx, results),
            Access::ClassConstant(pa) => try_resolve_in_closure_expr(pa.class, ctx, results),
        },
        Expression::Instantiation(inst) => {
            if let Some(ref args) = inst.argument_list {
                try_resolve_in_closure_args(&args.arguments, ctx, results)
            } else {
                false
            }
        }
        Expression::UnaryPrefix(u) => try_resolve_in_closure_expr(u.operand, ctx, results),
        Expression::UnaryPostfix(u) => try_resolve_in_closure_expr(u.operand, ctx, results),
        Expression::Yield(y) => match y {
            Yield::Value(yv) => {
                if let Some(val) = &yv.value {
                    try_resolve_in_closure_expr(val, ctx, results)
                } else {
                    false
                }
            }
            Yield::Pair(yp) => {
                try_resolve_in_closure_expr(yp.key, ctx, results)
                    || try_resolve_in_closure_expr(yp.value, ctx, results)
            }
            Yield::From(yf) => try_resolve_in_closure_expr(yf.iterator, ctx, results),
        },
        Expression::Throw(t) => try_resolve_in_closure_expr(t.exception, ctx, results),
        Expression::Clone(c) => try_resolve_in_closure_expr(c.object, ctx, results),
        Expression::Pipe(p) => {
            try_resolve_in_closure_expr(p.input, ctx, results)
                || try_resolve_in_closure_expr(p.callable, ctx, results)
        }
        _ => false,
    }
}

/// Dispatch a `Call` expression: recurse into function, method, and
/// static method calls, checking their argument lists for closures.
fn try_resolve_in_closure_call<'b>(
    call: &'b Call<'b>,
    ctx: &VarResolutionCtx<'_>,
    results: &mut Vec<ClassInfo>,
) -> bool {
    match call {
        Call::Function(fc) => {
            // Try with callable parameter inference from the function signature.
            if let Some(func_name) = extract_function_name_from_call(fc)
                && try_resolve_closure_in_call_args(
                    &fc.argument_list.arguments,
                    ctx,
                    results,
                    |arg_idx| infer_callable_params_from_function(&func_name, arg_idx, ctx),
                )
            {
                return true;
            }
            try_resolve_in_closure_args(&fc.argument_list.arguments, ctx, results)
        }
        Call::Method(mc) => {
            if try_resolve_in_closure_expr(mc.object, ctx, results) {
                return true;
            }
            // Try with callable parameter inference from the method signature.
            if let ClassLikeMemberSelector::Identifier(ident) = &mc.method {
                let method_name = ident.value.to_string();
                let obj_span = mc.object.span();
                if try_resolve_closure_in_call_args(
                    &mc.argument_list.arguments,
                    ctx,
                    results,
                    |arg_idx| {
                        infer_callable_params_from_receiver(
                            obj_span.start.offset,
                            obj_span.end.offset,
                            &method_name,
                            arg_idx,
                            ctx,
                        )
                    },
                ) {
                    return true;
                }
            }
            try_resolve_in_closure_args(&mc.argument_list.arguments, ctx, results)
        }
        Call::NullSafeMethod(mc) => {
            if try_resolve_in_closure_expr(mc.object, ctx, results) {
                return true;
            }
            if let ClassLikeMemberSelector::Identifier(ident) = &mc.method {
                let method_name = ident.value.to_string();
                let obj_span = mc.object.span();
                if try_resolve_closure_in_call_args(
                    &mc.argument_list.arguments,
                    ctx,
                    results,
                    |arg_idx| {
                        infer_callable_params_from_receiver(
                            obj_span.start.offset,
                            obj_span.end.offset,
                            &method_name,
                            arg_idx,
                            ctx,
                        )
                    },
                ) {
                    return true;
                }
            }
            try_resolve_in_closure_args(&mc.argument_list.arguments, ctx, results)
        }
        Call::StaticMethod(sc) => {
            if try_resolve_in_closure_expr(sc.class, ctx, results) {
                return true;
            }
            if let ClassLikeMemberSelector::Identifier(ident) = &sc.method {
                let method_name = ident.value.to_string();
                if try_resolve_closure_in_call_args(
                    &sc.argument_list.arguments,
                    ctx,
                    results,
                    |arg_idx| {
                        infer_callable_params_from_static_receiver(
                            sc.class,
                            &method_name,
                            arg_idx,
                            ctx,
                        )
                    },
                ) {
                    return true;
                }
            }
            try_resolve_in_closure_args(&sc.argument_list.arguments, ctx, results)
        }
    }
}

/// Walk a flat list of call arguments, recursing into each expression.
fn try_resolve_in_closure_args<'b>(
    arguments: &'b TokenSeparatedSequence<'b, Argument<'b>>,
    ctx: &VarResolutionCtx<'_>,
    results: &mut Vec<ClassInfo>,
) -> bool {
    for arg in arguments.iter() {
        let arg_expr = match arg {
            Argument::Positional(pos) => pos.value,
            Argument::Named(named) => named.value,
        };
        if try_resolve_in_closure_expr(arg_expr, ctx, results) {
            return true;
        }
    }
    false
}

/// Walk call arguments looking for a closure/arrow-function that
/// contains the cursor.  When found, resolve its parameters using
/// both explicit type hints and inferred types from the enclosing
/// call's signature (provided by `infer_fn`).
///
/// Returns `true` if the cursor was inside a closure argument and
/// resolution was performed.
fn try_resolve_closure_in_call_args<'b, F>(
    arguments: &'b TokenSeparatedSequence<'b, Argument<'b>>,
    ctx: &VarResolutionCtx<'_>,
    results: &mut Vec<ClassInfo>,
    infer_fn: F,
) -> bool
where
    F: Fn(usize) -> Vec<String>,
{
    for (arg_idx, arg) in arguments.iter().enumerate() {
        let arg_expr = match arg {
            Argument::Positional(pos) => pos.value,
            Argument::Named(named) => named.value,
        };
        let arg_span = arg_expr.span();
        if ctx.cursor_offset < arg_span.start.offset || ctx.cursor_offset > arg_span.end.offset {
            continue;
        }

        match arg_expr {
            Expression::Closure(closure) => {
                let body_start = closure.body.left_brace.start.offset;
                let body_end = closure.body.right_brace.end.offset;
                if ctx.cursor_offset >= body_start && ctx.cursor_offset <= body_end {
                    // Only run inference when the target variable is
                    // actually one of the closure's own parameters.
                    // If the target variable is NOT a closure param,
                    // the inference result would be unused anyway, so
                    // skipping it avoids an infinite recursion cycle.
                    let is_closure_param = closure
                        .parameter_list
                        .parameters
                        .iter()
                        .any(|p| *p.variable.name == *ctx.var_name);
                    if is_closure_param {
                        let inferred = infer_fn(arg_idx);
                        resolve_closure_params_with_inferred(
                            &closure.parameter_list,
                            ctx,
                            results,
                            &inferred,
                        );
                    } else {
                        resolve_closure_params(&closure.parameter_list, ctx, results);
                    }
                    super::resolution::walk_statements_for_assignments(
                        closure.body.statements.iter(),
                        ctx,
                        results,
                        false,
                    );
                    return true;
                }
            }
            Expression::ArrowFunction(arrow) => {
                let arrow_body_span = arrow.expression.span();
                if ctx.cursor_offset >= arrow.arrow.start.offset
                    && ctx.cursor_offset <= arrow_body_span.end.offset
                {
                    let is_closure_param = arrow
                        .parameter_list
                        .parameters
                        .iter()
                        .any(|p| *p.variable.name == *ctx.var_name);
                    if is_closure_param {
                        let inferred = infer_fn(arg_idx);
                        resolve_closure_params_with_inferred(
                            &arrow.parameter_list,
                            ctx,
                            results,
                            &inferred,
                        );
                        return true;
                    }
                    // Variable is not a parameter of this arrow
                    // function — it comes from the enclosing scope.
                    // Return false so the outer walk resolves it.
                    return false;
                }
            }
            _ => {}
        }
        // Not a direct closure — fall through so the normal recursive
        // walker handles nested closures (without inference).
        return false;
    }
    false
}

/// Resolve a variable's type from a closure / arrow-function
/// parameter list.  If the variable matches a typed parameter,
/// the resolved classes replace whatever is currently in `results`.
pub(in crate::completion) fn resolve_closure_params(
    parameter_list: &FunctionLikeParameterList<'_>,
    ctx: &VarResolutionCtx<'_>,
    results: &mut Vec<ClassInfo>,
) {
    resolve_closure_params_with_inferred(parameter_list, ctx, results, &[]);
}

/// Like [`resolve_closure_params`] but accepts a list of inferred
/// parameter types from the enclosing callable signature.  When a
/// closure parameter has no explicit type hint, the corresponding
/// entry in `inferred_types` (matched by positional index) is used
/// as a fallback.
fn resolve_closure_params_with_inferred(
    parameter_list: &FunctionLikeParameterList<'_>,
    ctx: &VarResolutionCtx<'_>,
    results: &mut Vec<ClassInfo>,
    inferred_types: &[String],
) {
    for (idx, param) in parameter_list.parameters.iter().enumerate() {
        let pname = param.variable.name.to_string();
        if pname == ctx.var_name {
            // 1. Try the explicit type hint first.
            if let Some(hint) = &param.hint {
                let type_str = extract_hint_string(hint);

                // When the explicit hint is a bare class name (no
                // generic args) and the inferred type from the callable
                // signature is the same class WITH generic args, prefer
                // the inferred type.  For example, the user writes
                // `function (Collection $customers)` but the callable
                // signature says `callable(Collection<int, Customer>, int): mixed`.
                // Using the inferred `Collection<int, Customer>` preserves
                // template substitution so that foreach iteration resolves
                // the element type.
                if let Some(inferred) = inferred_types.get(idx)
                    && inferred_type_is_more_specific(&type_str, inferred)
                {
                    let resolved = crate::completion::type_resolution::type_hint_to_classes(
                        inferred,
                        &ctx.current_class.name,
                        ctx.all_classes,
                        ctx.class_loader,
                    );
                    if !resolved.is_empty() {
                        *results = resolved;
                        break;
                    }
                }

                let resolved = crate::completion::type_resolution::type_hint_to_classes(
                    &type_str,
                    &ctx.current_class.name,
                    ctx.all_classes,
                    ctx.class_loader,
                );
                if !resolved.is_empty() {
                    // When the inferred type from the callable signature
                    // is a subclass of the explicit type hint, prefer
                    // the inferred type.  For example, the user writes
                    // `function (Model $item)` but the callable signature
                    // says `callable(BrandTranslation): void` where
                    // `BrandTranslation extends Model`.  The narrower
                    // inferred type gives better completion results.
                    if let Some(inferred) = inferred_types.get(idx) {
                        let inferred_resolved =
                            crate::completion::type_resolution::type_hint_to_classes(
                                inferred,
                                &ctx.current_class.name,
                                ctx.all_classes,
                                ctx.class_loader,
                            );
                        if !inferred_resolved.is_empty()
                            && inferred_resolved.iter().all(|inferred_cls| {
                                resolved.iter().any(|explicit_cls| {
                                    crate::completion::types::narrowing::is_subtype_of(
                                        inferred_cls,
                                        &explicit_cls.name,
                                        ctx.class_loader,
                                    )
                                })
                            })
                        {
                            *results = inferred_resolved;
                            break;
                        }
                    }
                    *results = resolved;
                    break;
                }
            }
            // 2. Fall back to the inferred type from the callable
            //    signature of the enclosing method/function call.
            if let Some(inferred) = inferred_types.get(idx) {
                let resolved = crate::completion::type_resolution::type_hint_to_classes(
                    inferred,
                    &ctx.current_class.name,
                    ctx.all_classes,
                    ctx.class_loader,
                );
                if !resolved.is_empty() {
                    *results = resolved;
                }
            }
            break;
        }
    }
}

/// Check whether the inferred callable-signature type is a more specific
/// version of the explicit type hint.
///
/// Returns `true` when the explicit hint is a bare class name (e.g.
/// `Collection`) and the inferred type is the same class with generic
/// arguments (e.g. `Collection<int, Customer>`).  Namespace-qualified
/// names are compared by their last segment so that `Collection` matches
/// `Illuminate\Support\Collection<int, Customer>`.
fn inferred_type_is_more_specific(explicit_hint: &str, inferred: &str) -> bool {
    // The explicit hint must not already carry generic args.
    if explicit_hint.contains('<') {
        return false;
    }
    // The inferred type must carry generic args.
    let angle = match inferred.find('<') {
        Some(pos) => pos,
        None => return false,
    };
    let inferred_base = &inferred[..angle];

    // Compare by short name so that `Collection` matches
    // `Illuminate\Support\Collection<…>`.
    let explicit_short = crate::util::short_name(explicit_hint);
    let inferred_short = crate::util::short_name(inferred_base);

    explicit_short.eq_ignore_ascii_case(inferred_short)
}

// ── Callable parameter inference helpers ────────────────────────────

/// Extract the function name from a `FunctionCall` AST node.
fn extract_function_name_from_call(fc: &FunctionCall<'_>) -> Option<String> {
    match fc.function {
        Expression::Identifier(ident) => Some(ident.value().to_string()),
        _ => None,
    }
}

/// Infer callable parameter types for a closure passed at position
/// `arg_idx` to a standalone function call.
fn infer_callable_params_from_function(
    func_name: &str,
    arg_idx: usize,
    ctx: &VarResolutionCtx<'_>,
) -> Vec<String> {
    let rctx = ctx.as_resolution_ctx();
    let func_info = if let Some(fl) = rctx.function_loader {
        fl(func_name)
    } else {
        None
    };
    if let Some(fi) = func_info {
        extract_callable_params_at(&fi.parameters, arg_idx, ctx)
    } else {
        vec![]
    }
}

/// Infer callable parameter types for a closure passed at position
/// `arg_idx` to an instance method call whose receiver expression
/// spans `[obj_start, obj_end)` in the source text.
fn infer_callable_params_from_receiver(
    obj_start: u32,
    obj_end: u32,
    method_name: &str,
    arg_idx: usize,
    ctx: &VarResolutionCtx<'_>,
) -> Vec<String> {
    // Guard against infinite recursion when nested closures reuse the
    // same variable name (e.g. `$q` in both an outer and inner closure).
    // The cycle is: infer_callable_params_from_receiver →
    // resolve_target_classes → resolve_variable_types →
    // walk_statements_for_assignments → try_resolve_closure_in_call_args
    // → infer_callable_params_from_receiver → ∞
    let depth = CLOSURE_INFER_DEPTH.with(|d| d.get());
    if depth >= MAX_CLOSURE_INFER_DEPTH {
        return vec![];
    }
    CLOSURE_INFER_DEPTH.with(|d| d.set(depth + 1));

    let start = obj_start as usize;
    let end = obj_end as usize;
    if end > ctx.content.len() {
        CLOSURE_INFER_DEPTH.with(|d| d.set(depth));
        return vec![];
    }
    let obj_text = ctx.content[start..end].trim();
    let rctx = ctx.as_resolution_ctx();
    let receiver_classes =
        crate::completion::resolver::resolve_target_classes(obj_text, AccessKind::Arrow, &rctx);

    let params = find_callable_params_on_classes(&receiver_classes, method_name, arg_idx, ctx);

    // Replace `$this` / `static` tokens with the receiver class FQN
    // so that `resolve_closure_params_with_inferred` resolves them
    // against the declaring class rather than the user's current class.
    let result = if let Some(receiver) = receiver_classes.first() {
        let receiver_fqn = receiver.fqn();
        params
            .into_iter()
            .map(|ty| replace_self_in_type(&ty, &receiver_fqn))
            .collect()
    } else {
        params
    };

    CLOSURE_INFER_DEPTH.with(|d| d.set(depth));
    result
}

/// Infer callable parameter types for a closure passed at position
/// `arg_idx` to a static method call.
fn infer_callable_params_from_static_receiver(
    class_expr: &Expression<'_>,
    method_name: &str,
    arg_idx: usize,
    ctx: &VarResolutionCtx<'_>,
) -> Vec<String> {
    let class_name = match class_expr {
        Expression::Self_(_) => Some(ctx.current_class.name.clone()),
        Expression::Static(_) => Some(ctx.current_class.name.clone()),
        Expression::Identifier(ident) => Some(ident.value().to_string()),
        Expression::Parent(_) => ctx.current_class.parent_class.clone(),
        _ => None,
    };
    let owner = class_name.and_then(|name| {
        ctx.all_classes
            .iter()
            .find(|c| c.name == name)
            .cloned()
            .or_else(|| (ctx.class_loader)(&name))
    });
    if let Some(ref cls) = owner {
        let resolved = crate::virtual_members::resolve_class_fully_maybe_cached(
            cls,
            ctx.class_loader,
            ctx.resolved_class_cache,
        );
        let params = find_callable_params_on_method(&resolved, method_name, arg_idx, ctx);
        let owner_fqn = cls.fqn();
        params
            .into_iter()
            .map(|ty| replace_self_in_type(&ty, &owner_fqn))
            .collect()
    } else {
        vec![]
    }
}

/// Search for the method `method_name` on each of `classes` and
/// extract callable parameter types at `arg_idx`.
fn find_callable_params_on_classes(
    classes: &[ClassInfo],
    method_name: &str,
    arg_idx: usize,
    ctx: &VarResolutionCtx<'_>,
) -> Vec<String> {
    for cls in classes {
        let resolved = crate::virtual_members::resolve_class_fully_maybe_cached(
            cls,
            ctx.class_loader,
            ctx.resolved_class_cache,
        );
        let result = find_callable_params_on_method(&resolved, method_name, arg_idx, ctx);
        if !result.is_empty() {
            return result;
        }
    }
    vec![]
}

/// Look up method `method_name` on `class` and extract callable
/// parameter types from the parameter at position `arg_idx`.
fn find_callable_params_on_method(
    class: &ClassInfo,
    method_name: &str,
    arg_idx: usize,
    ctx: &VarResolutionCtx<'_>,
) -> Vec<String> {
    let method = class.methods.iter().find(|m| m.name == method_name);
    if let Some(m) = method {
        extract_callable_params_at(&m.parameters, arg_idx, ctx)
    } else {
        vec![]
    }
}

/// Given a list of method/function parameters, look at the parameter
/// at `arg_idx`.  If its type hint is a `callable(…)` or
/// `Closure(…)`, extract and return the callable's parameter types.
fn extract_callable_params_at(
    params: &[crate::types::ParameterInfo],
    arg_idx: usize,
    _ctx: &VarResolutionCtx<'_>,
) -> Vec<String> {
    let param = params.get(arg_idx);
    if let Some(p) = param
        && let Some(ref hint) = p.type_hint
        && let Some(types) = crate::docblock::extract_callable_param_types(hint)
    {
        return types;
    }
    vec![]
}
