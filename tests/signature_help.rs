mod common;

use common::{create_test_backend, create_test_backend_with_function_stubs};
use tower_lsp::LanguageServer;
use tower_lsp::lsp_types::*;

/// Helper: open a file and request signature help at the given line/character.
async fn sig_help_at(
    backend: &phpantom_lsp::Backend,
    uri: &Url,
    text: &str,
    line: u32,
    character: u32,
) -> Option<SignatureHelp> {
    let open_params = DidOpenTextDocumentParams {
        text_document: TextDocumentItem {
            uri: uri.clone(),
            language_id: "php".to_string(),
            version: 1,
            text: text.to_string(),
        },
    };
    backend.did_open(open_params).await;

    let params = SignatureHelpParams {
        text_document_position_params: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier { uri: uri.clone() },
            position: Position { line, character },
        },
        work_done_progress_params: WorkDoneProgressParams::default(),
        context: None,
    };

    backend.signature_help(params).await.unwrap()
}

/// Extract the active parameter index from a SignatureHelp response.
fn active_param(sh: &SignatureHelp) -> u32 {
    sh.active_parameter.unwrap_or(0)
}

/// Extract the signature label from the first (and usually only) signature.
fn sig_label(sh: &SignatureHelp) -> &str {
    &sh.signatures[0].label
}

/// Extract parameter labels as strings from the first signature.
fn param_labels(sh: &SignatureHelp) -> Vec<String> {
    let sig = &sh.signatures[0];
    let params = sig.parameters.as_ref().unwrap();
    params
        .iter()
        .map(|pi| match &pi.label {
            ParameterLabel::Simple(s) => s.clone(),
            ParameterLabel::LabelOffsets([start, end]) => {
                sig.label[*start as usize..*end as usize].to_string()
            }
        })
        .collect()
}

// ═══════════════════════════════════════════════════════════════════════════
//  Same-file function
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn standalone_function_first_param() {
    let backend = create_test_backend();
    let uri = Url::parse("file:///sig_func.php").unwrap();
    let text = concat!(
        "<?php\n",
        "function greet(string $name, int $age): void {}\n",
        "greet(\n",
    );

    let sh = sig_help_at(&backend, &uri, text, 2, 6).await.unwrap();
    assert_eq!(sig_label(&sh), "greet(string $name, int $age): void");
    assert_eq!(active_param(&sh), 0);
    let pl = param_labels(&sh);
    assert_eq!(pl, vec!["string $name", "int $age"]);
}

#[tokio::test]
async fn standalone_function_second_param() {
    let backend = create_test_backend();
    let uri = Url::parse("file:///sig_func2.php").unwrap();
    let text = concat!(
        "<?php\n",
        "function greet(string $name, int $age): void {}\n",
        "greet('Alice', \n",
    );

    let sh = sig_help_at(&backend, &uri, text, 2, 15).await.unwrap();
    assert_eq!(active_param(&sh), 1);
}

#[tokio::test]
async fn standalone_function_no_params() {
    let backend = create_test_backend();
    let uri = Url::parse("file:///sig_noparam.php").unwrap();
    let text = concat!("<?php\n", "function doWork(): void {}\n", "doWork(\n",);

    let sh = sig_help_at(&backend, &uri, text, 2, 7).await.unwrap();
    assert_eq!(sig_label(&sh), "doWork(): void");
    assert!(sh.signatures[0].parameters.as_ref().unwrap().is_empty());
}

// ═══════════════════════════════════════════════════════════════════════════
//  Instance method on $this
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn this_method_call() {
    let backend = create_test_backend();
    let uri = Url::parse("file:///sig_this.php").unwrap();
    let text = concat!(
        "<?php\n",
        "class Greeter {\n",
        "    public function greet(string $name, int $age): string {\n",
        "        return '';\n",
        "    }\n",
        "    public function test() {\n",
        "        $this->greet(\n",
        "    }\n",
        "}\n",
    );

    let sh = sig_help_at(&backend, &uri, text, 6, 22).await.unwrap();
    assert!(sig_label(&sh).contains("greet"));
    assert!(sig_label(&sh).contains("string $name"));
    assert_eq!(active_param(&sh), 0);
}

#[tokio::test]
async fn this_method_second_param() {
    let backend = create_test_backend();
    let uri = Url::parse("file:///sig_this2.php").unwrap();
    let text = concat!(
        "<?php\n",
        "class Greeter {\n",
        "    public function greet(string $name, int $age): string {\n",
        "        return '';\n",
        "    }\n",
        "    public function test() {\n",
        "        $this->greet('Alice', \n",
        "    }\n",
        "}\n",
    );

    let sh = sig_help_at(&backend, &uri, text, 6, 30).await.unwrap();
    assert_eq!(active_param(&sh), 1);
}

