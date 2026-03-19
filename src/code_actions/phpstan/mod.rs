//! PHPStan code actions.
//!
//! Code actions that respond to PHPStan diagnostics. Each action parses
//! the PHPStan error message, extracts the relevant information, and
//! offers a quickfix that modifies the source code to resolve the issue.
//!
//! Currently implemented:
//!
//! - **Add `@throws`** — when PHPStan reports a
//!   `missingType.checkedException` error, offer to add a `@throws`
//!   tag to the enclosing function/method docblock and import the
//!   exception class if needed.
//! - **Remove `@throws`** — when PHPStan reports `throws.unusedType`
//!   or `throws.notThrowable`, offer to remove the offending `@throws`
//!   line from the docblock.
//! - **PHPStan ignore** — when the cursor is on a line with a PHPStan
//!   error, offer to add `@phpstan-ignore <identifier>`.  When PHPStan
//!   reports an unnecessary ignore, offer to remove it.

mod add_throws;
mod ignore;
mod remove_throws;

use tower_lsp::lsp_types::*;

use crate::Backend;

impl Backend {
    /// Collect all PHPStan-specific code actions.
    ///
    /// Called from [`Backend::handle_code_action`](super) to gather every
    /// PHPStan quickfix that applies at the given cursor/range.
    pub(crate) fn collect_phpstan_actions(
        &self,
        uri: &str,
        content: &str,
        params: &CodeActionParams,
        out: &mut Vec<CodeActionOrCommand>,
    ) {
        // ── PHPStan ignore / remove unnecessary ignore ──────────────
        self.collect_phpstan_ignore_actions(uri, content, params, out);

        // ── Add @throws for checked exceptions ──────────────────────
        self.collect_add_throws_actions(uri, content, params, out);

        // ── Remove invalid/unused @throws ───────────────────────────
        self.collect_remove_throws_actions(uri, content, params, out);
    }
}
