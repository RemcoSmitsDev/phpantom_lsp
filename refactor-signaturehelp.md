# Refactor: AST-based Signature Help

## Problem

Signature help currently uses text-based backward scanning (`named_args.rs` helpers) to detect what function/method is being called. This fails for:

1. **Property chain calls**: `$this->prop->method(` — the simplified `extract_subject_before_arrow` only reads one identifier back, producing `"prop->method"` instead of `"$this->prop->method"`.
2. **Chained method results**: `$obj->method1()->method2(` — `)` before `->` causes an early bail-out.
3. **`(new Foo())->method(`** — same `)` bail-out.
4. **Standalone function chains**: `app()->make(` — same pattern.
5. **Closure/variable invocations**: `$callback(` — not recognized as a callable.
6. **Deep null-safe chains**: `$obj?->prop?->method(` — only immediate `?->` is handled.
7. **Array access chains**: `$items[0]->method(` — `]` before `->` not handled.
8. **Strings containing parens/commas**: `$this->print(")")` — text scanning can be fooled by `)` inside strings.

The root cause is that signature help reinvents call-expression detection with character-level scanning, while the project already has a full AST (via Mago) and a `SymbolMap` that correctly extracts `subject_text` and `member_name` for every call expression in the file.

## Approach

Add a `CallSite` vector to `SymbolMap`. Each entry records the byte range of a call's argument list `(...)` and the call expression string. During a signature help request, binary-search the `call_sites` list for the innermost call whose argument list contains the cursor. This gives the call expression and active-parameter index from the AST, handling nesting, strings, and chains correctly.

## Data available from Mago

For every call expression, the Mago AST provides:

| Call kind | AST node | Subject accessor | ArgumentList accessor |
|-----------|----------|------------------|-----------------------|
| Function call | `Call::Function(fc)` | `fc.function` (Expression) | `fc.argument_list` |
| Method call | `Call::Method(mc)` | `mc.object` (Expression) + `mc.method` | `mc.argument_list` |
| Null-safe method | `Call::NullSafeMethod(mc)` | `mc.object` + `mc.method` | `mc.argument_list` |
| Static method | `Call::StaticMethod(sc)` | `sc.class` + `sc.method` | `sc.argument_list` |
| Constructor | `Instantiation { new, class, argument_list }` | `inst.class` | `inst.argument_list` (Option) |

Each `ArgumentList` has:
- `left_parenthesis: Span` — the `(` token, with `.start.offset` and `.end.offset`
- `right_parenthesis: Span` — the `)` token
- `arguments: TokenSeparatedSequence<Argument>` — the `.tokens` field contains comma `Token`s, each with `.start.offset`

The existing `expr_to_subject_text()` in `symbol_map.rs` already converts an AST expression into the subject string format that `resolve_callable` expects (e.g. `"$this->getService()"`, `"$this->prop"`, `"self"`, `"ClassName"`).

## Steps

### Step 1: Add `CallSite` to `SymbolMap`

**File: `src/symbol_map.rs`**

Add a new struct after `TemplateParamDef`:

```rust
pub(crate) struct CallSite {
    /// Byte offset immediately after the opening `(` (exclusive).
    /// The cursor must be > args_start to be "inside" the call.
    pub args_start: u32,
    /// Byte offset of the closing `)` (inclusive — cursor at this
    /// offset is still inside the call).  When the parser recovered
    /// from an unclosed paren, this is the span end the parser chose.
    pub args_end: u32,
    /// The call expression in the format `resolve_callable` expects:
    ///   - `"functionName"` for standalone function calls
    ///   - `"$subject->method"` for instance/null-safe method calls
    ///   - `"ClassName::method"` for static method calls
    ///   - `"new ClassName"` for constructor calls
    pub call_expression: String,
    /// Byte offsets of each top-level comma separator inside the
    /// argument list.  Used to compute the active parameter index:
    /// count how many comma offsets are < cursor offset.
    pub comma_offsets: Vec<u32>,
}
```

