mod common;

use common::create_test_backend;
use tower_lsp::LanguageServer;
use tower_lsp::lsp_types::*;

/// Helper: open a document and trigger completion at the given line/column.
async fn complete_at(
    backend: &phpantom_lsp::Backend,
    uri: &Url,
    src: &str,
    line: u32,
    character: u32,
) -> Vec<CompletionItem> {
    let open_params = DidOpenTextDocumentParams {
        text_document: TextDocumentItem {
            uri: uri.clone(),
            language_id: "php".to_string(),
            version: 1,
            text: src.to_string(),
        },
    };
    backend.did_open(open_params).await;

    let completion_params = CompletionParams {
        text_document_position: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier { uri: uri.clone() },
            position: Position { line, character },
        },
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: PartialResultParams::default(),
        context: None,
    };

    match backend.completion(completion_params).await.unwrap() {
        Some(CompletionResponse::Array(items)) => items,
        Some(CompletionResponse::List(list)) => list.items,
        None => vec![],
    }
}

fn method_names(items: &[CompletionItem]) -> Vec<&str> {
    items
        .iter()
        .filter(|i| i.kind == Some(CompletionItemKind::METHOD))
        .map(|i| i.filter_text.as_deref().unwrap_or(&i.label))
        .collect()
}

fn property_names(items: &[CompletionItem]) -> Vec<&str> {
    items
        .iter()
        .filter(|i| i.kind == Some(CompletionItemKind::PROPERTY))
        .map(|i| i.filter_text.as_deref().unwrap_or(&i.label))
        .collect()
}

// ─── Arrow function: callable param inference from generic collection ───────

/// `$collection->map(fn($u) => $u->…)` — infer `$u` from the `map` method's
/// callable parameter type after generic substitution.
#[tokio::test]
async fn test_arrow_fn_param_inferred_from_generic_map() {
    let backend = create_test_backend();
    let uri = Url::parse("file:///test/closure_infer_map.php").unwrap();

    let src = concat!(
        "<?php\n",
        "class User {\n",
        "    public function getName(): string { return ''; }\n",
        "    public function getEmail(): string { return ''; }\n",
        "}\n",
        "/**\n",
        " * @template TKey\n",
        " * @template TValue\n",
        " */\n",
        "class Collection {\n",
        "    /**\n",
        "     * @param callable(TValue): mixed $callback\n",
        "     * @return static\n",
        "     */\n",
        "    public function map(callable $callback): static {}\n",
        "}\n",
        "class UserService {\n",
        "    /** @return Collection<int, User> */\n",
        "    public function getUsers(): Collection {}\n",
        "    public function run(): void {\n",
        "        $users = $this->getUsers();\n",
        "        $users->map(fn($u) => $u->);\n",
        "    }\n",
        "}\n",
    );

    // Line 21: `        $users->map(fn($u) => $u->);`
    //                                          ^--- cursor after `->`
    let items = complete_at(&backend, &uri, src, 21, 34).await;
    let names = method_names(&items);
    assert!(
        names.contains(&"getName"),
        "Expected getName from inferred User type, got: {:?}",
        names,
    );
    assert!(
        names.contains(&"getEmail"),
        "Expected getEmail from inferred User type, got: {:?}",
        names,
    );
}

// ─── Closure: callable param inference from generic collection ──────────────

/// `$collection->each(function ($item) { $item->… })` — infer `$item` from
/// the `each` method's callable parameter type.
#[tokio::test]
async fn test_closure_param_inferred_from_generic_each() {
    let backend = create_test_backend();
    let uri = Url::parse("file:///test/closure_infer_each.php").unwrap();

    let src = concat!(
        "<?php\n",
        "class Product {\n",
        "    public function getPrice(): float { return 0.0; }\n",
        "    public function getSku(): string { return ''; }\n",
        "}\n",
        "/**\n",
        " * @template TKey\n",
        " * @template TValue\n",
        " */\n",
        "class Collection {\n",
        "    /**\n",
        "     * @param callable(TValue): void $callback\n",
        "     * @return static\n",
        "     */\n",
        "    public function each(callable $callback): static {}\n",
        "}\n",
        "class ProductService {\n",
        "    /** @return Collection<int, Product> */\n",
        "    public function getProducts(): Collection {}\n",
        "    public function run(): void {\n",
        "        $products = $this->getProducts();\n",
        "        $products->each(function ($item) {\n",
        "            $item->\n",
        "        });\n",
        "    }\n",
        "}\n",
    );

    // Line 22: `            $item->`  cursor after `->`
    let items = complete_at(&backend, &uri, src, 22, 19).await;
    let names = method_names(&items);
    assert!(
        names.contains(&"getPrice"),
        "Expected getPrice from inferred Product type, got: {:?}",
        names,
    );
    assert!(
        names.contains(&"getSku"),
        "Expected getSku from inferred Product type, got: {:?}",
        names,
    );
}

