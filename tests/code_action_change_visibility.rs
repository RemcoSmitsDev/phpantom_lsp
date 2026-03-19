//! Integration tests for the "Change visibility" code action.
//!
//! These tests exercise the full pipeline: parsing PHP source, walking
//! the AST to find the visibility modifier under the cursor, and
//! generating the `WorkspaceEdit` that replaces the keyword.

mod common;

use common::create_test_backend;
use tower_lsp::lsp_types::*;

/// Helper: send a code action request at the given line/character and
/// return the list of code actions.
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

/// Find all "Make ..." code actions from a list of actions.
fn find_visibility_actions(actions: &[CodeActionOrCommand]) -> Vec<&CodeAction> {
    actions
        .iter()
        .filter_map(|a| match a {
            CodeActionOrCommand::CodeAction(ca) if ca.title.starts_with("Make ") => Some(ca),
            _ => None,
        })
        .collect()
}

/// Extract the replacement text from a code action's workspace edit.
fn extract_edit_text(action: &CodeAction) -> String {
    let edit = action.edit.as_ref().expect("action should have an edit");
    let changes = edit.changes.as_ref().expect("edit should have changes");
    let edits: Vec<&TextEdit> = changes.values().flat_map(|v| v.iter()).collect();
    assert_eq!(edits.len(), 1, "expected exactly one text edit");
    edits[0].new_text.clone()
}

// ── Public method ───────────────────────────────────────────────────────────

#[test]
fn public_method_offers_protected_and_private() {
    let backend = create_test_backend();
    let uri = "file:///test.php";
    let content = r#"<?php
class Foo {
    public function bar(): void {}
}
"#;
    backend.update_ast(uri, content);

    // Cursor on the `public` keyword (line 2, col 4).
    let actions = get_code_actions(&backend, uri, content, 2, 6);
    let vis_actions = find_visibility_actions(&actions);

    assert_eq!(vis_actions.len(), 2);

    let titles: Vec<&str> = vis_actions.iter().map(|a| a.title.as_str()).collect();
    assert!(titles.contains(&"Make protected"), "titles: {:?}", titles);
    assert!(titles.contains(&"Make private"), "titles: {:?}", titles);
}

#[test]
fn public_method_make_protected_replaces_keyword() {
    let backend = create_test_backend();
    let uri = "file:///test.php";
    let content = r#"<?php
class Foo {
    public function bar(): void {}
}
"#;
    backend.update_ast(uri, content);

    let actions = get_code_actions(&backend, uri, content, 2, 6);
    let vis_actions = find_visibility_actions(&actions);

    let make_protected = vis_actions
        .iter()
        .find(|a| a.title == "Make protected")
        .expect("should have Make protected action");
    assert_eq!(extract_edit_text(make_protected), "protected");
    assert_eq!(
        make_protected.kind,
        Some(CodeActionKind::new("refactor.rewrite"))
    );
}

#[test]
fn public_method_make_private_replaces_keyword() {
    let backend = create_test_backend();
    let uri = "file:///test.php";
    let content = r#"<?php
class Foo {
    public function bar(): void {}
}
"#;
    backend.update_ast(uri, content);

    let actions = get_code_actions(&backend, uri, content, 2, 6);
    let vis_actions = find_visibility_actions(&actions);

    let make_private = vis_actions
        .iter()
        .find(|a| a.title == "Make private")
        .expect("should have Make private action");
    assert_eq!(extract_edit_text(make_private), "private");
}

// ── Protected method ────────────────────────────────────────────────────────

#[test]
fn protected_method_offers_public_and_private() {
    let backend = create_test_backend();
    let uri = "file:///test.php";
    let content = r#"<?php
class Foo {
    protected function bar(): void {}
}
"#;
    backend.update_ast(uri, content);

    let actions = get_code_actions(&backend, uri, content, 2, 6);
    let vis_actions = find_visibility_actions(&actions);

    assert_eq!(vis_actions.len(), 2);
    let titles: Vec<&str> = vis_actions.iter().map(|a| a.title.as_str()).collect();
    assert!(titles.contains(&"Make public"));
    assert!(titles.contains(&"Make private"));
}

// ── Private method ──────────────────────────────────────────────────────────

#[test]
fn private_method_offers_public_and_protected() {
    let backend = create_test_backend();
    let uri = "file:///test.php";
    let content = r#"<?php
class Foo {
    private function bar(): void {}
}
"#;
    backend.update_ast(uri, content);

    let actions = get_code_actions(&backend, uri, content, 2, 6);
    let vis_actions = find_visibility_actions(&actions);

    assert_eq!(vis_actions.len(), 2);
    let titles: Vec<&str> = vis_actions.iter().map(|a| a.title.as_str()).collect();
    assert!(titles.contains(&"Make public"));
    assert!(titles.contains(&"Make protected"));
}

// ── Property ────────────────────────────────────────────────────────────────