Add `pub call_sites: Vec<CallSite>` to the `SymbolMap` struct.

Add a lookup method:

```rust
pub fn find_enclosing_call_site(&self, offset: u32) -> Option<&CallSite> {
    // call_sites is sorted by args_start.  We want the innermost
    // (last) one whose range contains the cursor.
    self.call_sites.iter().rev().find(|cs| {
        offset > cs.args_start && offset <= cs.args_end
    })
}
```

### Step 2: Emit `CallSite` entries during AST extraction

**File: `src/symbol_map.rs`**

Thread a `&mut Vec<CallSite>` parameter through `extract_from_expression` and all the statement/class/method extraction functions (same pattern as the existing `spans`, `var_defs`, `scopes`, `template_defs` parameters).

Add a helper to build a `CallSite` from an `ArgumentList`:

```rust
fn emit_call_site(
    call_expression: String,
    argument_list: &ArgumentList<'_>,
    call_sites: &mut Vec<CallSite>,
) {
    if call_expression.is_empty() {
        return;
    }
    let args_start = argument_list.left_parenthesis.end.offset;
    let args_end = argument_list.right_parenthesis.start.offset;
    let comma_offsets: Vec<u32> = argument_list
        .arguments
        .tokens
        .iter()
        .map(|t| t.start.offset)
        .collect();
    call_sites.push(CallSite {
        args_start,
        args_end,
        call_expression,
        comma_offsets,
    });
}
```

In `extract_from_expression`, add `emit_call_site` calls for each variant:

- **`Call::Function(fc)`**: Use `expr_to_subject_text(fc.function)` as the call expression. That gives us the function name (e.g. `"strlen"`, `"App\\Helpers\\format"`). For variable-function calls like `$fn(`, it produces `"$fn"`.
- **`Call::Method(mc)`**: Build `format!("{}->{}", expr_to_subject_text(mc.object), method_name)`. This reuses `expr_to_subject_text` which already handles chains like `$this->prop`, `$this->getService()`, `app()`, etc.
- **`Call::NullSafeMethod(mc)`**: Same as Method but use `"->"` in the format string (signature help resolves null-safe the same way as regular arrow access). The `expr_to_subject_text` on the object side already includes `?->` for intermediate null-safe links.
- **`Call::StaticMethod(sc)`**: Build `format!("{}::{}", expr_to_subject_text(sc.class), method_name)`.
- **`Expression::Instantiation(inst)`**: Build `format!("new {}", expr_to_subject_text(inst.class))`. Only emit when `inst.argument_list` is `Some`.

In `extract_symbol_map`, sort `call_sites` by `args_start` after collection, and include it in the returned `SymbolMap`.

### Step 3: Rewrite `handle_signature_help` to use `CallSite`

**File: `src/signature_help.rs`**

Replace `detect_call_site` (the text-based scanner) with a new function that queries the symbol map:

```rust
fn detect_call_site_from_map(
    symbol_map: &SymbolMap,
    cursor_byte_offset: u32,
) -> Option<CallSiteContext> {
    let cs = symbol_map.find_enclosing_call_site(cursor_byte_offset)?;
    // Active parameter = number of commas before the cursor
    let active = cs.comma_offsets
        .iter()
        .filter(|&&comma| comma < cursor_byte_offset)
        .count() as u32;
    Some(CallSiteContext {
        call_expression: cs.call_expression.clone(),
        active_parameter: active,
    })
}
```

Update `handle_signature_help`:

1. Convert the LSP `Position` (line/character) to a byte offset (the helper `position_to_offset` already exists on `Backend`).
2. Look up the symbol map for this URI.
3. Call `detect_call_site_from_map`.
4. If the symbol map lookup fails (e.g. parse error ate the call site), **fall back** to the existing text-based `detect_call_site` so we don't regress on incomplete code.
5. Proceed with `resolve_signature` as before.

The fallback is important: when the user is mid-typing `foo(`, the parser may not have recovered the call node, so the symbol map won't contain it. The text-based scanner handles this because it only needs to find an unmatched `(`. Over time the fallback can be refined, but it keeps the refactor safe.