// ─── Explicit type hint takes precedence over inference ─────────────────────

/// When a closure parameter has an explicit type hint, it should take
/// precedence over the inferred callable parameter type.
#[tokio::test]
async fn test_explicit_type_hint_takes_precedence() {
    let backend = create_test_backend();
    let uri = Url::parse("file:///test/closure_explicit_wins.php").unwrap();

    let src = concat!(
        "<?php\n",
        "class Animal {\n",
        "    public function speak(): string { return ''; }\n",
        "}\n",
        "class Dog extends Animal {\n",
        "    public function fetch(): void {}\n",
        "}\n",
        "/**\n",
        " * @template TKey\n",
        " * @template TValue\n",
        " */\n",
        "class Collection {\n",
        "    /**\n",
        "     * @param callable(TValue): mixed $callback\n",
        "     * @return static\n",
        "     */\n",
        "    public function map(callable $callback): static {}\n",
        "}\n",
        "class Kennel {\n",
        "    /** @return Collection<int, Animal> */\n",
        "    public function getAnimals(): Collection {}\n",
        "    public function run(): void {\n",
        "        $animals = $this->getAnimals();\n",
        "        $animals->map(function (Dog $d) {\n",
        "            $d->\n",
        "        });\n",
        "    }\n",
        "}\n",
    );

    // Line 24: `            $d->`  cursor after `->`
    let items = complete_at(&backend, &uri, src, 24, 17).await;
    let names = method_names(&items);
    // Dog has `fetch` method, Animal does not
    assert!(
        names.contains(&"fetch"),
        "Explicit Dog type should win; expected fetch in {:?}",
        names,
    );
}

// ─── Non-callable parameter: no inference ───────────────────────────────────

/// When the method parameter at the closure's position is not a callable
/// type, no inference should be attempted and the param stays unresolved.
#[tokio::test]
async fn test_no_inference_for_non_callable_param() {
    let backend = create_test_backend();
    let uri = Url::parse("file:///test/closure_no_infer.php").unwrap();

    let src = concat!(
        "<?php\n",
        "class Formatter {\n",
        "    public function format(string $value): string { return ''; }\n",
        "}\n",
        "class Service {\n",
        "    public function run(): void {\n",
        "        $f = new Formatter();\n",
        "        $f->format(fn($x) => $x->);\n",
        "    }\n",
        "}\n",
    );

    // Line 7: `        $f->format(fn($x) => $x->);`
    //                                        ^--- cursor after `->`
    let items = complete_at(&backend, &uri, src, 7, 34).await;
    let names = method_names(&items);
    // No type should be inferred for $x since `format` expects `string`, not callable
    assert!(
        names.is_empty() || !names.contains(&"format"),
        "Should not infer a type from non-callable param, got: {:?}",
        names,
    );
}

// ─── Concrete callable type (non-generic) ───────────────────────────────────

/// When the method parameter is `callable(SomeClass): void` (no generics
/// involved), the closure param should be inferred as `SomeClass`.
#[tokio::test]
async fn test_concrete_callable_param_inference() {
    let backend = create_test_backend();
    let uri = Url::parse("file:///test/closure_concrete_callable.php").unwrap();

    let src = concat!(
        "<?php\n",
        "class Config {\n",
        "    public function get(string $key): string { return ''; }\n",
        "    public function set(string $key, string $val): void {}\n",
        "}\n",
        "class App {\n",
        "    /**\n",
        "     * @param callable(Config): void $callback\n",
        "     * @return void\n",
        "     */\n",
        "    public function configure(callable $callback): void {}\n",
        "}\n",
        "class Bootstrap {\n",
        "    public function run(): void {\n",
        "        $app = new App();\n",
        "        $app->configure(function ($cfg) {\n",
        "            $cfg->\n",
        "        });\n",
        "    }\n",
        "}\n",
    );

    // Line 16: `            $cfg->`  cursor after `->`
    let items = complete_at(&backend, &uri, src, 16, 18).await;
    let names = method_names(&items);
    assert!(
        names.contains(&"get"),
        "Expected get from inferred Config type, got: {:?}",
        names,
    );
    assert!(
        names.contains(&"set"),
        "Expected set from inferred Config type, got: {:?}",
        names,
    );
}

