//! "Fix return type" code actions for PHPStan return-type diagnostics.
//!
//! Handles four PHPStan identifiers:
//!
//! - **`return.void`** — a `void` function returns an expression.
//!   Two fixes: remove the return statement (keeping the expression
//!   as a standalone statement followed by `return;`), or change the
//!   return type to match the actual value.
//! - **`return.empty`** — a non-void function has a bare `return;`.
//!   Fix: change the return type to `void` and remove `@return`.
//! - **`return.type`** — the return type doesn't match what the
//!   function actually returns.  Fix: update the native return type
//!   to the actual type reported by PHPStan.
//! - **`missingType.return`** — no return type specified.
//!   Fix: add a return type hint.  The type is inferred from the
//!   function body by scanning return statements for literals,
//!   variable types, and `new ClassName()` expressions.
//!
//! **Code action kind:** `quickfix`.
//!
//! ## Two-phase resolve
//!
//! Phase 1 (`collect_fix_return_type_actions`) validates that the
//! action is applicable and emits a lightweight `CodeAction` with a
//! `data` payload but no `edit`.  Phase 2 (`resolve_fix_return_type`)
//! recomputes the workspace edit on demand when the user picks the
//! action.

use std::collections::HashMap;
use std::sync::Arc;

use tower_lsp::lsp_types::*;

use crate::Backend;
use crate::code_actions::{CodeActionData, make_code_action_data};
use crate::completion::resolver::Loaders;
use crate::completion::variable::resolution::resolve_variable_types;
use crate::php_type::PhpType;
use crate::types::{ClassInfo, ResolvedType};
use crate::util::{find_brace_match_line, find_semicolon_balanced, ranges_overlap};

// ── Return type inference result ────────────────────────────────────────────

/// The result of inferring a return type from a function body.
///
/// Separates the native PHP type hint (for the `: type` declaration)
/// from the effective PHPStan type (for a `@return` docblock tag).
/// When the two are identical, no docblock is needed.
struct InferredReturnType {
    /// Valid native PHP type hint (e.g. `array`, `int`, `Foo`).
    native: String,
    /// Full effective type including generics/shapes (e.g. `list<string>`).
    /// `None` when the native type already captures the full type.
    effective: Option<String>,
}

// ── PHPStan identifiers ─────────────────────────────────────────────────────

/// PHPStan identifier for "void function returns a value".
const RETURN_VOID_ID: &str = "return.void";

/// PHPStan identifier for "non-void function has empty return".
const RETURN_EMPTY_ID: &str = "return.empty";

/// PHPStan identifier for "return type doesn't match actual return".
const RETURN_TYPE_ID: &str = "return.type";

/// PHPStan identifier for "no return type specified".
const MISSING_TYPE_RETURN_ID: &str = "missingType.return";

/// Action kind string for the strip-expression fix (return.void).
const ACTION_KIND_STRIP_EXPR: &str = "phpstan.fixReturnType.stripExpr";

/// Action kind string for changing the return type to match the actual
/// return value.
const ACTION_KIND_CHANGE_TYPE_TO_ACTUAL: &str = "phpstan.fixReturnType.changeTypeToActual";

/// Action kind string for the change-return-type-to-void fix (return.empty).
const ACTION_KIND_CHANGE_TYPE: &str = "phpstan.fixReturnType.changeType";

/// Action kind string for adding a missing return type hint.
const ACTION_KIND_ADD_TYPE: &str = "phpstan.fixReturnType.addType";

/// Message fragment that identifies a `return.void` diagnostic.
const RETURN_VOID_MSG_SUFFIX: &str = "but should not return anything.";

/// Message fragment that identifies a `return.empty` diagnostic.
const RETURN_EMPTY_MSG_FRAGMENT: &str = "but empty return statement found.";

// ── Backend methods ─────────────────────────────────────────────────────────

impl Backend {
    /// Collect code actions for PHPStan `return.void`, `return.empty`,
    /// `return.type`, and `missingType.return` diagnostics.
    pub(crate) fn collect_fix_return_type_actions(
        &self,
        uri: &str,
        content: &str,
        params: &CodeActionParams,
        out: &mut Vec<CodeActionOrCommand>,
    ) {
        let phpstan_diags: Vec<Diagnostic> = {
            let cache = self.phpstan_last_diags.lock();
            cache.get(uri).cloned().unwrap_or_default()
        };

        for diag in &phpstan_diags {
            if !ranges_overlap(&diag.range, &params.range) {
                continue;
            }

            let identifier = match &diag.code {
                Some(NumberOrString::String(s)) => s.as_str(),
                _ => continue,
            };

            let diag_line = diag.range.start.line as usize;

            match identifier {
                RETURN_VOID_ID => {
                    if !diag.message.ends_with(RETURN_VOID_MSG_SUFFIX) {
                        continue;
                    }

                    // Verify the strip-expression fix is applicable.
                    if build_strip_return_expr_edit(content, diag_line).is_none() {
                        continue;
                    }

                    // ── Fix 1: Strip return expression ──────────────
                    let extra = serde_json::json!({
                        "diagnostic_line": diag_line,
                        "identifier": RETURN_VOID_ID,
                    });

                    out.push(CodeActionOrCommand::CodeAction(CodeAction {
                        title: "Remove return statement".to_string(),
                        kind: Some(CodeActionKind::QUICKFIX),
                        diagnostics: Some(vec![diag.clone()]),
                        edit: None,
                        command: None,
                        is_preferred: Some(false),
                        disabled: None,
                        data: Some(make_code_action_data(
                            ACTION_KIND_STRIP_EXPR,
                            uri,
                            &params.range,
                            extra,
                        )),
                    }));

                    // ── Fix 2: Change return type to match actual ───
                    // Extract the actual type from the message:
                    // "... returns {actual} but should not return anything."
                    // Skip when the actual type is `null` — returning null
                    // from a void function is not a type mismatch, it's
                    // just a habit.  The "Remove return statement" fix above
                    // handles it.
                    if let Some(actual_type) = extract_actual_type(&diag.message)
                        && actual_type != "null"
                    {
                        // Verify the change-type fix is applicable (the
                        // function has a return type that can be replaced).
                        if build_change_return_type_edits_to(content, diag_line, actual_type)
                            .is_some()
                        {
                            let extra = serde_json::json!({
                                "diagnostic_line": diag_line,
                                "identifier": RETURN_VOID_ID,
                                "actual_type": actual_type,
                            });

                            out.push(CodeActionOrCommand::CodeAction(CodeAction {
                                title: format!("Change return type to {}", actual_type),
                                kind: Some(CodeActionKind::QUICKFIX),
                                diagnostics: Some(vec![diag.clone()]),
                                edit: None,
                                command: None,
                                is_preferred: Some(true),
                                disabled: None,
                                data: Some(make_code_action_data(
                                    ACTION_KIND_CHANGE_TYPE_TO_ACTUAL,
                                    uri,
                                    &params.range,
                                    extra,
                                )),
                            }));
                        }
                    }
                }
                RETURN_EMPTY_ID => {
                    if !diag.message.contains(RETURN_EMPTY_MSG_FRAGMENT) {
                        continue;
                    }

                    // Verify the fix is applicable.
                    if build_change_return_type_edits_to(content, diag_line, "void").is_none() {
                        continue;
                    }

                    let title = "Change return type to void".to_string();

                    let extra = serde_json::json!({
                        "diagnostic_line": diag_line,
                        "identifier": RETURN_EMPTY_ID,
                    });

                    let data =
                        make_code_action_data(ACTION_KIND_CHANGE_TYPE, uri, &params.range, extra);

                    out.push(CodeActionOrCommand::CodeAction(CodeAction {
                        title,
                        kind: Some(CodeActionKind::QUICKFIX),
                        diagnostics: Some(vec![diag.clone()]),
                        edit: None,
                        command: None,
                        is_preferred: Some(true),
                        disabled: None,
                        data: Some(data),
                    }));
                }
                RETURN_TYPE_ID => {
                    // "Method Foo::bar() should return {expected} but returns {actual}."
                    let actual_type = match extract_return_type_actual(&diag.message) {
                        Some(t) => t,
                        None => continue,
                    };

                    // Verify the fix is applicable.
                    if build_change_return_type_edits_to(content, diag_line, actual_type).is_none()
                    {
                        continue;
                    }

                    let extra = serde_json::json!({
                        "diagnostic_line": diag_line,
                        "identifier": RETURN_TYPE_ID,
                        "actual_type": actual_type,
                    });

                    out.push(CodeActionOrCommand::CodeAction(CodeAction {
                        title: format!("Change return type to {}", actual_type),
                        kind: Some(CodeActionKind::QUICKFIX),
                        diagnostics: Some(vec![diag.clone()]),
                        edit: None,
                        command: None,
                        is_preferred: Some(false),
                        disabled: None,
                        data: Some(make_code_action_data(
                            ACTION_KIND_CHANGE_TYPE_TO_ACTUAL,
                            uri,
                            &params.range,
                            extra,
                        )),
                    }));
                }
                MISSING_TYPE_RETURN_ID => {
                    // "Method Foo::bar() has no return type specified."
                    // Defer type inference to the resolve phase — it can
                    // be expensive and the collect phase runs on every
                    // cursor move.  Just validate that the function has
                    // no return type yet.
                    let lines: Vec<&str> = content.lines().collect();

                    let brace_line = match find_open_brace_from_declaration(&lines, diag_line) {
                        Some(l) => l,
                        None => continue,
                    };
                    let (paren_line, paren_col) =
                        match find_close_paren_before_brace(&lines, brace_line) {
                            Some(p) => p,
                            None => continue,
                        };

                    // Check there is no existing return type.
                    if has_return_type_between(&lines, paren_line, paren_col, brace_line) {
                        continue;
                    }

                    let extra = serde_json::json!({
                        "diagnostic_line": diag_line,
                        "identifier": MISSING_TYPE_RETURN_ID,
                    });

                    out.push(CodeActionOrCommand::CodeAction(CodeAction {
                        title: "Add return type".to_string(),
                        kind: Some(CodeActionKind::QUICKFIX),
                        diagnostics: Some(vec![diag.clone()]),
                        edit: None,
                        command: None,
                        is_preferred: Some(true),
                        disabled: None,
                        data: Some(make_code_action_data(
                            ACTION_KIND_ADD_TYPE,
                            uri,
                            &params.range,
                            extra,
                        )),
                    }));
                }
                _ => continue,
            }
        }
    }

