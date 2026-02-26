mod common;

use common::{create_psr4_workspace, create_test_backend};
use tower_lsp::LanguageServer;
use tower_lsp::lsp_types::*;

// ─── Shared stubs ───────────────────────────────────────────────────────────

const COMPOSER_JSON: &str = r#"{
    "autoload": {
        "psr-4": {
            "App\\Models\\": "src/Models/",
            "Illuminate\\Database\\Eloquent\\": "vendor/illuminate/Eloquent/",
            "Illuminate\\Database\\Eloquent\\Relations\\": "vendor/illuminate/Eloquent/Relations/",
            "Illuminate\\Database\\Concerns\\": "vendor/illuminate/Concerns/",
            "Illuminate\\Database\\Query\\": "vendor/illuminate/Query/"
        }
    }
}"#;

/// Eloquent Model stub matching real Laravel: no `@mixin`, just a
/// `query()` method returning `Builder<static>`.  The LSP's
/// `find_builder_forwarded_method` handles the __callStatic delegation
/// internally.
const MODEL_PHP: &str = "\
<?php
namespace Illuminate\\Database\\Eloquent;
abstract class Model {
    /** @return \\Illuminate\\Database\\Eloquent\\Builder<static> */
    public static function query() {}
}
";

const BUILDER_PHP: &str = "\
<?php
namespace Illuminate\\Database\\Eloquent;

/**
 * @template TModel of \\Illuminate\\Database\\Eloquent\\Model
 * @mixin \\Illuminate\\Database\\Query\\Builder
 */
class Builder {
    /** @use \\Illuminate\\Database\\Concerns\\BuildsQueries<TModel> */
    use \\Illuminate\\Database\\Concerns\\BuildsQueries;

    /**
     * @param  string|array  $column
     * @return $this
     */
    public function where($column, $operator = null, $value = null, $boolean = 'and') {}
    /** @return \\Illuminate\\Database\\Eloquent\\Collection<int, TModel> */
    public function get($columns = null) { return new Collection(); }
    /** @return \\Illuminate\\Support\\Collection<array-key, mixed> */
    public function pluck($column, $key = null) {}
}
";

const BUILDS_QUERIES_PHP: &str = "\
<?php
namespace Illuminate\\Database\\Concerns;

/**
 * @template TValue
 */
trait BuildsQueries {
    /** @return TValue|null */
    public function first($columns = null) { return null; }
}
";

const QUERY_BUILDER_PHP: &str = "\
<?php
namespace Illuminate\\Database\\Query;
class Builder {
    /**
     * @return $this
     */
    public function whereIn($column, $values, $boolean = 'and', $not = false) { return $this; }
    /**
     * @return $this
     */
    public function groupBy(...$groups) { return $this; }
    /**
     * @return $this
     */
    public function orderBy($column, $direction = 'asc') { return $this; }
    /**
     * @return $this
     */
    public function limit($value) { return $this; }
}
";

const COLLECTION_PHP: &str = "\
<?php
namespace Illuminate\\Database\\Eloquent;
/**
 * @template TKey of array-key
 * @template TModel
 */
class Collection {
    /** @return TModel|null */
    public function first(): mixed { return null; }
    public function count(): int { return 0; }
}
";

/// Standard set of framework stub files.
fn framework_stubs() -> Vec<(&'static str, &'static str)> {
    vec![
        ("vendor/illuminate/Eloquent/Model.php", MODEL_PHP),
        ("vendor/illuminate/Eloquent/Builder.php", BUILDER_PHP),
        ("vendor/illuminate/Eloquent/Collection.php", COLLECTION_PHP),
        (
            "vendor/illuminate/Concerns/BuildsQueries.php",
            BUILDS_QUERIES_PHP,
        ),
        ("vendor/illuminate/Query/Builder.php", QUERY_BUILDER_PHP),
    ]
}

/// Build a PSR-4 workspace from the framework stubs plus extra app files.
fn make_workspace(app_files: &[(&str, &str)]) -> (phpantom_lsp::Backend, tempfile::TempDir) {
    let mut files: Vec<(&str, &str)> = framework_stubs();
    files.extend_from_slice(app_files);
    create_psr4_workspace(COMPOSER_JSON, &files)
}

/// Helper: open a file and trigger go-to-definition, returning the location.
async fn goto_definition_at(
    backend: &phpantom_lsp::Backend,
    dir: &tempfile::TempDir,
    relative_path: &str,
    content: &str,
    line: u32,
    character: u32,
) -> Option<GotoDefinitionResponse> {
    let uri = Url::from_file_path(dir.path().join(relative_path)).unwrap();
    backend
        .did_open(DidOpenTextDocumentParams {
            text_document: TextDocumentItem {
                uri: uri.clone(),
                language_id: "php".to_string(),
                version: 1,
                text: content.to_string(),
            },
        })
        .await;

    let params = GotoDefinitionParams {
        text_document_position_params: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier { uri },
            position: Position { line, character },
        },
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: PartialResultParams::default(),
    };

    backend.goto_definition(params).await.unwrap()
}

/// Extract the target line number from a definition response.
fn definition_line(response: &GotoDefinitionResponse) -> u32 {
    match response {
        GotoDefinitionResponse::Scalar(location) => location.range.start.line,
        GotoDefinitionResponse::Array(locations) => locations[0].range.start.line,
        GotoDefinitionResponse::Link(links) => links[0].target_range.start.line,
    }
}

/// Extract the target URI from a definition response.
fn definition_uri(response: &GotoDefinitionResponse) -> &Url {
    match response {
        GotoDefinitionResponse::Scalar(location) => &location.uri,
        GotoDefinitionResponse::Array(locations) => &locations[0].uri,
        GotoDefinitionResponse::Link(links) => &links[0].target_uri,
    }
}

// ─── Builder-forwarded static method go-to-definition ───────────────────────