// ═══════════════════════════════════════════════════════════════════════════
//  Instance method on a variable
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn variable_method_call() {
    let backend = create_test_backend();
    let uri = Url::parse("file:///sig_var.php").unwrap();
    let text = concat!(
        "<?php\n",
        "class Calculator {\n",
        "    public function add(int $a, int $b): int { return $a + $b; }\n",
        "}\n",
        "class Demo {\n",
        "    public function test() {\n",
        "        $calc = new Calculator();\n",
        "        $calc->add(\n",
        "    }\n",
        "}\n",
    );

    let sh = sig_help_at(&backend, &uri, text, 7, 19).await.unwrap();
    assert!(sig_label(&sh).contains("add"));
    assert!(sig_label(&sh).contains("int $a"));
    assert!(sig_label(&sh).contains("int $b"));
    assert_eq!(active_param(&sh), 0);
}

// ═══════════════════════════════════════════════════════════════════════════
//  Static method call
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn static_method_call() {
    let backend = create_test_backend();
    let uri = Url::parse("file:///sig_static.php").unwrap();
    let text = concat!(
        "<?php\n",
        "class MathUtil {\n",
        "    public static function clamp(int $value, int $min, int $max): int {\n",
        "        return max($min, min($max, $value));\n",
        "    }\n",
        "}\n",
        "MathUtil::clamp(\n",
    );

    let sh = sig_help_at(&backend, &uri, text, 6, 16).await.unwrap();
    assert!(sig_label(&sh).contains("clamp"));
    assert_eq!(active_param(&sh), 0);
    let pl = param_labels(&sh);
    assert_eq!(pl.len(), 3);
}

#[tokio::test]
async fn static_method_third_param() {
    let backend = create_test_backend();
    let uri = Url::parse("file:///sig_static3.php").unwrap();
    let text = concat!(
        "<?php\n",
        "class MathUtil {\n",
        "    public static function clamp(int $value, int $min, int $max): int {\n",
        "        return max($min, min($max, $value));\n",
        "    }\n",
        "}\n",
        "MathUtil::clamp(1, 0, \n",
    );

    let sh = sig_help_at(&backend, &uri, text, 6, 22).await.unwrap();
    assert_eq!(active_param(&sh), 2);
}

// ═══════════════════════════════════════════════════════════════════════════
//  self:: and static:: calls
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn self_static_method_call() {
    let backend = create_test_backend();
    let uri = Url::parse("file:///sig_self.php").unwrap();
    let text = concat!(
        "<?php\n",
        "class Foo {\n",
        "    public static function create(string $name): static {\n",
        "        return new static();\n",
        "    }\n",
        "    public function test() {\n",
        "        self::create(\n",
        "    }\n",
        "}\n",
    );

    let sh = sig_help_at(&backend, &uri, text, 6, 21).await.unwrap();
    assert!(sig_label(&sh).contains("create"));
    assert!(sig_label(&sh).contains("string $name"));
}

// ═══════════════════════════════════════════════════════════════════════════
//  Constructor call: new ClassName(
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn constructor_call() {
    let backend = create_test_backend();
    let uri = Url::parse("file:///sig_ctor.php").unwrap();
    let text = concat!(
        "<?php\n",
        "class User {\n",
        "    public function __construct(string $name, string $email) {}\n",
        "}\n",
        "new User(\n",
    );

    let sh = sig_help_at(&backend, &uri, text, 4, 9).await.unwrap();
    assert!(sig_label(&sh).contains("User"));
    assert!(sig_label(&sh).contains("string $name"));
    assert!(sig_label(&sh).contains("string $email"));
    assert_eq!(active_param(&sh), 0);
}

#[tokio::test]
async fn constructor_second_param() {
    let backend = create_test_backend();
    let uri = Url::parse("file:///sig_ctor2.php").unwrap();
    let text = concat!(
        "<?php\n",
        "class User {\n",
        "    public function __construct(string $name, string $email) {}\n",
        "}\n",
        "new User('Alice', \n",
    );

    let sh = sig_help_at(&backend, &uri, text, 4, 18).await.unwrap();
    assert_eq!(active_param(&sh), 1);
}

// ═══════════════════════════════════════════════════════════════════════════
//  No signature help outside parentheses
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn no_help_outside_parens() {
    let backend = create_test_backend();
    let uri = Url::parse("file:///sig_none.php").unwrap();
    let text = concat!("<?php\n", "foo();\n",);

    let sh = sig_help_at(&backend, &uri, text, 1, 6).await;
    assert!(sh.is_none());
}