    /// Infer the return type of the function at `func_line` by scanning
    /// all return statements in the body.
    ///
    /// For simple literals (`return 1;`, `return 'hello';`, `return new Foo()`)
    /// the type is inferred syntactically. For `$variable` returns, the
    /// full variable-resolution pipeline is used. All other expressions
    /// (method calls, function calls, complex expressions) produce `mixed`.
    ///
    /// Returns an [`InferredReturnType`] that separates the native PHP
    /// type hint from the richer effective type.  When they differ (e.g.
    /// `list<string>` vs `array`), the caller should add a `@return` tag.
    fn infer_return_type_for_function(
        &self,
        uri: &str,
        content: &str,
        func_line: usize,
    ) -> Option<InferredReturnType> {
        let lines: Vec<&str> = content.lines().collect();
        if func_line >= lines.len() {
            return None;
        }

        // Find the function body boundaries.
        let brace_line = find_open_brace_from_declaration(&lines, func_line)?;

        // Find the closing `}` that matches the `{` on `brace_line`.
        let body_end = find_brace_match_line(&lines, brace_line, |d| d == 0)?;

        // Set up the resolution infrastructure.
        let local_classes: Vec<Arc<ClassInfo>> =
            self.ast_map.read().get(uri).cloned().unwrap_or_default();
        let file_use_map: HashMap<String, String> = self.file_use_map(uri);
        let file_namespace: Option<String> = self.namespace_map.read().get(uri).cloned().flatten();
        let class_loader = self.class_loader_with(&local_classes, &file_use_map, &file_namespace);
        let function_loader = self.function_loader_with(&file_use_map, &file_namespace);

        // Find the enclosing class at the function line offset.
        let func_offset = content
            .lines()
            .take(func_line)
            .map(|l| l.len() + 1)
            .sum::<usize>() as u32;
        let enclosing_class = local_classes
            .iter()
            .find(|c| {
                !c.name.starts_with("__anonymous@")
                    && func_offset >= c.start_offset
                    && func_offset <= c.end_offset
            })
            .map(|c| ClassInfo::clone(c))
            .unwrap_or_default();

        // Scan return statements and resolve their types.
        let mut return_types: Vec<String> = Vec::new();
        let mut has_bare_return = false;
        let mut has_return_with_value = false;

        let mut brace_depth: i32 = 1;

        for (line_idx, line) in lines.iter().enumerate().take(body_end).skip(brace_line + 1) {
            let trimmed = line.trim();

            // Track brace depth to ignore return statements inside
            // nested closures, anonymous functions, and match blocks.
            for ch in line.chars() {
                match ch {
                    '{' => brace_depth += 1,
                    '}' => brace_depth -= 1,
                    _ => {}
                }
            }

            // Only inspect return statements at the outermost function level.
            if brace_depth != 1 {
                continue;
            }

            // Skip comments.
            if trimmed.starts_with("//") || trimmed.starts_with('*') || trimmed.starts_with("/*") {
                continue;
            }

            if trimmed == "return;" {
                has_bare_return = true;
                continue;
            }

            if let Some(rest) = trimmed.strip_prefix("return ") {
                let rest = rest.trim();
                if rest == ";" {
                    has_bare_return = true;
                    continue;
                }
                has_return_with_value = true;

                // Strip trailing `;`
                let expr = rest.strip_suffix(';').unwrap_or(rest).trim();

                // Try syntax-level inference first (cheap).
                if let Some(t) = infer_type_from_literal(expr) {
                    return_types.push(t);
                    continue;
                }

                // Fall back to the variable/expression resolver.
                // Compute byte offset of the expression for resolution.
                let line_start: usize = content.lines().take(line_idx).map(|l| l.len() + 1).sum();
                let expr_offset_in_line = line.find("return ").unwrap_or(0) + "return ".len();
                let expr_offset = (line_start + expr_offset_in_line) as u32;

                // Try variable resolution for `$var` expressions.
                if expr.starts_with('$') && !expr.contains(' ') {
                    let results = resolve_variable_types(
                        expr,
                        &enclosing_class,
                        &local_classes,
                        content,
                        expr_offset,
                        &class_loader,
                        Loaders::with_function(Some(&function_loader)),
                    );
                    let type_str = ResolvedType::type_strings_joined(&results);
                    if !type_str.is_empty() {
                        return_types.push(type_str);
                        continue;
                    }
                }

                // For other expressions, fall back to `mixed`.
                return_types.push("mixed".to_string());
            }
        }

        if !has_return_with_value && !has_bare_return {
            return Some(InferredReturnType {
                native: "void".to_string(),
                effective: None,
            });
        }

        if return_types.is_empty() && has_bare_return {
            return Some(InferredReturnType {
                native: "void".to_string(),
                effective: None,
            });
        }

        // Deduplicate types.
        return_types.sort();
        return_types.dedup();

        if has_bare_return {
            return_types.push("null".to_string());
            return_types.sort();
            return_types.dedup();
        }

        let effective = if return_types.len() == 1 {
            return_types.into_iter().next().unwrap()
        } else if return_types.len() <= 3 {
            return_types.join("|")
        } else {
            return None;
        };

        // Convert effective type → native PHP type hint.
        let parsed = PhpType::parse(&effective);
        let native = parsed
            .to_native_hint()
            .unwrap_or_else(|| "mixed".to_string());

        let needs_docblock = native != effective;
        Some(InferredReturnType {
            native,
            effective: if needs_docblock {
                Some(effective)
            } else {
                None
            },
        })
    }

