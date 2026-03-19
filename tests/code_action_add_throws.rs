//! Integration tests for the "Add @throws" code action.
//!
//! These tests exercise the full pipeline: a PHPStan diagnostic with
//! identifier `missingType.checkedException` triggers a code action
//! that inserts a `@throws` tag into the method docblock and (when
//! needed) adds a `use` import for the exception class.

mod common;

use common::create_test_backend;
use tower_lsp::lsp_types::*;

/// Inject a PHPStan diagnostic into the backend's cache and return it.
fn inject_phpstan_diag(
    backend: &phpantom_lsp::Backend,
    uri: &str,
    line: u32,
    message: &str,
    identifier: &str,
) -> Diagnostic {
    let diag = Diagnostic {
        range: Range {
            start: Position::new(line, 0),
            end: Position::new(line, 80),
        },
        severity: Some(DiagnosticSeverity::ERROR),
        code: Some(NumberOrString::String(identifier.to_string())),
        source: Some("PHPStan".to_string()),
        message: message.to_string(),
        ..Default::default()
    };
    {
        let mut cache = backend.phpstan_last_diags().lock();
        cache.entry(uri.to_string()).or_default().push(diag.clone());
    }
    diag
}

/// Helper: send a code action request at the given line/character.
fn get_code_actions(
    backend: &phpantom_lsp::Backend,
    uri: &str,
    content: &str,
    line: u32,
    character: u32,
) -> Vec<CodeActionOrCommand> {
    let params = CodeActionParams {
        text_document: TextDocumentIdentifier {
            uri: uri.parse().unwrap(),
        },
        range: Range {
            start: Position::new(line, character),
            end: Position::new(line, character),
        },
        context: CodeActionContext {
            diagnostics: vec![],
            only: None,
            trigger_kind: None,
        },
        work_done_progress_params: WorkDoneProgressParams {
            work_done_token: None,
        },
        partial_result_params: PartialResultParams {
            partial_result_token: None,
        },
    };

    backend.handle_code_action(uri, content, &params)
}

/// Find the "Add @throws" code action.
fn find_add_throws_action(actions: &[CodeActionOrCommand]) -> Option<&CodeAction> {
    actions.iter().find_map(|a| match a {
        CodeActionOrCommand::CodeAction(ca) if ca.title.starts_with("Add @throws") => Some(ca),
        _ => None,
    })
}

/// Extract all text edits from a code action's workspace edit, sorted by
/// file URI.
fn extract_edits(action: &CodeAction) -> Vec<TextEdit> {
    let edit = action.edit.as_ref().expect("action should have an edit");
    let changes = edit.changes.as_ref().expect("edit should have changes");
    changes.values().flat_map(|v| v.iter()).cloned().collect()
}

/// Combine text edits into the original content to produce the result.
/// Edits are applied in reverse order of their start position so that
/// earlier edits don't invalidate later offsets.
fn apply_edits(content: &str, edits: &[TextEdit]) -> String {
    let mut result = content.to_string();
    let mut sorted: Vec<&TextEdit> = edits.iter().collect();
    sorted.sort_by(|a, b| {
        b.range
            .start
            .line
            .cmp(&a.range.start.line)
            .then(b.range.start.character.cmp(&a.range.start.character))
    });

    for edit in sorted {
        let start = lsp_pos_to_offset(&result, edit.range.start);
        let end = lsp_pos_to_offset(&result, edit.range.end);
        result.replace_range(start..end, &edit.new_text);
    }
    result
}

fn lsp_pos_to_offset(content: &str, pos: Position) -> usize {
    let mut offset = 0;
    for (i, line) in content.lines().enumerate() {
        if i == pos.line as usize {
            return offset + pos.character as usize;
        }
        offset += line.len() + 1; // +1 for newline
    }
    content.len()
}

// ── Basic: adds @throws into existing multi-line docblock ───────────────────

#[test]
fn adds_throws_to_existing_docblock() {
    let backend = create_test_backend();
    let uri = "file:///test.php";
    let content = r#"<?php
namespace App\Controllers;

class FooController {
    /**
     * Do something.
     */
    public function bar(): void {
        throw new \App\Exceptions\BarException();
    }
}
"#;
    backend.update_ast(uri, content);

    inject_phpstan_diag(
        &backend,
        uri,
        8, // the throw line
        "Method App\\Controllers\\FooController::bar() throws checked exception App\\Exceptions\\BarException but it's missing from the PHPDoc @throws tag.",
        "missingType.checkedException",
    );

    let actions = get_code_actions(&backend, uri, content, 8, 10);
    let action = find_add_throws_action(&actions).expect("should offer Add @throws action");

    assert_eq!(action.kind, Some(CodeActionKind::QUICKFIX));
    assert_eq!(action.is_preferred, Some(true));
    assert!(
        action.title.contains("BarException"),
        "title should mention exception: {}",
        action.title
    );

    let edits = extract_edits(action);
    let result = apply_edits(content, &edits);

    assert!(
        result.contains("@throws BarException"),
        "should insert @throws tag:\n{}",
        result
    );
    assert!(
        result.contains("use App\\Exceptions\\BarException;"),
        "should add use import:\n{}",
        result
    );
}

