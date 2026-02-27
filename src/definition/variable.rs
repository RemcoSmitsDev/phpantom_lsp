/// Variable definition resolution.
///
/// This module handles go-to-definition for `$variable` references,
/// jumping from a variable usage to its most recent assignment or
/// declaration site.
///
/// The primary path parses the file into an AST and walks the enclosing
/// scope to find the variable's definition site with byte-accurate
/// offsets.  This correctly handles:
///   - Array destructuring: `[$a, $b] = explode(',', $str)`
///   - List destructuring:  `list($a, $b) = func()`
///   - Multi-line parameter lists
///   - Nested scopes (closures, arrow functions)
///
/// Supported definition sites (searched bottom-up from cursor):
///   - **Assignment**: `$var = …` (but not `==` / `===`)
///   - **Parameter**: `Type $var` in a function/method signature
///   - **Foreach**: `as $var` / `=> $var`
///   - **Catch**: `catch (…Exception $var)`
///   - **Static / global**: `static $var` / `global $var`
///   - **Array destructuring**: `[$a, $b] = …` / `list($a, $b) = …`
///
/// When the cursor is already at the definition site (e.g. on a
/// parameter), the module falls through to type-hint resolution:
/// it extracts the type hint and jumps to the first class-like type
/// in it (e.g. `HtmlString` in `HtmlString|string $content`).
///
/// The text-based heuristic scanner is kept as a fallback when the
/// AST parse fails (malformed PHP, parser panic, etc.).
use mago_span::HasSpan;
use mago_syntax::ast::sequence::TokenSeparatedSequence;
use mago_syntax::ast::*;
use tower_lsp::lsp_types::*;

use crate::Backend;
use crate::composer;
use crate::parser::with_parsed_program;
use crate::util::offset_to_position;

// ═══════════════════════════════════════════════════════════════════════
// AST-based variable definition search result
// ═══════════════════════════════════════════════════════════════════════

/// Result of searching for a variable definition in the AST.
#[derive(Default)]
enum VarDefSearchResult {
    /// No definition site found for this variable in the current scope.
    #[default]
    NotFound,
    /// The cursor is already sitting on the definition site (e.g. on a
    /// parameter declaration).  The caller should fall through to
    /// type-hint resolution.
    AtDefinition,
    /// Found a prior definition at the given byte offset.
    /// `offset` is the start of the `$var` token, `end_offset` is the end.
    FoundAt { offset: u32, end_offset: u32 },
}

impl Backend {
    // ──────────────────────────────────────────────────────────────────────
    // Variable go-to-definition helpers
    // ──────────────────────────────────────────────────────────────────────

    /// Returns `true` when the cursor is sitting on a `$variable` token.
    ///
    /// `extract_word_at_position` strips `$`, so we peek at the character
    /// immediately before the word to see if it is `$`.
    pub(super) fn cursor_is_on_variable(content: &str, position: Position, _word: &str) -> bool {
        let lines: Vec<&str> = content.lines().collect();
        let line_idx = position.line as usize;
        if line_idx >= lines.len() {
            return false;
        }
        let line = lines[line_idx];
        let chars: Vec<char> = line.chars().collect();
        let col = (position.character as usize).min(chars.len());

        // Find where `word` starts on this line (same logic as
        // extract_word_at_position: walk left from cursor).
        let is_word_char = |c: char| c.is_alphanumeric() || c == '_' || c == '\\';
        let mut start = col;
        if start < chars.len() && is_word_char(chars[start]) {
            // on a word char
        } else if start > 0 && is_word_char(chars[start - 1]) {
            start -= 1;
        } else {
            return false;
        }
        while start > 0 && is_word_char(chars[start - 1]) {
            start -= 1;
        }

        // The character just before the word must be `$`.
        if start == 0 {
            return false;
        }
        if chars[start - 1] != '$' {
            return false;
        }

        // If the `$` is preceded by `::`, this is a static property access
        // (e.g. `Config::$defaultLocale`), not a local variable.
        if start >= 3 && chars[start - 2] == ':' && chars[start - 3] == ':' {
            return false;
        }

        true
    }

    /// Find the most recent assignment or declaration of `$var_name` before
    /// `position` and return its location.
    ///
    /// **Primary path:** Parse the file into an AST and walk the enclosing
    /// scope to find the definition site with exact byte offsets.
    ///
    /// **Fallback:** When the AST parse fails, fall back to the text-based
    /// heuristic scanner.
    pub(super) fn resolve_variable_definition(
        content: &str,
        uri: &str,
        position: Position,
        var_name: &str,
    ) -> Option<Location> {
        // Try the AST-based path first.
        if let Some(result) =
            Self::resolve_variable_definition_ast(content, uri, position, var_name)
        {
            return result;
        }

        // Fallback: text-based heuristic scanner.
        #[allow(deprecated)] // fallback when AST parse fails
        Self::resolve_variable_definition_text(content, uri, position, var_name)
    }

    /// AST-based variable definition resolution.
    ///
    /// Returns:
    /// - `Some(Some(location))` — found a prior definition, jump there
    /// - `Some(None)` — cursor is at a definition site (fall through to type-hint)
    ///   OR no definition found in the AST (don't fall back to text)
    /// - `None` — AST parse failed, caller should try the text-based fallback
    fn resolve_variable_definition_ast(
        content: &str,
        uri: &str,
        position: Position,
        var_name: &str,
    ) -> Option<Option<Location>> {
        let cursor_offset = Self::position_to_offset(content, position);

        let result = with_parsed_program(
            content,
            "resolve_variable_definition_ast",
            |program, content| {
                find_variable_definition_in_program(program, content, var_name, cursor_offset)
            },
        );

        match result {
            VarDefSearchResult::NotFound => {
                // The AST parse succeeded but found no definition — return
                // Some(None) so the caller knows not to fall back to text.
                Some(None)
            }
            VarDefSearchResult::AtDefinition => {
                // Cursor is at the definition — return Some(None) so the
                // caller falls through to type-hint resolution.
                Some(None)
            }
            VarDefSearchResult::FoundAt { offset, end_offset } => {
                let target_uri = Url::parse(uri).ok()?;
                let start_pos = offset_to_position(content, offset as usize);
                let end_pos = offset_to_position(content, end_offset as usize);
                Some(Some(Location {
                    uri: target_uri,
                    range: Range {
                        start: start_pos,
                        end: end_pos,
                    },
                }))
            }
        }
    }

    /// Text-based fallback for variable definition resolution.
    ///
    /// Recognised definition sites (searched bottom-up):
    ///   - Assignment:          `$var = …`  (but not `==` / `===`)
    ///   - Parameter:           `Type $var` in a function/method signature
    ///   - Foreach:             `as $var`  /  `=> $var`
    ///   - Catch:               `catch (…Exception $var)`
    ///   - Static / global:     `static $var` / `global $var`
    ///
    /// **Deprecated:** The AST-based `resolve_variable_definition_ast`
    /// and the precomputed `SymbolMap::find_var_definition` supersede
    /// this heuristic.  Retained only as a fallback when the AST parse
    /// fails.
    #[deprecated(note = "text-search fallback — prefer AST-based variable resolution")]
    fn resolve_variable_definition_text(
        content: &str,
        uri: &str,
        position: Position,
        var_name: &str,
    ) -> Option<Location> {
        let lines: Vec<&str> = content.lines().collect();
        let cursor_line = position.line as usize;
        let cursor_col = position.character as usize;

        // If the cursor line itself defines the variable, the user is
        // already at a definition site.
        #[allow(deprecated)]
        if cursor_line < lines.len()
            && let Some(def_col) = Self::line_defines_variable(lines[cursor_line], var_name)
        {
            let def_end = def_col + var_name.len();
            if cursor_col >= def_col && cursor_col <= def_end {
                return None;
            }
        }

        let search_end = cursor_line + 1;

        for line_idx in (0..search_end).rev() {
            let line = lines[line_idx];

            if !line.contains(var_name) {
                continue;
            }

            #[allow(deprecated)]
            if let Some(col) = Self::line_defines_variable(line, var_name) {
                if line_idx == cursor_line {
                    if col >= cursor_col {
                        continue;
                    }
                    if Self::line_defines_variable_as_assignment(line, var_name, col) {
                        continue;
                    }
                }
                let target_uri = Url::parse(uri).ok()?;
                return Some(Location {
                    uri: target_uri,
                    range: Range {
                        start: Position {
                            line: line_idx as u32,
                            character: col as u32,
                        },
                        end: Position {
                            line: line_idx as u32,
                            character: (col + var_name.len()) as u32,
                        },
                    },
                });
            }
        }

        None
    }