    /// Resolve a "Fix return type" code action by computing the full
    /// workspace edit.  Dispatches on the `action_kind` stored in the
    /// data payload.
    pub(crate) fn resolve_fix_return_type(
        &self,
        data: &CodeActionData,
        content: &str,
    ) -> Option<WorkspaceEdit> {
        let extra = &data.extra;
        let diag_line = extra.get("diagnostic_line")?.as_u64()? as usize;

        let doc_uri: Url = data.uri.parse().ok()?;

        match data.action_kind.as_str() {
            ACTION_KIND_STRIP_EXPR => {
                let edit = build_strip_return_expr_edit(content, diag_line)?;
                let mut changes = HashMap::new();
                changes.insert(doc_uri, vec![edit]);
                Some(WorkspaceEdit {
                    changes: Some(changes),
                    document_changes: None,
                    change_annotations: None,
                })
            }
            ACTION_KIND_CHANGE_TYPE_TO_ACTUAL => {
                let actual_type = extra.get("actual_type")?.as_str()?;
                let edits = build_change_return_type_edits_to(content, diag_line, actual_type)?;
                let mut changes = HashMap::new();
                changes.insert(doc_uri, edits);
                Some(WorkspaceEdit {
                    changes: Some(changes),
                    document_changes: None,
                    change_annotations: None,
                })
            }
            ACTION_KIND_CHANGE_TYPE => {
                let edits = build_change_return_type_edits_to(content, diag_line, "void")?;
                let mut changes = HashMap::new();
                changes.insert(doc_uri, edits);
                Some(WorkspaceEdit {
                    changes: Some(changes),
                    document_changes: None,
                    change_annotations: None,
                })
            }
            ACTION_KIND_ADD_TYPE => {
                // Infer the type now (deferred from collect phase).
                let inferred =
                    self.infer_return_type_for_function(&data.uri, content, diag_line)?;

                let lines: Vec<&str> = content.lines().collect();
                let brace_line = find_open_brace_from_declaration(&lines, diag_line)?;
                let (paren_line, paren_col) = find_close_paren_before_brace(&lines, brace_line)?;

                let mut edits = Vec::new();

                // Insert `: native_type` after the closing paren.
                edits.push(TextEdit {
                    range: Range {
                        start: Position::new(paren_line as u32, (paren_col + 1) as u32),
                        end: Position::new(paren_line as u32, (paren_col + 1) as u32),
                    },
                    new_text: format!(": {}", inferred.native),
                });

                // When the effective type is richer than the native hint,
                // add a `@return` docblock tag.
                if let Some(ref eff) = inferred.effective {
                    let func_line =
                        find_function_keyword_line(&lines, paren_line).unwrap_or(diag_line);
                    let indent = lines[func_line]
                        .chars()
                        .take_while(|c| c.is_whitespace())
                        .collect::<String>();

                    // Insert a single-line docblock above the function.
                    let docblock = format!(
                        "{}/**\n{} * @return {}\n{} */\n",
                        indent, indent, eff, indent
                    );
                    edits.push(TextEdit {
                        range: Range {
                            start: Position::new(func_line as u32, 0),
                            end: Position::new(func_line as u32, 0),
                        },
                        new_text: docblock,
                    });
                }

                let mut changes = HashMap::new();
                changes.insert(doc_uri, edits);
                Some(WorkspaceEdit {
                    changes: Some(changes),
                    document_changes: None,
                    change_annotations: None,
                })
            }
            _ => None,
        }
    }
}

// ── Edit builders ───────────────────────────────────────────────────────────

/// Build a `TextEdit` that fixes `return {expr};` in a void function.
///
/// The replacement depends on context:
///
/// - **`return null;`** → `return;` (null is not a meaningful value).
/// - **All other expressions** → `{expr};\n{indent}return;` (keep
///   the expression as a standalone statement and add a bare
///   `return;` on the next line).
///
/// When the return is the last statement before the function's closing
/// `}`, the bare `return;` is omitted since it would be redundant.
///
/// Handles multiline return expressions by scanning forward from the
/// `return` keyword to the matching `;`, respecting string literals and
/// parenthesis nesting.
fn build_strip_return_expr_edit(content: &str, diag_line: usize) -> Option<TextEdit> {
    let lines: Vec<&str> = content.lines().collect();
    if diag_line >= lines.len() {
        return None;
    }

    let line_text = lines[diag_line];

    // Find `return ` (with trailing space) on the diagnostic line.
    let return_col = line_text.find("return ")?;

    // Verify this is not `return;` (no expression).
    let after_return = &line_text[return_col + "return".len()..];
    let trimmed = after_return.trim();
    if trimmed == ";" {
        // Already a bare return — nothing to fix.
        return None;
    }

    // Compute the byte offset within `content` where this line starts.
    let line_start_byte: usize = lines[..diag_line]
        .iter()
        .map(|l| l.len() + 1) // +1 for newline
        .sum();

    // The return statement starts at `return` keyword.
    let return_byte = line_start_byte + return_col;

    // Walk forward from after `return` to find the terminating `;`,
    // respecting string literals and balanced parentheses.
    let after_keyword_byte = return_byte + "return".len();
    let semi_offset = find_semicolon_balanced(&content[after_keyword_byte..])?;
    let semi_byte = after_keyword_byte + semi_offset;

    // Build the replacement range: from `return` keyword through `;`.
    let stmt_end_byte = semi_byte + 1;

    // Compute line/col for the start (the `return` keyword).
    let start_line = diag_line as u32;
    let start_char = return_col as u32;

    // Compute line/col for the end (after `;`).
    let end_line = content[..stmt_end_byte].matches('\n').count() as u32;
    let end_line_start = content[..stmt_end_byte]
        .rfind('\n')
        .map(|p| p + 1)
        .unwrap_or(0);
    let end_char = (stmt_end_byte - end_line_start) as u32;

    // Extract the expression text (between `return ` and `;`).
    let expr_start = return_byte + "return ".len();
    let expr_text = content[expr_start..semi_byte].trim();

    // Case 1: `return null;` → just replace with `return;`.
    if expr_text == "null" {
        return Some(TextEdit {
            range: Range {
                start: Position::new(start_line, start_char),
                end: Position::new(end_line, end_char),
            },
            new_text: "return;".to_string(),
        });
    }

    // Capture the indentation of the return line.
    let indent = &line_text[..return_col];

    // Check whether this return is the last statement in the function
    // body.  If the only thing between the `;` and the function's
    // closing `}` is whitespace, the `return;` is redundant.
    let needs_bare_return = !is_last_statement_in_function(content, stmt_end_byte);

    let new_text = if needs_bare_return {
        format!("{};\n{}return;", expr_text, indent)
    } else {
        format!("{};", expr_text)
    };

    Some(TextEdit {
        range: Range {
            start: Position::new(start_line, start_char),
            end: Position::new(end_line, end_char),
        },
        new_text,
    })
}