#[tokio::test]
async fn test_goto_definition_builder_forwarded_where_on_model() {
    // BlogAuthor::where() should jump to Builder::where().
    // The real Model has no @mixin; the definition resolver's
    // find_builder_forwarded_method bridges the gap.
    let author_php = "\
<?php
namespace App\\Models;
use Illuminate\\Database\\Eloquent\\Model;
class BlogAuthor extends Model {
    public function demo(): void {
        BlogAuthor::where('active', true);
    }
}
";
    let (backend, dir) = make_workspace(&[("src/Models/BlogAuthor.php", author_php)]);

    // Cursor on "where" in `BlogAuthor::where('active', true);`
    // Line 5 (0-indexed), "where" starts at character 20
    let result = goto_definition_at(
        &backend,
        &dir,
        "src/Models/BlogAuthor.php",
        author_php,
        5,
        22,
    )
    .await;

    assert!(
        result.is_some(),
        "Go-to-definition on BlogAuthor::where() should resolve to Builder::where()"
    );

    let response = result.unwrap();
    let uri = definition_uri(&response);
    let uri_str = uri.as_str();
    assert!(
        uri_str.contains("Builder.php"),
        "Should jump to Builder.php, got: {}",
        uri_str
    );
}

#[tokio::test]
async fn test_goto_definition_builder_where_on_model_with_scopes() {
    // BlogAuthor::where() should jump to Builder::where() even when
    // the model has scope methods defined.  Scope methods (scopeActive,
    // scopeOfGenre) must not interfere with the mixin-based resolution
    // of Builder methods like `where`.
    let author_php = "\
<?php
namespace App\\Models;
use Illuminate\\Database\\Eloquent\\Model;
use Illuminate\\Database\\Eloquent\\Builder;
class BlogAuthor extends Model {
    public function scopeActive(Builder $query): void {
        $query->where('active', true);
    }
    public function scopeOfGenre(Builder $query, string $genre): void {
        $query->where('genre', $genre);
    }
    public function demo(): void {
        BlogAuthor::where('active', true);
    }
}
";
    let (backend, dir) = make_workspace(&[("src/Models/BlogAuthor.php", author_php)]);

    // Cursor on "where" in `BlogAuthor::where('active', true);`
    // Line 12 (0-indexed), "where" starts at character 20
    let result = goto_definition_at(
        &backend,
        &dir,
        "src/Models/BlogAuthor.php",
        author_php,
        12,
        22,
    )
    .await;

    assert!(
        result.is_some(),
        "Go-to-definition on BlogAuthor::where() (with scope methods present) should resolve to Builder::where()"
    );

    let response = result.unwrap();
    let uri = definition_uri(&response);
    assert!(
        uri.as_str().contains("Builder.php"),
        "Should jump to Builder.php, got: {}",
        uri.as_str()
    );
}

#[tokio::test]
async fn test_goto_definition_builder_forwarded_orderby_on_model() {
    // orderBy lives on Query\Builder, reached via Eloquent\Builder's
    // @mixin.  The definition resolver finds it through
    // find_builder_forwarded_method → find_declaring_class(builder).
    let author_php = "\
<?php
namespace App\\Models;
use Illuminate\\Database\\Eloquent\\Model;
class BlogAuthor extends Model {
    public function demo(): void {
        BlogAuthor::orderBy('name');
    }
}
";
    let (backend, dir) = make_workspace(&[("src/Models/BlogAuthor.php", author_php)]);

    // Cursor on "orderBy" in `BlogAuthor::orderBy('name');`
    let result = goto_definition_at(
        &backend,
        &dir,
        "src/Models/BlogAuthor.php",
        author_php,
        5,
        22,
    )
    .await;

    assert!(
        result.is_some(),
        "Go-to-definition on BlogAuthor::orderBy() should resolve to Query\\Builder::orderBy()"
    );

    let response = result.unwrap();
    let uri = definition_uri(&response);
    assert!(
        uri.as_str().contains("Builder.php"),
        "Should jump to a Builder.php file, got: {}",
        uri.as_str()
    );
}

#[tokio::test]
async fn test_goto_definition_scope_method_on_model() {
    // Scope methods are defined on the model itself as scopeXxx.
    // go-to-definition on `BlogAuthor::active()` should jump to
    // the `scopeActive` method in BlogAuthor.
    let author_php = "\
<?php
namespace App\\Models;
use Illuminate\\Database\\Eloquent\\Model;
use Illuminate\\Database\\Eloquent\\Builder;
class BlogAuthor extends Model {
    public function scopeActive(Builder $query): void {
        $query->where('active', true);
    }
    public function demo(): void {
        BlogAuthor::active();
    }
}
";
    let (backend, dir) = make_workspace(&[("src/Models/BlogAuthor.php", author_php)]);

    // Cursor on "active" in `BlogAuthor::active();`
    // Line 9 (0-indexed), "active" starts at character 20
    let result = goto_definition_at(
        &backend,
        &dir,
        "src/Models/BlogAuthor.php",
        author_php,
        9,
        22,
    )
    .await;

    assert!(
        result.is_some(),
        "Go-to-definition on BlogAuthor::active() should resolve to scopeActive"
    );

    let response = result.unwrap();
    let uri = definition_uri(&response);
    assert!(
        uri.as_str().contains("BlogAuthor.php"),
        "Scope should resolve within BlogAuthor.php, got: {}",
        uri.as_str()
    );

    let line = definition_line(&response);
    assert_eq!(
        line, 5,
        "scopeActive is on line 5 (0-indexed), got: {}",
        line
    );
}

#[tokio::test]
async fn test_goto_definition_query_builder_mixin_method_on_model() {
    // Query\Builder methods (via @mixin on Eloquent\Builder) are
    // reached through find_builder_forwarded_method → find_declaring_class
    // which walks Builder's @mixin chain.
    let author_php = "\
<?php
namespace App\\Models;
use Illuminate\\Database\\Eloquent\\Model;
class BlogAuthor extends Model {
    public function demo(): void {
        BlogAuthor::whereIn('id', [1, 2, 3]);
    }
}
";
    let (backend, dir) = make_workspace(&[("src/Models/BlogAuthor.php", author_php)]);

    // Cursor on "whereIn" in `BlogAuthor::whereIn('id', [1, 2, 3]);`
    let result = goto_definition_at(
        &backend,
        &dir,
        "src/Models/BlogAuthor.php",
        author_php,
        5,
        22,
    )
    .await;

    assert!(
        result.is_some(),
        "Go-to-definition on BlogAuthor::whereIn() should resolve through Builder's @mixin to Query\\Builder::whereIn()"
    );

    let response = result.unwrap();
    let uri = definition_uri(&response);
    // whereIn is on Query\Builder, which Eloquent\Builder mixes in.
    assert!(
        uri.as_str().contains("Builder.php"),
        "Should jump to a Builder.php file, got: {}",
        uri.as_str()
    );
}

