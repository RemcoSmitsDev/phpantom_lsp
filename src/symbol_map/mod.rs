//! Precomputed symbol-location map for a single PHP file.
//!
//! During `update_ast`, every navigable symbol occurrence (class reference,
//! member access, variable, function call, etc.) is recorded as a
//! [`SymbolSpan`] in a flat, sorted vec.  At request time a binary search
//! on this vec replaces character-level backward-walking and
//! provides instant rejection when the cursor lands on whitespace, a
//! string literal, a comment, or any other non-navigable token.
//!
//! The map also stores variable definition sites ([`VarDefSite`]) and
//! scope boundaries so that go-to-definition for `$variable` can be
//! answered entirely from precomputed data without re-parsing.
//!
//! Docblock type references (from `@param`, `@return`, `@var`,
//! `@template`, `@method`, etc.) are extracted by a dedicated string
//! scanner during the AST walk, since docblocks are trivia in the
//! `mago_syntax` AST and produce no expression/statement nodes.
//!
//! The module is split into submodules:
//!
//! - [`docblock`] — Docblock symbol extraction helpers (type span
//!   emission, `@template` / `@method` tag scanning, navigability
//!   filtering, and `get_docblock_text_with_offset`)
//! - [`extraction`] — AST walk that builds a [`SymbolMap`] from a
//!   parsed PHP program (`extract_symbol_map` and all
//!   `extract_from_*` helpers)

pub(crate) mod docblock;
mod extraction;

// Re-export the public entry point from extraction.
pub(crate) use extraction::extract_symbol_map;

// ─── Data structures ────────────────────────────────────────────────────────