/// Build a list of `TextEdit`s that change the enclosing function's
/// return type to `target_type` and, when the target is `void`,
/// optionally remove the `@return` docblock tag.
///
/// Returns `None` if the enclosing function cannot be found or its
/// return type already matches `target_type`.
fn build_change_return_type_edits_to(
    content: &str,
    diag_line: usize,
    target_type: &str,
) -> Option<Vec<TextEdit>> {
    let lines: Vec<&str> = content.lines().collect();
    if diag_line >= lines.len() {
        return None;
    }

    let mut edits = Vec::new();

    // ── Step 1: Find the opening `{` of the function body ───────────
    // The diagnostic is on a `return` statement inside the body, so
    // search backward to find the enclosing function's opening brace.
    let brace_line = find_function_open_brace_line(&lines, diag_line)?;

    // ── Step 2: Find the `)` that closes the parameter list ─────────
    let (paren_line, paren_col) = find_close_paren_before_brace(&lines, brace_line)?;

    // ── Step 3: Find the return type hint between `)` and `{` ───────
    let type_edit = find_return_type_edit(&lines, paren_line, paren_col, brace_line, target_type)?;
    edits.push(type_edit);

    // ── Step 4: Find the function signature line ────────────────────
    let func_line = find_function_keyword_line(&lines, paren_line)?;

    // ── Step 5: Remove @return from docblock when target is void ────
    if target_type == "void"
        && let Some(return_tag_edit) = find_and_remove_return_tag(&lines, func_line)
    {
        edits.push(return_tag_edit);
    }

    Some(edits)
}

/// Extract the actual return type from a `return.void` diagnostic
/// message.
///
/// Message format:
/// `{desc} with return type void returns {actual} but should not return anything.`
///
/// Returns the `{actual}` type string, or `None` if the message
/// doesn't match.
fn extract_actual_type(message: &str) -> Option<&str> {
    let marker = " returns ";
    let start = message.find(marker)? + marker.len();
    let rest = &message[start..];
    let end = rest.find(" but should not return anything.")?;
    let actual = rest[..end].trim();
    if actual.is_empty() {
        return None;
    }
    Some(actual)
}

/// Extract the actual return type from a `return.type` diagnostic
/// message.
///
/// Message format:
/// `{desc} should return {expected} but returns {actual}.`
///
/// Returns the `{actual}` type string, or `None` if the message
/// doesn't match.
fn extract_return_type_actual(message: &str) -> Option<&str> {
    let marker = " but returns ";
    let start = message.find(marker)? + marker.len();
    let rest = &message[start..];
    // Strip the trailing period.
    let actual = rest.strip_suffix('.')?.trim();
    if actual.is_empty() {
        return None;
    }
    Some(actual)
}

/// Infer a PHP type from a literal return expression (cheap, no
/// resolution needed).
///
/// Returns `None` for anything that isn't a simple literal — the
/// caller should fall back to the full type resolver for those.
fn infer_type_from_literal(expr: &str) -> Option<String> {
    // Integer literal.
    if expr.parse::<i64>().is_ok() {
        return Some("int".to_string());
    }

    // Float literal.
    if expr.contains('.') && expr.parse::<f64>().is_ok() {
        return Some("float".to_string());
    }

    // Boolean literals.
    if expr == "true" || expr == "false" {
        return Some("bool".to_string());
    }

    // Null.
    if expr == "null" {
        return Some("null".to_string());
    }

    // String literals.
    if (expr.starts_with('\'') && expr.ends_with('\''))
        || (expr.starts_with('"') && expr.ends_with('"'))
    {
        return Some("string".to_string());
    }

    // Array literal.
    if expr == "[]" || expr.starts_with("array(") {
        return Some("array".to_string());
    }

    // `new ClassName(...)` — extract the class name.
    if let Some(rest) = expr.strip_prefix("new ") {
        let class_name = rest
            .split(|c: char| c == '(' || c.is_whitespace())
            .next()
            .unwrap_or("")
            .trim();
        if !class_name.is_empty() {
            return Some(class_name.to_string());
        }
    }

    // Not a literal — caller should use the full resolver.
    None
}

/// Check whether there is already a return type hint between `)` and
/// `{`.  Returns `true` if a `:` is found in that region.
fn has_return_type_between(
    lines: &[&str],
    paren_line: usize,
    paren_col: usize,
    brace_line: usize,
) -> bool {
    for (line_idx, line) in lines
        .iter()
        .enumerate()
        .take(brace_line + 1)
        .skip(paren_line)
    {
        let start_col = if line_idx == paren_line {
            paren_col + 1
        } else {
            0
        };
        let end_col = if line_idx == brace_line {
            line.find('{').unwrap_or(line.len())
        } else {
            line.len()
        };
        if start_col <= end_col && line[start_col..end_col].contains(':') {
            return true;
        }
    }
    false
}