// ── No import needed when exception is in same namespace ────────────────────

#[test]
fn no_import_when_same_namespace() {
    let backend = create_test_backend();
    let uri = "file:///test.php";
    let content = r#"<?php
namespace App\Exceptions;

class Thrower {
    /**
     * Do something.
     */
    public function go(): void {
        throw new BarException();
    }
}
"#;
    backend.update_ast(uri, content);

    inject_phpstan_diag(
        &backend,
        uri,
        8,
        "Method App\\Exceptions\\Thrower::go() throws checked exception App\\Exceptions\\BarException but it's missing from the PHPDoc @throws tag.",
        "missingType.checkedException",
    );

    let actions = get_code_actions(&backend, uri, content, 8, 10);
    let action = find_add_throws_action(&actions).expect("should offer Add @throws action");

    let edits = extract_edits(action);
    let result = apply_edits(content, &edits);

    assert!(
        result.contains("@throws BarException"),
        "should insert @throws tag:\n{}",
        result
    );
    // Should NOT add a use import — same namespace.
    assert!(
        !result.contains("use App\\Exceptions\\BarException"),
        "should NOT add use import for same-namespace class:\n{}",
        result
    );
}

// ── No import when already imported ─────────────────────────────────────────

#[test]
fn no_import_when_already_imported() {
    let backend = create_test_backend();
    let uri = "file:///test.php";
    let content = r#"<?php
namespace App\Controllers;

use App\Exceptions\BarException;

class FooController {
    /**
     * Do something.
     */
    public function bar(): void {
        throw new BarException();
    }
}
"#;
    backend.update_ast(uri, content);

    inject_phpstan_diag(
        &backend,
        uri,
        10,
        "Method App\\Controllers\\FooController::bar() throws checked exception App\\Exceptions\\BarException but it's missing from the PHPDoc @throws tag.",
        "missingType.checkedException",
    );

    let actions = get_code_actions(&backend, uri, content, 10, 10);
    let action = find_add_throws_action(&actions).expect("should offer Add @throws action");

    let edits = extract_edits(action);
    let result = apply_edits(content, &edits);

    assert!(
        result.contains("@throws BarException"),
        "should insert @throws tag:\n{}",
        result
    );
    // Count occurrences of the use statement — should still be exactly 1.
    let use_count = result.matches("use App\\Exceptions\\BarException;").count();
    assert_eq!(
        use_count, 1,
        "should NOT duplicate existing use import:\n{}",
        result
    );
}

// ── Creates new docblock when none exists ───────────────────────────────────

#[test]
fn creates_docblock_when_none_exists() {
    let backend = create_test_backend();
    let uri = "file:///test.php";
    let content = r#"<?php
namespace App\Controllers;

class FooController {
    public function bar(): void {
        throw new \App\Exceptions\BarException();
    }
}
"#;
    backend.update_ast(uri, content);

    inject_phpstan_diag(
        &backend,
        uri,
        5,
        "Method App\\Controllers\\FooController::bar() throws checked exception App\\Exceptions\\BarException but it's missing from the PHPDoc @throws tag.",
        "missingType.checkedException",
    );

    let actions = get_code_actions(&backend, uri, content, 5, 10);
    let action = find_add_throws_action(&actions).expect("should offer Add @throws action");

    let edits = extract_edits(action);
    let result = apply_edits(content, &edits);

    assert!(
        result.contains("/**"),
        "should create a docblock:\n{}",
        result
    );
    assert!(
        result.contains("@throws BarException"),
        "should insert @throws tag:\n{}",
        result
    );
    assert!(
        result.contains("use App\\Exceptions\\BarException;"),
        "should add use import:\n{}",
        result
    );
    // The generated docblock must be aligned with the method signature.
    // Each docblock line should start with exactly the same indentation
    // as `public function bar`.
    let expected_fragment =
        "    /**\n     * @throws BarException\n     */\n    public function bar(): void {";
    assert!(
        result.contains(expected_fragment),
        "docblock should be aligned with the method signature:\n{}",
        result
    );
}

// ── Standalone function ─────────────────────────────────────────────────────