#[tokio::test]
async fn no_help_on_unknown_function() {
    let backend = create_test_backend();
    let uri = Url::parse("file:///sig_unknown.php").unwrap();
    let text = concat!("<?php\n", "unknownFunc(\n",);

    let sh = sig_help_at(&backend, &uri, text, 1, 12).await;
    assert!(sh.is_none());
}

// ═══════════════════════════════════════════════════════════════════════════
//  Nested calls — signature help for inner call
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn nested_call_inner_function() {
    let backend = create_test_backend();
    let uri = Url::parse("file:///sig_nested.php").unwrap();
    let text = concat!(
        "<?php\n",
        "function outer(string $x): void {}\n",
        "function inner(int $y, int $z): int { return $y; }\n",
        "outer(inner(\n",
    );

    // Cursor is inside inner(
    let sh = sig_help_at(&backend, &uri, text, 3, 12).await.unwrap();
    assert!(sig_label(&sh).contains("inner"));
    assert_eq!(active_param(&sh), 0);
}

// ═══════════════════════════════════════════════════════════════════════════
//  Variadic parameter — active index stays on last param
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn variadic_parameter_clamped() {
    let backend = create_test_backend();
    let uri = Url::parse("file:///sig_variadic.php").unwrap();
    let text = concat!(
        "<?php\n",
        "function logMessage(string $level, string ...$parts): void {}\n",
        "logMessage('info', 'a', 'b', \n",
    );

    let sh = sig_help_at(&backend, &uri, text, 2, 29).await.unwrap();
    // 3 commas → active_parameter = 3, but last param (index 1) is variadic
    // so it should be clamped to 1.
    assert_eq!(active_param(&sh), 1);
}

// ═══════════════════════════════════════════════════════════════════════════
//  Inherited method
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn inherited_method() {
    let backend = create_test_backend();
    let uri = Url::parse("file:///sig_inherit.php").unwrap();
    let text = concat!(
        "<?php\n",
        "class Base {\n",
        "    public function doWork(int $count): void {}\n",
        "}\n",
        "class Child extends Base {}\n",
        "class Demo {\n",
        "    public function test() {\n",
        "        $child = new Child();\n",
        "        $child->doWork(\n",
        "    }\n",
        "}\n",
    );

    let sh = sig_help_at(&backend, &uri, text, 8, 23).await.unwrap();
    assert!(sig_label(&sh).contains("doWork"));
    assert!(sig_label(&sh).contains("int $count"));
}

// ═══════════════════════════════════════════════════════════════════════════
//  Trait method
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn trait_method() {
    let backend = create_test_backend();
    let uri = Url::parse("file:///sig_trait.php").unwrap();
    let text = concat!(
        "<?php\n",
        "trait Greetable {\n",
        "    public function greet(string $whom): string { return 'hi'; }\n",
        "}\n",
        "class Person {\n",
        "    use Greetable;\n",
        "}\n",
        "class Demo {\n",
        "    public function test() {\n",
        "        $p = new Person();\n",
        "        $p->greet(\n",
        "    }\n",
        "}\n",
    );

    let sh = sig_help_at(&backend, &uri, text, 10, 18).await.unwrap();
    assert!(sig_label(&sh).contains("greet"));
    assert!(sig_label(&sh).contains("string $whom"));
}

// ═══════════════════════════════════════════════════════════════════════════
//  Built-in (stub) function
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn stub_function() {
    let backend = create_test_backend_with_function_stubs();
    let uri = Url::parse("file:///sig_stub.php").unwrap();
    let text = concat!("<?php\n", "str_contains(\n",);

    let sh = sig_help_at(&backend, &uri, text, 1, 13).await.unwrap();
    assert!(sig_label(&sh).contains("str_contains"));
    assert!(sig_label(&sh).contains("$haystack"));
    assert!(sig_label(&sh).contains("$needle"));
    assert_eq!(active_param(&sh), 0);
}

#[tokio::test]
async fn stub_function_second_param() {
    let backend = create_test_backend_with_function_stubs();
    let uri = Url::parse("file:///sig_stub2.php").unwrap();
    let text = concat!("<?php\n", "str_contains('hello', \n",);

    let sh = sig_help_at(&backend, &uri, text, 1, 22).await.unwrap();
    assert_eq!(active_param(&sh), 1);
}