    /// Find a whole-word occurrence of `var_name` in `line`, skipping
    /// partial matches like `$item` inside `$items`.
    fn find_whole_var(line: &str, var_name: &str) -> Option<usize> {
        let is_ident_char = |c: char| c.is_alphanumeric() || c == '_';
        let mut start = 0;
        while let Some(pos) = line[start..].find(var_name) {
            let abs = start + pos;
            let after = abs + var_name.len();
            let boundary_ok =
                after >= line.len() || !line[after..].starts_with(|c: char| is_ident_char(c));
            if boundary_ok {
                return Some(abs);
            }
            start = abs + 1;
        }
        None
    }

    /// Returns `true` when the definition at `def_col` is a plain
    /// assignment (`$var = …`), as opposed to a parameter, foreach,
    /// catch, or static/global declaration.
    fn line_defines_variable_as_assignment(line: &str, var_name: &str, def_col: usize) -> bool {
        let after_var = def_col + var_name.len();
        if after_var > line.len() {
            return false;
        }
        let rest = &line[after_var..];
        let rest_trimmed = rest.trim_start();
        rest_trimmed.starts_with('=') && !rest_trimmed.starts_with("==")
    }

    /// Heuristically decide whether `line` *defines* (assigns / declares)
    /// `$var_name`.
    ///
    /// Returns `Some(column)` with the byte offset of the variable on the
    /// line when it is a definition site, or `None` otherwise.
    ///
    /// **Deprecated:** Only used by the deprecated
    /// `resolve_variable_definition_text`.  Will be removed together
    /// with it once the AST-based paths are stable.
    #[deprecated(note = "only used by resolve_variable_definition_text")]
    fn line_defines_variable(line: &str, var_name: &str) -> Option<usize> {
        let var_pos = Self::find_whole_var(line, var_name)?;
        let after_var = var_pos + var_name.len();
        let rest = &line[after_var..];

        // 1. Assignment: `$var =` but NOT `$var ==` / `$var ===`
        let rest_trimmed = rest.trim_start();
        if rest_trimmed.starts_with('=') && !rest_trimmed.starts_with("==") {
            return Some(var_pos);
        }

        // 2. Foreach value: `as $var` or `=> $var`
        let before = line[..var_pos].trim_end();
        if before.ends_with("as") || before.ends_with("=>") {
            return Some(var_pos);
        }

        // 3. Function / method parameter
        if (line.contains("function ")
            || line.contains("function(")
            || line.contains("fn ")
            || line.contains("fn("))
            && before.contains('(')
        {
            return Some(var_pos);
        }

        // 4. Catch variable: `catch (SomeException $var)`
        if before.contains("catch") && before.contains('(') {
            return Some(var_pos);
        }

        // 5. Static / global declarations
        if before.ends_with("static") || before.ends_with("global") {
            return Some(var_pos);
        }

        None
    }

    // ─── Type-Hint Resolution at Variable Definition ────────────────────

    /// When the cursor is on a variable that is already at its definition
    /// site (parameter, property, promoted property), extract the type hint
    /// and jump to the first class-like type in it.
    ///
    /// For example, given `public readonly HtmlString|string $content,`
    /// this returns the location of the `HtmlString` class definition.
    pub(super) fn resolve_type_hint_at_variable(
        &self,
        uri: &str,
        content: &str,
        position: Position,
        var_name: &str,
    ) -> Option<Location> {
        // Try AST-based type-hint extraction first.
        if let Some(result) =
            self.resolve_type_hint_at_variable_ast(uri, content, position, var_name)
        {
            return Some(result);
        }

        // Fall back to text-based extraction.
        self.resolve_type_hint_at_variable_text(uri, content, position, var_name)
    }

    /// AST-based type-hint resolution: extract the type hint from the AST
    /// node where the variable is defined (parameter, catch, property).
    fn resolve_type_hint_at_variable_ast(
        &self,
        uri: &str,
        content: &str,
        position: Position,
        var_name: &str,
    ) -> Option<Location> {
        let cursor_offset = Self::position_to_offset(content, position);

        let type_hint_str: Option<String> = with_parsed_program(
            content,
            "resolve_type_hint_at_variable_ast",
            |program, _| find_type_hint_at_definition(program, var_name, cursor_offset),
        );

        let type_hint = type_hint_str?;
        self.resolve_type_hint_string_to_location(uri, content, &type_hint)
    }

    /// Text-based type-hint resolution (original implementation).
    fn resolve_type_hint_at_variable_text(
        &self,
        uri: &str,
        content: &str,
        position: Position,
        var_name: &str,
    ) -> Option<Location> {
        let lines: Vec<&str> = content.lines().collect();
        let line_idx = position.line as usize;
        if line_idx >= lines.len() {
            return None;
        }
        let line = lines[line_idx];

        let var_pos = Self::find_whole_var(line, var_name)?;

        let before_raw = line[..var_pos].trim_end();

        let before = match before_raw.rfind('(') {
            Some(pos) => before_raw[pos + 1..].trim_start(),
            None => before_raw,
        };

        let type_hint = match before.rsplit_once(char::is_whitespace) {
            Some((_, t)) => t,
            None => before,
        };
        if type_hint.is_empty() {
            return None;
        }

        self.resolve_type_hint_string_to_location(uri, content, type_hint)
    }