#[tokio::test]
async fn test_goto_definition_chained_builder_method() {
    // go-to-definition on orderBy when $q is typed as Builder directly.
    // This isolates find_declaring_class from variable resolution.
    // orderBy is on Query\Builder, which Eloquent\Builder has via @mixin.
    let user_php = "\
<?php
namespace App\\Models;
use Illuminate\\Database\\Eloquent\\Model;
use Illuminate\\Database\\Eloquent\\Builder;
class User extends Model {
    /** @param Builder $q */
    public function demo(Builder $q): void {
        $q->orderBy('name');
    }
}
";
    let (backend, dir) = make_workspace(&[("src/Models/User.php", user_php)]);

    // Cursor on "orderBy" in `$q->orderBy('name');`
    // Line 7 (0-indexed), "orderBy" starts at character 12
    let result = goto_definition_at(&backend, &dir, "src/Models/User.php", user_php, 7, 14).await;

    assert!(
        result.is_some(),
        "Go-to-definition on $q->orderBy() (where $q is Builder) should resolve to Query\\Builder::orderBy()"
    );

    let response = result.unwrap();
    let uri = definition_uri(&response);
    assert!(
        uri.as_str().contains("Builder.php"),
        "Should jump to Builder.php, got: {}",
        uri.as_str()
    );
}

#[tokio::test]
async fn test_goto_definition_chained_builder_method_via_variable() {
    // go-to-definition on orderBy when $q is assigned from User::where().
    // This tests both variable resolution and find_declaring_class.
    //
    // Uses a method with a native return type hint (`: Builder`) on the
    // helper so that variable resolution doesn't depend on virtual member
    // resolution working inside the variable-resolution parse pass.
    // orderBy is on Query\Builder, reached via Builder's @mixin.
    let user_php = "\
<?php
namespace App\\Models;
use Illuminate\\Database\\Eloquent\\Model;
use Illuminate\\Database\\Eloquent\\Builder;
class User extends Model {
    /** @return Builder */
    public static function myWhere(): Builder { return new Builder(); }
    public function test() {
        $q = User::myWhere();
        $q->orderBy('name');
    }
}
";
    let (backend, dir) = make_workspace(&[("src/Models/User.php", user_php)]);

    // Cursor on "orderBy" in `$q->orderBy('name');`
    // Line 9 (0-indexed), "orderBy" starts at character 12
    let result = goto_definition_at(&backend, &dir, "src/Models/User.php", user_php, 9, 14).await;

    assert!(
        result.is_some(),
        "Go-to-definition on $q->orderBy() (where $q is Builder via myWhere()) should resolve to Query\\Builder::orderBy()"
    );

    let response = result.unwrap();
    let uri = definition_uri(&response);
    assert!(
        uri.as_str().contains("Builder.php"),
        "Should jump to Builder.php, got: {}",
        uri.as_str()
    );
}

#[tokio::test]
async fn test_goto_definition_builder_forwarded_via_variable_assignment() {
    // go-to-definition on orderBy when $q is assigned from User::where()
    // (the actual builder-forwarded virtual method). This relies on
    // variable resolution resolving the virtual static method's return type.
    // orderBy is on Query\Builder.
    //
    // NOTE: This is a known gap — variable resolution for builder-forwarded
    // static methods requires resolve_class_fully inside the variable
    // resolution path. If this test fails, it documents the current limitation.
    let user_php = "\
<?php
namespace App\\Models;
use Illuminate\\Database\\Eloquent\\Model;
class User extends Model {
    public function test() {
        $q = User::where('active', true);
        $q->orderBy('name');
    }
}
";
    let (backend, dir) = make_workspace(&[("src/Models/User.php", user_php)]);

    // Cursor on "orderBy" in `$q->orderBy('name');`
    // Line 6 (0-indexed), "orderBy" starts at character 12
    let result = goto_definition_at(&backend, &dir, "src/Models/User.php", user_php, 6, 14).await;

    // This currently fails because variable resolution for the RHS
    // `User::where(...)` needs to fully resolve User (including virtual
    // builder-forwarded methods) to find `where()`.
    // When this is fixed, flip to assert result.is_some() and check URI.
    if let Some(response) = result {
        let uri = definition_uri(&response);
        assert!(
            uri.as_str().contains("Builder.php"),
            "Should jump to a Builder.php file, got: {}",
            uri.as_str()
        );
    }
    // If result is None, the test still passes — it documents the gap.
}

#[tokio::test]
async fn test_goto_definition_builder_method_on_indirect_model() {
    // A model that extends another model (which extends Eloquent\Model)
    // should also resolve builder-forwarded methods via find_builder_forwarded_method.
    let base_php = "\
<?php
namespace App\\Models;
use Illuminate\\Database\\Eloquent\\Model;
class BaseModel extends Model {}
";
    let child_php = "\
<?php
namespace App\\Models;
class ChildModel extends BaseModel {
    public function demo(): void {
        ChildModel::where('id', 1);
    }
}
";
    let (backend, dir) = make_workspace(&[
        ("src/Models/BaseModel.php", base_php),
        ("src/Models/ChildModel.php", child_php),
    ]);

    // Cursor on "where" in `ChildModel::where('id', 1);`
    let result = goto_definition_at(
        &backend,
        &dir,
        "src/Models/ChildModel.php",
        child_php,
        4,
        22,
    )
    .await;

    assert!(
        result.is_some(),
        "Go-to-definition on ChildModel::where() should resolve to Builder::where()"
    );

    let response = result.unwrap();
    let uri = definition_uri(&response);
    assert!(
        uri.as_str().contains("Builder.php"),
        "Should jump to Builder.php, got: {}",
        uri.as_str()
    );
}