// ─── Arrow function with properties ─────────────────────────────────────────

/// Inferred type should resolve both methods and properties.
#[tokio::test]
async fn test_inferred_type_resolves_properties() {
    let backend = create_test_backend();
    let uri = Url::parse("file:///test/closure_infer_props.php").unwrap();

    let src = concat!(
        "<?php\n",
        "class Point {\n",
        "    public float $x;\n",
        "    public float $y;\n",
        "    public function distanceTo(Point $other): float { return 0.0; }\n",
        "}\n",
        "/**\n",
        " * @template TKey\n",
        " * @template TValue\n",
        " */\n",
        "class Collection {\n",
        "    /**\n",
        "     * @param callable(TValue): mixed $callback\n",
        "     * @return static\n",
        "     */\n",
        "    public function map(callable $callback): static {}\n",
        "}\n",
        "class Geometry {\n",
        "    /** @return Collection<int, Point> */\n",
        "    public function getPoints(): Collection {}\n",
        "    public function run(): void {\n",
        "        $points = $this->getPoints();\n",
        "        $points->map(fn($p) => $p->);\n",
        "    }\n",
        "}\n",
    );

    // Line 22: `        $points->map(fn($p) => $p->);`
    //                                           ^--- cursor after `->`
    let items = complete_at(&backend, &uri, src, 22, 35).await;
    let props = property_names(&items);
    assert!(
        props.contains(&"x"),
        "Expected property x from inferred Point type, got: {:?}",
        props,
    );
    assert!(
        props.contains(&"y"),
        "Expected property y from inferred Point type, got: {:?}",
        props,
    );
    let methods = method_names(&items);
    assert!(
        methods.contains(&"distanceTo"),
        "Expected distanceTo from inferred Point type, got: {:?}",
        methods,
    );
}

// ─── Static method call with callable param ─────────────────────────────────

/// `MyClass::process(function ($item) { $item->… })` — infer from static
/// method's callable parameter type.
#[tokio::test]
async fn test_static_method_callable_param_inference() {
    let backend = create_test_backend();
    let uri = Url::parse("file:///test/closure_static_infer.php").unwrap();

    let src = concat!(
        "<?php\n",
        "class Order {\n",
        "    public function getTotal(): float { return 0.0; }\n",
        "    public function getStatus(): string { return ''; }\n",
        "}\n",
        "class Processor {\n",
        "    /**\n",
        "     * @param callable(Order): void $handler\n",
        "     * @return void\n",
        "     */\n",
        "    public static function handle(callable $handler): void {}\n",
        "}\n",
        "class Runner {\n",
        "    public function run(): void {\n",
        "        Processor::handle(function ($order) {\n",
        "            $order->\n",
        "        });\n",
        "    }\n",
        "}\n",
    );

    // Line 15: `            $order->`  cursor after `->`
    let items = complete_at(&backend, &uri, src, 15, 21).await;
    let names = method_names(&items);
    assert!(
        names.contains(&"getTotal"),
        "Expected getTotal from inferred Order type, got: {:?}",
        names,
    );
    assert!(
        names.contains(&"getStatus"),
        "Expected getStatus from inferred Order type, got: {:?}",
        names,
    );
}

// ─── Multiple callable parameters ──────────────────────────────────────────