/// Check whether the byte position `after_semi` (just past a `;`) is
/// the last statement in its enclosing function body.
///
/// Scans forward from `after_semi` through whitespace, comments, and
/// closing braces.  If only `}` characters (closing nested blocks like
/// `if`/`foreach`/`try`) and whitespace/comments appear between the
/// `;` and the function's own closing `}`, then the statement is the
/// last one in the function and a trailing `return;` would be
/// redundant.
///
/// Returns `false` when any other statement or token appears, meaning
/// the `return;` is needed to exit early.
fn is_last_statement_in_function(content: &str, after_semi: usize) -> bool {
    let bytes = content.as_bytes();
    let mut i = after_semi;

    while i < bytes.len() {
        match bytes[i] {
            b' ' | b'\t' | b'\r' | b'\n' => {
                i += 1;
            }
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'/' => {
                // Line comment — skip to end of line.
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'*' => {
                // Block comment — skip to `*/`.
                i += 2;
                while i + 1 < bytes.len() {
                    if bytes[i] == b'*' && bytes[i + 1] == b'/' {
                        i += 2;
                        break;
                    }
                    i += 1;
                }
            }
            b'}' => {
                // Closing brace — could be an `if`/`foreach`/etc.
                // block or the function itself.  Keep scanning to
                // see if anything other than more `}` and whitespace
                // follows.
                i += 1;
            }
            _ => return false,
        }
    }
    // Reached end of content with only `}` and whitespace — the
    // statement was the last one.
    true
}

// ── Helpers ─────────────────────────────────────────────────────────────────

/// Walk backward from `start_line` to find the line containing the
/// opening `{` of the enclosing function body.
///
/// The opening brace is the first `{` found scanning backward that is
/// not inside a string or comment.  We use a simple heuristic: look
/// for a line whose trimmed content ends with `{` or contains `{`
/// after a `)`.
///
/// **Limitation:** Braces inside string literals and comments are
/// counted, which can produce wrong results in rare cases.  A fully
/// correct backward scan would require re-parsing from the top of the
/// file.  This simple heuristic works for typical PHP code.
fn find_function_open_brace_line(lines: &[&str], start_line: usize) -> Option<usize> {
    // Track brace depth: we start inside the function body (depth 1)
    // and look backward for the opening `{`.
    let mut depth: i32 = 0;
    for i in (0..start_line).rev() {
        let line = lines[i];
        // Count braces on this line (simple heuristic, ignoring strings).
        for ch in line.chars() {
            match ch {
                '{' => depth -= 1,
                '}' => depth += 1,
                _ => {}
            }
        }
        if depth < 0 {
            return Some(i);
        }
    }
    None
}

/// Search forward from a declaration line to find the opening `{` of
/// the function body.
///
/// Checks the declaration line itself and up to 5 lines after it.
/// Returns the line number containing `{`, or `None`.
fn find_open_brace_from_declaration(lines: &[&str], decl_line: usize) -> Option<usize> {
    let end = (decl_line + 6).min(lines.len());
    (decl_line..end).find(|&i| lines[i].contains('{'))
}

/// Find the closing `)` of the parameter list before the opening `{`.
///
/// Scans backward from `brace_line` looking for `)`.
fn find_close_paren_before_brace(lines: &[&str], brace_line: usize) -> Option<(usize, usize)> {
    // First check the brace line itself (before the `{`).
    let brace_text = lines[brace_line];
    if let Some(brace_pos) = brace_text.rfind('{') {
        let before_brace = &brace_text[..brace_pos];
        if let Some(paren_pos) = before_brace.rfind(')') {
            return Some((brace_line, paren_pos));
        }
    }

    // Walk backward to find `)`.
    for i in (0..brace_line).rev() {
        if let Some(paren_pos) = lines[i].rfind(')') {
            return Some((i, paren_pos));
        }
    }

    None
}

/// Gather the source text between the closing `)` at `(paren_line, paren_col)`
/// and the opening `{` on `brace_line`.
///
/// The result spans from column `paren_col + 1` on `paren_line` to just
/// before the `{` on `brace_line`, with newlines between lines.
fn gather_between_paren_and_brace(
    lines: &[&str],
    paren_line: usize,
    paren_col: usize,
    brace_line: usize,
) -> String {
    let mut between = String::new();

    for (line_idx, line) in lines
        .iter()
        .enumerate()
        .take(brace_line + 1)
        .skip(paren_line)
    {
        let start_col = if line_idx == paren_line {
            paren_col + 1
        } else {
            0
        };
        let end_col = if line_idx == brace_line {
            line.find('{').unwrap_or(line.len())
        } else {
            line.len()
        };
        if start_col <= end_col {
            between.push_str(&line[start_col..end_col]);
        }
        if line_idx < brace_line {
            between.push('\n');
        }
    }

    between
}

/// Find the return type hint between the closing `)` and opening `{`,
/// and build a `TextEdit` that replaces it with `: {target_type}`.
///
/// Looks for the pattern `: TypeName` (with optional whitespace and
/// nullable `?` prefix).  Returns `None` if the current type already
/// matches `target_type`.
fn find_return_type_edit(
    lines: &[&str],
    paren_line: usize,
    paren_col: usize,
    brace_line: usize,
    target_type: &str,
) -> Option<TextEdit> {
    // Gather the text between `)` and `{` across potentially multiple
    // lines.
    let between = gather_between_paren_and_brace(lines, paren_line, paren_col, brace_line);

    // Find `: Type` in the between text.
    let colon_pos = between.find(':')?;
    let after_colon = &between[colon_pos + 1..];
    let type_start_offset = after_colon.find(|c: char| !c.is_whitespace()).unwrap_or(0);
    let type_text_start = colon_pos + 1 + type_start_offset;
    let type_text = &between[type_text_start..];

    // The type name ends at the first whitespace, `{`, or end of
    // the between text.
    let type_len = type_text
        .find(|c: char| c.is_whitespace() || c == '{')
        .unwrap_or(type_text.len());

    if type_len == 0 {
        return None;
    }

    let type_name = &type_text[..type_len];
    if type_name == target_type {
        return None;
    }

    // Convert the offset within `between` to a line/col position.
    // The colon_pos tells us where `:` is; the type starts at
    // `type_text_start` and ends at `type_text_start + type_len`.

    // Map `colon_pos` back to an absolute line/col.
    let colon_abs = map_between_offset_to_position(lines, paren_line, paren_col, colon_pos)?;
    let type_end_abs =
        map_between_offset_to_position(lines, paren_line, paren_col, type_text_start + type_len)?;

    Some(TextEdit {
        range: Range {
            start: Position::new(colon_abs.0 as u32, colon_abs.1 as u32),
            end: Position::new(type_end_abs.0 as u32, type_end_abs.1 as u32),
        },
        new_text: format!(": {}", target_type),
    })
}

/// Map an offset within the "between" text back to an absolute
/// (line, col) position in the original source.
fn map_between_offset_to_position(
    lines: &[&str],
    paren_line: usize,
    paren_col: usize,
    offset: usize,
) -> Option<(usize, usize)> {
    // Re-walk the between region character by character.
    let mut remaining = offset;
    for (line_idx, line) in lines.iter().enumerate().skip(paren_line) {
        let start_col = if line_idx == paren_line {
            paren_col + 1
        } else {
            0
        };
        let end_col = line.len();
        let span = end_col - start_col;

        if remaining <= span {
            return Some((line_idx, start_col + remaining));
        }
        remaining -= span;

        // Account for the newline character.
        if remaining == 0 {
            // Exactly at the newline boundary — start of next line.
            return Some((line_idx + 1, 0));
        }
        remaining -= 1; // for the '\n'
    }
    None
}

/// Walk backward from `start_line` to find a line containing the
/// `function ` keyword (the function/method signature).
fn find_function_keyword_line(lines: &[&str], start_line: usize) -> Option<usize> {
    (0..=start_line)
        .rev()
        .find(|&i| lines[i].contains("function ") || lines[i].contains("function("))
}

/// Look for a docblock above the function signature and remove any
/// `@return` tag line from it.
fn find_and_remove_return_tag(lines: &[&str], func_line: usize) -> Option<TextEdit> {
    if func_line == 0 {
        return None;
    }

    // Walk backward from the line before the function to find the
    // docblock.  Skip attribute lines like `#[Override]`.
    let mut doc_end_line = None;
    for i in (0..func_line).rev() {
        let trimmed = lines[i].trim();
        if trimmed.ends_with("*/") {
            doc_end_line = Some(i);
            break;
        }
        // Skip attributes and blank lines between function and docblock.
        if trimmed.starts_with("#[") || trimmed.is_empty() {
            continue;
        }
        // Hit non-docblock, non-attribute content — no docblock.
        break;
    }

    let doc_end_line = doc_end_line?;

    // Find the start of the docblock.
    let mut doc_start_line = doc_end_line;
    for i in (0..=doc_end_line).rev() {
        let trimmed = lines[i].trim();
        if trimmed.starts_with("/**") {
            doc_start_line = i;
            break;
        }
        if trimmed.starts_with('*') || trimmed.starts_with("/*") {
            continue;
        }
        break;
    }

    // Look for a `@return` line within the docblock.
    let return_line =
        (doc_start_line..=doc_end_line).find(|&i| lines[i].trim().contains("@return"))?;

    Some(TextEdit {
        range: Range {
            start: Position::new(return_line as u32, 0),
            end: Position::new((return_line + 1) as u32, 0),
        },
        new_text: String::new(),
    })
}

// ── Stale detection ─────────────────────────────────────────────────────────

/// Check whether a `return.void` or `return.empty` diagnostic is stale.
///
/// For `return.void`: the diagnostic is stale when the diagnostic line
/// contains `return;` (bare return, no expression) — meaning the
/// expression has already been stripped.
///
/// For `return.empty`: the diagnostic is stale when the enclosing
/// function's return type declaration already says `void`.
///
/// Called from `is_stale_phpstan_diagnostic` in `diagnostics/mod.rs`.
pub(crate) fn is_fix_return_type_stale(content: &str, diag_line: usize, identifier: &str) -> bool {
    let lines: Vec<&str> = content.lines().collect();

    if diag_line >= lines.len() {
        return true; // line doesn't exist any more → stale
    }

    match identifier {
        RETURN_VOID_ID => {
            // Stale if the line no longer contains a return with an
            // expression (user either stripped it or changed the type).
            let trimmed = lines[diag_line].trim();
            !trimmed.contains("return ") || trimmed == "return;"
        }
        RETURN_TYPE_ID => {
            // No content heuristic — the fix might be to change the
            // code rather than the type, so we can't tell from the
            // source alone.  Cleared eagerly by codeAction/resolve.
            false
        }
        MISSING_TYPE_RETURN_ID => {
            // The diagnostic is reported on the function declaration
            // line itself.  Stale if `)` is followed by `:` (a return
            // type has been added).  Simple text check on the line.
            let line = lines[diag_line];
            if let Some(paren_pos) = line.rfind(')') {
                line[paren_pos + 1..].contains(':')
            } else {
                false
            }
        }
        RETURN_EMPTY_ID => {
            // Stale if the enclosing function's return type is already
            // `void`.  The diagnostic is on a `return;` inside the
            // body, so search backward for the opening brace.
            let brace_line = match find_function_open_brace_line(&lines, diag_line) {
                Some(l) => l,
                None => return false,
            };
            let (paren_line, paren_col) = match find_close_paren_before_brace(&lines, brace_line) {
                Some(p) => p,
                None => return false,
            };

            // Gather text between `)` and `{` and check if the return
            // type is already `void`.
            let between =
                gather_between_paren_and_brace(&lines, paren_line, paren_col, brace_line);

            // Look for `: void` in the between text.
            if let Some(colon_pos) = between.find(':') {
                let after_colon = between[colon_pos + 1..].trim();
                // The type name after `:` ends at whitespace or `{`.
                let type_word = after_colon
                    .split(|c: char| c.is_whitespace() || c == '{')
                    .next()
                    .unwrap_or("");
                type_word == "void"
            } else {
                false
            }
        }
        _ => false,
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── build_strip_return_expr_edit ────────────────────────────────

    #[test]
    fn removes_return_keeps_expression_omits_redundant_return() {
        // Last statement in function — no need for bare `return;`.
        let content = "<?php\nfunction foo(): void {\n    return 42;\n}\n";
        let edit = build_strip_return_expr_edit(content, 2).unwrap();
        assert_eq!(edit.range.start, Position::new(2, 4));
        assert_eq!(edit.range.end, Position::new(2, 14));
        assert_eq!(edit.new_text, "42;");
    }

    #[test]
    fn removes_return_string() {
        // Last statement in function — no bare `return;`.
        let content = "<?php\nfunction foo(): void {\n    return 'hello';\n}\n";
        let edit = build_strip_return_expr_edit(content, 2).unwrap();
        assert_eq!(edit.new_text, "'hello';");
    }

    #[test]
    fn removes_return_method_call() {
        // Last statement in method — no bare `return;`.
        let content = "<?php\nclass A {\n    public function run(): void {\n        return $this->doWork();\n    }\n}\n";
        let edit = build_strip_return_expr_edit(content, 3).unwrap();
        assert_eq!(edit.new_text, "$this->doWork();");
    }

    #[test]
    fn removes_return_in_if_block_with_more_code() {
        // NOT the last statement — there's `echo 'more';` after the if block.
        let content = "<?php\nclass A {\n    public function run(): void {\n        if (true) {\n            return $this->doWork();\n        }\n        echo 'more';\n    }\n}\n";
        let edit = build_strip_return_expr_edit(content, 4).unwrap();
        assert_eq!(edit.new_text, "$this->doWork();\n            return;");
    }

    #[test]
    fn return_null_becomes_bare_return() {
        // `return null;` → `return;` (null is not meaningful in void)
        let content = "<?php\nfunction foo(): void {\n    return null;\n}\n";
        let edit = build_strip_return_expr_edit(content, 2).unwrap();
        assert_eq!(edit.new_text, "return;");
    }

    #[test]
    fn strips_return_expression_variable() {
        // Last statement — no bare `return;`.
        let content = "<?php\nfunction foo(): void {\n    return $value;\n}\n";
        let edit = build_strip_return_expr_edit(content, 2).unwrap();
        assert_eq!(edit.new_text, "$value;");
        assert_eq!(edit.range.start, Position::new(2, 4));
    }

    #[test]
    fn strips_multiline_return_expression() {
        // Last statement — no bare `return;`.
        let content =
            "<?php\nfunction foo(): void {\n    return array(\n        1,\n        2\n    );\n}\n";
        let edit = build_strip_return_expr_edit(content, 2).unwrap();
        assert_eq!(edit.new_text, "array(\n        1,\n        2\n    );");
        assert_eq!(edit.range.start, Position::new(2, 4));
        // The `;` is on line 5 (0-indexed)
        assert_eq!(edit.range.end.line, 5);
    }

    #[test]
    fn strips_return_in_if_block_last_statement() {
        // return is inside an if block, but it IS the last statement
        // in the function (only `}` closers follow).
        let content = "<?php\nclass A {\n    public function run(): void {\n        if (true) {\n            return $this->doWork();\n        }\n    }\n}\n";
        let edit = build_strip_return_expr_edit(content, 4).unwrap();
        assert_eq!(edit.new_text, "$this->doWork();");
    }

    #[test]
    fn returns_none_when_already_bare_return() {
        let content = "<?php\nfunction foo(): void {\n    return;\n}\n";
        assert!(build_strip_return_expr_edit(content, 2).is_none());
    }

    #[test]
    fn returns_none_for_invalid_line() {
        let content = "<?php\n";
        assert!(build_strip_return_expr_edit(content, 5).is_none());
    }

    #[test]
    fn returns_none_when_no_return_on_line() {
        let content = "<?php\nfunction foo(): void {\n    $x = 1;\n}\n";
        assert!(build_strip_return_expr_edit(content, 2).is_none());
    }

    // ── build_change_return_type_edits_to ───────────────────────────

    #[test]
    fn changes_return_type_to_void() {
        let content = "<?php\nfunction foo(): int {\n    return;\n}\n";
        let edits = build_change_return_type_edits_to(content, 2, "void").unwrap();
        assert_eq!(edits.len(), 1);
        let edit = &edits[0];
        assert_eq!(edit.new_text, ": void");
        // Verify it replaces `: int`
        let lines: Vec<&str> = content.lines().collect();
        let replaced = &lines[edit.range.start.line as usize]
            [edit.range.start.character as usize..edit.range.end.character as usize];
        assert_eq!(replaced, ": int");
    }

    #[test]
    fn changes_return_type_string() {
        let content = "<?php\nfunction foo(): string {\n    return;\n}\n";
        let edits = build_change_return_type_edits_to(content, 2, "void").unwrap();
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].new_text, ": void");
    }

    #[test]
    fn changes_return_type_to_actual() {
        let content = "<?php\nfunction foo(): void {\n    return 42;\n}\n";
        let edits = build_change_return_type_edits_to(content, 2, "int").unwrap();
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].new_text, ": int");
    }

    #[test]
    fn changes_void_to_string() {
        let content = "<?php\nfunction foo(): void {\n    return 'hello';\n}\n";
        let edits = build_change_return_type_edits_to(content, 2, "string").unwrap();
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].new_text, ": string");
    }

    #[test]
    fn changes_nullable_return_type() {
        let content = "<?php\nfunction foo(): ?string {\n    return;\n}\n";
        let edits = build_change_return_type_edits_to(content, 2, "void").unwrap();
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].new_text, ": void");
    }

    #[test]
    fn changes_return_type_and_removes_return_tag() {
        let content =
            "<?php\n/**\n * @return int The value\n */\nfunction foo(): int {\n    return;\n}\n";
        let edits = build_change_return_type_edits_to(content, 5, "void").unwrap();
        assert_eq!(edits.len(), 2);

        // One edit replaces the type, one removes the @return line.
        let type_edit = edits.iter().find(|e| e.new_text == ": void").unwrap();
        let tag_edit = edits.iter().find(|e| e.new_text.is_empty()).unwrap();

        // The type edit should be on the function line (line 4).
        assert_eq!(type_edit.range.start.line, 4);

        // The @return tag is on line 2.
        assert_eq!(tag_edit.range.start.line, 2);
        assert_eq!(tag_edit.range.end.line, 3);
    }

    #[test]
    fn does_not_change_when_already_void() {
        let content = "<?php\nfunction foo(): void {\n    return;\n}\n";
        assert!(build_change_return_type_edits_to(content, 2, "void").is_none());
    }

    #[test]
    fn does_not_change_when_already_matches_actual() {
        let content = "<?php\nfunction foo(): int {\n    return 42;\n}\n";
        assert!(build_change_return_type_edits_to(content, 2, "int").is_none());
    }

    #[test]
    fn returns_none_when_no_function_found() {
        let content = "<?php\nreturn;\n";
        assert!(build_change_return_type_edits_to(content, 1, "void").is_none());
    }

    #[test]
    fn changes_method_return_type() {
        let content =
            "<?php\nclass Foo {\n    public function bar(): string {\n        return;\n    }\n}\n";
        let edits = build_change_return_type_edits_to(content, 3, "void").unwrap();
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].new_text, ": void");
    }

    // ── is_fix_return_type_stale ───────────────────────────────────

    #[test]
    fn stale_when_return_has_no_expression() {
        let content = "<?php\nfunction foo(): void {\n    return;\n}\n";
        assert!(is_fix_return_type_stale(content, 2, "return.void"));
    }

    #[test]
    fn not_stale_when_return_has_expression() {
        let content = "<?php\nfunction foo(): void {\n    return 42;\n}\n";
        assert!(!is_fix_return_type_stale(content, 2, "return.void"));
    }

    #[test]
    fn stale_return_empty_when_type_is_void() {
        let content = "<?php\nfunction foo(): void {\n    return;\n}\n";
        assert!(is_fix_return_type_stale(content, 2, "return.empty"));
    }

    #[test]
    fn not_stale_return_empty_when_type_is_not_void() {
        let content = "<?php\nfunction foo(): int {\n    return;\n}\n";
        assert!(!is_fix_return_type_stale(content, 2, "return.empty"));
    }

    #[test]
    fn stale_when_line_gone() {
        let content = "<?php\n";
        assert!(is_fix_return_type_stale(content, 5, "return.void"));
        assert!(is_fix_return_type_stale(content, 5, "return.empty"));
    }

    #[test]
    fn not_stale_for_unknown_identifier() {
        let content = "<?php\nfunction foo(): void {\n    return;\n}\n";
        assert!(!is_fix_return_type_stale(content, 2, "other.id"));
    }

    // ── Message matching ───────────────────────────────────────────

    #[test]
    fn matches_return_void_message() {
        let msg =
            "Method Foo::bar() with return type void returns int but should not return anything.";
        assert!(msg.ends_with(RETURN_VOID_MSG_SUFFIX));
    }

    // ── extract_actual_type ─────────────────────────────────────────

    #[test]
    fn extracts_actual_type_int() {
        let msg =
            "Method Foo::bar() with return type void returns int but should not return anything.";
        assert_eq!(extract_actual_type(msg), Some("int"));
    }

    #[test]
    fn extracts_actual_type_string() {
        let msg =
            "Function foo() with return type void returns string but should not return anything.";
        assert_eq!(extract_actual_type(msg), Some("string"));
    }

    #[test]
    fn extracts_actual_type_union() {
        let msg = "Method X::y() with return type void returns int|string but should not return anything.";
        assert_eq!(extract_actual_type(msg), Some("int|string"));
    }

    #[test]
    fn extracts_actual_type_null() {
        let msg =
            "Method X::y() with return type void returns null but should not return anything.";
        assert_eq!(extract_actual_type(msg), Some("null"));
    }

    #[test]
    fn extract_actual_type_returns_none_for_unrelated_message() {
        let msg = "Some other message.";
        assert_eq!(extract_actual_type(msg), None);
    }

    // ── extract_return_type_actual (return.type) ────────────────────

    #[test]
    fn extracts_return_type_actual_int() {
        let msg = "Method Foo::bar() should return string but returns int.";
        assert_eq!(extract_return_type_actual(msg), Some("int"));
    }

    #[test]
    fn extracts_return_type_actual_union() {
        let msg = "Function foo() should return int but returns int|string.";
        assert_eq!(extract_return_type_actual(msg), Some("int|string"));
    }

    #[test]
    fn extracts_return_type_actual_class() {
        let msg = "Method X::y() should return self but returns App\\Models\\User.";
        assert_eq!(extract_return_type_actual(msg), Some("App\\Models\\User"));
    }

    #[test]
    fn extract_return_type_actual_returns_none_for_unrelated() {
        let msg = "Some other message.";
        assert_eq!(extract_return_type_actual(msg), None);
    }

    // ── has_return_type_between ─────────────────────────────────────

    #[test]
    fn detects_existing_return_type() {
        let lines = vec!["function foo(): int {"];
        // paren at col 13 (the ')'), brace_line = 0
        assert!(has_return_type_between(&lines, 0, 13, 0));
    }

    #[test]
    fn detects_no_return_type() {
        let lines = vec!["function foo() {"];
        // paren at col 13, brace_line = 0
        assert!(!has_return_type_between(&lines, 0, 13, 0));
    }

    // ── infer_type_from_literal ─────────────────────────────────────

    #[test]
    fn literal_int() {
        assert_eq!(infer_type_from_literal("42"), Some("int".to_string()));
        assert_eq!(infer_type_from_literal("-1"), Some("int".to_string()));
    }

    #[test]
    fn literal_float() {
        assert_eq!(infer_type_from_literal("1.5"), Some("float".to_string()));
    }

    #[test]
    fn literal_bool() {
        assert_eq!(infer_type_from_literal("true"), Some("bool".to_string()));
        assert_eq!(infer_type_from_literal("false"), Some("bool".to_string()));
    }

    #[test]
    fn literal_string() {
        assert_eq!(
            infer_type_from_literal("'hello'"),
            Some("string".to_string())
        );
        assert_eq!(
            infer_type_from_literal("\"world\""),
            Some("string".to_string())
        );
    }

    #[test]
    fn literal_array() {
        assert_eq!(infer_type_from_literal("[]"), Some("array".to_string()));
    }

    #[test]
    fn literal_new_class() {
        assert_eq!(
            infer_type_from_literal("new Foo()"),
            Some("Foo".to_string())
        );
    }

    #[test]
    fn literal_null() {
        assert_eq!(infer_type_from_literal("null"), Some("null".to_string()));
    }

    #[test]
    fn non_literal_returns_none() {
        assert_eq!(infer_type_from_literal("$var"), None);
        assert_eq!(infer_type_from_literal("$this->bar()"), None);
        assert_eq!(infer_type_from_literal("foo()"), None);
        assert_eq!(infer_type_from_literal("Str::toUpper($x)"), None);
    }

    // ── stale detection for new identifiers ─────────────────────────

    #[test]
    fn return_type_never_stale_via_heuristic() {
        // return.type is only cleared by codeAction/resolve, not by
        // content heuristics, because the right fix might be to change
        // the code rather than the type.
        let content = "<?php\nfunction foo(): int {\n    $x = 1;\n}\n";
        assert!(!is_fix_return_type_stale(content, 2, "return.type"));

        let content2 = "<?php\nfunction foo(): int {\n    return 'hello';\n}\n";
        assert!(!is_fix_return_type_stale(content2, 2, "return.type"));
    }

    #[test]
    fn stale_missing_type_when_type_added() {
        let content = "<?php\nfunction foo(): int {\n    return 1;\n}\n";
        // missingType.return is reported on the function declaration line
        assert!(is_fix_return_type_stale(content, 1, "missingType.return"));
    }

    #[test]
    fn stale_missing_type_multiline_signature() {
        let content = "<?php\nfunction foo(\n    int $x\n): int {\n    return $x;\n}\n";
        // The diagnostic is on the `function` line (line 1), but the
        // `)` and `: int` are on line 3.  PHPStan reports on the
        // function keyword line.  Our simple check looks at the diag
        // line for `)...:`  which won't find it on line 1.  That's
        // acceptable — the diagnostic will be cleared by the next
        // PHPStan run instead of eagerly.
        assert!(!is_fix_return_type_stale(content, 1, "missingType.return"));
    }

    #[test]
    fn not_stale_missing_type_when_no_type() {
        let content = "<?php\nfunction foo() {\n    return 1;\n}\n";
        assert!(!is_fix_return_type_stale(content, 1, "missingType.return"));
    }

    #[test]
    fn matches_return_empty_message() {
        let msg = "Method App\\Foo::bar() should return int but empty return statement found.";
        assert!(msg.contains(RETURN_EMPTY_MSG_FRAGMENT));
    }

    #[test]
    fn rejects_unrelated_message() {
        let msg = "Call to function assert() with true will always evaluate to true.";
        assert!(!msg.ends_with(RETURN_VOID_MSG_SUFFIX));
        assert!(!msg.contains(RETURN_EMPTY_MSG_FRAGMENT));
    }

    // ── Helper tests ───────────────────────────────────────────────

    // ── Docblock @return removal ───────────────────────────────────

    #[test]
    fn change_to_actual_does_not_remove_return_tag() {
        let content = "<?php\n/**\n * @return int The value\n */\nfunction foo(): void {\n    return 42;\n}\n";
        let edits = build_change_return_type_edits_to(content, 5, "int").unwrap();
        // Should only change the type hint, NOT remove the @return tag
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].new_text, ": int");
    }

    // ── add return type (missingType.return) ────────────────────────

    #[test]
    fn add_return_type_inserts_after_close_paren_helper() {
        let content = "<?php\nfunction foo() {\n    return 1;\n}\n";
        let lines: Vec<&str> = content.lines().collect();
        let brace_line = find_function_open_brace_line(&lines, 2).unwrap();
        let (paren_line, paren_col) = find_close_paren_before_brace(&lines, brace_line).unwrap();
        assert!(!has_return_type_between(
            &lines, paren_line, paren_col, brace_line
        ));
        assert_eq!(paren_line, 1);
        assert_eq!(paren_col, 13);
    }

    #[test]
    fn removes_return_tag_from_multiline_docblock() {
        let content = "<?php\n/**\n * Does something.\n * @return int\n */\nfunction foo(): int {\n    return;\n}\n";
        let edits = build_change_return_type_edits_to(content, 6, "void").unwrap();
        assert_eq!(edits.len(), 2);
        let tag_edit = edits.iter().find(|e| e.new_text.is_empty()).unwrap();
        assert_eq!(tag_edit.range.start.line, 3);
        assert_eq!(tag_edit.range.end.line, 4);
    }

    #[test]
    fn no_return_tag_edit_when_no_docblock() {
        let content = "<?php\nfunction foo(): int {\n    return;\n}\n";
        let edits = build_change_return_type_edits_to(content, 2, "void").unwrap();
        assert_eq!(edits.len(), 1); // Only the type edit, no tag edit.
    }

    #[test]
    fn no_return_tag_edit_when_docblock_has_no_return() {
        let content =
            "<?php\n/**\n * Does something.\n */\nfunction foo(): int {\n    return;\n}\n";
        let edits = build_change_return_type_edits_to(content, 5, "void").unwrap();
        assert_eq!(edits.len(), 1); // Only the type edit, no tag edit.
    }

    // ── Integration: apply strip edit ──────────────────────────────

    #[test]
    fn apply_strip_edit_produces_correct_content() {
        // `return 42;` is the last statement → replaced with just `42;`
        // (no redundant `return;` since it's the last statement).
        let content = "<?php\nfunction foo(): void {\n    return 42;\n}\n";
        let edit = build_strip_return_expr_edit(content, 2).unwrap();

        // Apply the edit manually.
        let lines: Vec<&str> = content.lines().collect();
        let mut result = String::new();
        for (i, line) in lines.iter().enumerate() {
            if i > 0 {
                result.push('\n');
            }
            if i == edit.range.start.line as usize {
                let prefix = &line[..edit.range.start.character as usize];
                let suffix = if edit.range.end.line as usize == i {
                    &line[edit.range.end.character as usize..]
                } else {
                    ""
                };
                result.push_str(prefix);
                result.push_str(&edit.new_text);
                result.push_str(suffix);
            } else {
                result.push_str(line);
            }
        }
        result.push('\n');

        assert_eq!(result, "<?php\nfunction foo(): void {\n    42;\n}\n");
    }

    #[test]
    fn apply_strip_edit_null_produces_bare_return() {
        let content = "<?php\nfunction foo(): void {\n    return null;\n}\n";
        let edit = build_strip_return_expr_edit(content, 2).unwrap();

        let lines: Vec<&str> = content.lines().collect();
        let mut result = String::new();
        for (i, line) in lines.iter().enumerate() {
            if i > 0 {
                result.push('\n');
            }
            if i == edit.range.start.line as usize {
                let prefix = &line[..edit.range.start.character as usize];
                let suffix = if edit.range.end.line as usize == i {
                    &line[edit.range.end.character as usize..]
                } else {
                    ""
                };
                result.push_str(prefix);
                result.push_str(&edit.new_text);
                result.push_str(suffix);
            } else {
                result.push_str(line);
            }
        }
        result.push('\n');

        assert_eq!(result, "<?php\nfunction foo(): void {\n    return;\n}\n");
    }

    // ── Stale detection after strip fix ─────────────────────────────

    #[test]
    fn stale_after_strip_fix() {
        // Before fix: not stale.
        let before = "<?php\nfunction foo(): void {\n    return 42;\n}\n";
        assert!(!is_fix_return_type_stale(before, 2, "return.void"));

        // After fix (expression kept, no redundant return;): stale
        // because the line no longer has `return ` (it now has `42;`).
        let after = "<?php\nfunction foo(): void {\n    42;\n}\n";
        assert!(is_fix_return_type_stale(after, 2, "return.void"));
    }
}