#[test]
fn works_with_standalone_function() {
    let backend = create_test_backend();
    let uri = "file:///test.php";
    let content = r#"<?php
/**
 * Do things.
 */
function doThings(): void {
    throw new \App\Exceptions\ThingException();
}
"#;
    backend.update_ast(uri, content);

    inject_phpstan_diag(
        &backend,
        uri,
        5,
        "Function doThings() throws checked exception App\\Exceptions\\ThingException but it's missing from the PHPDoc @throws tag.",
        "missingType.checkedException",
    );

    let actions = get_code_actions(&backend, uri, content, 5, 10);
    let action = find_add_throws_action(&actions).expect("should offer Add @throws action");

    let edits = extract_edits(action);
    let result = apply_edits(content, &edits);

    assert!(
        result.contains("@throws ThingException"),
        "should insert @throws tag:\n{}",
        result
    );
}

// ── Does not duplicate existing @throws ─────────────────────────────────────

#[test]
fn no_action_when_already_documented() {
    let backend = create_test_backend();
    let uri = "file:///test.php";
    let content = r#"<?php
namespace App\Controllers;

use App\Exceptions\BarException;

class FooController {
    /**
     * @throws BarException
     */
    public function bar(): void {
        throw new BarException();
    }
}
"#;
    backend.update_ast(uri, content);

    inject_phpstan_diag(
        &backend,
        uri,
        10,
        "Method App\\Controllers\\FooController::bar() throws checked exception App\\Exceptions\\BarException but it's missing from the PHPDoc @throws tag.",
        "missingType.checkedException",
    );

    let actions = get_code_actions(&backend, uri, content, 10, 10);
    let action = find_add_throws_action(&actions);
    assert!(
        action.is_none(),
        "should NOT offer action when @throws already documented"
    );
}

// ── Ignores non-matching diagnostics ────────────────────────────────────────

#[test]
fn ignores_other_phpstan_identifiers() {
    let backend = create_test_backend();
    let uri = "file:///test.php";
    let content = r#"<?php
class Foo {
    /**
     * Summary.
     */
    public function bar(): void {
        $x = 1;
    }
}
"#;
    backend.update_ast(uri, content);

    inject_phpstan_diag(
        &backend,
        uri,
        6,
        "Some other PHPStan error.",
        "return.unusedType",
    );

    let actions = get_code_actions(&backend, uri, content, 6, 10);
    let action = find_add_throws_action(&actions);
    assert!(
        action.is_none(),
        "should NOT offer action for non-checkedException identifiers"
    );
}

// ── Single-line docblock ────────────────────────────────────────────────────

#[test]
fn expands_single_line_docblock() {
    let backend = create_test_backend();
    let uri = "file:///test.php";
    let content = r#"<?php
namespace App\Controllers;

use App\Exceptions\BarException;

class FooController {
    /** Do something. */
    public function bar(): void {
        throw new BarException();
    }
}
"#;
    backend.update_ast(uri, content);

    inject_phpstan_diag(
        &backend,
        uri,
        8,
        "Method App\\Controllers\\FooController::bar() throws checked exception App\\Exceptions\\BarException but it's missing from the PHPDoc @throws tag.",
        "missingType.checkedException",
    );

    let actions = get_code_actions(&backend, uri, content, 8, 10);
    let action = find_add_throws_action(&actions).expect("should offer Add @throws action");

    let edits = extract_edits(action);
    let result = apply_edits(content, &edits);

    assert!(
        result.contains("@throws BarException"),
        "should insert @throws tag:\n{}",
        result
    );
    assert!(
        result.contains("Do something."),
        "should preserve summary:\n{}",
        result
    );
}

// ── Docblock with existing @throws for different exception ──────────────────

#[test]
fn appends_second_throws_tag() {
    let backend = create_test_backend();
    let uri = "file:///test.php";
    let content = r#"<?php
namespace App\Controllers;

use App\Exceptions\FooException;
use App\Exceptions\BarException;

class FooController {
    /**
     * Do something.
     *
     * @throws FooException
     */
    public function bar(): void {
        throw new FooException();
        throw new BarException();
    }
}
"#;
    backend.update_ast(uri, content);

    inject_phpstan_diag(
        &backend,
        uri,
        14,
        "Method App\\Controllers\\FooController::bar() throws checked exception App\\Exceptions\\BarException but it's missing from the PHPDoc @throws tag.",
        "missingType.checkedException",
    );

    let actions = get_code_actions(&backend, uri, content, 14, 10);
    let action = find_add_throws_action(&actions).expect("should offer Add @throws action");

    let edits = extract_edits(action);
    let result = apply_edits(content, &edits);

    assert!(
        result.contains("@throws FooException"),
        "should keep existing @throws:\n{}",
        result
    );
    assert!(
        result.contains("@throws BarException"),
        "should add new @throws:\n{}",
        result
    );
}