/// A single navigable symbol occurrence in a file.
///
/// Stored in a sorted vec keyed by `start` offset so that a binary
/// search can locate the symbol (or gap) at any byte position in O(log n).
#[derive(Debug, Clone)]
pub(crate) struct SymbolSpan {
    /// Byte offset of the first character of this symbol token.
    pub start: u32,
    /// Byte offset one past the last character of this symbol token.
    pub end: u32,
    /// What kind of navigable symbol this is.
    pub kind: SymbolKind,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub(crate) enum SymbolKind {
    /// Class/interface/trait/enum name in a type context:
    /// type hint, `new Foo`, `extends Foo`, `implements Foo`,
    /// `use` statement target, `catch (Foo $e)`, etc.
    ClassReference {
        name: String,
        /// `true` when the original PHP source used a leading `\`
        /// (fully-qualified name).  When set, the resolver should use the
        /// name as-is without prepending the file's namespace.
        is_fqn: bool,
    },
    /// Class/interface/trait/enum name at its *declaration* site
    /// (`class Foo`, `interface Bar`, etc.).  Not navigable for
    /// go-to-definition (the cursor is already at the definition),
    /// but useful for document highlights and other features.
    ClassDeclaration { name: String },

    /// Member name on the RHS of `->`, `?->`, or `::`.
    /// `subject_text` is the source text of the LHS expression.
    MemberAccess {
        subject_text: String,
        member_name: String,
        is_static: bool,
        is_method_call: bool,
    },

    /// A `$variable` token (usage or definition site).
    Variable {
        /// Name without `$` prefix.
        name: String,
    },

    /// Standalone function call name (not a method call).
    FunctionCall { name: String },

    /// `self`, `static`, or `parent` keyword in a navigable context.
    SelfStaticParent { keyword: String },

    /// A constant name in a navigable context (`define()` name,
    /// class constant access, standalone constant reference).
    ConstantReference { name: String },

    /// A method, property, or constant name at its *declaration* site.
    ///
    /// Not navigable for go-to-definition or hover (the cursor is
    /// already at the definition), but needed for find-references and
    /// rename so that the declaration site participates in the match.
    MemberDeclaration {
        /// The member name (e.g. `"save"`, `"name"`, `"MAX_SIZE"`).
        /// For properties this is the name WITHOUT the `$` prefix.
        name: String,
        /// Whether this is a static member (`static function`, `static $prop`,
        /// or class constant — constants are always accessed statically).
        is_static: bool,
    },
}

// ─── Template parameter definition site structures ──────────────────────────

/// A `@template` parameter definition site discovered during docblock extraction.
///
/// Stored in `SymbolMap::template_defs`, sorted by `name_offset`.
/// When a `ClassReference` cannot be resolved to an actual class, the
/// resolver checks whether it matches a template parameter in scope and
/// jumps to the `@template` tag that declares it.
#[derive(Debug, Clone)]
pub(crate) struct TemplateParamDef {
    /// Byte offset of the template parameter *name* token (e.g. the `T`
    /// in `@template T of Foo`).
    pub name_offset: u32,
    /// Template parameter name (e.g. `"TKey"`, `"TModel"`).
    pub name: String,
    /// Upper bound from the `of` clause (e.g. `"array-key"` in
    /// `@template TKey of array-key`), or `None` when unbounded.
    pub bound: Option<String>,
    /// Variance annotation from the `@template` tag.
    pub variance: crate::types::TemplateVariance,
    /// Start of the scope where this template parameter is visible.
    /// For class-level templates this is the docblock start offset;
    /// for method/function-level templates it is the docblock start offset.
    pub scope_start: u32,
    /// End of the scope where this template parameter is visible.
    /// For class-level templates this is the class closing-brace offset;
    /// for method-level templates it is the method closing-brace offset;
    /// for function-level templates it is the function closing-brace offset.
    /// When the scope end cannot be determined (e.g. abstract method), this
    /// is set to `u32::MAX` so the parameter is visible to end-of-file.
    pub scope_end: u32,
}

// ─── Call site structures ───────────────────────────────────────────────────

/// A call expression site discovered during the AST walk.
///
/// Stored in `SymbolMap::call_sites`, sorted by `args_start`.
/// Used by signature help to find the innermost call whose argument
/// list contains the cursor and to compute the active parameter index
/// from precomputed comma offsets.
#[derive(Debug, Clone)]
pub(crate) struct CallSite {
    /// Byte offset immediately after the opening `(`.
    /// The cursor must be > `args_start` to be "inside" the call.
    pub args_start: u32,
    /// Byte offset of the closing `)`.
    /// When the parser recovered from an unclosed paren, this is the
    /// span end the parser chose.
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

// ─── Variable definition site structures ────────────────────────────────────

/// A variable definition site discovered during the AST walk.
///
/// Stored in `SymbolMap::var_defs`, sorted by `(scope_start, offset)`,
/// so that go-to-definition for `$var` can be answered entirely from
/// the precomputed map without any scanning at request time.
#[derive(Debug, Clone)]
pub(crate) struct VarDefSite {
    /// Byte offset of the `$var` token at the definition site.
    pub offset: u32,
    /// Variable name *without* `$` prefix.
    pub name: String,
    /// What kind of definition this is.
    pub kind: VarDefKind,
    /// Byte offset of the enclosing scope's opening brace (method body,
    /// function body, closure body) or `0` for top-level code.  Used to
    /// scope the backward search to the correct function/method.
    pub scope_start: u32,
    /// Byte offset from which this definition becomes "visible".
    ///
    /// For **assignments** (`$x = expr;`), this is the end of the
    /// statement — the RHS of an assignment still sees the *previous*
    /// definition of the variable, not the one being written.
    ///
    /// For **parameters**, **foreach**, **catch**, **static**, **global**,
    /// and **destructuring** definitions this equals `offset` (the
    /// definition is immediately visible).
    pub effective_from: u32,
}

/// The kind of variable definition site.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum VarDefKind {
    Assignment,
    Parameter,
    Property,
    Foreach,
    Catch,
    StaticDecl,
    GlobalDecl,
    ArrayDestructuring,
    ListDestructuring,
}

/// Per-file symbol location index.
///
/// The `spans` vec is sorted by `start` offset.  Gaps between spans
/// represent non-navigable regions (whitespace, operators, string
/// literal interiors, comment interiors, numeric literals, etc.).
/// When the cursor falls in a gap, the lookup returns `None`
/// immediately — no parsing, no text scanning.
#[derive(Debug, Clone, Default)]
pub(crate) struct SymbolMap {
    pub spans: Vec<SymbolSpan>,
    /// Variable definition sites, sorted by `(scope_start, offset)`.
    pub var_defs: Vec<VarDefSite>,
    /// Scope boundaries `(start_offset, end_offset)` for functions,
    /// methods, closures, and arrow functions.  Used by
    /// `find_enclosing_scope` to determine which scope the cursor is in.
    pub scopes: Vec<(u32, u32)>,
    /// Template parameter definition sites from `@template` docblock tags,
    /// sorted by `name_offset`.  Used to resolve template parameter names
    /// (e.g. `TKey`, `TModel`) that appear in docblock types but are not
    /// actual class names.
    pub template_defs: Vec<TemplateParamDef>,
    /// Call expression sites, sorted by `args_start`.
    /// Used by signature help to find the innermost call containing the
    /// cursor and to compute the active parameter index from AST data.
    pub call_sites: Vec<CallSite>,
}

impl SymbolMap {
    /// Find the symbol span (if any) that contains `offset`.
    ///
    /// Uses binary search on the sorted `spans` vec.  Returns `None`
    /// when the offset falls in a gap between spans (whitespace,
    /// string interior, comment interior, etc.).
    pub fn lookup(&self, offset: u32) -> Option<&SymbolSpan> {
        let idx = self.spans.partition_point(|s| s.start <= offset);
        if idx == 0 {
            return None;
        }
        let candidate = &self.spans[idx - 1];
        if offset < candidate.end {
            Some(candidate)
        } else {
            None
        }
    }