/// When a method has multiple parameters and the callable is not at
/// position 0, the inference should use the correct parameter index.
#[tokio::test]
async fn test_callable_at_non_zero_position() {
    let backend = create_test_backend();
    let uri = Url::parse("file:///test/closure_second_arg.php").unwrap();

    let src = concat!(
        "<?php\n",
        "class Item {\n",
        "    public function getWeight(): float { return 0.0; }\n",
        "}\n",
        "/**\n",
        " * @template TKey\n",
        " * @template TValue\n",
        " */\n",
        "class Collection {\n",
        "    /**\n",
        "     * @param int $size\n",
        "     * @param callable(TValue): void $callback\n",
        "     * @return void\n",
        "     */\n",
        "    public function chunk(int $size, callable $callback): void {}\n",
        "}\n",
        "class Warehouse {\n",
        "    /** @return Collection<int, Item> */\n",
        "    public function getItems(): Collection {}\n",
        "    public function run(): void {\n",
        "        $items = $this->getItems();\n",
        "        $items->chunk(100, function ($item) {\n",
        "            $item->\n",
        "        });\n",
        "    }\n",
        "}\n",
    );

    // Line 22: `            $item->`  cursor after `->`
    let items = complete_at(&backend, &uri, src, 22, 19).await;
    let names = method_names(&items);
    assert!(
        names.contains(&"getWeight"),
        "Expected getWeight from inferred Item type at arg position 1, got: {:?}",
        names,
    );
}

// ─── Closure parameter with Closure() type ──────────────────────────────────

/// When the method parameter uses `Closure(Type): ReturnType` syntax.
#[tokio::test]
async fn test_closure_type_syntax_inference() {
    let backend = create_test_backend();
    let uri = Url::parse("file:///test/closure_type_syntax.php").unwrap();

    let src = concat!(
        "<?php\n",
        "class Logger {\n",
        "    public function info(string $msg): void {}\n",
        "    public function error(string $msg): void {}\n",
        "}\n",
        "class Pipeline {\n",
        "    /**\n",
        "     * @param Closure(Logger): void $step\n",
        "     * @return static\n",
        "     */\n",
        "    public function pipe(\\Closure $step): static {}\n",
        "}\n",
        "class Runner {\n",
        "    public function run(): void {\n",
        "        $pipeline = new Pipeline();\n",
        "        $pipeline->pipe(fn($log) => $log->);\n",
        "    }\n",
        "}\n",
    );

    // Line 15: `        $pipeline->pipe(fn($log) => $log->);`
    //                                                ^--- cursor after `->`
    let items = complete_at(&backend, &uri, src, 15, 42).await;
    let names = method_names(&items);
    assert!(
        names.contains(&"info"),
        "Expected info from inferred Logger type, got: {:?}",
        names,
    );
    assert!(
        names.contains(&"error"),
        "Expected error from inferred Logger type, got: {:?}",
        names,
    );
}

// ─── $this-> receiver ───────────────────────────────────────────────────────

/// Callable inference when the method is called on `$this->`.
#[tokio::test]
async fn test_inference_on_this_method_call() {
    let backend = create_test_backend();
    let uri = Url::parse("file:///test/closure_this_call.php").unwrap();

    let src = concat!(
        "<?php\n",
        "class Task {\n",
        "    public function execute(): bool { return true; }\n",
        "    public function getName(): string { return ''; }\n",
        "}\n",
        "class TaskRunner {\n",
        "    /**\n",
        "     * @param callable(Task): void $fn\n",
        "     * @return void\n",
        "     */\n",
        "    public function runWith(callable $fn): void {}\n",
        "    public function run(): void {\n",
        "        $this->runWith(function ($task) {\n",
        "            $task->\n",
        "        });\n",
        "    }\n",
        "}\n",
    );

    // Line 13: `            $task->`  cursor after `->`
    let items = complete_at(&backend, &uri, src, 13, 19).await;
    let names = method_names(&items);
    assert!(
        names.contains(&"execute"),
        "Expected execute from inferred Task type, got: {:?}",
        names,
    );
    assert!(
        names.contains(&"getName"),
        "Expected getName from inferred Task type, got: {:?}",
        names,
    );
}

// ─── Unit tests for extract_callable_param_types ────────────────────────────

#[test]
fn test_extract_callable_param_types_basic() {
    use phpantom_lsp::docblock::extract_callable_param_types;

    assert_eq!(
        extract_callable_param_types("callable(User): void"),
        Some(vec!["User".to_string()]),
    );
    assert_eq!(
        extract_callable_param_types("callable(User, int): void"),
        Some(vec!["User".to_string(), "int".to_string()]),
    );
    assert_eq!(
        extract_callable_param_types("Closure(string): bool"),
        Some(vec!["string".to_string()]),
    );
}