    /// Given a type-hint string (e.g. `HtmlString|string`, `?Foo`),
    /// resolve it to the definition location of the first class-like type.
    fn resolve_type_hint_string_to_location(
        &self,
        uri: &str,
        content: &str,
        type_hint: &str,
    ) -> Option<Location> {
        let scalars = [
            "string", "int", "float", "bool", "array", "callable", "iterable", "object", "mixed",
            "void", "never", "null", "false", "true", "self", "static", "parent",
        ];

        let class_name = type_hint
            .split(['|', '&'])
            .map(|t| t.trim_start_matches('?'))
            .find(|t| !t.is_empty() && !scalars.contains(&t.to_lowercase().as_str()))?;

        let ctx = self.file_context(uri);

        let fqn = Self::resolve_to_fqn(class_name, &ctx.use_map, &ctx.namespace);

        let mut candidates = vec![fqn];
        if class_name.contains('\\') && !candidates.contains(&class_name.to_string()) {
            candidates.push(class_name.to_string());
        }

        // Try same-file first.
        for fqn in &candidates {
            if let Some(location) = self.find_definition_in_ast_map(fqn, content, uri) {
                return Some(location);
            }
        }

        // Try PSR-4 resolution.
        // resolve_class_in_file parses, caches, and uses keyword_offset
        // (AST-based), falling back to text search only when the parser
        // fails.
        let workspace_root = self
            .workspace_root
            .lock()
            .ok()
            .and_then(|guard| guard.clone());

        if let Some(workspace_root) = workspace_root
            && let Ok(mappings) = self.psr4_mappings.lock()
        {
            for fqn in &candidates {
                if let Some(file_path) =
                    composer::resolve_class_path(&mappings, &workspace_root, fqn)
                    && let Some(location) = self.resolve_class_in_file(&file_path, fqn)
                {
                    return Some(location);
                }
            }
        }

        None
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Free functions: AST-based variable definition finding
// ═══════════════════════════════════════════════════════════════════════

/// Top-level entry: find the definition site of `var_name` in the parsed
/// program at the given cursor offset.
fn find_variable_definition_in_program(
    program: &Program<'_>,
    _content: &str,
    var_name: &str,
    cursor_offset: u32,
) -> VarDefSearchResult {
    // Walk top-level statements, drilling into the scope that contains
    // the cursor.
    find_in_statements(program.statements.iter(), var_name, cursor_offset)
}

/// Walk a sequence of statements looking for the scope that contains the
/// cursor, then search within that scope for the variable definition.
fn find_in_statements<'a, I>(
    statements: I,
    var_name: &str,
    cursor_offset: u32,
) -> VarDefSearchResult
where
    I: Iterator<Item = &'a Statement<'a>>,
{
    let stmts: Vec<&Statement> = statements.collect();

    // Step 1: Check if the cursor is inside a class, function, or namespace.
    for &stmt in &stmts {
        match stmt {
            Statement::Class(class) => {
                let start = class.left_brace.start.offset;
                let end = class.right_brace.end.offset;
                if cursor_offset >= start && cursor_offset <= end {
                    return find_in_class_members(class.members.iter(), var_name, cursor_offset);
                }
            }
            Statement::Interface(iface) => {
                let start = iface.left_brace.start.offset;
                let end = iface.right_brace.end.offset;
                if cursor_offset >= start && cursor_offset <= end {
                    return find_in_class_members(iface.members.iter(), var_name, cursor_offset);
                }
            }
            Statement::Trait(trait_def) => {
                let start = trait_def.left_brace.start.offset;
                let end = trait_def.right_brace.end.offset;
                if cursor_offset >= start && cursor_offset <= end {
                    return find_in_class_members(
                        trait_def.members.iter(),
                        var_name,
                        cursor_offset,
                    );
                }
            }
            Statement::Enum(enum_def) => {
                let start = enum_def.left_brace.start.offset;
                let end = enum_def.right_brace.end.offset;
                if cursor_offset >= start && cursor_offset <= end {
                    return find_in_class_members(enum_def.members.iter(), var_name, cursor_offset);
                }
            }
            Statement::Namespace(ns) => {
                // Recurse into namespace body.
                let result = find_in_statements(ns.statements().iter(), var_name, cursor_offset);
                if !matches!(result, VarDefSearchResult::NotFound) {
                    return result;
                }
            }
            Statement::Function(func) => {
                let body_start = func.body.left_brace.start.offset;
                let body_end = func.body.right_brace.end.offset;
                if cursor_offset >= body_start && cursor_offset <= body_end {
                    return find_in_function_scope(
                        &func.parameter_list,
                        func.body.statements.iter(),
                        var_name,
                        cursor_offset,
                    );
                }
            }
            _ => {}
        }
    }

    // Step 2: Cursor is in top-level code.  Walk all statements.
    find_def_in_statement_list(&stmts, var_name, cursor_offset, None)
}

/// Search class-like members (methods) for the scope containing the cursor.
fn find_in_class_members<'a, I>(
    members: I,
    var_name: &str,
    cursor_offset: u32,
) -> VarDefSearchResult
where
    I: Iterator<Item = &'a ClassLikeMember<'a>>,
{
    for member in members {
        if let ClassLikeMember::Method(method) = member
            && let MethodBody::Concrete(body) = &method.body
        {
            let body_start = body.left_brace.start.offset;
            let body_end = body.right_brace.end.offset;
            if cursor_offset >= body_start && cursor_offset <= body_end {
                return find_in_function_scope(
                    &method.parameter_list,
                    body.statements.iter(),
                    var_name,
                    cursor_offset,
                );
            }
        }
    }
    VarDefSearchResult::NotFound
}

/// Search within a function/method scope: check parameters first, then
/// walk the body statements.
fn find_in_function_scope<'a, I>(
    params: &FunctionLikeParameterList<'a>,
    body_statements: I,
    var_name: &str,
    cursor_offset: u32,
) -> VarDefSearchResult
where
    I: Iterator<Item = &'a Statement<'a>>,
{
    let stmts: Vec<&Statement> = body_statements.collect();

    // Check if the cursor is inside a nested closure/arrow function.
    if let Some(result) = find_in_nested_closure(&stmts, var_name, cursor_offset) {
        return result;
    }

    // Search body statements for definition sites.
    let body_result = find_def_in_statement_list(&stmts, var_name, cursor_offset, None);
    if !matches!(body_result, VarDefSearchResult::NotFound) {
        return body_result;
    }

    // Check function parameters (searched last because they precede
    // all body statements — if a body assignment exists, it's more
    // recent and takes priority).
    find_in_params(params, var_name, cursor_offset)
}