    /// Find the innermost scope that contains `offset`.
    ///
    /// Returns the `scope_start` (opening brace offset) of the innermost
    /// function/method/closure body that contains the cursor, or `0` when
    /// the cursor is in top-level code.
    pub fn find_enclosing_scope(&self, offset: u32) -> u32 {
        let mut best: u32 = 0;
        for &(start, end) in &self.scopes {
            if start <= offset && offset <= end && start > best {
                best = start;
            }
        }
        best
    }

    /// Find the `@template` definition for a template parameter name at
    /// the given cursor offset.
    ///
    /// Returns the closest (most specific) `TemplateParamDef` whose scope
    /// covers `cursor_offset` and whose name matches.  Method-level
    /// template params are preferred over class-level ones because their
    /// `scope_start` is larger (they are defined later in the file).
    pub fn find_template_def(&self, name: &str, cursor_offset: u32) -> Option<&TemplateParamDef> {
        // Iterate in reverse so that narrower / later-defined scopes
        // (method-level) are checked before broader ones (class-level).
        self.template_defs.iter().rev().find(|d| {
            d.name == name && cursor_offset >= d.scope_start && cursor_offset <= d.scope_end
        })
    }

    /// Find the most recent definition of `$var_name` before
    /// `cursor_offset` within the same scope.
    ///
    /// The caller should obtain `scope_start` via
    /// [`find_enclosing_scope`].
    pub fn find_var_definition(
        &self,
        var_name: &str,
        cursor_offset: u32,
        scope_start: u32,
    ) -> Option<&VarDefSite> {
        self.var_defs.iter().rev().find(|d| {
            d.name == var_name && d.scope_start == scope_start && d.effective_from <= cursor_offset
        })
    }

    /// Check whether `cursor_offset` is physically sitting on a variable
    /// definition token (the `$var` token of an assignment LHS, parameter,
    /// foreach binding, etc.).
    ///
    /// This is used to detect the "already at definition" case *before*
    /// the `effective_from`-based lookup, because the assignment LHS token
    /// exists at the definition site even though the definition hasn't
    /// "taken effect" yet (its `effective_from` is past the cursor).
    #[allow(dead_code)]
    pub fn is_at_var_definition(&self, var_name: &str, cursor_offset: u32) -> bool {
        self.var_def_kind_at(var_name, cursor_offset).is_some()
    }

    /// If the cursor is physically on a variable definition token, return
    /// the [`VarDefKind`] of that definition.
    ///
    /// This is a more informative variant of [`is_at_var_definition`] that
    /// lets the caller decide how to handle different definition kinds
    /// (e.g. skip type-hint navigation for parameters and catch variables).
    pub fn var_def_kind_at(&self, var_name: &str, cursor_offset: u32) -> Option<&VarDefKind> {
        // No scope check needed: if the cursor is physically within a
        // VarDefSite's `$var` token, it IS that definition — two different
        // definitions cannot occupy the same bytes.  This also correctly
        // handles parameters, which are physically before the opening
        // brace of the function body (outside `find_enclosing_scope`'s
        // range) but whose VarDefSite has scope_start set to that brace.
        self.var_defs
            .iter()
            .find(|d| {
                d.name == var_name
                    && cursor_offset >= d.offset
                    && cursor_offset < d.offset + 1 + d.name.len() as u32
            })
            .map(|d| &d.kind)
    }

    /// Find the innermost call site whose argument list contains `offset`.
    ///
    /// `call_sites` is sorted by `args_start`.  We want the innermost
    /// (last) one whose range contains the cursor, so we iterate in
    /// reverse and return the first match.
    pub fn find_enclosing_call_site(&self, offset: u32) -> Option<&CallSite> {
        self.call_sites
            .iter()
            .rev()
            .find(|cs| offset >= cs.args_start && offset <= cs.args_end)
    }
}

#[cfg(test)]
mod tests;
