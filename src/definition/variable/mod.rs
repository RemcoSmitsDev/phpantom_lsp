/// Variable definition resolution.
///
/// This module handles go-to-definition for `$variable` references,
/// jumping from a variable usage to its most recent assignment or
/// declaration site.
///
/// The primary path parses the file into an AST and walks the enclosing
/// scope to find the variable's definition site with byte-accurate
/// offsets.  This correctly handles:
///   - Array destructuring: `[$a, $b] = explode(',', $str)`
///   - List destructuring:  `list($a, $b) = func()`
///   - Multi-line parameter lists
///   - Nested scopes (closures, arrow functions)
///
/// Supported definition sites (searched bottom-up from cursor):
///   - **Assignment**: `$var = …` (but not `==` / `===`)
///   - **Parameter**: `Type $var` in a function/method signature
///   - **Foreach**: `as $var` / `=> $var`
///   - **Catch**: `catch (…Exception $var)`
///   - **Static / global**: `static $var` / `global $var`
///   - **Array destructuring**: `[$a, $b] = …` / `list($a, $b) = …`
///
/// When the cursor is already at the definition site (e.g. on a
/// parameter), the module falls through to type-hint resolution:
/// it extracts the type hint and jumps to the first class-like type
/// in it (e.g. `HtmlString` in `HtmlString|string $content`).
///
/// When the AST parse fails (malformed PHP, parser panic), the function
/// returns `None` rather than falling back to text heuristics.
///
/// ## Submodules
///
/// - [`var_definition`]: AST walk that finds variable definition sites
///   (assignments, parameters, foreach, catch, static/global,
///   destructuring).
/// - [`type_hint`]: AST walk that extracts the type hint string at a
///   variable's definition site (parameter, property, closure/arrow
///   function parameter).
use tower_lsp::lsp_types::*;

use crate::Backend;
use crate::composer;
use crate::parser::with_parsed_program;
use crate::util::{offset_to_position, position_to_offset};

mod type_hint;
mod var_definition;

use type_hint::find_type_hint_at_definition;
use var_definition::find_variable_definition_in_program;

// ═══════════════════════════════════════════════════════════════════════
// AST-based variable definition search result
// ═══════════════════════════════════════════════════════════════════════

/// Result of searching for a variable definition in the AST.
#[derive(Default)]
pub(super) enum VarDefSearchResult {
    /// No definition site found for this variable in the current scope.
    #[default]
    NotFound,
    /// The cursor is already sitting on the definition site (e.g. on a
    /// parameter declaration).  The caller should fall through to
    /// type-hint resolution.
    AtDefinition,
    /// Found a prior definition at the given byte offset.
    /// `offset` is the start of the `$var` token, `end_offset` is the end.
    FoundAt { offset: u32, end_offset: u32 },
}

impl Backend {
    // ──────────────────────────────────────────────────────────────────────
    // Variable go-to-definition helpers
    // ──────────────────────────────────────────────────────────────────────

    /// Find the most recent assignment or declaration of `$var_name` before
    /// `position` and return its location.
    ///
    /// Parses the file into an AST and walks the enclosing scope to find
    /// the definition site with exact byte offsets.  Returns `None` when
    /// the AST parse fails or no definition is found.
    pub(super) fn resolve_variable_definition(
        content: &str,
        uri: &str,
        position: Position,
        var_name: &str,
    ) -> Option<Location> {
        Self::resolve_variable_definition_ast(content, uri, position, var_name)?
    }

    /// AST-based variable definition resolution.
    ///
    /// Returns:
    /// - `Some(Some(location))` — found a prior definition, jump there
    /// - `Some(None)` — cursor is at a definition site (fall through to type-hint)
    ///   OR no definition found in the AST
    /// - `None` — AST parse failed
    fn resolve_variable_definition_ast(
        content: &str,
        uri: &str,
        position: Position,
        var_name: &str,
    ) -> Option<Option<Location>> {
        let cursor_offset = position_to_offset(content, position);

        let result = with_parsed_program(
            content,
            "resolve_variable_definition_ast",
            |program, content| {
                find_variable_definition_in_program(program, content, var_name, cursor_offset)
            },
        );

        match result {
            VarDefSearchResult::NotFound => {
                // The AST parse succeeded but found no definition — return
                // Some(None) so the caller knows not to fall back to text.
                Some(None)
            }
            VarDefSearchResult::AtDefinition => {
                // Cursor is at the definition — return Some(None) so the
                // caller falls through to type-hint resolution.
                Some(None)
            }
            VarDefSearchResult::FoundAt { offset, end_offset } => {
                let target_uri = Url::parse(uri).ok()?;
                let start_pos = offset_to_position(content, offset as usize);
                let end_pos = offset_to_position(content, end_offset as usize);
                Some(Some(Location {
                    uri: target_uri,
                    range: Range {
                        start: start_pos,
                        end: end_pos,
                    },
                }))
            }
        }
    }