#[test]
fn test_extract_callable_param_types_empty() {
    use phpantom_lsp::docblock::extract_callable_param_types;

    assert_eq!(
        extract_callable_param_types("callable(): void"),
        Some(vec![]),
    );
    assert_eq!(
        extract_callable_param_types("Closure(): mixed"),
        Some(vec![]),
    );
}

#[test]
fn test_extract_callable_param_types_generic() {
    use phpantom_lsp::docblock::extract_callable_param_types;

    assert_eq!(
        extract_callable_param_types("callable(Collection<int, User>): void"),
        Some(vec!["Collection<int, User>".to_string()]),
    );
    assert_eq!(
        extract_callable_param_types("callable(array<string, int>, User): bool"),
        Some(vec!["array<string, int>".to_string(), "User".to_string(),]),
    );
}

#[test]
fn test_extract_callable_param_types_fqn() {
    use phpantom_lsp::docblock::extract_callable_param_types;

    assert_eq!(
        extract_callable_param_types("\\Closure(\\App\\Models\\User): void"),
        Some(vec!["\\App\\Models\\User".to_string()]),
    );
    assert_eq!(
        extract_callable_param_types("?callable(User): void"),
        Some(vec!["User".to_string()]),
    );
}

#[test]
fn test_extract_callable_param_types_none() {
    use phpantom_lsp::docblock::extract_callable_param_types;

    // Bare types without parameter list
    assert_eq!(extract_callable_param_types("Closure"), None);
    assert_eq!(extract_callable_param_types("callable"), None);
    assert_eq!(extract_callable_param_types("User"), None);
    assert_eq!(extract_callable_param_types("string"), None);
}

#[test]
fn test_extract_callable_param_types_nested_callable() {
    use phpantom_lsp::docblock::extract_callable_param_types;

    // callable whose parameter is itself a callable
    assert_eq!(
        extract_callable_param_types("callable(callable(int): bool): void"),
        Some(vec!["callable(int): bool".to_string()]),
    );
}

#[test]
fn test_extract_callable_param_types_array_shape() {
    use phpantom_lsp::docblock::extract_callable_param_types;

    assert_eq!(
        extract_callable_param_types("callable(array{name: string, age: int}): void"),
        Some(vec!["array{name: string, age: int}".to_string()]),
    );
}

#[test]
fn test_extract_callable_param_types_union_with_null() {
    use phpantom_lsp::docblock::extract_callable_param_types;

    // `Closure(Builder): mixed|null` — union at the top level
    assert_eq!(
        extract_callable_param_types("Closure(Builder): mixed|null"),
        Some(vec!["Builder".to_string()]),
    );

    // `callable(User): void|null`
    assert_eq!(
        extract_callable_param_types("callable(User): void|null"),
        Some(vec!["User".to_string()]),
    );

    // `null|callable(Order): bool`
    assert_eq!(
        extract_callable_param_types("null|callable(Order): bool"),
        Some(vec!["Order".to_string()]),
    );
}

#[test]
fn test_extract_callable_param_types_parenthesized_group() {
    use phpantom_lsp::docblock::extract_callable_param_types;

    // `(Closure(Builder<Brand>): mixed)|null` — parenthesized callable in union
    assert_eq!(
        extract_callable_param_types("(Closure(Builder<Brand>): mixed)|null"),
        Some(vec!["Builder<Brand>".to_string()]),
    );

    // `(\\Closure(\\App\\Models\\User): mixed)|string|null`
    assert_eq!(
        extract_callable_param_types("(\\Closure(\\App\\Models\\User): mixed)|string|null"),
        Some(vec!["\\App\\Models\\User".to_string()]),
    );
}

#[test]
fn test_extract_callable_param_types_parenthesized_no_union() {
    use phpantom_lsp::docblock::extract_callable_param_types;

    // Bare parenthesized callable without union suffix
    assert_eq!(
        extract_callable_param_types("(Closure(Config): void)"),
        Some(vec!["Config".to_string()]),
    );
}