### Step 4: Fix `resolve_callable` to handle chain subjects

**File: `src/signature_help.rs`**

The `resolve_callable` method currently has this in the `->` branch:

```rust
let owner_classes: Vec<ClassInfo> =
    if subject == "$this" || subject == "self" || subject == "static" {
        current_class.cloned().into_iter().collect()
    } else if subject.starts_with('$') {
        // ... resolve_target_classes ...
    } else {
        vec![]   // ← PROBLEM: bare property names, call chains, etc. produce nothing
    };
```

Change the `else` branch to also call `resolve_target_classes`:

```rust
let owner_classes: Vec<ClassInfo> =
    if subject == "$this" || subject == "self" || subject == "static" {
        current_class.cloned().into_iter().collect()
    } else {
        Self::resolve_target_classes(subject, crate::AccessKind::Arrow, &rctx)
    };
```

`resolve_target_classes` already handles `$var`, `$this->prop`, `$this->method()`, `ClassName::make()`, bare class names, `app()`, etc. The only reason the old code had the `starts_with('$')` guard was that the text extractor never produced chain subjects. With AST-based extraction those subjects now appear, so the guard must go.

### Step 5: Handle `new ClassName->method` in `resolve_callable`

When `expr_to_subject_text` processes `(new Foo())->bar(`, it produces something like `"Foo->bar"` (Instantiation resolves to the class name). The `resolve_callable` `->` branch splits this as `subject = "Foo"`, `method = "bar"`. After step 4, `resolve_target_classes("Foo", ...)` hits the bare-class-name handler and returns `Foo`'s `ClassInfo`. No special-casing needed for this case beyond what step 4 already provides.

However, `"new ClassName"` prefix expressions (when `expr_to_subject_text` returns just the class name from an `Instantiation`) that chain through `->` need the `new` prefix stripped or the class name resolved as a bare name. Verify that `resolve_target_classes` handles this, and add a test.

### Step 6: Remove unused text-scanning helpers

After the refactor, the following items in `signature_help.rs` become dead code:

- `extract_call_expression_rich` (the function we prototyped in the first attempt)
- `extract_call_subject_for_sig`
- `count_top_level_commas` — the AST comma offsets replace this
- The imports from `named_args` (`find_enclosing_open_paren`, `position_to_char_offset`, `split_args_top_level`)
- The imports from `subject_extraction`

Keep `detect_call_site` (the text-based version) as the fallback, but rename it to `detect_call_site_text_fallback` to make the intent clear. It still uses `extract_call_expression` from `named_args.rs` which is fine for the fallback role.

Actually, the text-based fallback should ideally also produce correct chain expressions. For the fallback path we can continue using `extract_call_expression` from `named_args.rs` (the simple one) since the fallback only fires when the AST is broken, and in that case the simple extractor is "good enough" — it handles `$var->method(` and `Foo::bar(` and `new Foo(` and `functionName(` which covers the vast majority of unclosed-paren scenarios.

### Step 7: Update unit tests

**File: `src/signature_help.rs` (mod tests)**

The existing `detect_call_site` unit tests test the text scanner. These should be:

1. **Kept** for the renamed `detect_call_site_text_fallback` (they still test the fallback path).
2. **Supplemented** with new unit tests for `detect_call_site_from_map` that parse PHP, build a symbol map, and verify the call expression and active parameter. These tests should cover:
   - Simple function call: `foo($a, |)` → expression `"foo"`, active param 1
   - Method call: `$obj->bar(|)` → `"$obj->bar"`, active 0
   - Property chain: `$this->prop->method(|)` → `"$this->prop->method"`, active 0
   - Chained method result: `$obj->first()->second(|)` → `"$obj->first()->second"`, active 0
   - Static method: `Foo::bar(|)` → `"Foo::bar"`, active 0
   - Constructor: `new Foo(|)` → `"new Foo"`, active 0
   - Nested call (inner): `foo(bar(|))` → `"bar"`, active 0
   - Nested call (outer): `foo(bar(), |)` → `"foo"`, active 1
   - String with commas: `foo('a,b', |)` → `"foo"`, active 1 (commas inside strings are not in `tokens`)
   - `(new Foo())->method(|)` → something resolvable to Foo's method