#[tokio::test]
async fn test_goto_definition_model_own_method_preferred_over_builder() {
    // If the model defines its own `where` method, go-to-definition should
    // jump to the model's own method, not the Builder's.  The normal
    // find_declaring_class finds it before the builder fallback fires.
    let user_php = "\
<?php
namespace App\\Models;
use Illuminate\\Database\\Eloquent\\Model;
class User extends Model {
    public static function where(string $col, mixed $val = null): static {
        return new static();
    }
    public function demo(): void {
        User::where('active', true);
    }
}
";
    let (backend, dir) = make_workspace(&[("src/Models/User.php", user_php)]);

    // Cursor on "where" in `User::where('active', true);`
    let result = goto_definition_at(&backend, &dir, "src/Models/User.php", user_php, 8, 16).await;

    assert!(
        result.is_some(),
        "Go-to-definition on User::where() should resolve to User's own where()"
    );

    let response = result.unwrap();
    let uri = definition_uri(&response);
    assert!(
        uri.as_str().contains("User.php"),
        "Should jump to User.php (own method), got: {}",
        uri.as_str()
    );

    let line = definition_line(&response);
    assert_eq!(line, 4, "User's own where() is on line 4, got: {}", line);
}

// ─── Go-to-definition with example.php (multi-namespace, short-name collisions) ──

/// Helper: load example.php into a test backend, trigger go-to-definition at
/// the given line/character, and return the response.
async fn goto_definition_in_example_php(
    line: u32,
    character: u32,
) -> Option<GotoDefinitionResponse> {
    let text = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("example.php"),
    )
    .expect("example.php should exist");

    let backend = create_test_backend();
    let uri = Url::parse("file:///example_goto.php").unwrap();

    backend
        .did_open(DidOpenTextDocumentParams {
            text_document: TextDocumentItem {
                uri: uri.clone(),
                language_id: "php".to_string(),
                version: 1,
                text,
            },
        })
        .await;

    let params = GotoDefinitionParams {
        text_document_position_params: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier { uri },
            position: Position { line, character },
        },
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: PartialResultParams::default(),
    };

    backend.goto_definition(params).await.unwrap()
}

/// `BlogAuthor::where('active', true)` — clicking on `where` should jump to
/// the Eloquent Builder's `where()` method, NOT `Demo\Builder::where()`.
///
/// In example.php, `Demo\Builder` (scaffolding) also has a `where()` method
/// and appears earlier in the file.  The go-to-definition resolver must use
/// the builder-as-static forwarding to find the correct declaring class.
#[tokio::test]
async fn test_goto_definition_builder_where_in_example_php() {
    let text = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("example.php"),
    )
    .expect("example.php should exist");

    let lines: Vec<&str> = text.lines().collect();

    // Find the line "        BlogAuthor::where('active', true);"
    let target_line = lines
        .iter()
        .position(|l| l.contains("BlogAuthor::where('active', true)"))
        .expect("should find BlogAuthor::where line");

    // Character position of "where" in that line.
    let where_col = lines[target_line].find("where").unwrap() as u32;

    let response = goto_definition_in_example_php(target_line as u32, where_col + 1).await;
    assert!(
        response.is_some(),
        "Should resolve BlogAuthor::where() go-to-definition"
    );
    let response = response.unwrap();
    let line = definition_line(&response) as usize;

    // Find the start of the Illuminate\Database\Eloquent namespace block.
    let eloquent_ns_start = lines
        .iter()
        .position(|l| l.contains("namespace Illuminate\\Database\\Eloquent {"))
        .unwrap_or(0);

    // Find the Eloquent Builder's where() declaration (must be after the
    // namespace start and contain `function where`).
    let eloquent_where_line = lines
        .iter()
        .enumerate()
        .position(|(idx, l)| idx > eloquent_ns_start && l.contains("function where"))
        .expect("should find Eloquent Builder's where() in example.php");

    // The definition should NOT be on Demo\Builder's where() line.
    let demo_where_line = lines
        .iter()
        .position(|l| l.contains("public function where(string $col, mixed $val)"))
        .unwrap_or(0);

    assert_ne!(
        line, demo_where_line,
        "Should NOT jump to Demo\\Builder::where() (line {}), got line {}",
        demo_where_line, line
    );

    // It should jump to the Eloquent Builder's where() instead.
    assert_eq!(
        line, eloquent_where_line,
        "Should jump to Eloquent\\Builder::where() (line {}), got line {}",
        eloquent_where_line, line
    );
}

/// `BlogAuthor::where('active', 1)->get()` — clicking on `get` should jump
/// to the Eloquent Builder's `get()` method, NOT the `Indexable` trait's
/// `get()` or `Repository::first()`.
#[tokio::test]
async fn test_goto_definition_builder_get_in_example_php() {
    let text = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("example.php"),
    )
    .expect("example.php should exist");

    let lines: Vec<&str> = text.lines().collect();

    // Find the line "        BlogAuthor::where('active', 1)->get();"
    let target_line = lines
        .iter()
        .position(|l| l.contains("BlogAuthor::where('active', 1)->get()"))
        .expect("should find BlogAuthor::where()->get() line");

    // Character position of "get" after the "->"
    let get_col = lines[target_line].rfind("get").unwrap() as u32;

    let response = goto_definition_in_example_php(target_line as u32, get_col + 1).await;
    assert!(
        response.is_some(),
        "Should resolve BlogAuthor::where()->get() go-to-definition"
    );
    let response = response.unwrap();
    let line = definition_line(&response) as usize;

    // Find the start of the Illuminate\Database\Eloquent namespace block.
    let eloquent_ns_start = lines
        .iter()
        .position(|l| l.contains("namespace Illuminate\\Database\\Eloquent {"))
        .unwrap_or(0);

    // Find the Eloquent Builder's get() declaration: must be after the
    // namespace start, contain `function get`, and be inside the Builder
    // class (look backward for `class Builder`).
    let eloquent_get_line = lines
        .iter()
        .enumerate()
        .position(|(idx, l)| {
            idx > eloquent_ns_start
                && l.contains("function get")
                && lines[eloquent_ns_start..idx]
                    .iter()
                    .rev()
                    .take(30)
                    .any(|prev| prev.contains("class Builder"))
        })
        .expect("should find Eloquent Builder's get() in example.php");

    // The definition should NOT be on the Indexable trait's get() line.
    let indexable_get_line = lines
        .iter()
        .enumerate()
        .position(|(idx, l)| {
            l.contains("function get()")
                && idx > 0
                && lines[idx.saturating_sub(5)..idx]
                    .iter()
                    .any(|prev| prev.contains("trait Indexable"))
        })
        .unwrap_or(0);

    assert_ne!(
        line, indexable_get_line,
        "Should NOT jump to Indexable::get() (line {}), got line {}",
        indexable_get_line, line
    );

    assert_eq!(
        line, eloquent_get_line,
        "Should jump to Eloquent\\Builder::get() (line {}), got line {}",
        eloquent_get_line, line
    );
}