#[test]
fn test_extract_callable_param_types_union_non_callable_parts() {
    use phpantom_lsp::docblock::extract_callable_param_types;

    // Union where no part is a callable — should return None
    assert_eq!(extract_callable_param_types("string|null"), None,);

    // Union with a class name but no callable signature
    assert_eq!(extract_callable_param_types("Closure|null"), None,);
}

// ─── $this / static in callable param types resolve to receiver class ───────

/// When a method signature uses `$this` in a callable parameter type (e.g.
/// `callable($this, mixed): $this`), the inferred closure parameter should
/// resolve to the receiver class, not the class the user is editing.
#[tokio::test]
async fn test_this_in_callable_param_resolves_to_receiver() {
    let backend = create_test_backend();
    let uri = Url::parse("file:///test/closure_this_receiver.php").unwrap();

    let src = concat!(
        "<?php\n",
        "trait Conditionable {\n",
        "    /**\n",
        "     * @param callable($this, mixed): $this $callback\n",
        "     * @return $this\n",
        "     */\n",
        "    public function when(bool $condition, callable $callback): static {}\n",
        "}\n",
        "class Builder {\n",
        "    use Conditionable;\n",
        "    public function where(string $col, mixed $val): static { return $this; }\n",
        "    public function orderBy(string $col): static { return $this; }\n",
        "}\n",
        "class UserController {\n",
        "    public function index(): void {\n",
        "        $builder = new Builder();\n",
        "        $builder->when(true, function ($query) {\n",
        "            $query->\n",
        "        });\n",
        "    }\n",
        "}\n",
    );

    // Line 17: `            $query->`  cursor after `->`
    let items = complete_at(&backend, &uri, src, 17, 21).await;
    let names = method_names(&items);
    assert!(
        names.contains(&"where"),
        "Expected 'where' from Builder (receiver), got: {:?}",
        names,
    );
    assert!(
        names.contains(&"orderBy"),
        "Expected 'orderBy' from Builder (receiver), got: {:?}",
        names,
    );
}

/// Same scenario but with `static` instead of `$this` in the callable param type.
#[tokio::test]
async fn test_static_in_callable_param_resolves_to_receiver() {
    let backend = create_test_backend();
    let uri = Url::parse("file:///test/closure_static_receiver.php").unwrap();

    let src = concat!(
        "<?php\n",
        "class Pipeline {\n",
        "    /**\n",
        "     * @param callable(static): void $callback\n",
        "     * @return static\n",
        "     */\n",
        "    public function tap(callable $callback): static {}\n",
        "    public function send(mixed $data): static { return $this; }\n",
        "    public function through(array $pipes): static { return $this; }\n",
        "}\n",
        "class SomeService {\n",
        "    public function run(): void {\n",
        "        $pipeline = new Pipeline();\n",
        "        $pipeline->tap(function ($p) {\n",
        "            $p->\n",
        "        });\n",
        "    }\n",
        "}\n",
    );

    // Line 14: `            $p->`  cursor after `->`
    let items = complete_at(&backend, &uri, src, 14, 17).await;
    let names = method_names(&items);
    assert!(
        names.contains(&"send"),
        "Expected 'send' from Pipeline (receiver), got: {:?}",
        names,
    );
    assert!(
        names.contains(&"through"),
        "Expected 'through' from Pipeline (receiver), got: {:?}",
        names,
    );
}

/// `$this` in a callable param type on a static method call should also
/// resolve to the receiver class.
#[tokio::test]
async fn test_this_in_callable_param_static_call_resolves_to_receiver() {
    let backend = create_test_backend();
    let uri = Url::parse("file:///test/closure_this_static_call.php").unwrap();

    let src = concat!(
        "<?php\n",
        "class QueryBuilder {\n",
        "    /**\n",
        "     * @param callable($this): void $callback\n",
        "     * @return static\n",
        "     */\n",
        "    public static function create(callable $callback): static {}\n",
        "    public function limit(int $n): static { return $this; }\n",
        "    public function offset(int $n): static { return $this; }\n",
        "}\n",
        "class Controller {\n",
        "    public function index(): void {\n",
        "        QueryBuilder::create(function ($qb) {\n",
        "            $qb->\n",
        "        });\n",
        "    }\n",
        "}\n",
    );

    // Line 13: `            $qb->`  cursor after `->`
    let items = complete_at(&backend, &uri, src, 13, 18).await;
    let names = method_names(&items);
    assert!(
        names.contains(&"limit"),
        "Expected 'limit' from QueryBuilder (static receiver), got: {:?}",
        names,
    );
    assert!(
        names.contains(&"offset"),
        "Expected 'offset' from QueryBuilder (static receiver), got: {:?}",
        names,
    );
}