### Step 8: Update integration tests

**File: `tests/signature_help.rs`**

Add integration tests that exercise the full pipeline (parse → symbol map → signature help → resolution):

- `property_chain_method_call` — `$outer->inner->process(`
- `this_property_chain_method_call` — `$this->service->execute(`
- `deep_property_chain_method_call` — `$garage->car->engine->start(`
- `method_return_chain` — `$b->where('name')->limit(`
- `this_method_return_chain` — `$this->repo->find(1)->update(`
- `function_return_chain` — `makeWidget()->configure(`
- `static_method_return_chain` — `Query::create()->filter(`
- `new_expression_chain` — `(new Printer())->print(`
- `nullsafe_method_call` — `$fmt?->format(`
- `property_then_method_chain` — `$this->app->logger->log(`
- `property_then_method_chain_second_param` — verify active parameter index
- `nested_call_correct_site` — cursor inside inner vs outer call

### Step 9: Update `example.php`

Add signature help examples to the `SignatureHelpDemo` class:

```php
class SignatureHelpDemo
{
    public function demo(): void
    {
        // Place cursor inside parentheses to see parameter hints.
        $user = new User('Alice', 'alice@example.com');
        createUser('Alice', 'alice@example.com');       // standalone function
        $user->setStatus(Status::Active);               // instance method
        User::findByEmail('alice@example.com');         // static method
        new User('Bob', 'bob@example.com');             // constructor

        // Chains — all of these now show signature help:
        $user->getProfile()->setBio('Hello');           // method return chain
        (new User('X', 'x@x.com'))->setStatus(Status::Active); // (new ...)->method
    }
}
```

### Step 10: CI checks, docs, cleanup

- `cargo test` — all existing + new tests green
- `cargo clippy -- -D warnings` and `cargo clippy --tests -- -D warnings`
- `cargo fmt --check`
- `php -l example.php`
- Update `docs/CHANGELOG.md` under `## [Unreleased]` / `### Fixed`
- Update `docs/todo.md` if there was an entry about signature help gaps (there isn't currently, but check)

## Session breakdown

| Session | Steps | Description |
|---------|-------|-------------|
| 1 | 1–2 | Add `CallSite` struct, thread it through extraction, emit call sites |
| 2 | 3–5 | Rewrite `handle_signature_help` to use symbol map with text fallback, fix `resolve_callable` |
| 3 | 6–8 | Remove dead code, write unit + integration tests |
| 4 | 9–10 | Update `example.php`, changelog, CI checks |

Sessions 3 and 4 could probably be combined.

## Risks and mitigations

**Parser error recovery.** When the user is mid-typing `foo(`, Mago may not produce a `Call::Function` node, so no `CallSite` is emitted. Mitigation: the text-based fallback catches this. Over time we can also try parsing a patched version (insert `);` at cursor) and building a temporary symbol map from it, but that's an optimization for later.

**`expr_to_subject_text` format mismatches.** The string format that `expr_to_subject_text` produces must match what `resolve_callable` / `resolve_target_classes` expects. This is already true for go-to-definition (which uses the same `subject_text` from `MemberAccess` spans), so the format is already battle-tested. One thing to watch: `expr_to_subject_text` includes argument text for conditional return types (e.g. `"app(User::class)"`). For signature help we want the call expression to include this so that `resolve_target_classes` can resolve conditional returns correctly.

**Performance.** Adding a `Vec<CallSite>` to every `SymbolMap` is negligible. A typical file has hundreds of call sites at most, and each is a small struct. The `find_enclosing_call_site` reverse linear scan is O(n) but n is small; if it ever matters we can switch to a binary search on sorted ranges.