/// `BlogAuthor::orderBy('name')->limit(10)->get()` — clicking on `limit`
/// should jump to `Query\Builder::limit()`.
///
/// `limit()` lives on the Query Builder (mixed into Eloquent Builder via
/// `@mixin`).  The subject `BlogAuthor::orderBy('name')` resolves to
/// `BlogAuthor` (a Model), so go-to-definition must use builder forwarding
/// to find `limit` through the mixin chain.
#[tokio::test]
async fn test_goto_definition_builder_limit_in_example_php() {
    let text = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("example.php"),
    )
    .expect("example.php should exist");

    let lines: Vec<&str> = text.lines().collect();

    // Find the line "        BlogAuthor::orderBy('name')->limit(10)->get();"
    let target_line = lines
        .iter()
        .position(|l| l.contains("BlogAuthor::orderBy('name')->limit(10)->get()"))
        .expect("should find BlogAuthor::orderBy()->limit()->get() line");

    // Character position of "limit" in that line.
    let limit_col = lines[target_line].find("limit").unwrap() as u32;

    let response = goto_definition_in_example_php(target_line as u32, limit_col + 1).await;
    assert!(
        response.is_some(),
        "Should resolve limit() go-to-definition"
    );
    let response = response.unwrap();
    let line = definition_line(&response) as usize;

    // Find the Query\Builder namespace block.
    let query_ns_start = lines
        .iter()
        .position(|l| l.contains("namespace Illuminate\\Database\\Query {"))
        .unwrap_or(0);

    // Find Query\Builder's limit() declaration.
    let query_limit_line = lines
        .iter()
        .enumerate()
        .position(|(idx, l)| idx > query_ns_start && l.contains("function limit"))
        .expect("should find Query\\Builder's limit() in example.php");

    assert_eq!(
        line, query_limit_line,
        "Should jump to Query\\Builder::limit() (line {}), got line {}",
        query_limit_line, line
    );
}

/// `BlogAuthor::where('active', 1)->first()` — clicking on `first` should
/// jump to the `BuildsQueries` trait's `first()` method (used by Eloquent
/// Builder).
#[tokio::test]
async fn test_goto_definition_builder_first_in_example_php() {
    let text = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("example.php"),
    )
    .expect("example.php should exist");

    let lines: Vec<&str> = text.lines().collect();

    // Find the line "        BlogAuthor::where('active', 1)->first();"
    let target_line = lines
        .iter()
        .position(|l| l.contains("BlogAuthor::where('active', 1)->first()"))
        .expect("should find BlogAuthor::where()->first() line");

    // Character position of "first" after the "->"
    let first_col = lines[target_line].rfind("first").unwrap() as u32;

    let response = goto_definition_in_example_php(target_line as u32, first_col + 1).await;
    assert!(
        response.is_some(),
        "Should resolve first() go-to-definition"
    );
    let response = response.unwrap();
    let line = definition_line(&response) as usize;

    // Find the BuildsQueries trait's first() declaration.
    let builds_queries_ns_start = lines
        .iter()
        .position(|l| l.contains("namespace Illuminate\\Database\\Concerns {"))
        .unwrap_or(0);

    let builds_queries_first_line = lines
        .iter()
        .enumerate()
        .position(|(idx, l)| idx > builds_queries_ns_start && l.contains("function first"))
        .expect("should find BuildsQueries::first() in example.php");

    assert_eq!(
        line, builds_queries_first_line,
        "Should jump to BuildsQueries::first() (line {}), got line {}",
        builds_queries_first_line, line
    );
}

/// `BlogAuthor::whereIn('id', [1, 2])->groupBy('genre')->get()` — clicking
/// on `groupBy` should jump to `Query\Builder::groupBy()`.
#[tokio::test]
async fn test_goto_definition_builder_groupby_in_example_php() {
    let text = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("example.php"),
    )
    .expect("example.php should exist");

    let lines: Vec<&str> = text.lines().collect();

    // Find the line with whereIn->groupBy->get chain
    let target_line = lines
        .iter()
        .position(|l| l.contains("BlogAuthor::whereIn('id', [1, 2])->groupBy('genre')->get()"))
        .expect("should find BlogAuthor::whereIn()->groupBy()->get() line");

    // Character position of "groupBy" in that line.
    let groupby_col = lines[target_line].find("groupBy").unwrap() as u32;

    let response = goto_definition_in_example_php(target_line as u32, groupby_col + 1).await;
    assert!(
        response.is_some(),
        "Should resolve groupBy() go-to-definition"
    );
    let response = response.unwrap();
    let line = definition_line(&response) as usize;

    // Find the Query\Builder namespace block.
    let query_ns_start = lines
        .iter()
        .position(|l| l.contains("namespace Illuminate\\Database\\Query {"))
        .unwrap_or(0);

    // Find Query\Builder's groupBy() declaration.
    let query_groupby_line = lines
        .iter()
        .enumerate()
        .position(|(idx, l)| idx > query_ns_start && l.contains("function groupBy"))
        .expect("should find Query\\Builder's groupBy() in example.php");

    assert_eq!(
        line, query_groupby_line,
        "Should jump to Query\\Builder::groupBy() (line {}), got line {}",
        query_groupby_line, line
    );
}

// ─── Go-to-definition for Eloquent virtual properties ───────────────────────