/// Check if the cursor is inside a closure or arrow function nested
/// within the given statements.  If so, resolve within that inner scope.
fn find_in_nested_closure(
    stmts: &[&Statement<'_>],
    var_name: &str,
    cursor_offset: u32,
) -> Option<VarDefSearchResult> {
    for &stmt in stmts {
        let stmt_span = stmt.span();
        if cursor_offset < stmt_span.start.offset || cursor_offset > stmt_span.end.offset {
            continue;
        }
        if let Some(result) = find_closure_in_statement(stmt, var_name, cursor_offset) {
            return Some(result);
        }
    }
    None
}

/// Recursively check a statement for closures/arrow functions that
/// contain the cursor.
fn find_closure_in_statement(
    stmt: &Statement<'_>,
    var_name: &str,
    cursor_offset: u32,
) -> Option<VarDefSearchResult> {
    match stmt {
        Statement::Expression(expr_stmt) => {
            find_closure_in_expression(expr_stmt.expression, var_name, cursor_offset)
        }
        Statement::Return(ret) => {
            if let Some(expr) = ret.value {
                find_closure_in_expression(expr, var_name, cursor_offset)
            } else {
                None
            }
        }
        Statement::If(if_stmt) => {
            // Check condition
            if let Some(r) = find_closure_in_expression(if_stmt.condition, var_name, cursor_offset)
            {
                return Some(r);
            }
            // Check body statements
            for inner in if_stmt.body.statements() {
                if let Some(r) = find_closure_in_statement(inner, var_name, cursor_offset) {
                    return Some(r);
                }
            }
            None
        }
        Statement::Foreach(foreach) => {
            for inner in foreach.body.statements() {
                if let Some(r) = find_closure_in_statement(inner, var_name, cursor_offset) {
                    return Some(r);
                }
            }
            None
        }
        Statement::While(while_stmt) => {
            for inner in while_stmt.body.statements() {
                if let Some(r) = find_closure_in_statement(inner, var_name, cursor_offset) {
                    return Some(r);
                }
            }
            None
        }
        Statement::For(for_stmt) => {
            for inner in for_stmt.body.statements() {
                if let Some(r) = find_closure_in_statement(inner, var_name, cursor_offset) {
                    return Some(r);
                }
            }
            None
        }
        Statement::Try(try_stmt) => {
            for inner in try_stmt.block.statements.iter() {
                if let Some(r) = find_closure_in_statement(inner, var_name, cursor_offset) {
                    return Some(r);
                }
            }
            for catch in try_stmt.catch_clauses.iter() {
                for inner in catch.block.statements.iter() {
                    if let Some(r) = find_closure_in_statement(inner, var_name, cursor_offset) {
                        return Some(r);
                    }
                }
            }
            if let Some(ref finally) = try_stmt.finally_clause {
                for inner in finally.block.statements.iter() {
                    if let Some(r) = find_closure_in_statement(inner, var_name, cursor_offset) {
                        return Some(r);
                    }
                }
            }
            None
        }
        Statement::Block(block) => {
            for inner in block.statements.iter() {
                if let Some(r) = find_closure_in_statement(inner, var_name, cursor_offset) {
                    return Some(r);
                }
            }
            None
        }
        _ => None,
    }
}

/// Recursively check an expression for closures/arrow functions that
/// contain the cursor.
fn find_closure_in_expression(
    expr: &Expression<'_>,
    var_name: &str,
    cursor_offset: u32,
) -> Option<VarDefSearchResult> {
    // Quick span check: if cursor is not inside this expression, skip.
    let span = expr.span();
    if cursor_offset < span.start.offset || cursor_offset > span.end.offset {
        return None;
    }

    match expr {
        Expression::Closure(closure) => {
            let body_start = closure.body.left_brace.start.offset;
            let body_end = closure.body.right_brace.end.offset;
            if cursor_offset >= body_start && cursor_offset <= body_end {
                return Some(find_in_function_scope(
                    &closure.parameter_list,
                    closure.body.statements.iter(),
                    var_name,
                    cursor_offset,
                ));
            }
            None
        }
        Expression::ArrowFunction(arrow) => {
            // Arrow functions have a single expression body.
            // The scope includes parameters.
            let body_span = arrow.expression.span();
            if cursor_offset >= body_span.start.offset && cursor_offset <= body_span.end.offset {
                // Check parameters first.
                let param_result = find_in_params(&arrow.parameter_list, var_name, cursor_offset);
                if !matches!(param_result, VarDefSearchResult::NotFound) {
                    return Some(param_result);
                }
            }
            // Recurse into the body expression.
            find_closure_in_expression(arrow.expression, var_name, cursor_offset)
        }
        Expression::Assignment(assignment) => {
            if let Some(r) = find_closure_in_expression(assignment.rhs, var_name, cursor_offset) {
                return Some(r);
            }
            find_closure_in_expression(assignment.lhs, var_name, cursor_offset)
        }
        Expression::Call(call) => match call {
            Call::Function(func_call) => {
                for arg in func_call.argument_list.arguments.iter() {
                    let arg_expr: &Expression<'_> = arg.value();
                    if let Some(r) = find_closure_in_expression(arg_expr, var_name, cursor_offset) {
                        return Some(r);
                    }
                }
                find_closure_in_expression(func_call.function, var_name, cursor_offset)
            }
            Call::Method(method_call) => {
                for arg in method_call.argument_list.arguments.iter() {
                    let arg_expr: &Expression<'_> = arg.value();
                    if let Some(r) = find_closure_in_expression(arg_expr, var_name, cursor_offset) {
                        return Some(r);
                    }
                }
                find_closure_in_expression(method_call.object, var_name, cursor_offset)
            }
            Call::StaticMethod(static_call) => {
                for arg in static_call.argument_list.arguments.iter() {
                    let arg_expr: &Expression<'_> = arg.value();
                    if let Some(r) = find_closure_in_expression(arg_expr, var_name, cursor_offset) {
                        return Some(r);
                    }
                }
                find_closure_in_expression(static_call.class, var_name, cursor_offset)
            }
            _ => None,
        },
        Expression::Parenthesized(p) => {
            find_closure_in_expression(p.expression, var_name, cursor_offset)
        }
        Expression::Instantiation(inst) => {
            if let Some(ref args) = inst.argument_list {
                for arg in args.arguments.iter() {
                    if let Some(r) =
                        find_closure_in_expression(arg.value(), var_name, cursor_offset)
                    {
                        return Some(r);
                    }
                }
            }
            None
        }
        Expression::Array(arr) => {
            for elem in arr.elements.iter() {
                let value = match elem {
                    ArrayElement::KeyValue(kv) => kv.value,
                    ArrayElement::Value(v) => v.value,
                    _ => continue,
                };
                if let Some(r) = find_closure_in_expression(value, var_name, cursor_offset) {
                    return Some(r);
                }
            }
            None
        }

        _ => None,
    }
}

/// Search parameters for a matching variable definition.
fn find_in_params(
    params: &FunctionLikeParameterList<'_>,
    var_name: &str,
    cursor_offset: u32,
) -> VarDefSearchResult {
    for param in params.parameters.iter() {
        let pname = param.variable.name.to_string();
        if pname == var_name {
            let var_start = param.variable.span.start.offset;
            let var_end = param.variable.span.end.offset;

            // Check if cursor is on this parameter's variable.
            if cursor_offset >= var_start && cursor_offset < var_end {
                return VarDefSearchResult::AtDefinition;
            }

            // Otherwise, this parameter is a definition site.
            return VarDefSearchResult::FoundAt {
                offset: var_start,
                end_offset: var_end,
            };
        }
    }
    VarDefSearchResult::NotFound
}

/// Represents a definition site found during a statement walk.
#[derive(Clone, Copy)]
struct DefSite {
    /// Byte offset of the `$var` token start.
    offset: u32,
    /// Byte offset of the `$var` token end.
    end_offset: u32,
}

/// Walk a flat list of statements, collecting variable definition sites
/// that occur before the cursor.  Returns the most recent one, or
/// `AtDefinition` if the cursor is sitting on a definition.
fn find_def_in_statement_list(
    stmts: &[&Statement<'_>],
    var_name: &str,
    cursor_offset: u32,
    initial: Option<DefSite>,
) -> VarDefSearchResult {
    let mut best: Option<DefSite> = initial;

    for &stmt in stmts {
        let stmt_span = stmt.span();

        // Skip statements that start after the cursor (but we still
        // need to check foreach/try/if that *contain* the cursor).
        let starts_before_cursor = stmt_span.start.offset < cursor_offset;
        // Also allow "starts at cursor" for AtDefinition checks.
        let starts_at_or_before_cursor = stmt_span.start.offset <= cursor_offset;

        match stmt {
            Statement::Expression(expr_stmt) => {
                if !starts_at_or_before_cursor {
                    continue;
                }
                // Check for closures/arrow functions containing the cursor.
                if cursor_offset >= stmt_span.start.offset
                    && cursor_offset <= stmt_span.end.offset
                    && let Some(result) =
                        find_closure_in_expression(expr_stmt.expression, var_name, cursor_offset)
                {
                    return result;
                }
                if let Some(result) =
                    find_def_in_expression(expr_stmt.expression, var_name, cursor_offset)
                {
                    match result {
                        ExprDefResult::AtDefinition => return VarDefSearchResult::AtDefinition,
                        ExprDefResult::Found(site) => best = Some(site),
                    }
                }
            }

            Statement::Foreach(foreach) => {
                // Check the foreach key/value variables as definition sites.
                if let Some(result) = check_foreach_def(foreach, var_name, cursor_offset) {
                    match result {
                        ExprDefResult::AtDefinition => return VarDefSearchResult::AtDefinition,
                        ExprDefResult::Found(site) => best = Some(site),
                    }
                }

                // Recurse into body if cursor is inside.
                let body_span = foreach.body.span();
                if cursor_offset >= body_span.start.offset && cursor_offset <= body_span.end.offset
                {
                    let body_stmts: Vec<&Statement> = foreach.body.statements().iter().collect();
                    let result =
                        find_def_in_statement_list(&body_stmts, var_name, cursor_offset, best);
                    return result;
                }
            }

            Statement::Try(try_stmt) => {
                // Walk try block.
                let try_stmts: Vec<&Statement> = try_stmt.block.statements.iter().collect();
                let try_span = try_stmt.block.span();
                if cursor_offset >= try_span.start.offset && cursor_offset <= try_span.end.offset {
                    return find_def_in_statement_list(&try_stmts, var_name, cursor_offset, best);
                }

                // Walk catch clauses.
                for catch in try_stmt.catch_clauses.iter() {
                    // Check if catch variable matches.
                    if let Some(ref var) = catch.variable
                        && var.name == var_name
                    {
                        let var_start = var.span.start.offset;
                        let var_end = var.span.end.offset;
                        if cursor_offset >= var_start && cursor_offset < var_end {
                            return VarDefSearchResult::AtDefinition;
                        }
                        if var_start < cursor_offset {
                            best = Some(DefSite {
                                offset: var_start,
                                end_offset: var_end,
                            });
                        }
                    }

                    let catch_span = catch.block.span();
                    if cursor_offset >= catch_span.start.offset
                        && cursor_offset <= catch_span.end.offset
                    {
                        let catch_stmts: Vec<&Statement> = catch.block.statements.iter().collect();
                        return find_def_in_statement_list(
                            &catch_stmts,
                            var_name,
                            cursor_offset,
                            best,
                        );
                    }
                }

                // Walk finally clause.
                if let Some(ref finally) = try_stmt.finally_clause {
                    let finally_span = finally.block.span();
                    if cursor_offset >= finally_span.start.offset
                        && cursor_offset <= finally_span.end.offset
                    {
                        let finally_stmts: Vec<&Statement> =
                            finally.block.statements.iter().collect();
                        return find_def_in_statement_list(
                            &finally_stmts,
                            var_name,
                            cursor_offset,
                            best,
                        );
                    }
                }
            }

            Statement::If(if_stmt) => {
                // Walk all branches of the if statement.
                for inner in if_stmt.body.statements() {
                    let inner_span = inner.span();
                    if cursor_offset >= inner_span.start.offset
                        && cursor_offset <= inner_span.end.offset
                    {
                        let inner_stmts = vec![inner];
                        return find_def_in_statement_list(
                            &inner_stmts,
                            var_name,
                            cursor_offset,
                            best,
                        );
                    }
                    if starts_before_cursor && inner_span.end.offset < cursor_offset {
                        let inner_stmts = vec![inner];
                        let result =
                            find_def_in_statement_list(&inner_stmts, var_name, cursor_offset, best);
                        if let VarDefSearchResult::FoundAt { offset, end_offset } = result {
                            best = Some(DefSite { offset, end_offset });
                        }
                    }
                }
            }

            Statement::While(while_stmt) => {
                let body_span = while_stmt.body.span();
                if cursor_offset >= body_span.start.offset && cursor_offset <= body_span.end.offset
                {
                    let body_stmts: Vec<&Statement> = while_stmt.body.statements().iter().collect();
                    return find_def_in_statement_list(&body_stmts, var_name, cursor_offset, best);
                }
            }

            Statement::DoWhile(do_while) => {
                let inner_span = do_while.statement.span();
                if cursor_offset >= inner_span.start.offset
                    && cursor_offset <= inner_span.end.offset
                {
                    let inner_stmts = vec![do_while.statement];
                    return find_def_in_statement_list(&inner_stmts, var_name, cursor_offset, best);
                }
            }

            Statement::For(for_stmt) => {
                // Check initializations for assignments.
                if starts_at_or_before_cursor {
                    for init_expr in for_stmt.initializations.iter() {
                        if let Some(result) =
                            find_def_in_expression(init_expr, var_name, cursor_offset)
                        {
                            match result {
                                ExprDefResult::AtDefinition => {
                                    return VarDefSearchResult::AtDefinition;
                                }
                                ExprDefResult::Found(site) => best = Some(site),
                            }
                        }
                    }
                }
                let body_span = for_stmt.body.span();
                if cursor_offset >= body_span.start.offset && cursor_offset <= body_span.end.offset
                {
                    let body_stmts: Vec<&Statement> = for_stmt.body.statements().iter().collect();
                    return find_def_in_statement_list(&body_stmts, var_name, cursor_offset, best);
                }
            }

            Statement::Switch(switch_stmt) => {
                for case in switch_stmt.body.cases() {
                    let case_stmts: Vec<&Statement> = case.statements().iter().collect();
                    // Check if cursor is in any case.
                    for &inner in &case_stmts {
                        let inner_span: mago_span::Span = inner.span();
                        if cursor_offset >= inner_span.start.offset
                            && cursor_offset <= inner_span.end.offset
                        {
                            return find_def_in_statement_list(
                                &case_stmts,
                                var_name,
                                cursor_offset,
                                best,
                            );
                        }
                    }
                    // Scan completed cases for definitions.
                    if starts_before_cursor {
                        let result =
                            find_def_in_statement_list(&case_stmts, var_name, cursor_offset, best);
                        if let VarDefSearchResult::FoundAt { offset, end_offset } = result {
                            best = Some(DefSite { offset, end_offset });
                        }
                    }
                }
            }

            Statement::Block(block) => {
                let block_stmts: Vec<&Statement> = block.statements.iter().collect();
                let block_span = block.span();
                if cursor_offset >= block_span.start.offset
                    && cursor_offset <= block_span.end.offset
                {
                    return find_def_in_statement_list(&block_stmts, var_name, cursor_offset, best);
                }
                if starts_before_cursor {
                    let result =
                        find_def_in_statement_list(&block_stmts, var_name, cursor_offset, best);
                    if let VarDefSearchResult::FoundAt { offset, end_offset } = result {
                        best = Some(DefSite { offset, end_offset });
                    }
                }
            }

            Statement::Global(global) => {
                if !starts_at_or_before_cursor {
                    continue;
                }
                for var in global.variables.iter() {
                    if let Variable::Direct(dv) = var
                        && dv.name == var_name
                    {
                        let var_start = dv.span.start.offset;
                        let var_end = dv.span.end.offset;
                        if cursor_offset >= var_start && cursor_offset < var_end {
                            return VarDefSearchResult::AtDefinition;
                        }
                        best = Some(DefSite {
                            offset: var_start,
                            end_offset: var_end,
                        });
                    }
                }
            }

            Statement::Static(static_stmt) => {
                if !starts_at_or_before_cursor {
                    continue;
                }
                for item in static_stmt.items.iter() {
                    let dv = item.variable();
                    if dv.name == var_name {
                        let var_start = dv.span.start.offset;
                        let var_end = dv.span.end.offset;
                        if cursor_offset >= var_start && cursor_offset < var_end {
                            return VarDefSearchResult::AtDefinition;
                        }
                        best = Some(DefSite {
                            offset: var_start,
                            end_offset: var_end,
                        });
                    }
                }
            }

            Statement::Return(ret) => {
                if !starts_at_or_before_cursor {
                    continue;
                }
                if let Some(expr) = ret.value
                    && let Some(result) = find_def_in_expression(expr, var_name, cursor_offset)
                {
                    match result {
                        ExprDefResult::AtDefinition => {
                            return VarDefSearchResult::AtDefinition;
                        }
                        ExprDefResult::Found(site) => best = Some(site),
                    }
                }
            }

            _ => {}
        }
    }

    match best {
        Some(site) => VarDefSearchResult::FoundAt {
            offset: site.offset,
            end_offset: site.end_offset,
        },
        None => VarDefSearchResult::NotFound,
    }
}

/// Result of checking an expression for a variable definition.
enum ExprDefResult {
    /// The cursor is on the definition.
    AtDefinition,
    /// Found a definition site.
    Found(DefSite),
}

/// Check if an expression contains a definition of `var_name`.
fn find_def_in_expression(
    expr: &Expression<'_>,
    var_name: &str,
    cursor_offset: u32,
) -> Option<ExprDefResult> {
    if let Expression::Assignment(assignment) = expr {
        if !assignment.operator.is_assign() {
            return None;
        }

        // ── Array destructuring: `[$a, $b] = …` / `list($a, $b) = …` ──
        match assignment.lhs {
            Expression::Array(arr) => {
                return find_var_in_destructuring_tss(&arr.elements, var_name, cursor_offset);
            }
            Expression::List(list) => {
                return find_var_in_destructuring_tss(&list.elements, var_name, cursor_offset);
            }
            _ => {}
        }

        // ── Direct variable assignment ──
        if let Expression::Variable(Variable::Direct(dv)) = assignment.lhs
            && dv.name == var_name
        {
            let var_start = dv.span.start.offset;
            let var_end = dv.span.end.offset;
            if cursor_offset >= var_start && cursor_offset < var_end {
                return Some(ExprDefResult::AtDefinition);
            }
            // When the cursor is inside the RHS of this assignment
            // (e.g. `$value = $value->value` with cursor on the RHS
            // `$value`), do NOT count this assignment as a definition
            // site.  The user wants to jump to the *original*
            // declaration (e.g. a parameter), not to the LHS of the
            // same statement.
            let rhs_span = assignment.rhs.span();
            if cursor_offset >= rhs_span.start.offset && cursor_offset <= rhs_span.end.offset {
                return None;
            }
            if var_start < cursor_offset {
                return Some(ExprDefResult::Found(DefSite {
                    offset: var_start,
                    end_offset: var_end,
                }));
            }
        }
    }

    None
}

/// Search array/list destructuring elements for our variable.
/// Search `TokenSeparatedSequence` destructuring elements (from Array/List expressions).
fn find_var_in_destructuring_tss(
    elements: &TokenSeparatedSequence<'_, ArrayElement<'_>>,
    var_name: &str,
    cursor_offset: u32,
) -> Option<ExprDefResult> {
    find_var_in_destructuring_iter(elements.iter(), var_name, cursor_offset)
}

/// Search `Sequence` destructuring elements (from foreach targets, etc.).
/// Core destructuring search implementation.
fn find_var_in_destructuring_iter<'a>(
    elements: impl Iterator<Item = &'a ArrayElement<'a>>,
    var_name: &str,
    cursor_offset: u32,
) -> Option<ExprDefResult> {
    for element in elements {
        let value = match element {
            ArrayElement::KeyValue(kv) => kv.value,
            ArrayElement::Value(v) => v.value,
            _ => continue,
        };

        // Handle nested destructuring: `[[$a, $b], $c] = …`
        match value {
            Expression::Array(arr) => {
                if let Some(r) =
                    find_var_in_destructuring_tss(&arr.elements, var_name, cursor_offset)
                {
                    return Some(r);
                }
            }
            Expression::List(list) => {
                if let Some(r) =
                    find_var_in_destructuring_tss(&list.elements, var_name, cursor_offset)
                {
                    return Some(r);
                }
            }
            Expression::Variable(Variable::Direct(dv)) => {
                if dv.name == var_name {
                    let var_start = dv.span.start.offset;
                    let var_end = dv.span.end.offset;
                    if cursor_offset >= var_start && cursor_offset < var_end {
                        return Some(ExprDefResult::AtDefinition);
                    }
                    if var_start < cursor_offset {
                        return Some(ExprDefResult::Found(DefSite {
                            offset: var_start,
                            end_offset: var_end,
                        }));
                    }
                }
            }
            _ => {}
        }
    }
    None
}

/// Check foreach key/value variables as definition sites.
fn check_foreach_def(
    foreach: &Foreach<'_>,
    var_name: &str,
    cursor_offset: u32,
) -> Option<ExprDefResult> {
    // Check value variable.
    let value_expr = foreach.target.value();
    if let Expression::Variable(Variable::Direct(dv)) = value_expr
        && dv.name == var_name
    {
        let var_start = dv.span.start.offset;
        let var_end = dv.span.end.offset;
        if cursor_offset >= var_start && cursor_offset < var_end {
            return Some(ExprDefResult::AtDefinition);
        }
        if var_start < cursor_offset {
            return Some(ExprDefResult::Found(DefSite {
                offset: var_start,
                end_offset: var_end,
            }));
        }
    }

    // Check value as destructuring: `foreach ($x as [$a, $b])`
    match value_expr {
        Expression::Array(arr) => {
            if let Some(r) = find_var_in_destructuring_tss(&arr.elements, var_name, cursor_offset) {
                return Some(r);
            }
        }
        Expression::List(list) => {
            if let Some(r) = find_var_in_destructuring_tss(&list.elements, var_name, cursor_offset)
            {
                return Some(r);
            }
        }
        _ => {}
    }

    // Check key variable.
    if let Some(key_expr) = foreach.target.key()
        && let Expression::Variable(Variable::Direct(dv)) = key_expr
        && dv.name == var_name
    {
        let var_start = dv.span.start.offset;
        let var_end = dv.span.end.offset;
        if cursor_offset >= var_start && cursor_offset < var_end {
            return Some(ExprDefResult::AtDefinition);
        }
        if var_start < cursor_offset {
            return Some(ExprDefResult::Found(DefSite {
                offset: var_start,
                end_offset: var_end,
            }));
        }
    }

    None
}

// ═══════════════════════════════════════════════════════════════════════
// AST-based type-hint extraction at definition sites
// ═══════════════════════════════════════════════════════════════════════

/// Find the type hint string for a variable at its definition site
/// in the AST.
fn find_type_hint_at_definition(
    program: &Program<'_>,
    var_name: &str,
    cursor_offset: u32,
) -> Option<String> {
    find_type_hint_in_statements(program.statements.iter(), var_name, cursor_offset)
}

/// Walk statements looking for the scope that contains the cursor,
/// then extract the type hint.
fn find_type_hint_in_statements<'a, I>(
    statements: I,
    var_name: &str,
    cursor_offset: u32,
) -> Option<String>
where
    I: Iterator<Item = &'a Statement<'a>>,
{
    for stmt in statements {
        match stmt {
            Statement::Class(class) => {
                let start = class.left_brace.start.offset;
                let end = class.right_brace.end.offset;
                if cursor_offset >= start && cursor_offset <= end {
                    return find_type_hint_in_class_members(
                        class.members.iter(),
                        var_name,
                        cursor_offset,
                    );
                }
            }
            Statement::Interface(iface) => {
                let start = iface.left_brace.start.offset;
                let end = iface.right_brace.end.offset;
                if cursor_offset >= start && cursor_offset <= end {
                    return find_type_hint_in_class_members(
                        iface.members.iter(),
                        var_name,
                        cursor_offset,
                    );
                }
            }
            Statement::Trait(trait_def) => {
                let start = trait_def.left_brace.start.offset;
                let end = trait_def.right_brace.end.offset;
                if cursor_offset >= start && cursor_offset <= end {
                    return find_type_hint_in_class_members(
                        trait_def.members.iter(),
                        var_name,
                        cursor_offset,
                    );
                }
            }
            Statement::Enum(enum_def) => {
                let start = enum_def.left_brace.start.offset;
                let end = enum_def.right_brace.end.offset;
                if cursor_offset >= start && cursor_offset <= end {
                    return find_type_hint_in_class_members(
                        enum_def.members.iter(),
                        var_name,
                        cursor_offset,
                    );
                }
            }
            Statement::Namespace(ns) => {
                if let Some(hint) =
                    find_type_hint_in_statements(ns.statements().iter(), var_name, cursor_offset)
                {
                    return Some(hint);
                }
            }
            Statement::Function(func) => {
                // Check parameter list span (cursor might be on a
                // parameter declaration, which is outside the body).
                let param_span = func.parameter_list.span();
                if cursor_offset >= param_span.start.offset
                    && cursor_offset <= param_span.end.offset
                {
                    return find_type_hint_in_params(&func.parameter_list, var_name, cursor_offset);
                }
                let body_start = func.body.left_brace.start.offset;
                let body_end = func.body.right_brace.end.offset;
                if cursor_offset >= body_start && cursor_offset <= body_end {
                    return find_type_hint_in_params(&func.parameter_list, var_name, cursor_offset);
                }
            }
            _ => {}
        }
    }
    None
}

