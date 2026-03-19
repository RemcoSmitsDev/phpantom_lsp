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
//! - **PHPStan ignore** — when the cursor is on a line with a PHPStan
//!   error, offer to add `@phpstan-ignore <identifier>`.  When PHPStan
//!   reports an unnecessary ignore, offer to remove it.
//! - **Add @throws** — when PHPStan reports a
//!   `missingType.checkedException` error, offer to add a `@throws`
//!   tag to the enclosing function/method docblock and import the
//!   exception class if needed.
//! - **Remove @throws** — when PHPStan reports `throws.unusedType`
//!   (a `@throws` tag for a type that is never thrown) or
//!   `throws.notThrowable` (a `@throws` tag for a type that is not
//!   a subtype of `Throwable`), offer to remove the offending
//!   `@throws` line from the docblock.
//! - **Change visibility** — when the cursor is on a method, property,
//!   constant, or promoted constructor parameter with an explicit
//!   visibility modifier, offer to change it to each alternative
//!   (`public` ↔ `protected` ↔ `private`).
//! - **Update docblock** — when the cursor is on a function or method
//!   whose existing docblock's `@param`/`@return` tags don't match the
//!   signature, offer to patch the docblock (add missing params, remove
//!   stale ones, reorder, fix contradicted types, remove redundant
//!   `@return void`).

mod add_throws;
mod change_visibility;
pub(crate) mod implement_methods;
mod import_class;
mod phpstan_ignore;
mod remove_throws;
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

        // ── PHPStan ignore / remove unnecessary ignore ──────────────────
        self.collect_phpstan_ignore_actions(uri, content, params, &mut actions);

        // ── Add @throws for checked exceptions ──────────────────────────
        self.collect_add_throws_actions(uri, content, params, &mut actions);

        // ── Remove invalid/unused @throws ───────────────────────────────
        self.collect_remove_throws_actions(uri, content, params, &mut actions);

        // ── Change visibility ───────────────────────────────────────────
        self.collect_change_visibility_actions(uri, content, params, &mut actions);

        // ── Update docblock to match signature ──────────────────────────
        self.collect_update_docblock_actions(uri, content, params, &mut actions);

        actions
    }
}