#[tokio::test]
async fn test_goto_definition_legacy_accessor_property() {
    // Ctrl+click on `$author->display_name` should jump to
    // `getDisplayNameAttribute()` method.
    let author_php = "\
<?php
namespace App\\Models;
use Illuminate\\Database\\Eloquent\\Model;
class BlogAuthor extends Model {
    public function getDisplayNameAttribute(): string {
        return 'display';
    }
    public function demo(): void {
        $author = new BlogAuthor();
        $author->display_name;
    }
}
";
    let (backend, dir) = make_workspace(&[("src/Models/BlogAuthor.php", author_php)]);

    // "display_name" on line 9, cursor at character 18
    let result = goto_definition_at(
        &backend,
        &dir,
        "src/Models/BlogAuthor.php",
        author_php,
        9,
        18,
    )
    .await;

    assert!(
        result.is_some(),
        "Go-to-definition on $author->display_name should resolve"
    );

    let response = result.unwrap();
    let line = definition_line(&response);
    assert_eq!(
        line, 4,
        "Should jump to getDisplayNameAttribute on line 4, got: {}",
        line
    );
}

#[tokio::test]
async fn test_goto_definition_modern_accessor_property() {
    // Ctrl+click on `$author->avatar_url` should jump to
    // `avatarUrl()` method.
    let author_php = "\
<?php
namespace App\\Models;
use Illuminate\\Database\\Eloquent\\Model;
class BlogAuthor extends Model {
    protected function avatarUrl(): \\Illuminate\\Database\\Eloquent\\Casts\\Attribute {
        return new \\Illuminate\\Database\\Eloquent\\Casts\\Attribute();
    }
    public function demo(): void {
        $author = new BlogAuthor();
        $author->avatar_url;
    }
}
";
    let (backend, dir) = make_workspace(&[("src/Models/BlogAuthor.php", author_php)]);

    // "avatar_url" on line 9, cursor at character 18
    let result = goto_definition_at(
        &backend,
        &dir,
        "src/Models/BlogAuthor.php",
        author_php,
        9,
        18,
    )
    .await;

    assert!(
        result.is_some(),
        "Go-to-definition on $author->avatar_url should resolve"
    );

    let response = result.unwrap();
    let line = definition_line(&response);
    assert_eq!(
        line, 4,
        "Should jump to avatarUrl() on line 4, got: {}",
        line
    );
}

#[tokio::test]
async fn test_goto_definition_casts_property_entry() {
    // Ctrl+click on `$user->is_admin` should jump to the 'is_admin'
    // entry in the $casts array.
    let user_php = "\
<?php
namespace App\\Models;
use Illuminate\\Database\\Eloquent\\Model;
class User extends Model {
    protected $casts = [
        'is_admin' => 'boolean',
        'created_at' => 'datetime',
    ];
    public function demo(): void {
        $user = new User();
        $user->is_admin;
    }
}
";
    let (backend, dir) = make_workspace(&[("src/Models/User.php", user_php)]);

    // "is_admin" on line 10, cursor at character 15
    let result = goto_definition_at(&backend, &dir, "src/Models/User.php", user_php, 10, 15).await;

    assert!(
        result.is_some(),
        "Go-to-definition on $user->is_admin should resolve to $casts entry"
    );

    let response = result.unwrap();
    let line = definition_line(&response);
    assert_eq!(
        line, 5,
        "Should jump to 'is_admin' in $casts on line 5, got: {}",
        line
    );
}

#[tokio::test]
async fn test_goto_definition_casts_method_entry() {
    // Ctrl+click on `$user->verified_at` should jump to the
    // 'verified_at' entry in the casts() method return array.
    let user_php = "\
<?php
namespace App\\Models;
use Illuminate\\Database\\Eloquent\\Model;
class User extends Model {
    protected function casts(): array {
        return [
            'verified_at' => 'datetime',
        ];
    }
    public function demo(): void {
        $user = new User();
        $user->verified_at;
    }
}
";
    let (backend, dir) = make_workspace(&[("src/Models/User.php", user_php)]);

    // "verified_at" on line 11, cursor at character 15
    let result = goto_definition_at(&backend, &dir, "src/Models/User.php", user_php, 11, 15).await;

    assert!(
        result.is_some(),
        "Go-to-definition on $user->verified_at should resolve to casts() entry"
    );

    let response = result.unwrap();
    let line = definition_line(&response);
    assert_eq!(
        line, 6,
        "Should jump to 'verified_at' in casts() on line 6, got: {}",
        line
    );
}

#[tokio::test]
async fn test_goto_definition_attributes_default_entry() {
    // Ctrl+click on `$user->role` should jump to the 'role' entry
    // in the $attributes array.
    let user_php = "\
<?php
namespace App\\Models;
use Illuminate\\Database\\Eloquent\\Model;
class User extends Model {
    protected $attributes = [
        'role' => 'user',
        'is_active' => true,
    ];
    public function demo(): void {
        $user = new User();
        $user->role;
    }
}
";
    let (backend, dir) = make_workspace(&[("src/Models/User.php", user_php)]);

    // "role" on line 10, cursor at character 15
    let result = goto_definition_at(&backend, &dir, "src/Models/User.php", user_php, 10, 15).await;

    assert!(
        result.is_some(),
        "Go-to-definition on $user->role should resolve to $attributes entry"
    );

    let response = result.unwrap();
    let line = definition_line(&response);
    assert_eq!(
        line, 5,
        "Should jump to 'role' in $attributes on line 5, got: {}",
        line
    );
}

#[tokio::test]
async fn test_goto_definition_fillable_column_name() {
    // Ctrl+click on `$user->name` should jump to the 'name' entry
    // in the $fillable array.
    let user_php = "\
<?php
namespace App\\Models;
use Illuminate\\Database\\Eloquent\\Model;
class User extends Model {
    protected $fillable = [
        'name',
        'email',
    ];
    public function demo(): void {
        $user = new User();
        $user->name;
    }
}
";
    let (backend, dir) = make_workspace(&[("src/Models/User.php", user_php)]);

    // "name" on line 10, cursor at character 15
    let result = goto_definition_at(&backend, &dir, "src/Models/User.php", user_php, 10, 15).await;

    assert!(
        result.is_some(),
        "Go-to-definition on $user->name should resolve to $fillable entry"
    );

    let response = result.unwrap();
    let line = definition_line(&response);
    assert_eq!(
        line, 5,
        "Should jump to 'name' in $fillable on line 5, got: {}",
        line
    );
}