// ═══════════════════════════════════════════════════════════════════════════
//  Parameter label offsets are correct substrings
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn parameter_label_offsets_match_label() {
    let backend = create_test_backend();
    let uri = Url::parse("file:///sig_offsets.php").unwrap();
    let text = concat!(
        "<?php\n",
        "function mix(string $a, int $b, bool $c): void {}\n",
        "mix(\n",
    );

    let sh = sig_help_at(&backend, &uri, text, 2, 4).await.unwrap();
    let sig = &sh.signatures[0];
    let params = sig.parameters.as_ref().unwrap();

    for pi in params {
        match &pi.label {
            ParameterLabel::LabelOffsets([start, end]) => {
                let substr = &sig.label[*start as usize..*end as usize];
                // Each extracted label should be a valid parameter representation.
                assert!(
                    substr.contains('$'),
                    "Parameter label offset '{}' should contain a $ sign",
                    substr
                );
            }
            ParameterLabel::Simple(s) => {
                assert!(
                    sig.label.contains(s.as_str()),
                    "Simple label '{}' should be substring of '{}'",
                    s,
                    sig.label
                );
            }
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  String arguments with commas don't confuse parameter counting
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn string_with_commas_ignored() {
    let backend = create_test_backend();
    let uri = Url::parse("file:///sig_strcomma.php").unwrap();
    let text = concat!(
        "<?php\n",
        "function pair(string $a, string $b): void {}\n",
        "pair('a,b,c', \n",
    );

    let sh = sig_help_at(&backend, &uri, text, 2, 14).await.unwrap();
    // The comma inside the string should not be counted.
    assert_eq!(active_param(&sh), 1);
}

// ═══════════════════════════════════════════════════════════════════════════
//  Nested call arguments don't confuse parameter counting
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn nested_call_args_not_counted() {
    let backend = create_test_backend();
    let uri = Url::parse("file:///sig_nestedcount.php").unwrap();
    let text = concat!(
        "<?php\n",
        "function outer(int $x, int $y): void {}\n",
        "function inner(int $a, int $b): int { return 0; }\n",
        "outer(inner(1, 2), \n",
    );

    let sh = sig_help_at(&backend, &uri, text, 3, 19).await.unwrap();
    assert!(sig_label(&sh).contains("outer"));
    // inner(1, 2) is one argument to outer, then the comma after it
    // puts us on the second parameter.
    assert_eq!(active_param(&sh), 1);
}

// ═══════════════════════════════════════════════════════════════════════════
//  parent:: calls
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn parent_method_call() {
    let backend = create_test_backend();
    let uri = Url::parse("file:///sig_parent.php").unwrap();
    let text = concat!(
        "<?php\n",
        "class Base {\n",
        "    public function __construct(string $name) {}\n",
        "}\n",
        "class Child extends Base {\n",
        "    public function __construct(string $name, int $age) {\n",
        "        parent::__construct(\n",
        "    }\n",
        "}\n",
    );

    let sh = sig_help_at(&backend, &uri, text, 6, 28).await.unwrap();
    assert!(sig_label(&sh).contains("__construct"));
    assert!(sig_label(&sh).contains("string $name"));
    // The parent __construct only has 1 param ($name).
    let pl = param_labels(&sh);
    assert_eq!(pl.len(), 1);
}

// ═══════════════════════════════════════════════════════════════════════════
//  Cursor right after open paren (no typing yet)
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn cursor_right_after_open_paren() {
    let backend = create_test_backend();
    let uri = Url::parse("file:///sig_openparen.php").unwrap();
    let text = concat!("<?php\n", "function test(int $x): void {}\n", "test(",);

    let sh = sig_help_at(&backend, &uri, text, 2, 5).await.unwrap();
    assert!(sig_label(&sh).contains("test"));
    assert_eq!(active_param(&sh), 0);
}

// ═══════════════════════════════════════════════════════════════════════════
//  Cursor after comma with spaces
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn cursor_after_comma_with_spaces() {
    let backend = create_test_backend();
    let uri = Url::parse("file:///sig_spaces.php").unwrap();
    let text = concat!(
        "<?php\n",
        "function pair(string $a, string $b): void {}\n",
        "pair('x',   ",
    );

    let sh = sig_help_at(&backend, &uri, text, 2, 12).await.unwrap();
    assert_eq!(active_param(&sh), 1);
}

// ═══════════════════════════════════════════════════════════════════════════
//  Cross-file via PSR-4
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn cross_file_psr4_method() {
    let composer_json = r#"{
        "autoload": {
            "psr-4": { "App\\": "src/" }
        }
    }"#;
    let service_php = concat!(
        "<?php\n",
        "namespace App;\n",
        "class Service {\n",
        "    public function process(string $input, int $retries): bool {\n",
        "        return true;\n",
        "    }\n",
        "}\n",
    );
    let client_php = concat!(
        "<?php\n",
        "namespace App;\n",
        "class Client {\n",
        "    public function run() {\n",
        "        $svc = new Service();\n",
        "        $svc->process(\n",
        "    }\n",
        "}\n",
    );

    let (backend, _dir) = common::create_psr4_workspace(
        composer_json,
        &[
            ("src/Service.php", service_php),
            ("src/Client.php", client_php),
        ],
    );

    let service_uri = Url::from_file_path(_dir.path().join("src/Service.php")).unwrap();
    let client_uri = Url::from_file_path(_dir.path().join("src/Client.php")).unwrap();

    // Open both files
    backend
        .did_open(DidOpenTextDocumentParams {
            text_document: TextDocumentItem {
                uri: service_uri,
                language_id: "php".to_string(),
                version: 1,
                text: service_php.to_string(),
            },
        })
        .await;

    backend
        .did_open(DidOpenTextDocumentParams {
            text_document: TextDocumentItem {
                uri: client_uri.clone(),
                language_id: "php".to_string(),
                version: 1,
                text: client_php.to_string(),
            },
        })
        .await;

    let params = SignatureHelpParams {
        text_document_position_params: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier {
                uri: client_uri.clone(),
            },
            position: Position {
                line: 5,
                character: 22,
            },
        },
        work_done_progress_params: WorkDoneProgressParams::default(),
        context: None,
    };

    let sh = backend.signature_help(params).await.unwrap().unwrap();
    assert!(sig_label(&sh).contains("process"));
    assert!(sig_label(&sh).contains("string $input"));
    assert!(sig_label(&sh).contains("int $retries"));
}

// ═══════════════════════════════════════════════════════════════════════════
//  Return type appears in signature label
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn return_type_in_label() {
    let backend = create_test_backend();
    let uri = Url::parse("file:///sig_ret.php").unwrap();
    let text = concat!(
        "<?php\n",
        "function compute(int $x): float {}\n",
        "compute(\n",
    );

    let sh = sig_help_at(&backend, &uri, text, 2, 8).await.unwrap();
    assert!(
        sig_label(&sh).ends_with(": float"),
        "Label should end with return type, got: {}",
        sig_label(&sh)
    );
}

#[tokio::test]
async fn no_return_type_omitted() {
    let backend = create_test_backend();
    let uri = Url::parse("file:///sig_noret.php").unwrap();
    let text = concat!("<?php\n", "function doStuff($x) {}\n", "doStuff(\n",);

    let sh = sig_help_at(&backend, &uri, text, 2, 8).await.unwrap();
    assert!(
        !sig_label(&sh).contains(':'),
        "Label should not contain ':' when there's no return type, got: {}",
        sig_label(&sh)
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  Reference parameter
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn reference_parameter() {
    let backend = create_test_backend();
    let uri = Url::parse("file:///sig_ref.php").unwrap();
    let text = concat!(
        "<?php\n",
        "function swap(int &$a, int &$b): void {}\n",
        "swap(\n",
    );

    let sh = sig_help_at(&backend, &uri, text, 2, 5).await.unwrap();
    let pl = param_labels(&sh);
    assert_eq!(pl[0], "int &$a");
    assert_eq!(pl[1], "int &$b");
}

// ═══════════════════════════════════════════════════════════════════════════
//  Variadic parameter display
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn variadic_parameter_in_label() {
    let backend = create_test_backend();
    let uri = Url::parse("file:///sig_variadic_label.php").unwrap();
    let text = concat!(
        "<?php\n",
        "function collect(string ...$items): array {}\n",
        "collect(\n",
    );

    let sh = sig_help_at(&backend, &uri, text, 2, 8).await.unwrap();
    assert!(
        sig_label(&sh).contains("...$items"),
        "Label should show variadic, got: {}",
        sig_label(&sh)
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  Cursor in the middle of typing an argument
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn cursor_mid_argument() {
    let backend = create_test_backend();
    let uri = Url::parse("file:///sig_mid.php").unwrap();
    let text = concat!(
        "<?php\n",
        "function pair(string $a, string $b): void {}\n",
        "pair($x",
    );

    // Cursor is at end of `$x` (still first argument)
    let sh = sig_help_at(&backend, &uri, text, 2, 7).await.unwrap();
    assert_eq!(active_param(&sh), 0);
}

// ═══════════════════════════════════════════════════════════════════════════
//  Multiple signatures not applicable (PHP doesn't have overloading)
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn single_signature_returned() {
    let backend = create_test_backend();
    let uri = Url::parse("file:///sig_single.php").unwrap();
    let text = concat!("<?php\n", "function doIt(int $n): void {}\n", "doIt(\n",);

    let sh = sig_help_at(&backend, &uri, text, 2, 5).await.unwrap();
    assert_eq!(sh.signatures.len(), 1);
    assert_eq!(sh.active_signature, Some(0));
}

// ═══════════════════════════════════════════════════════════════════════════
//  AST-based chain resolution (property chains, method return chains, etc.)
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn property_chain_method_call() {
    let backend = create_test_backend();
    let uri = Url::parse("file:///sig_prop_chain.php").unwrap();
    let text = concat!(
        "<?php\n",
        "class Inner {\n",
        "    public function process(string $data, int $flags): bool { return true; }\n",
        "}\n",
        "class Outer {\n",
        "    /** @var Inner */\n",
        "    public Inner $inner;\n",
        "}\n",
        "class Demo {\n",
        "    public function test() {\n",
        "        $outer = new Outer();\n",
        "        $outer->inner->process();\n",
        "    }\n",
        "}\n",
    );

    let sh = sig_help_at(&backend, &uri, text, 11, 31).await.unwrap();
    assert!(sig_label(&sh).contains("process"));
    assert!(sig_label(&sh).contains("string $data"));
    assert!(sig_label(&sh).contains("int $flags"));
    assert_eq!(active_param(&sh), 0);
}

#[tokio::test]
async fn this_property_chain_method_call() {
    let backend = create_test_backend();
    let uri = Url::parse("file:///sig_this_prop_chain.php").unwrap();
    let text = concat!(
        "<?php\n",
        "class Service {\n",
        "    public function execute(string $cmd): string { return ''; }\n",
        "}\n",
        "class Controller {\n",
        "    /** @var Service */\n",
        "    public Service $service;\n",
        "    public function run() {\n",
        "        $this->service->execute();\n",
        "    }\n",
        "}\n",
    );

    let sh = sig_help_at(&backend, &uri, text, 8, 32).await.unwrap();
    assert!(sig_label(&sh).contains("execute"));
    assert!(sig_label(&sh).contains("string $cmd"));
    assert_eq!(active_param(&sh), 0);
}

#[tokio::test]
async fn deep_property_chain_method_call() {
    let backend = create_test_backend();
    let uri = Url::parse("file:///sig_deep_chain.php").unwrap();
    let text = concat!(
        "<?php\n",
        "class Engine {\n",
        "    public function start(int $rpm): bool { return true; }\n",
        "}\n",
        "class Car {\n",
        "    /** @var Engine */\n",
        "    public Engine $engine;\n",
        "}\n",
        "class Garage {\n",
        "    /** @var Car */\n",
        "    public Car $car;\n",
        "}\n",
        "class Demo {\n",
        "    public function test() {\n",
        "        $garage = new Garage();\n",
        "        $garage->car->engine->start();\n",
        "    }\n",
        "}\n",
    );

    let sh = sig_help_at(&backend, &uri, text, 15, 36).await.unwrap();
    assert!(sig_label(&sh).contains("start"));
    assert!(sig_label(&sh).contains("int $rpm"));
    assert_eq!(active_param(&sh), 0);
}

#[tokio::test]
async fn method_return_chain() {
    let backend = create_test_backend();
    let uri = Url::parse("file:///sig_method_chain.php").unwrap();
    let text = concat!(
        "<?php\n",
        "class Builder {\n",
        "    public function where(string $col): self { return $this; }\n",
        "    public function limit(int $n): self { return $this; }\n",
        "}\n",
        "class Demo {\n",
        "    public function test() {\n",
        "        $b = new Builder();\n",
        "        $b->where('name')->limit();\n",
        "    }\n",
        "}\n",
    );

    let sh = sig_help_at(&backend, &uri, text, 8, 33).await.unwrap();
    assert!(sig_label(&sh).contains("limit"));
    assert!(sig_label(&sh).contains("int $n"));
    assert_eq!(active_param(&sh), 0);
}

#[tokio::test]
async fn function_return_chain() {
    let backend = create_test_backend();
    let uri = Url::parse("file:///sig_func_chain.php").unwrap();
    let text = concat!(
        "<?php\n",
        "class Widget {\n",
        "    public function configure(string $key, string $val): self { return $this; }\n",
        "}\n",
        "/** @return Widget */\n",
        "function makeWidget(): Widget { return new Widget(); }\n",
        "class Demo {\n",
        "    public function test() {\n",
        "        makeWidget()->configure();\n",
        "    }\n",
        "}\n",
    );

    let sh = sig_help_at(&backend, &uri, text, 8, 32).await.unwrap();
    assert!(sig_label(&sh).contains("configure"));
    assert!(sig_label(&sh).contains("string $key"));
    assert_eq!(active_param(&sh), 0);
}

#[tokio::test]
async fn static_method_return_chain() {
    let backend = create_test_backend();
    let uri = Url::parse("file:///sig_static_chain.php").unwrap();
    let text = concat!(
        "<?php\n",
        "class Query {\n",
        "    public static function create(): self { return new self(); }\n",
        "    public function filter(string $expr): self { return $this; }\n",
        "}\n",
        "class Demo {\n",
        "    public function test() {\n",
        "        Query::create()->filter();\n",
        "    }\n",
        "}\n",
    );

    let sh = sig_help_at(&backend, &uri, text, 7, 32).await.unwrap();
    assert!(sig_label(&sh).contains("filter"));
    assert!(sig_label(&sh).contains("string $expr"));
    assert_eq!(active_param(&sh), 0);
}

#[tokio::test]
async fn new_expression_chain() {
    let backend = create_test_backend();
    let uri = Url::parse("file:///sig_new_chain.php").unwrap();
    let text = concat!(
        "<?php\n",
        "class Printer {\n",
        "    public function print(string $text): void {}\n",
        "}\n",
        "class Demo {\n",
        "    public function test() {\n",
        "        (new Printer())->print();\n",
        "    }\n",
        "}\n",
    );

    let sh = sig_help_at(&backend, &uri, text, 6, 31).await.unwrap();
    assert!(sig_label(&sh).contains("print"));
    assert!(sig_label(&sh).contains("string $text"));
    assert_eq!(active_param(&sh), 0);
}

#[tokio::test]
async fn nullsafe_method_call() {
    let backend = create_test_backend();
    let uri = Url::parse("file:///sig_nullsafe.php").unwrap();
    let text = concat!(
        "<?php\n",
        "class Formatter {\n",
        "    public function format(string $pattern): string { return ''; }\n",
        "}\n",
        "class Demo {\n",
        "    public function test() {\n",
        "        /** @var Formatter|null $fmt */\n",
        "        $fmt = null;\n",
        "        $fmt?->format();\n",
        "    }\n",
        "}\n",
    );

    let sh = sig_help_at(&backend, &uri, text, 8, 22).await.unwrap();
    assert!(sig_label(&sh).contains("format"));
    assert!(sig_label(&sh).contains("string $pattern"));
    assert_eq!(active_param(&sh), 0);
}

#[tokio::test]
async fn property_then_method_chain_second_param() {
    let backend = create_test_backend();
    let uri = Url::parse("file:///sig_chain_2nd.php").unwrap();
    let text = concat!(
        "<?php\n",
        "class Logger {\n",
        "    public function log(string $level, string $msg): void {}\n",
        "}\n",
        "class App {\n",
        "    /** @var Logger */\n",
        "    public Logger $logger;\n",
        "}\n",
        "class Demo {\n",
        "    public function test() {\n",
        "        $app = new App();\n",
        "        $app->logger->log('info', );\n",
        "    }\n",
        "}\n",
    );

    let sh = sig_help_at(&backend, &uri, text, 11, 33).await.unwrap();
    assert!(sig_label(&sh).contains("log"));
    assert_eq!(active_param(&sh), 1);
}

#[tokio::test]
async fn nested_call_correct_site() {
    let backend = create_test_backend();
    let uri = Url::parse("file:///sig_nested_site.php").unwrap();
    let text = concat!(
        "<?php\n",
        "function outer(int $a, int $b): int { return 0; }\n",
        "function inner(string $s): string { return ''; }\n",
        "outer(inner(\n",
    );

    // Cursor inside inner() — should resolve to inner, param 0
    let sh = sig_help_at(&backend, &uri, text, 3, 12).await.unwrap();
    assert!(sig_label(&sh).contains("inner"));
    assert!(sig_label(&sh).contains("string $s"));
    assert_eq!(active_param(&sh), 0);
}

#[tokio::test]
async fn zero_param_method_closed_parens() {
    let backend = create_test_backend();
    let uri = Url::parse("file:///sig_zero_param.php").unwrap();
    let text = concat!(
        "<?php\n",
        "class Pen {\n",
        "    public function write(): string { return ''; }\n",
        "}\n",
        "class Demo {\n",
        "    public function test() {\n",
        "        $pen = new Pen();\n",
        "        $pen->write();\n",
        "    }\n",
        "}\n",
    );
    // Cursor between ( and ) of write() — line 7, char 20
    // "        $pen->write();" — '(' at char 19, ')' at 20
    let result = sig_help_at(&backend, &uri, text, 7, 20).await;
    assert!(
        result.is_some(),
        "signature help should fire for zero-param method"
    );
    let sh = result.unwrap();
    assert!(sig_label(&sh).contains("write"));
}

#[tokio::test]
async fn constructor_no_explicit_ctor() {
    let backend = create_test_backend();
    let uri = Url::parse("file:///sig_no_ctor.php").unwrap();
    let text = concat!(
        "<?php\n",
        "class Simple {\n",
        "    public function greet(): string { return 'hi'; }\n",
        "}\n",
        "$obj = new Simple();\n",
    );
    // Cursor between ( and ) of new Simple() — line 4, char 18
    // "$obj = new Simple();" — '(' at 17, ')' at 18
    let result = sig_help_at(&backend, &uri, text, 4, 18).await;
    assert!(
        result.is_some(),
        "signature help should fire for class with no __construct"
    );
    let sh = result.unwrap();
    assert!(sig_label(&sh).contains("Simple"));
}

#[tokio::test]
async fn generic_chain_with_new_expression_arg() {
    let backend = create_test_backend();
    let uri = Url::parse("file:///sig_generic_new_arg.php").unwrap();
    let text = concat!(
        "<?php\n",
        "class Product {\n",
        "    public function getPrice(int $qty): float { return 0.0; }\n",
        "}\n",
        "/** @template T */\n",
        "class Wrap {\n",
        "    /** @return T */\n",
        "    public function first(): mixed { return null; }\n",
        "}\n",
        "class Mapper {\n",
        "    /**\n",
        "     * @template T\n",
        "     * @param T $item\n",
        "     * @return Wrap<T>\n",
        "     */\n",
        "    public function wrap(object $item): Wrap { return new Wrap(); }\n",
        "}\n",
        "class Demo {\n",
        "    public function test() {\n",
        "        $mapper = new Mapper();\n",
        "        $mapper->wrap(new Product())->first()->getPrice();\n",
        "    }\n",
        "}\n",
    );

    // Cursor between ( and ) of getPrice() — line 20
    // "        $mapper->wrap(new Product())->first()->getPrice();"
    //  '(' for getPrice is at char 54, ')' at 55
    let sh = sig_help_at(&backend, &uri, text, 20, 56).await.unwrap();
    assert!(sig_label(&sh).contains("getPrice"));
    assert!(sig_label(&sh).contains("int $qty"));
    assert_eq!(active_param(&sh), 0);
}

#[tokio::test]
async fn array_access_method_call() {
    let backend = create_test_backend();
    let uri = Url::parse("file:///sig_array_access.php").unwrap();
    let text = concat!(
        "<?php\n",
        "class Pen {\n",
        "    public function write(string $text): string { return ''; }\n",
        "}\n",
        "class Demo {\n",
        "    public function test() {\n",
        "        /** @var list<Pen> $pens */\n",
        "        $pens = [];\n",
        "        $pens[0]->write();\n",
        "    }\n",
        "}\n",
    );
    // Line 8: "        $pens[0]->write();"
    // '(' at char 23, ')' at 24
    let result = sig_help_at(&backend, &uri, text, 8, 24).await;
    assert!(
        result.is_some(),
        "signature help should fire for array access method call"
    );
    let sh = result.unwrap();
    assert!(sig_label(&sh).contains("write"));
}

#[tokio::test]
async fn class_string_variable_static_call() {
    let backend = create_test_backend();
    let uri = Url::parse("file:///sig_class_string.php").unwrap();
    let text = concat!(
        "<?php\n",
        "class Pen {\n",
        "    public static function make(string $ink): static { return new static(); }\n",
        "}\n",
        "class Demo {\n",
        "    public function test() {\n",
        "        $cls = Pen::class;\n",
        "        $cls::make();\n",
        "    }\n",
        "}\n",
    );
    // Line 7: "        $cls::make();"
    // '(' at char 18, ')' at 19
    let sh = sig_help_at(&backend, &uri, text, 7, 19).await.unwrap();
    assert!(sig_label(&sh).contains("make"));
    assert!(sig_label(&sh).contains("string $ink"));
    assert_eq!(active_param(&sh), 0);
}

#[tokio::test]
async fn first_class_callable_invocation() {
    let backend = create_test_backend();
    let uri = Url::parse("file:///sig_fcc.php").unwrap();
    let text = concat!(
        "<?php\n",
        "function makePen(string $ink): Pen { return new Pen(); }\n",
        "class Pen {}\n",
        "class Demo {\n",
        "    public function test() {\n",
        "        $fn = makePen(...);\n",
        "        $fn();\n",
        "    }\n",
        "}\n",
    );
    // Line 6: "        $fn();"
    // '(' at char 11, ')' at 12
    let sh = sig_help_at(&backend, &uri, text, 6, 12).await.unwrap();
    assert!(sig_label(&sh).contains("makePen"));
    assert!(sig_label(&sh).contains("string $ink"));
    assert_eq!(active_param(&sh), 0);
}