/// Arrow function variant: `$this` in callable param resolves to receiver.
#[tokio::test]
async fn test_this_in_callable_param_arrow_fn() {
    let backend = create_test_backend();
    let uri = Url::parse("file:///test/closure_this_arrow.php").unwrap();

    let src = concat!(
        "<?php\n",
        "class Filterable {\n",
        "    /**\n",
        "     * @param callable($this): bool $predicate\n",
        "     * @return static\n",
        "     */\n",
        "    public function filter(callable $predicate): static {}\n",
        "    public function isActive(): bool { return true; }\n",
        "    public function getName(): string { return ''; }\n",
        "}\n",
        "class App {\n",
        "    public function run(): void {\n",
        "        $f = new Filterable();\n",
        "        $f->filter(fn($item) => $item->);\n",
        "    }\n",
        "}\n",
    );

    // Line 13: `        $f->filter(fn($item) => $item->);`
    //                                             ^--- cursor after `->`
    let items = complete_at(&backend, &uri, src, 13, 39).await;
    let names = method_names(&items);
    assert!(
        names.contains(&"isActive"),
        "Expected 'isActive' from Filterable (receiver), got: {:?}",
        names,
    );
    assert!(
        names.contains(&"getName"),
        "Expected 'getName' from Filterable (receiver), got: {:?}",
        names,
    );
}

/// When a closure parameter has an explicit bare type hint (e.g.
/// `Collection $customers`) and the callable signature infers a more
/// specific generic form (e.g. `Collection<int, Customer>`), the inferred
/// type should be used so that foreach iteration resolves the element type.
///
/// Reproduces: `Customer::chunk(10, function (Collection $customers) { foreach ($customers as $customer) { $customer->… } })`
#[tokio::test]
async fn test_explicit_bare_hint_uses_inferred_generics_for_foreach() {
    let backend = create_test_backend();
    let uri = Url::parse("file:///test/closure_explicit_generic_foreach.php").unwrap();

    let src = concat!(
        "<?php\n",
        "class Customer {\n",
        "    public function isActiveMember(): bool { return true; }\n",
        "    public function getEmail(): string { return ''; }\n",
        "}\n",
        "/**\n",
        " * @template TKey of array-key\n",
        " * @template TValue\n",
        " * @implements IteratorAggregate<TKey, TValue>\n",
        " */\n",
        "class Collection {\n",
        "    /** @return TValue|null */\n",
        "    public function first(): mixed { return null; }\n",
        "    /** @return int */\n",
        "    public function count(): int { return 0; }\n",
        "}\n",
        "/**\n",
        " * @template TModel\n",
        " */\n",
        "class Builder {\n",
        "    /**\n",
        "     * @param callable(Collection<int, TModel>, int): mixed $callback\n",
        "     * @return bool\n",
        "     */\n",
        "    public function chunk(int $count, callable $callback): bool { return true; }\n",
        "    /** @return static */\n",
        "    public function where(string $col, mixed $val = null): static { return $this; }\n",
        "}\n",
        "class Service {\n",
        "    /** @return Builder<Customer> */\n",
        "    public function query(): Builder { return new Builder(); }\n",
        "    public function run(): void {\n",
        "        $this->query()->chunk(10, function (Collection $customers): void {\n",
        "            foreach ($customers as $customer) {\n",
        "                $customer->\n",
        "            }\n",
        "        });\n",
        "    }\n",
        "}\n",
    );

    // Line 34: `                $customer->` inside the foreach body.
    let items = complete_at(&backend, &uri, src, 34, 28).await;
    let names = method_names(&items);
    assert!(
        names.contains(&"isActiveMember"),
        "Expected isActiveMember from Customer via foreach over Collection<int, Customer>, got: {:?}",
        names,
    );
    assert!(
        names.contains(&"getEmail"),
        "Expected getEmail from Customer via foreach over Collection<int, Customer>, got: {:?}",
        names,
    );
}