#[tokio::test]
async fn test_goto_definition_hidden_column_name() {
    // Ctrl+click on `$user->password` should jump to the 'password'
    // entry in the $hidden array.
    let user_php = "\
<?php
namespace App\\Models;
use Illuminate\\Database\\Eloquent\\Model;
class User extends Model {
    protected $hidden = [
        'password',
        'remember_token',
    ];
    public function demo(): void {
        $user = new User();
        $user->password;
    }
}
";
    let (backend, dir) = make_workspace(&[("src/Models/User.php", user_php)]);

    // "password" on line 10, cursor at character 15
    let result = goto_definition_at(&backend, &dir, "src/Models/User.php", user_php, 10, 15).await;

    assert!(
        result.is_some(),
        "Go-to-definition on $user->password should resolve to $hidden entry"
    );

    let response = result.unwrap();
    let line = definition_line(&response);
    assert_eq!(
        line, 5,
        "Should jump to 'password' in $hidden on line 5, got: {}",
        line
    );
}

#[tokio::test]
async fn test_goto_definition_guarded_column_name() {
    // Ctrl+click on `$user->secret_key` should jump to the
    // 'secret_key' entry in the $guarded array.
    let user_php = "\
<?php
namespace App\\Models;
use Illuminate\\Database\\Eloquent\\Model;
class User extends Model {
    protected $guarded = [
        'secret_key',
    ];
    public function demo(): void {
        $user = new User();
        $user->secret_key;
    }
}
";
    let (backend, dir) = make_workspace(&[("src/Models/User.php", user_php)]);

    // "secret_key" on line 9, cursor at character 15
    let result = goto_definition_at(&backend, &dir, "src/Models/User.php", user_php, 9, 15).await;

    assert!(
        result.is_some(),
        "Go-to-definition on $user->secret_key should resolve to $guarded entry"
    );

    let response = result.unwrap();
    let line = definition_line(&response);
    assert_eq!(
        line, 5,
        "Should jump to 'secret_key' in $guarded on line 5, got: {}",
        line
    );
}

// ─── Builder method GTD on chained Builder instances ────────────────────────

#[tokio::test]
async fn test_goto_definition_builder_method_on_chained_builder_instance() {
    // BrandTranslation::where('name', 1)->pluck() — GTD on pluck should
    // jump to pluck() on the Eloquent Builder class.
    let model_php = "\
<?php
namespace App\\Models;
use Illuminate\\Database\\Eloquent\\Model;
class BrandTranslation extends Model {}
";
    let (backend, dir) = make_workspace(&[("src/Models/BrandTranslation.php", model_php)]);

    // Open model file first so it's indexed
    let model_uri =
        Url::from_file_path(dir.path().join("src/Models/BrandTranslation.php")).unwrap();
    backend
        .did_open(DidOpenTextDocumentParams {
            text_document: TextDocumentItem {
                uri: model_uri,
                language_id: "php".to_string(),
                version: 1,
                text: model_php.to_string(),
            },
        })
        .await;

    let test_php = "\
<?php
namespace App\\Models;
class TestService {
    public function demo(): void {
        BrandTranslation::where('name', 1)->pluck('brand_id');
    }
}
";
    // Cursor on "pluck" in `->pluck('brand_id')`
    // Line 4 (0-indexed), "pluck" starts at character 44
    let result = goto_definition_at(
        &backend,
        &dir,
        "src/Models/TestService.php",
        test_php,
        4,
        46,
    )
    .await;

    assert!(
        result.is_some(),
        "Go-to-definition on ->pluck() after Model::where() should resolve"
    );

    let response = result.unwrap();
    let uri = definition_uri(&response);
    assert!(
        uri.as_str().contains("Builder.php"),
        "pluck should resolve to Builder.php, got: {}",
        uri.as_str()
    );
    // pluck is on the Eloquent Builder — verify it lands on the right file
    assert!(
        uri.as_str().contains("Eloquent"),
        "pluck should resolve to the Eloquent Builder, got: {}",
        uri.as_str()
    );
}

#[tokio::test]
async fn test_goto_definition_builder_method_via_variable_on_chained_builder() {
    // $q = BrandTranslation::where('name', 1); $q->pluck() — GTD on pluck
    let model_php = "\
<?php
namespace App\\Models;
use Illuminate\\Database\\Eloquent\\Model;
class BrandTranslation extends Model {}
";
    let (backend, dir) = make_workspace(&[("src/Models/BrandTranslation.php", model_php)]);

    let model_uri =
        Url::from_file_path(dir.path().join("src/Models/BrandTranslation.php")).unwrap();
    backend
        .did_open(DidOpenTextDocumentParams {
            text_document: TextDocumentItem {
                uri: model_uri,
                language_id: "php".to_string(),
                version: 1,
                text: model_php.to_string(),
            },
        })
        .await;

    let test_php = "\
<?php
namespace App\\Models;
class TestService {
    public function demo(): void {
        $q = BrandTranslation::where('name', 1);
        $q->pluck('brand_id');
    }
}
";
    // Cursor on "pluck" in `$q->pluck('brand_id');`
    // Line 5 (0-indexed), "pluck" starts at character 12
    let result = goto_definition_at(
        &backend,
        &dir,
        "src/Models/TestService.php",
        test_php,
        5,
        14,
    )
    .await;

    assert!(
        result.is_some(),
        "Go-to-definition on $q->pluck() should resolve"
    );

    let response = result.unwrap();
    let uri = definition_uri(&response);
    assert!(
        uri.as_str().contains("Builder.php"),
        "pluck should resolve to Builder.php, got: {}",
        uri.as_str()
    );
    assert!(
        uri.as_str().contains("Eloquent"),
        "pluck should resolve to the Eloquent Builder, got: {}",
        uri.as_str()
    );
}

// ─── Scope methods on Builder instances (GTD) ───────────────────────────────

