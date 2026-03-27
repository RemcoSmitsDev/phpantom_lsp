//! Code actions — `textDocument/codeAction` handler.
//!
//! This module provides code actions for PHP files:
//!
//! - **Import class** — when the cursor is on an unresolved class name,
//!   offer to add a `use` statement for matching classes found in the
//!   class index, classmap, and stubs.
//! - **Remove unused import** — when the cursor is on (or a diagnostic
//!   overlaps with) an unused `use` statement, offer to remove it.
//!   Also offers a bulk "Remove all unused imports" action.
//! - **Implement missing methods** — when the cursor is inside a
//!   concrete class that extends an abstract class or implements an
//!   interface with unimplemented methods, offer to generate stubs.
//! - **Replace deprecated call** — when the cursor is on a deprecated
//!   function or method call that has a `#[Deprecated(replacement: "...")]`
//!   template, offer to rewrite the call to the suggested replacement.
//! - **PHPStan quickfixes** — a family of code actions that respond to
//!   PHPStan diagnostics.  See the [`phpstan`] submodule for details.
//! - **Change visibility** — when the cursor is on a method, property,
//!   constant, or promoted constructor parameter with an explicit
//!   visibility modifier, offer to change it to each alternative
//!   (`public` ↔ `protected` ↔ `private`).
//! - **Update docblock** — when the cursor is on a function or method
//!   whose existing docblock's `@param`/`@return` tags don't match the
//!   signature, offer to patch the docblock (add missing params, remove
//!   stale ones, reorder, fix contradicted types, remove redundant
//!   `@return void`).
//! - **Promote constructor parameter** — when the cursor is on a
//!   constructor parameter that has a matching property declaration and
//!   `$this->name = $name;` assignment, offer to convert it into a
//!   constructor-promoted property.
//! - **Generate constructor** — when the cursor is inside a class that
//!   has non-static properties but no `__construct` method, offer to
//!   generate a constructor that accepts each qualifying property as a
//!   parameter and assigns it.
//! - **Generate getter/setter** — when the cursor is on a property
//!   declaration, offer to generate `getX()` / `setX()` accessor
//!   methods (or `isX()` for `bool` properties).  Readonly properties
//!   only get a getter.  Static properties generate static methods.
//! - **Generate property hooks** — when the cursor is on a property
//!   declaration (PHP 8.4+), offer to generate `get` and/or `set`
//!   hooks inline on the property.  Static properties are skipped.
//!   Readonly properties only get a `get` hook.  Interface properties
//!   generate abstract hook signatures without bodies.

mod change_visibility;
pub(crate) mod cursor_context;
mod extract_function;
mod extract_variable;
mod generate_constructor;
mod generate_getter_setter;
mod generate_property_hooks;
pub(crate) mod implement_methods;
mod import_class;
mod inline_variable;
mod phpstan;
mod promote_constructor_param;
mod remove_unused_import;
mod replace_deprecated;
mod update_docblock;

use tower_lsp::lsp_types::*;

use crate::Backend;

impl Backend {
    /// Handle a `textDocument/codeAction` request.
    ///
    /// Returns a list of code actions applicable at the given range.
    pub fn handle_code_action(
        &self,
        uri: &str,
        content: &str,
        params: &CodeActionParams,
    ) -> Vec<CodeActionOrCommand> {
        let mut actions = Vec::new();

        // ── Import class ────────────────────────────────────────────────
        self.collect_import_class_actions(uri, content, params, &mut actions);

        // ── Remove unused imports ───────────────────────────────────────
        self.collect_remove_unused_import_actions(uri, content, params, &mut actions);

        // ── Implement missing methods ───────────────────────────────────
        self.collect_implement_methods_actions(uri, content, params, &mut actions);

        // ── Replace deprecated call ─────────────────────────────────────
        self.collect_replace_deprecated_actions(uri, content, params, &mut actions);

        // ── PHPStan-specific quickfixes ─────────────────────────────────
        self.collect_phpstan_actions(uri, content, params, &mut actions);

        // ── Change visibility ───────────────────────────────────────────
        self.collect_change_visibility_actions(uri, content, params, &mut actions);

        // ── Update docblock to match signature ──────────────────────────
        self.collect_update_docblock_actions(uri, content, params, &mut actions);

        // ── Promote constructor parameter ───────────────────────────────────
        self.collect_promote_constructor_param_actions(uri, content, params, &mut actions);

        // ── Generate constructor ────────────────────────────────────────────
        self.collect_generate_constructor_actions(uri, content, params, &mut actions);

        // ── Generate getter/setter ──────────────────────────────────────────
        self.collect_generate_getter_setter_actions(uri, content, params, &mut actions);

        // ── Generate property hooks (PHP 8.4+) ─────────────────────────────
        self.collect_generate_property_hook_actions(uri, content, params, &mut actions);

        // ── Extract variable ────────────────────────────────────────────
        self.collect_extract_variable_actions(uri, content, params, &mut actions);

        // ── Extract function / method ───────────────────────────────────
        self.collect_extract_function_actions(uri, content, params, &mut actions);

        // ── Inline variable ─────────────────────────────────────────────
        self.collect_inline_variable_actions(uri, content, params, &mut actions);

        actions
    }
}