    // ─── Type-Hint Resolution at Variable Definition ────────────────────

    /// When the cursor is on a variable that is already at its definition
    /// site (parameter, property, promoted property, catch variable),
    /// extract the type hint and jump to the first class-like type in it.
    ///
    /// For example, given `public readonly HtmlString|string $content,`
    /// this returns the location of the `HtmlString` class definition.
    pub(super) fn resolve_type_hint_at_variable(
        &self,
        uri: &str,
        content: &str,
        position: Position,
        var_name: &str,
    ) -> Option<Location> {
        self.resolve_type_hint_at_variable_ast(uri, content, position, var_name)
    }

    /// AST-based type-hint resolution: extract the type hint from the AST
    /// node where the variable is defined (parameter, catch, property).
    fn resolve_type_hint_at_variable_ast(
        &self,
        uri: &str,
        content: &str,
        position: Position,
        var_name: &str,
    ) -> Option<Location> {
        let cursor_offset = position_to_offset(content, position);

        let type_hint_str: Option<String> = with_parsed_program(
            content,
            "resolve_type_hint_at_variable_ast",
            |program, _| find_type_hint_at_definition(program, var_name, cursor_offset),
        );

        let type_hint = type_hint_str?;
        self.resolve_type_hint_string_to_location(uri, content, &type_hint)
    }

    /// Given a type-hint string (e.g. `HtmlString|string`, `?Foo`),
    /// resolve it to the definition location of the first class-like type.
    fn resolve_type_hint_string_to_location(
        &self,
        uri: &str,
        content: &str,
        type_hint: &str,
    ) -> Option<Location> {
        let scalars = [
            "string", "int", "float", "bool", "array", "callable", "iterable", "object", "mixed",
            "void", "never", "null", "false", "true", "self", "static", "parent",
        ];

        let class_name = type_hint
            .split(['|', '&'])
            .map(|t| t.trim_start_matches('?'))
            .find(|t| !t.is_empty() && !scalars.contains(&t.to_lowercase().as_str()))?;

        let ctx = self.file_context(uri);

        let fqn = Self::resolve_to_fqn(class_name, &ctx.use_map, &ctx.namespace);

        let mut candidates = vec![fqn];
        if class_name.contains('\\') && !candidates.contains(&class_name.to_string()) {
            candidates.push(class_name.to_string());
        }

        // Try same-file first.
        for fqn in &candidates {
            if let Some(location) = self.find_definition_in_ast_map(fqn, content, uri) {
                return Some(location);
            }
        }

        // Try Composer classmap: direct FQN → file path lookup.
        // This covers vendor classes that haven't been loaded into ast_map
        // yet (cold Ctrl+Click on a type hint never used in completion).
        for fqn in &candidates {
            if let Ok(cmap) = self.classmap.lock()
                && let Some(file_path) = cmap.get(fqn.as_str()).cloned()
            {
                drop(cmap);
                if let Some(location) = self.resolve_class_in_file(&file_path, fqn) {
                    return Some(location);
                }
            }
        }

        // Try PSR-4 resolution as a last resort.
        // PSR-4 mappings only cover user code (from composer.json).
        // Vendor classes are resolved by the classmap above.
        let workspace_root = self
            .workspace_root
            .lock()
            .ok()
            .and_then(|guard| guard.clone());

        if let Some(workspace_root) = workspace_root
            && let Ok(mappings) = self.psr4_mappings.lock()
        {
            for fqn in &candidates {
                if let Some(file_path) =
                    composer::resolve_class_path(&mappings, &workspace_root, fqn)
                    && let Some(location) = self.resolve_class_in_file(&file_path, fqn)
                {
                    return Some(location);
                }
            }
        }

        None
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Tests
// ═══════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests;