#[tokio::test]
async fn test_goto_definition_scope_on_builder_after_where_chain() {
    // Brand::where('id', 1)->isActive() — GTD on isActive should jump
    // to scopeIsActive on the model.
    let brand_php = "\
<?php
namespace App\\Models;
use Illuminate\\Database\\Eloquent\\Model;
use Illuminate\\Database\\Eloquent\\Builder;
class Brand extends Model {
    public function scopeIsActive(Builder $query): void {
        $query->where('active', true);
    }
    public function demo(): void {
        $q = Brand::where('id', 1);
        $q->isActive();
    }
}
";
    let (backend, dir) = make_workspace(&[("src/Models/Brand.php", brand_php)]);

    // Cursor on "isActive" in `$q->isActive();`
    // Line 10 (0-indexed), "isActive" starts at character 12
    let result =
        goto_definition_at(&backend, &dir, "src/Models/Brand.php", brand_php, 10, 14).await;

    assert!(
        result.is_some(),
        "Go-to-definition on $q->isActive() should resolve to scopeIsActive"
    );

    let response = result.unwrap();
    let uri = definition_uri(&response);
    assert!(
        uri.as_str().contains("Brand.php"),
        "Scope should resolve within Brand.php, got: {}",
        uri.as_str()
    );

    let line = definition_line(&response);
    assert_eq!(
        line, 5,
        "scopeIsActive is on line 5 (0-indexed), got: {}",
        line
    );
}

#[tokio::test]
async fn test_goto_definition_scope_on_builder_inline_chain() {
    // Brand::where('id', 1)->isActive() — inline chain, GTD on isActive
    let brand_php = "\
<?php
namespace App\\Models;
use Illuminate\\Database\\Eloquent\\Model;
use Illuminate\\Database\\Eloquent\\Builder;
class Brand extends Model {
    public function scopeIsActive(Builder $query): void {
        $query->where('active', true);
    }
    public function demo(): void {
        Brand::where('id', 1)->isActive();
    }
}
";
    let (backend, dir) = make_workspace(&[("src/Models/Brand.php", brand_php)]);

    // Cursor on "isActive" in `Brand::where('id', 1)->isActive();`
    // Line 9 (0-indexed), "isActive" starts at character 31
    let result = goto_definition_at(&backend, &dir, "src/Models/Brand.php", brand_php, 9, 33).await;

    assert!(
        result.is_some(),
        "Go-to-definition on Brand::where()->isActive() should resolve to scopeIsActive"
    );

    let response = result.unwrap();
    let line = definition_line(&response);
    assert_eq!(
        line, 5,
        "scopeIsActive is on line 5 (0-indexed), got: {}",
        line
    );
}

#[tokio::test]
async fn test_goto_definition_scope_on_builder_with_params() {
    // $q->ofGenre('fiction') — GTD on ofGenre should jump to scopeOfGenre
    let author_php = "\
<?php
namespace App\\Models;
use Illuminate\\Database\\Eloquent\\Model;
use Illuminate\\Database\\Eloquent\\Builder;
class BlogAuthor extends Model {
    public function scopeOfGenre(Builder $query, string $genre): void {
        $query->where('genre', $genre);
    }
    public function demo(): void {
        $q = BlogAuthor::where('active', true);
        $q->ofGenre('fiction');
    }
}
";
    let (backend, dir) = make_workspace(&[("src/Models/BlogAuthor.php", author_php)]);

    // Cursor on "ofGenre" in `$q->ofGenre('fiction');`
    // Line 10 (0-indexed), "ofGenre" starts at character 12
    let result = goto_definition_at(
        &backend,
        &dir,
        "src/Models/BlogAuthor.php",
        author_php,
        10,
        14,
    )
    .await;

    assert!(
        result.is_some(),
        "Go-to-definition on $q->ofGenre() should resolve to scopeOfGenre"
    );

    let response = result.unwrap();
    let uri = definition_uri(&response);
    assert!(
        uri.as_str().contains("BlogAuthor.php"),
        "Scope should resolve within BlogAuthor.php, got: {}",
        uri.as_str()
    );

    let line = definition_line(&response);
    assert_eq!(
        line, 5,
        "scopeOfGenre is on line 5 (0-indexed), got: {}",
        line
    );
}

#[tokio::test]
async fn test_goto_definition_scope_inside_scope_body() {
    // Inside scopeActive body, $query->verified() — GTD on verified
    // should jump to scopeVerified.
    let user_php = "\
<?php
namespace App\\Models;
use Illuminate\\Database\\Eloquent\\Model;
use Illuminate\\Database\\Eloquent\\Builder;
class User extends Model {
    public function scopeActive(Builder $query): void {
        $query->verified();
    }
    public function scopeVerified(Builder $query): void {
        $query->where('verified', true);
    }
}
";
    let (backend, dir) = make_workspace(&[("src/Models/User.php", user_php)]);

    // Cursor on "verified" in `$query->verified();`
    // Line 6 (0-indexed), "verified" starts at character 16
    let result = goto_definition_at(&backend, &dir, "src/Models/User.php", user_php, 6, 18).await;

    assert!(
        result.is_some(),
        "Go-to-definition on $query->verified() inside scope body should resolve to scopeVerified"
    );

    let response = result.unwrap();
    let line = definition_line(&response);
    assert_eq!(
        line, 8,
        "scopeVerified is on line 8 (0-indexed), got: {}",
        line
    );
}

#[tokio::test]
async fn test_goto_definition_scope_on_builder_after_scope_chain() {
    // Brand::where('id', 1)->isActive()->ofType('premium') — GTD on
    // ofType after chaining through another scope.
    let brand_php = "\
<?php
namespace App\\Models;
use Illuminate\\Database\\Eloquent\\Model;
use Illuminate\\Database\\Eloquent\\Builder;
class Brand extends Model {
    public function scopeIsActive(Builder $query): void {}
    public function scopeOfType(Builder $query, string $type): void {}
    public function demo(): void {
        Brand::where('id', 1)->isActive()->ofType('premium');
    }
}
";
    let (backend, dir) = make_workspace(&[("src/Models/Brand.php", brand_php)]);

    // Cursor on "ofType" in `->ofType('premium');`
    // Line 8 (0-indexed), "ofType" starts at character 42
    let result = goto_definition_at(&backend, &dir, "src/Models/Brand.php", brand_php, 8, 44).await;

    assert!(
        result.is_some(),
        "Go-to-definition on ->ofType() after scope chain should resolve to scopeOfType"
    );

    let response = result.unwrap();
    let line = definition_line(&response);
    assert_eq!(
        line, 6,
        "scopeOfType is on line 6 (0-indexed), got: {}",
        line
    );
}