#[test]
fn property_offers_visibility_change() {
    let backend = create_test_backend();
    let uri = "file:///test.php";
    let content = r#"<?php
class Foo {
    protected string $bar = '';
}
"#;
    backend.update_ast(uri, content);

    let actions = get_code_actions(&backend, uri, content, 2, 6);
    let vis_actions = find_visibility_actions(&actions);

    assert_eq!(vis_actions.len(), 2);
    let titles: Vec<&str> = vis_actions.iter().map(|a| a.title.as_str()).collect();
    assert!(titles.contains(&"Make public"));
    assert!(titles.contains(&"Make private"));
}

// ── Constant ────────────────────────────────────────────────────────────────

#[test]
fn constant_offers_visibility_change() {
    let backend = create_test_backend();
    let uri = "file:///test.php";
    let content = r#"<?php
class Foo {
    private const BAR = 42;
}
"#;
    backend.update_ast(uri, content);

    let actions = get_code_actions(&backend, uri, content, 2, 6);
    let vis_actions = find_visibility_actions(&actions);

    assert_eq!(vis_actions.len(), 2);
    let titles: Vec<&str> = vis_actions.iter().map(|a| a.title.as_str()).collect();
    assert!(titles.contains(&"Make public"));
    assert!(titles.contains(&"Make protected"));
}

// ── Promoted constructor parameter ──────────────────────────────────────────

#[test]
fn promoted_param_offers_visibility_change() {
    let backend = create_test_backend();
    let uri = "file:///test.php";
    let content = r#"<?php
class User {
    public function __construct(
        private string $name,
        protected int $age,
    ) {}
}
"#;
    backend.update_ast(uri, content);

    // Cursor on `private` of promoted $name param (line 3).
    let actions = get_code_actions(&backend, uri, content, 3, 10);
    let vis_actions = find_visibility_actions(&actions);

    assert_eq!(vis_actions.len(), 2);
    let titles: Vec<&str> = vis_actions.iter().map(|a| a.title.as_str()).collect();
    assert!(titles.contains(&"Make public"), "titles: {:?}", titles);
    assert!(titles.contains(&"Make protected"), "titles: {:?}", titles);
}

#[test]
fn promoted_param_edit_replaces_correct_keyword() {
    let backend = create_test_backend();
    let uri = "file:///test.php";
    let content = r#"<?php
class User {
    public function __construct(
        private string $name,
    ) {}
}
"#;
    backend.update_ast(uri, content);

    let actions = get_code_actions(&backend, uri, content, 3, 10);
    let vis_actions = find_visibility_actions(&actions);

    let make_public = vis_actions
        .iter()
        .find(|a| a.title == "Make public")
        .expect("should have Make public");

    let edit = make_public.edit.as_ref().unwrap();
    let changes = edit.changes.as_ref().unwrap();
    let edits: Vec<&TextEdit> = changes.values().flat_map(|v| v.iter()).collect();
    assert_eq!(edits.len(), 1);
    assert_eq!(edits[0].new_text, "public");

    // The edit range should only cover the `private` keyword, not the
    // method-level `public`.
    let range = &edits[0].range;
    let keyword_in_source =
        &content[line_col_to_offset(content, range.start.line, range.start.character)
            ..line_col_to_offset(content, range.end.line, range.end.character)];
    assert_eq!(keyword_in_source, "private");
}

// ── Interface ───────────────────────────────────────────────────────────────

#[test]
fn interface_method_offers_visibility_change() {
    let backend = create_test_backend();
    let uri = "file:///test.php";
    let content = r#"<?php
interface Renderable {
    public function render(): string;
}
"#;
    backend.update_ast(uri, content);

    let actions = get_code_actions(&backend, uri, content, 2, 6);
    let vis_actions = find_visibility_actions(&actions);

    // Interfaces only have public methods, but the action still offers alternatives.
    assert_eq!(vis_actions.len(), 2);
}

// ── Trait ────────────────────────────────────────────────────────────────────

#[test]
fn trait_method_offers_visibility_change() {
    let backend = create_test_backend();
    let uri = "file:///test.php";
    let content = r#"<?php
trait Loggable {
    protected function log(string $msg): void {}
}
"#;
    backend.update_ast(uri, content);

    let actions = get_code_actions(&backend, uri, content, 2, 6);
    let vis_actions = find_visibility_actions(&actions);

    assert_eq!(vis_actions.len(), 2);
    let titles: Vec<&str> = vis_actions.iter().map(|a| a.title.as_str()).collect();
    assert!(titles.contains(&"Make public"));
    assert!(titles.contains(&"Make private"));
}

// ── Enum ────────────────────────────────────────────────────────────────────