/// Search class members for a method containing the cursor, then
/// extract the type hint from its parameters or promoted properties.
fn find_type_hint_in_class_members<'a, I>(
    members: I,
    var_name: &str,
    cursor_offset: u32,
) -> Option<String>
where
    I: Iterator<Item = &'a ClassLikeMember<'a>>,
{
    for member in members {
        match member {
            ClassLikeMember::Method(method) => {
                if let MethodBody::Concrete(body) = &method.body {
                    let body_start = body.left_brace.start.offset;
                    let body_end = body.right_brace.end.offset;
                    if cursor_offset >= body_start && cursor_offset <= body_end {
                        // Check if cursor is on a closure/arrow function parameter.
                        let body_stmts: Vec<&Statement> = body.statements.iter().collect();
                        if let Some(hint) =
                            find_type_hint_in_nested_closure(&body_stmts, var_name, cursor_offset)
                        {
                            return Some(hint);
                        }
                        return find_type_hint_in_params(
                            &method.parameter_list,
                            var_name,
                            cursor_offset,
                        );
                    }
                }
                // Also check parameter list span directly (cursor might
                // be on the parameter itself, outside the body).
                let param_span = method.parameter_list.span();
                if cursor_offset >= param_span.start.offset
                    && cursor_offset <= param_span.end.offset
                {
                    return find_type_hint_in_params(
                        &method.parameter_list,
                        var_name,
                        cursor_offset,
                    );
                }
            }
            ClassLikeMember::Property(property) => {
                // Check property variables.
                for var in property.variables().iter() {
                    if var.name == var_name {
                        let var_start = var.span.start.offset;
                        let var_end = var.span.end.offset;
                        if cursor_offset >= var_start && cursor_offset < var_end {
                            return property.hint().map(|h| Backend::extract_hint_string(h));
                        }
                    }
                }
            }
            _ => {}
        }
    }
    None
}