#[test]
fn enum_method_offers_visibility_change() {
    let backend = create_test_backend();
    let uri = "file:///test.php";
    let content = r#"<?php
enum Color {
    case Red;
    case Green;

    public function label(): string {
        return match($this) {
            self::Red => 'red',
            self::Green => 'green',
        };
    }
}
"#;
    backend.update_ast(uri, content);

    let actions = get_code_actions(&backend, uri, content, 5, 6);
    let vis_actions = find_visibility_actions(&actions);

    assert_eq!(vis_actions.len(), 2);
}

// ── Namespace ───────────────────────────────────────────────────────────────

#[test]
fn works_inside_namespace() {
    let backend = create_test_backend();
    let uri = "file:///test.php";
    let content = r#"<?php
namespace App\Models;

class User {
    private string $email;
}
"#;
    backend.update_ast(uri, content);

    let actions = get_code_actions(&backend, uri, content, 4, 6);
    let vis_actions = find_visibility_actions(&actions);

    assert_eq!(vis_actions.len(), 2);
    let titles: Vec<&str> = vis_actions.iter().map(|a| a.title.as_str()).collect();
    assert!(titles.contains(&"Make public"));
    assert!(titles.contains(&"Make protected"));
}

// ── No visibility ───────────────────────────────────────────────────────────

#[test]
fn no_action_outside_class() {
    let backend = create_test_backend();
    let uri = "file:///test.php";
    let content = r#"<?php
function globalFn(): void {}
"#;
    backend.update_ast(uri, content);

    let actions = get_code_actions(&backend, uri, content, 1, 4);
    let vis_actions = find_visibility_actions(&actions);

    assert!(vis_actions.is_empty());
}

#[test]
fn no_action_on_trait_use() {
    let backend = create_test_backend();
    let uri = "file:///test.php";
    let content = r#"<?php
class Foo {
    use SomeTrait;
}
"#;
    backend.update_ast(uri, content);

    let actions = get_code_actions(&backend, uri, content, 2, 6);
    let vis_actions = find_visibility_actions(&actions);

    assert!(vis_actions.is_empty());
}

#[test]
fn no_action_on_enum_case() {
    let backend = create_test_backend();
    let uri = "file:///test.php";
    let content = r#"<?php
enum Status {
    case Active;
}
"#;
    backend.update_ast(uri, content);

    let actions = get_code_actions(&backend, uri, content, 2, 6);
    let vis_actions = find_visibility_actions(&actions);

    assert!(vis_actions.is_empty());
}

// ── Cursor anywhere in member ───────────────────────────────────────────────

#[test]
fn action_available_with_cursor_on_function_keyword() {
    let backend = create_test_backend();
    let uri = "file:///test.php";
    let content = r#"<?php
class Foo {
    public function bar(): void {}
}
"#;
    backend.update_ast(uri, content);

    // Cursor on `function` keyword.
    let actions = get_code_actions(&backend, uri, content, 2, 14);
    let vis_actions = find_visibility_actions(&actions);

    assert_eq!(vis_actions.len(), 2);
}

#[test]
fn action_available_with_cursor_on_method_name() {
    let backend = create_test_backend();
    let uri = "file:///test.php";
    let content = r#"<?php
class Foo {
    public function bar(): void {}
}
"#;
    backend.update_ast(uri, content);

    // Cursor on `bar` method name.
    let actions = get_code_actions(&backend, uri, content, 2, 21);
    let vis_actions = find_visibility_actions(&actions);

    assert_eq!(vis_actions.len(), 2);
}

#[test]
fn no_action_with_cursor_inside_method_body() {
    let backend = create_test_backend();
    let uri = "file:///test.php";
    let content = r#"<?php
class Foo {
    public function bar(): void {
        echo 'hello';
    }
}
"#;
    backend.update_ast(uri, content);

    // Cursor inside the method body — should NOT offer visibility change.
    let actions = get_code_actions(&backend, uri, content, 3, 10);
    let vis_actions = find_visibility_actions(&actions);

    assert_eq!(vis_actions.len(), 0);
}

// ── Static method ───────────────────────────────────────────────────────────

#[test]
fn static_method_offers_visibility_change() {
    let backend = create_test_backend();
    let uri = "file:///test.php";
    let content = r#"<?php
class Foo {
    public static function create(): self {}
}
"#;
    backend.update_ast(uri, content);

    let actions = get_code_actions(&backend, uri, content, 2, 6);
    let vis_actions = find_visibility_actions(&actions);

    assert_eq!(vis_actions.len(), 2);
    let titles: Vec<&str> = vis_actions.iter().map(|a| a.title.as_str()).collect();
    assert!(titles.contains(&"Make protected"));
    assert!(titles.contains(&"Make private"));
}

// ── Helper ──────────────────────────────────────────────────────────────────

fn line_col_to_offset(content: &str, line: u32, col: u32) -> usize {
    let mut offset = 0;
    for (i, l) in content.lines().enumerate() {
        if i == line as usize {
            return offset + col as usize;
        }
        offset += l.len() + 1;
    }
    offset
}