/// Check if cursor is on a closure/arrow function parameter and extract
/// its type hint.
fn find_type_hint_in_nested_closure(
    stmts: &[&Statement<'_>],
    var_name: &str,
    cursor_offset: u32,
) -> Option<String> {
    for &stmt in stmts {
        let stmt_span = stmt.span();
        if cursor_offset < stmt_span.start.offset || cursor_offset > stmt_span.end.offset {
            continue;
        }
        if let Some(hint) = find_type_hint_in_closure_stmt(stmt, var_name, cursor_offset) {
            return Some(hint);
        }
    }
    None
}

/// Recursively search a statement for a closure/arrow function whose
/// parameter the cursor is on.
fn find_type_hint_in_closure_stmt(
    stmt: &Statement<'_>,
    var_name: &str,
    cursor_offset: u32,
) -> Option<String> {
    match stmt {
        Statement::Expression(expr_stmt) => {
            find_type_hint_in_closure_expr(expr_stmt.expression, var_name, cursor_offset)
        }
        Statement::Return(ret) => ret
            .value
            .and_then(|expr| find_type_hint_in_closure_expr(expr, var_name, cursor_offset)),
        Statement::If(if_stmt) => {
            for inner in if_stmt.body.statements() {
                if let Some(h) = find_type_hint_in_closure_stmt(inner, var_name, cursor_offset) {
                    return Some(h);
                }
            }
            None
        }
        Statement::Foreach(foreach) => {
            for inner in foreach.body.statements() {
                if let Some(h) = find_type_hint_in_closure_stmt(inner, var_name, cursor_offset) {
                    return Some(h);
                }
            }
            None
        }
        Statement::Block(block) => {
            for inner in block.statements.iter() {
                if let Some(h) = find_type_hint_in_closure_stmt(inner, var_name, cursor_offset) {
                    return Some(h);
                }
            }
            None
        }
        _ => None,
    }
}

/// Recursively search an expression for a closure/arrow function whose
/// parameter the cursor is on.
fn find_type_hint_in_closure_expr(
    expr: &Expression<'_>,
    var_name: &str,
    cursor_offset: u32,
) -> Option<String> {
    let span = expr.span();
    if cursor_offset < span.start.offset || cursor_offset > span.end.offset {
        return None;
    }

    match expr {
        Expression::Closure(closure) => {
            let body_start = closure.body.left_brace.start.offset;
            let body_end = closure.body.right_brace.end.offset;
            if cursor_offset >= body_start && cursor_offset <= body_end {
                // Check nested closures first.
                let body_stmts: Vec<&Statement> = closure.body.statements.iter().collect();
                if let Some(hint) =
                    find_type_hint_in_nested_closure(&body_stmts, var_name, cursor_offset)
                {
                    return Some(hint);
                }
                return find_type_hint_in_params(&closure.parameter_list, var_name, cursor_offset);
            }
            // Check parameter list directly.
            let param_span = closure.parameter_list.span();
            if cursor_offset >= param_span.start.offset && cursor_offset <= param_span.end.offset {
                return find_type_hint_in_params(&closure.parameter_list, var_name, cursor_offset);
            }
            None
        }
        Expression::ArrowFunction(arrow) => {
            let param_span = arrow.parameter_list.span();
            if cursor_offset >= param_span.start.offset && cursor_offset <= param_span.end.offset {
                return find_type_hint_in_params(&arrow.parameter_list, var_name, cursor_offset);
            }
            // Body expression.
            find_type_hint_in_closure_expr(arrow.expression, var_name, cursor_offset)
        }
        Expression::Assignment(assignment) => {
            find_type_hint_in_closure_expr(assignment.rhs, var_name, cursor_offset)
                .or_else(|| find_type_hint_in_closure_expr(assignment.lhs, var_name, cursor_offset))
        }
        Expression::Call(call) => match call {
            Call::Function(func_call) => {
                for arg in func_call.argument_list.arguments.iter() {
                    let arg_expr: &Expression<'_> = arg.value();
                    if let Some(h) =
                        find_type_hint_in_closure_expr(arg_expr, var_name, cursor_offset)
                    {
                        return Some(h);
                    }
                }
                None
            }
            Call::Method(method_call) => {
                for arg in method_call.argument_list.arguments.iter() {
                    let arg_expr: &Expression<'_> = arg.value();
                    if let Some(h) =
                        find_type_hint_in_closure_expr(arg_expr, var_name, cursor_offset)
                    {
                        return Some(h);
                    }
                }
                None
            }
            Call::StaticMethod(static_call) => {
                for arg in static_call.argument_list.arguments.iter() {
                    let arg_expr: &Expression<'_> = arg.value();
                    if let Some(h) =
                        find_type_hint_in_closure_expr(arg_expr, var_name, cursor_offset)
                    {
                        return Some(h);
                    }
                }
                None
            }
            _ => None,
        },
        Expression::Parenthesized(p) => {
            find_type_hint_in_closure_expr(p.expression, var_name, cursor_offset)
        }
        _ => None,
    }
}

/// Extract the type hint string for `var_name` from a parameter list.
fn find_type_hint_in_params(
    params: &FunctionLikeParameterList<'_>,
    var_name: &str,
    _cursor_offset: u32,
) -> Option<String> {
    for param in params.parameters.iter() {
        if param.variable.name == var_name {
            return param.hint.as_ref().map(|h| Backend::extract_hint_string(h));
        }
    }
    None
}

// ═══════════════════════════════════════════════════════════════════════
// Tests
// ═══════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: parse PHP code and find a variable definition.
    fn find_def(php: &str, var_name: &str, cursor_offset: u32) -> VarDefSearchResult {
        with_parsed_program(php, "test", |program, content| {
            find_variable_definition_in_program(program, content, var_name, cursor_offset)
        })
    }

    /// Helper: find the byte offset of a substring occurrence in the source.
    /// `occurrence` is 0-based (0 = first, 1 = second, etc.).
    fn find_offset(src: &str, needle: &str, occurrence: usize) -> u32 {
        let mut start = 0;
        for _ in 0..=occurrence {
            let pos = src[start..].find(needle).unwrap_or_else(|| {
                panic!("Could not find occurrence {} of {:?}", occurrence, needle)
            });
            if start == 0 && occurrence == 0 {
                return pos as u32;
            }
            start += pos + 1;
        }
        (start - 1) as u32
    }

    #[test]
    fn assignment_found() {
        let php = "<?php\n$foo = 42;\necho $foo;\n";
        // cursor on the `$foo` in `echo $foo`
        let cursor = find_offset(php, "$foo", 1);
        match find_def(php, "$foo", cursor) {
            VarDefSearchResult::FoundAt { offset, .. } => {
                let def_offset = find_offset(php, "$foo", 0);
                assert_eq!(offset, def_offset);
            }
            other => panic!(
                "Expected FoundAt, got {:?}",
                matches!(other, VarDefSearchResult::NotFound)
            ),
        }
    }

    #[test]
    fn at_definition_returns_at_definition() {
        let php = "<?php\n$foo = 42;\n";
        let cursor = find_offset(php, "$foo", 0);
        assert!(matches!(
            find_def(php, "$foo", cursor),
            VarDefSearchResult::AtDefinition
        ));
    }

    #[test]
    fn parameter_found() {
        let php = "<?php\nfunction test($bar) {\n    echo $bar;\n}\n";
        let cursor = find_offset(php, "$bar", 1);
        match find_def(php, "$bar", cursor) {
            VarDefSearchResult::FoundAt { offset, .. } => {
                let def_offset = find_offset(php, "$bar", 0);
                assert_eq!(offset, def_offset);
            }
            other => panic!(
                "Expected FoundAt, got {:?}",
                matches!(other, VarDefSearchResult::NotFound)
            ),
        }
    }

    #[test]
    fn foreach_value_found() {
        let php = "<?php\nforeach ($items as $item) {\n    echo $item;\n}\n";
        // The cursor on `$item` in `echo $item`
        let cursor = find_offset(php, "$item;", 0);
        match find_def(php, "$item", cursor) {
            VarDefSearchResult::FoundAt { offset, .. } => {
                // The definition is the `$item` in `as $item`
                let def_offset = find_offset(php, "$item)", 0);
                assert_eq!(offset, def_offset);
            }
            other => panic!(
                "Expected FoundAt, got {:?}",
                matches!(other, VarDefSearchResult::NotFound)
            ),
        }
    }

    #[test]
    fn foreach_key_found() {
        let php = "<?php\nforeach ($items as $key => $val) {\n    echo $key;\n}\n";
        let cursor = find_offset(php, "$key;", 0);
        match find_def(php, "$key", cursor) {
            VarDefSearchResult::FoundAt { offset, .. } => {
                let def_offset = find_offset(php, "$key =>", 0);
                assert_eq!(offset, def_offset);
            }
            other => panic!(
                "Expected FoundAt, got {:?}",
                matches!(other, VarDefSearchResult::NotFound)
            ),
        }
    }

    #[test]
    fn catch_variable_found() {
        let php = "<?php\ntry {\n} catch (Exception $e) {\n    echo $e;\n}\n";
        let cursor = find_offset(php, "$e;", 0);
        match find_def(php, "$e", cursor) {
            VarDefSearchResult::FoundAt { offset, .. } => {
                let def_offset = find_offset(php, "$e)", 0);
                assert_eq!(offset, def_offset);
            }
            other => panic!(
                "Expected FoundAt, got {:?}",
                matches!(other, VarDefSearchResult::NotFound)
            ),
        }
    }

    #[test]
    fn static_variable_found() {
        let php = "<?php\nfunction test() {\n    static $count = 0;\n    $count++;\n}\n";
        let cursor = find_offset(php, "$count+", 0);
        match find_def(php, "$count", cursor) {
            VarDefSearchResult::FoundAt { offset, .. } => {
                let def_offset = find_offset(php, "$count =", 0);
                assert_eq!(offset, def_offset);
            }
            other => panic!(
                "Expected FoundAt, got {:?}",
                matches!(other, VarDefSearchResult::NotFound)
            ),
        }
    }

    #[test]
    fn global_variable_found() {
        let php = "<?php\nfunction test() {\n    global $config;\n    echo $config;\n}\n";
        // Find the `$config` in `echo $config;` — use the "echo " prefix to
        // locate the right occurrence.
        let echo_pos = php.find("echo $config").unwrap();
        let cursor = (echo_pos + "echo ".len()) as u32;
        match find_def(php, "$config", cursor) {
            VarDefSearchResult::FoundAt { offset, .. } => {
                // The definition is the `$config` in `global $config;`.
                let expected = php.find("$config").unwrap() as u32;
                assert_eq!(offset, expected);
            }
            other => panic!(
                "Expected FoundAt, got {:?}",
                matches!(other, VarDefSearchResult::NotFound)
            ),
        }
    }

    #[test]
    fn array_destructuring_found() {
        let php = "<?php\n[$a, $b] = explode(',', $str);\necho $a;\n";
        let cursor = find_offset(php, "$a;", 0);
        match find_def(php, "$a", cursor) {
            VarDefSearchResult::FoundAt { offset, .. } => {
                let def_offset = find_offset(php, "$a,", 0);
                assert_eq!(offset, def_offset);
            }
            other => panic!(
                "Expected FoundAt, got {:?}",
                matches!(other, VarDefSearchResult::NotFound)
            ),
        }
    }

    #[test]
    fn list_destructuring_found() {
        let php = "<?php\nlist($a, $b) = func();\necho $a;\n";
        let cursor = find_offset(php, "$a;", 0);
        match find_def(php, "$a", cursor) {
            VarDefSearchResult::FoundAt { offset, .. } => {
                let def_offset = find_offset(php, "$a,", 0);
                assert_eq!(offset, def_offset);
            }
            other => panic!(
                "Expected FoundAt, got {:?}",
                matches!(other, VarDefSearchResult::NotFound)
            ),
        }
    }

    #[test]
    fn method_parameter_found() {
        let php = concat!(
            "<?php\n",
            "class Foo {\n",
            "    public function bar(string $x): void {\n",
            "        echo $x;\n",
            "    }\n",
            "}\n",
        );
        let cursor = find_offset(php, "$x;", 0);
        match find_def(php, "$x", cursor) {
            VarDefSearchResult::FoundAt { offset, .. } => {
                let def_offset = find_offset(php, "$x)", 0);
                assert_eq!(offset, def_offset);
            }
            other => panic!(
                "Expected FoundAt, got {:?}",
                matches!(other, VarDefSearchResult::NotFound)
            ),
        }
    }

    #[test]
    fn most_recent_assignment_wins() {
        let php = "<?php\n$x = 1;\n$x = 2;\necho $x;\n";
        let cursor = find_offset(php, "$x;", 0);
        match find_def(php, "$x", cursor) {
            VarDefSearchResult::FoundAt { offset, .. } => {
                // Should find `$x = 2` (second assignment), not `$x = 1`.
                let second_assign = find_offset(php, "$x = 2", 0);
                assert_eq!(offset, second_assign);
            }
            other => panic!(
                "Expected FoundAt, got {:?}",
                matches!(other, VarDefSearchResult::NotFound)
            ),
        }
    }

    #[test]
    fn not_found_when_no_definition() {
        let php = "<?php\necho $unknown;\n";
        let cursor = find_offset(php, "$unknown", 0);
        assert!(matches!(
            find_def(php, "$unknown", cursor),
            VarDefSearchResult::NotFound
        ));
    }

    #[test]
    fn closure_scope_isolation() {
        let php = concat!(
            "<?php\n",
            "$outer = 1;\n",
            "$fn = function($inner) {\n",
            "    echo $inner;\n",
            "};\n",
        );
        // Cursor on `$inner` in the echo — should find the parameter.
        let echo_pos = php.find("echo $inner").unwrap();
        let cursor = (echo_pos + "echo ".len()) as u32;
        match find_def(php, "$inner", cursor) {
            VarDefSearchResult::FoundAt { offset, .. } => {
                let def_offset = find_offset(php, "$inner)", 0);
                assert_eq!(offset, def_offset);
            }
            other => panic!(
                "Expected FoundAt, got {:?}",
                matches!(other, VarDefSearchResult::NotFound)
            ),
        }
    }

    #[test]
    fn arrow_function_parameter() {
        let php = "<?php\n$fn = fn($x) => $x + 1;\n";
        // Cursor on `$x` after `=>` — find the unique `$x +` pattern
        let body_pos = php.find("$x + 1").unwrap();
        let cursor = body_pos as u32;
        match find_def(php, "$x", cursor) {
            VarDefSearchResult::FoundAt { offset, .. } => {
                let def_offset = find_offset(php, "$x)", 0);
                assert_eq!(offset, def_offset);
            }
            other => panic!(
                "Expected FoundAt, got {:?}",
                matches!(other, VarDefSearchResult::NotFound)
            ),
        }
    }

    #[test]
    fn type_hint_extraction_for_parameter() {
        let php = concat!(
            "<?php\n",
            "class Foo {\n",
            "    public function bar(Request $req): void {\n",
            "        echo $req;\n",
            "    }\n",
            "}\n",
        );
        let cursor_offset = find_offset(php, "$req)", 0);
        let result: Option<String> = with_parsed_program(php, "test", |program, _| {
            find_type_hint_at_definition(program, "$req", cursor_offset)
        });
        assert_eq!(result, Some("Request".to_string()));
    }

    #[test]
    fn type_hint_extraction_union() {
        let php = "<?php\nfunction test(Foo|Bar $x): void { echo $x; }\n";
        // Place cursor on `$x` in the parameter list.
        let param_pos = php.find("$x)").unwrap();
        let cursor_offset = param_pos as u32;
        let result: Option<String> = with_parsed_program(php, "test", |program, _| {
            find_type_hint_at_definition(program, "$x", cursor_offset)
        });
        assert_eq!(result, Some("Foo|Bar".to_string()));
    }

    #[test]
    fn type_hint_extraction_nullable() {
        let php = "<?php\nfunction test(?Foo $x): void { echo $x; }\n";
        let param_pos = php.find("$x)").unwrap();
        let cursor_offset = param_pos as u32;
        let result: Option<String> = with_parsed_program(php, "test", |program, _| {
            find_type_hint_at_definition(program, "$x", cursor_offset)
        });
        assert_eq!(result, Some("?Foo".to_string()));
    }
}
