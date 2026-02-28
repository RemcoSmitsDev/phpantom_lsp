//! Precomputed symbol-location map for a single PHP file.
//!
//! During `update_ast`, every navigable symbol occurrence (class reference,
//! member access, variable, function call, etc.) is recorded as a
//! [`SymbolSpan`] in a flat, sorted vec.  At request time a binary search
//! on this vec replaces the character-level backward-walking in
//! `extract_word_at_position` / `extract_member_access_context` and
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

use mago_span::HasSpan;
use mago_syntax::ast::sequence::TokenSeparatedSequence;
use mago_syntax::ast::*;

use crate::docblock::types::{split_intersection_depth0, split_type_token, split_union_depth0};

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

// ─── Docblock helpers ───────────────────────────────────────────────────────

/// Non-navigable type names (scalars, pseudo-types, PHPStan utility types).
/// Types in this list are skipped when extracting docblock symbol spans.
const NON_NAVIGABLE: &[&str] = &[
    "int",
    "integer",
    "float",
    "double",
    "string",
    "bool",
    "boolean",
    "array",
    "object",
    "mixed",
    "void",
    "null",
    "true",
    "false",
    "never",
    "resource",
    "callable",
    "iterable",
    "static",
    "self",
    "parent",
    "class-string",
    "positive-int",
    "negative-int",
    "non-empty-string",
    "non-empty-array",
    "non-empty-list",
    "numeric-string",
    "numeric",
    "scalar",
    "list",
    "non-falsy-string",
    "literal-string",
    "callable-string",
    "array-key",
    "value-of",
    "key-of",
    "int-mask",
    "int-mask-of",
    "no-return",
    "empty",
    "number",
];

/// Returns `true` when a type name refers to a class/interface that the
/// user should be able to navigate to.
fn is_navigable_type(name: &str) -> bool {
    let base = name.split('<').next().unwrap_or(name);
    let base = base.split('{').next().unwrap_or(base);
    let lower = base.trim().to_ascii_lowercase();
    if lower.is_empty() {
        return false;
    }
    !NON_NAVIGABLE.contains(&lower.as_str())
}

/// Construct a `ClassReference` `SymbolSpan` from a raw identifier string.
///
/// Detects whether the name is fully-qualified (leading `\`) and sets
/// `is_fqn` accordingly.  The leading `\` is stripped from the stored
/// `name` in all cases.
fn class_ref_span(start: u32, end: u32, raw_name: &str) -> SymbolSpan {
    let is_fqn = raw_name.starts_with('\\');
    let name = raw_name.strip_prefix('\\').unwrap_or(raw_name).to_string();
    SymbolSpan {
        start,
        end,
        kind: SymbolKind::ClassReference { name, is_fqn },
    }
}

/// Like [`crate::docblock::get_docblock_text_for_node`] but also returns
/// the byte offset of the `/**` opening within the file.
pub fn get_docblock_text_with_offset<'a>(
    trivia: &'a [Trivia<'a>],
    content: &str,
    node: &impl HasSpan,
) -> Option<(&'a str, u32)> {
    let node_start = node.span().start.offset;
    let candidate_idx = trivia.partition_point(|t| t.span.start.offset < node_start);
    if candidate_idx == 0 {
        return None;
    }

    let content_bytes = content.as_bytes();
    let mut covered_from = node_start;

    for i in (0..candidate_idx).rev() {
        let t = &trivia[i];
        let t_end = t.span.end.offset;

        let gap = content_bytes
            .get(t_end as usize..covered_from as usize)
            .unwrap_or(&[]);
        if !gap.iter().all(u8::is_ascii_whitespace) {
            return None;
        }

        match t.kind {
            TriviaKind::DocBlockComment => {
                return Some((t.value, t.span.start.offset));
            }
            TriviaKind::WhiteSpace
            | TriviaKind::SingleLineComment
            | TriviaKind::MultiLineComment
            | TriviaKind::HashComment => {
                covered_from = t.span.start.offset;
            }
        }
    }

    None
}

/// Scan a docblock for type references in supported tags and emit
/// `SymbolSpan` entries with file-level byte offsets.
/// Extract navigable symbol spans from a docblock and return template
/// parameter definitions `(name, name_byte_offset)` found in `@template` tags.
///
/// The caller is responsible for wrapping the returned pairs into
/// [`TemplateParamDef`] entries with the appropriate scope.
fn extract_docblock_symbols(
    docblock: &str,
    base_offset: u32,
    spans: &mut Vec<SymbolSpan>,
) -> Vec<(String, u32)> {
    // Tags whose immediate next token is a type.
    const TYPE_FIRST_TAGS: &[&str] = &[
        "@param",
        "@return",
        "@throws",
        "@var",
        "@property",
        "@property-read",
        "@property-write",
        "@mixin",
        "@extends",
        "@implements",
        "@use",
        "@template-extends",
        "@template-implements",
        "@phpstan-return",
        "@phpstan-param",
        "@psalm-return",
        "@psalm-param",
        "@phpstan-var",
        "@psalm-var",
    ];

    let mut line_start: usize = 0;
    let mut template_params: Vec<(String, u32)> = Vec::new();

    for line in docblock.split('\n') {
        if let Some(at_pos) = line.find('@') {
            let tag_start_in_line = at_pos;
            let after_at = &line[tag_start_in_line..];

            let tag_end = after_at
                .find(|c: char| c.is_whitespace())
                .unwrap_or(after_at.len());
            let tag = &after_at[..tag_end];
            let tag_lower = tag.to_ascii_lowercase();

            if tag_lower == "@method" {
                extract_method_tag_symbols(
                    line,
                    tag_start_in_line,
                    tag_end,
                    line_start,
                    base_offset,
                    spans,
                );
                line_start += line.len() + 1;
                continue;
            }

            // @template tags: `@template T of BoundType`
            // @template-covariant / @template-contravariant are variants.
            // The first token after the tag is the parameter name (skip it),
            // then if followed by `of`, the next token is the bound type.
            if tag_lower == "@template"
                || tag_lower == "@template-covariant"
                || tag_lower == "@template-contravariant"
                || tag_lower == "@phpstan-template"
                || tag_lower == "@psalm-template"
                || tag_lower == "@phpstan-template-covariant"
                || tag_lower == "@psalm-template-covariant"
                || tag_lower == "@phpstan-template-contravariant"
                || tag_lower == "@psalm-template-contravariant"
            {
                if let Some(tp) = extract_template_tag_symbols(
                    after_at,
                    tag_end,
                    tag_start_in_line,
                    line_start,
                    base_offset,
                    spans,
                ) {
                    template_params.push(tp);
                }
                line_start += line.len() + 1;
                continue;
            }

            let is_type_first = TYPE_FIRST_TAGS.iter().any(|t| tag_lower == *t);

            if is_type_first {
                let after_tag = &after_at[tag_end..];
                let after_tag_trimmed = after_tag.trim_start();
                if !after_tag_trimmed.is_empty() {
                    let type_start_in_line =
                        tag_start_in_line + tag_end + (after_tag.len() - after_tag_trimmed.len());

                    let (type_token, _remainder) = split_type_token(after_tag_trimmed);
                    if !type_token.is_empty() {
                        emit_type_spans(
                            type_token,
                            base_offset + (line_start + type_start_in_line) as u32,
                            spans,
                        );
                    }
                }
            }
        }

        line_start += line.len() + 1;
    }

    template_params
}

/// Emit `SymbolSpan` entries for a type token, splitting unions and
/// intersections and skipping scalars.
fn emit_type_spans(type_token: &str, token_file_offset: u32, spans: &mut Vec<SymbolSpan>) {
    // Split on union `|` at depth 0.
    let union_parts = split_union_depth0(type_token);
    if union_parts.len() > 1 {
        let mut offset = 0usize;
        for part in &union_parts {
            if let Some(pos) = type_token[offset..].find(part) {
                let part_offset = offset + pos;
                emit_type_spans(part.trim(), token_file_offset + part_offset as u32, spans);
                offset = part_offset + part.len();
            }
        }
        return;
    }

    // Split on intersection `&` at depth 0.
    let intersection_parts = split_intersection_depth0(type_token);
    if intersection_parts.len() > 1 {
        let mut offset = 0usize;
        for part in &intersection_parts {
            if let Some(pos) = type_token[offset..].find(part) {
                let part_offset = offset + pos;
                emit_type_spans(part.trim(), token_file_offset + part_offset as u32, spans);
                offset = part_offset + part.len();
            }
        }
        return;
    }

    // Handle PHPStan conditional return types:
    //   ($paramName is Type ? TrueType : FalseType)
    //   ($paramName is not Type ? TrueType : FalseType)
    if type_token.starts_with('(') && type_token.ends_with(')') {
        let inner = &type_token[1..type_token.len() - 1];
        // Look for ` is ` at depth 0 to identify a conditional type.
        if let Some(is_pos) = find_keyword_depth0(inner, " is ") {
            let after_is = &inner[is_pos + 4..];
            // Skip optional `not ` keyword.
            let (after_keyword, keyword_extra) = if let Some(rest) = after_is.strip_prefix("not ") {
                (rest, 4usize)
            } else {
                (after_is, 0usize)
            };
            // Find ` ? ` at depth 0 to separate condition type from true branch.
            if let Some(q_pos) = find_keyword_depth0(after_keyword, " ? ") {
                let condition_type = after_keyword[..q_pos].trim();
                let after_q = &after_keyword[q_pos + 3..];
                // Find ` : ` at depth 0 to separate true branch from false branch.
                if let Some(c_pos) = find_keyword_depth0(after_q, " : ") {
                    let true_type = after_q[..c_pos].trim();
                    let false_type = after_q[c_pos + 3..].trim();

                    // Byte offset of the condition type within the original token.
                    // token_file_offset points at `(`, +1 for `(`, +is_pos for `$param`,
                    // +4 for ` is `, +keyword_extra for optional `not `.
                    let cond_offset_in_inner = is_pos + 4 + keyword_extra;
                    let cond_leading =
                        after_keyword[..q_pos].len() - after_keyword[..q_pos].trim_start().len();
                    let cond_file_offset =
                        token_file_offset + 1 + (cond_offset_in_inner + cond_leading) as u32;
                    if !condition_type.is_empty() {
                        emit_type_spans(condition_type, cond_file_offset, spans);
                    }

                    // True type offset.
                    let true_region = &after_q[..c_pos];
                    let true_leading = true_region.len() - true_region.trim_start().len();
                    let true_offset_in_inner = cond_offset_in_inner + q_pos + 3;
                    let true_file_offset =
                        token_file_offset + 1 + (true_offset_in_inner + true_leading) as u32;
                    if !true_type.is_empty() {
                        emit_type_spans(true_type, true_file_offset, spans);
                    }

                    // False type offset.
                    let false_region = &after_q[c_pos + 3..];
                    let false_leading = false_region.len() - false_region.trim_start().len();
                    let false_offset_in_inner = true_offset_in_inner + c_pos + 3;
                    let false_file_offset =
                        token_file_offset + 1 + (false_offset_in_inner + false_leading) as u32;
                    if !false_type.is_empty() {
                        emit_type_spans(false_type, false_file_offset, spans);
                    }
                    return;
                }
            }
        }
    }

    // Single type — strip nullable prefix.
    let (type_name, extra_offset) = if let Some(rest) = type_token.strip_prefix('?') {
        (rest, 1u32)
    } else {
        (type_token, 0u32)
    };

    if type_name.is_empty() {
        return;
    }

    // Handle `$this` as a self-reference (equivalent to `static`).
    if type_name == "$this" {
        let start = token_file_offset + extra_offset;
        let end = start + type_name.len() as u32;
        spans.push(SymbolSpan {
            start,
            end,
            kind: SymbolKind::SelfStaticParent {
                keyword: "static".to_string(),
            },
        });
        return;
    }

    // Strip generic suffix and array suffix to get the base type name.
    let base = type_name.split('<').next().unwrap_or(type_name);
    let base = base.split('{').next().unwrap_or(base);
    let base = base.strip_suffix("[]").unwrap_or(base);

    let name_for_check = base.strip_prefix('\\').unwrap_or(base).trim();

    if is_navigable_type(name_for_check) {
        let is_fqn = base.starts_with('\\');
        let name = base.strip_prefix('\\').unwrap_or(base).trim().to_string();
        let start = token_file_offset + extra_offset;
        let end = start + base.len() as u32;

        spans.push(SymbolSpan {
            start,
            end,
            kind: SymbolKind::ClassReference { name, is_fqn },
        });
    }

    // Recurse into generic type arguments: `Foo<Bar, Baz>` → process `Bar, Baz`.
    if let Some(gen_start) = type_name.find('<') {
        // Find the matching closing `>` (respecting nesting depth).
        let inner_start = gen_start + 1;
        let bytes = type_name.as_bytes();
        let mut depth = 1u32;
        let mut gen_end = inner_start;
        while gen_end < bytes.len() && depth > 0 {
            match bytes[gen_end] {
                b'<' => depth += 1,
                b'>' => depth -= 1,
                _ => {}
            }
            if depth > 0 {
                gen_end += 1;
            }
        }
        if depth == 0 {
            let inner = &type_name[inner_start..gen_end];
            // Split on `,` at depth 0 to get individual type arguments.
            let mut d = 0u32;
            let mut arg_start_idx = 0usize;
            let inner_bytes = inner.as_bytes();
            for i in 0..=inner_bytes.len() {
                let at_end = i == inner_bytes.len();
                let is_comma = !at_end && inner_bytes[i] == b',' && d == 0;
                if at_end && d == 0 || is_comma {
                    let arg = &inner[arg_start_idx..i];
                    let trimmed = arg.trim();
                    if !trimmed.is_empty() {
                        // Compute the offset of the trimmed arg within inner.
                        let leading_ws = arg.len() - arg.trim_start().len();
                        let arg_file_offset = token_file_offset
                            + extra_offset
                            + (inner_start + arg_start_idx + leading_ws) as u32;
                        emit_type_spans(trimmed, arg_file_offset, spans);
                    }
                    arg_start_idx = i + 1;
                } else if !at_end {
                    match inner_bytes[i] {
                        b'<' | b'(' | b'{' => d += 1,
                        b'>' | b')' | b'}' if d > 0 => d -= 1,
                        _ => {}
                    }
                }
            }
        }
    }
}

/// Find the byte position of `keyword` (e.g. `" is "`, `" ? "`, `" : "`)
/// within `s` at parenthesis/angle-bracket depth 0.  Returns `None` when
/// the keyword only appears inside nested delimiters.
fn find_keyword_depth0(s: &str, keyword: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    let kw_bytes = keyword.as_bytes();
    let kw_len = kw_bytes.len();
    if bytes.len() < kw_len {
        return None;
    }
    let mut depth = 0i32;
    for i in 0..=bytes.len() - kw_len {
        match bytes[i] {
            b'<' | b'(' | b'{' => depth += 1,
            b'>' | b')' | b'}' => {
                if depth > 0 {
                    depth -= 1;
                }
            }
            _ => {}
        }
        if depth == 0 && &bytes[i..i + kw_len] == kw_bytes {
            return Some(i);
        }
    }
    None
}

/// Handle `@template` (and variants) tags which have the form:
/// `@template T of BoundType`
///
/// The first token after the tag is the template parameter name — its
/// `(name, byte_offset)` pair is returned so the caller can record a
/// [`TemplateParamDef`].  If followed by the keyword `of`, the next
/// token is the bound type which is emitted as a `ClassReference`.
fn extract_template_tag_symbols(
    after_at: &str,
    tag_end: usize,
    tag_start_in_line: usize,
    line_start: usize,
    base_offset: u32,
    spans: &mut Vec<SymbolSpan>,
) -> Option<(String, u32)> {
    // Skip the tag itself to get to the template parameter name.
    let after_tag = &after_at[tag_end..];
    let after_tag_trimmed = after_tag.trim_start();
    if after_tag_trimmed.is_empty() {
        return None;
    }

    // The first non-whitespace token is the parameter name (e.g. `T`, `TNode`).
    let param_end = after_tag_trimmed
        .find(|c: char| c.is_whitespace())
        .unwrap_or(after_tag_trimmed.len());

    let param_name = &after_tag_trimmed[..param_end];
    // Compute the byte offset of the param name within the file.
    let param_offset_in_after_at = after_at.len() - after_tag_trimmed.len();
    let param_file_offset =
        base_offset + (line_start + tag_start_in_line + param_offset_in_after_at) as u32;

    let result = Some((param_name.to_string(), param_file_offset));

    let after_param = &after_tag_trimmed[param_end..];
    let after_param_trimmed = after_param.trim_start();

    // Check for `of` keyword.
    if !after_param_trimmed.starts_with("of ") && !after_param_trimmed.starts_with("of\t") {
        return result;
    }

    // Skip `of` and whitespace to get to the bound type.
    let after_of = &after_param_trimmed[2..]; // skip "of"
    let after_of_trimmed = after_of.trim_start();
    if after_of_trimmed.is_empty() {
        return result;
    }

    // Compute the offset of the bound type within the original line.
    // after_at starts at tag_start_in_line within the line.
    // after_of_trimmed starts at:
    //   tag_start_in_line + tag_end + (whitespace before param)
    //   + param_end + (whitespace before "of") + 2 + (whitespace after "of")
    let bound_offset_in_after_at = after_at.len() - after_of_trimmed.len();
    let bound_start_in_line = tag_start_in_line + bound_offset_in_after_at;

    let (type_token, _remainder) = split_type_token(after_of_trimmed);
    if !type_token.is_empty() {
        emit_type_spans(
            type_token,
            base_offset + (line_start + bound_start_in_line) as u32,
            spans,
        );
    }

    result
}

/// Handle `@method` tags which have the form:
/// `@method [static] ReturnType methodName(ParamType $p, ...)`
fn extract_method_tag_symbols(
    line: &str,
    tag_start_in_line: usize,
    tag_end_in_tag: usize,
    line_start: usize,
    base_offset: u32,
    spans: &mut Vec<SymbolSpan>,
) {
    let after_tag = &line[tag_start_in_line + tag_end_in_tag..];
    let after_tag_trimmed = after_tag.trim_start();
    if after_tag_trimmed.is_empty() {
        return;
    }

    let mut rest = after_tag_trimmed;
    let mut rest_offset =
        tag_start_in_line + tag_end_in_tag + (after_tag.len() - after_tag_trimmed.len());

    // Skip optional `static` keyword.
    if rest.starts_with("static ") || rest.starts_with("static\t") {
        let skip = "static".len();
        let after_static = rest[skip..].trim_start();
        let whitespace_len = rest.len() - skip - after_static.len();
        rest_offset += skip + whitespace_len;
        rest = after_static;
    }

    if rest.is_empty() {
        return;
    }

    // Extract return type.
    let (return_type, remainder) = split_type_token(rest);
    if !return_type.is_empty() {
        emit_type_spans(
            return_type,
            base_offset + (line_start + rest_offset) as u32,
            spans,
        );
    }

    // After the return type, find the `(` for parameter list.
    if let Some(paren_pos) = remainder.find('(') {
        let close = remainder[paren_pos..].find(')');
        if let Some(close_pos) = close {
            let inner = &remainder[paren_pos + 1..paren_pos + close_pos];
            let inner_offset_in_line = rest_offset
                + return_type.len()
                + (remainder.len() - rest[return_type.len()..].len())
                + paren_pos
                + 1;

            // Simple comma-split at depth 0 for parameters.
            let mut depth = 0i32;
            let mut param_start = 0usize;

            for (i, ch) in inner.char_indices() {
                match ch {
                    '<' | '(' | '{' => depth += 1,
                    '>' | ')' | '}' => depth -= 1,
                    ',' if depth == 0 => {
                        let param = inner[param_start..i].trim();
                        emit_method_param_type(
                            param,
                            line_start,
                            inner_offset_in_line,
                            param_start,
                            base_offset,
                            spans,
                        );
                        param_start = i + 1;
                    }
                    _ => {}
                }
            }
            // Last parameter.
            let param = inner[param_start..].trim();
            emit_method_param_type(
                param,
                line_start,
                inner_offset_in_line,
                param_start,
                base_offset,
                spans,
            );
        }
    }
}

/// Emit a type span for a single parameter in a `@method` tag's parameter list.
fn emit_method_param_type(
    param: &str,
    line_start: usize,
    inner_offset_in_line: usize,
    param_start_in_inner: usize,
    base_offset: u32,
    spans: &mut Vec<SymbolSpan>,
) {
    if param.is_empty() {
        return;
    }
    // A parameter looks like `TypeHint $varName` or just `$varName`.
    if let Some(dollar_pos) = param.find('$') {
        let type_part = param[..dollar_pos].trim();
        if !type_part.is_empty() {
            let type_start_in_param = param.find(type_part).unwrap_or(0);
            let (type_token, _) = split_type_token(type_part);
            if !type_token.is_empty() {
                let file_offset = base_offset
                    + (line_start
                        + inner_offset_in_line
                        + param_start_in_inner
                        + type_start_in_param) as u32;
                emit_type_spans(type_token, file_offset, spans);
            }
        }
    }
}

// ─── AST extraction ─────────────────────────────────────────────────────────

/// Build a [`SymbolMap`] from a parsed PHP program.
///
/// Walks every statement recursively and emits [`SymbolSpan`] entries for
/// every navigable symbol occurrence.
pub(crate) fn extract_symbol_map(program: &Program<'_>, content: &str) -> SymbolMap {
    let mut spans = Vec::new();
    let mut var_defs = Vec::new();
    let mut scopes = Vec::new();
    let mut template_defs = Vec::new();
    let mut call_sites = Vec::new();
    let trivias = program.trivia.as_slice();

    for stmt in program.statements.iter() {
        extract_from_statement(
            stmt,
            &mut spans,
            &mut var_defs,
            &mut scopes,
            &mut template_defs,
            &mut call_sites,
            trivias,
            content,
            0,
        );
    }

    // Sort by start offset for binary search.
    spans.sort_by_key(|s| s.start);

    // Deduplicate overlapping spans (keep the first / most specific).
    spans.dedup_by(|b, a| a.start == b.start && a.end == b.end);

    // Sort var_defs by (scope_start, offset) for efficient lookup.
    var_defs.sort_by(|a, b| {
        a.scope_start
            .cmp(&b.scope_start)
            .then(a.offset.cmp(&b.offset))
    });

    // Sort scopes by start offset.
    scopes.sort_by_key(|s| s.0);

    // Sort template_defs by name_offset for binary search / reverse scan.
    template_defs.sort_by_key(|d| d.name_offset);

    // Sort call_sites by args_start for reverse-scan lookup.
    call_sites.sort_by_key(|cs| cs.args_start);

    SymbolMap {
        spans,
        var_defs,
        scopes,
        template_defs,
        call_sites,
    }
}

// ─── Statement extractor ────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
fn extract_from_statement<'a>(
    stmt: &Statement<'a>,
    spans: &mut Vec<SymbolSpan>,
    var_defs: &mut Vec<VarDefSite>,
    scopes: &mut Vec<(u32, u32)>,
    template_defs: &mut Vec<TemplateParamDef>,
    call_sites: &mut Vec<CallSite>,
    trivias: &[Trivia<'a>],
    content: &str,
    scope_start: u32,
) {
    match stmt {
        Statement::Namespace(ns) => {
            for inner in ns.statements().iter() {
                extract_from_statement(
                    inner,
                    spans,
                    var_defs,
                    scopes,
                    template_defs,
                    call_sites,
                    trivias,
                    content,
                    scope_start,
                );
            }
        }
        Statement::Class(class) => {
            extract_from_class(
                class,
                spans,
                var_defs,
                scopes,
                template_defs,
                call_sites,
                trivias,
                content,
            );
        }
        Statement::Interface(iface) => {
            extract_from_interface(
                iface,
                spans,
                var_defs,
                scopes,
                template_defs,
                call_sites,
                trivias,
                content,
            );
        }
        Statement::Trait(trait_def) => {
            extract_from_trait(
                trait_def,
                spans,
                var_defs,
                scopes,
                template_defs,
                call_sites,
                trivias,
                content,
            );
        }
        Statement::Enum(enum_def) => {
            extract_from_enum(
                enum_def,
                spans,
                var_defs,
                scopes,
                template_defs,
                call_sites,
                trivias,
                content,
            );
        }
        Statement::Function(func) => {
            extract_from_function(
                func,
                spans,
                var_defs,
                scopes,
                template_defs,
                call_sites,
                trivias,
                content,
            );
        }
        Statement::Use(use_stmt) => {
            extract_from_use_statement(use_stmt, spans);
        }
        Statement::Expression(expr_stmt) => {
            extract_inline_docblock(expr_stmt, trivias, content, spans);
            extract_from_expression(
                expr_stmt.expression,
                spans,
                var_defs,
                scopes,
                template_defs,
                call_sites,
                trivias,
                content,
                scope_start,
            );
        }
        Statement::Return(ret) => {
            extract_inline_docblock(ret, trivias, content, spans);
            if let Some(val) = ret.value {
                extract_from_expression(
                    val,
                    spans,
                    var_defs,
                    scopes,
                    template_defs,
                    call_sites,
                    trivias,
                    content,
                    scope_start,
                );
            }
        }
        Statement::Echo(echo) => {
            extract_inline_docblock(echo, trivias, content, spans);
            for expr in echo.values.iter() {
                extract_from_expression(
                    expr,
                    spans,
                    var_defs,
                    scopes,
                    template_defs,
                    call_sites,
                    trivias,
                    content,
                    scope_start,
                );
            }
        }
        Statement::If(if_stmt) => {
            extract_from_expression(
                if_stmt.condition,
                spans,
                var_defs,
                scopes,
                template_defs,
                call_sites,
                trivias,
                content,
                scope_start,
            );
            extract_from_if_body(
                &if_stmt.body,
                spans,
                var_defs,
                scopes,
                template_defs,
                call_sites,
                trivias,
                content,
                scope_start,
            );
        }
        Statement::While(while_stmt) => {
            extract_from_expression(
                while_stmt.condition,
                spans,
                var_defs,
                scopes,
                template_defs,
                call_sites,
                trivias,
                content,
                scope_start,
            );
            extract_from_while_body(
                &while_stmt.body,
                spans,
                var_defs,
                scopes,
                template_defs,
                call_sites,
                trivias,
                content,
                scope_start,
            );
        }
        Statement::DoWhile(do_while) => {
            extract_from_statement(
                do_while.statement,
                spans,
                var_defs,
                scopes,
                template_defs,
                call_sites,
                trivias,
                content,
                scope_start,
            );
            extract_from_expression(
                do_while.condition,
                spans,
                var_defs,
                scopes,
                template_defs,
                call_sites,
                trivias,
                content,
                scope_start,
            );
        }
        Statement::For(for_stmt) => {
            for expr in for_stmt.initializations.iter() {
                extract_from_expression(
                    expr,
                    spans,
                    var_defs,
                    scopes,
                    template_defs,
                    call_sites,
                    trivias,
                    content,
                    scope_start,
                );
            }
            for expr in for_stmt.conditions.iter() {
                extract_from_expression(
                    expr,
                    spans,
                    var_defs,
                    scopes,
                    template_defs,
                    call_sites,
                    trivias,
                    content,
                    scope_start,
                );
            }
            for expr in for_stmt.increments.iter() {
                extract_from_expression(
                    expr,
                    spans,
                    var_defs,
                    scopes,
                    template_defs,
                    call_sites,
                    trivias,
                    content,
                    scope_start,
                );
            }
            extract_from_for_body(
                &for_stmt.body,
                spans,
                var_defs,
                scopes,
                template_defs,
                call_sites,
                trivias,
                content,
                scope_start,
            );
        }
        Statement::Foreach(foreach_stmt) => {
            extract_from_expression(
                foreach_stmt.expression,
                spans,
                var_defs,
                scopes,
                template_defs,
                call_sites,
                trivias,
                content,
                scope_start,
            );
            // key and value are accessed via the target.
            if let Some(key_expr) = foreach_stmt.target.key() {
                extract_from_expression(
                    key_expr,
                    spans,
                    var_defs,
                    scopes,
                    template_defs,
                    call_sites,
                    trivias,
                    content,
                    scope_start,
                );
                // Emit VarDefSite for foreach key variable.
                if let Expression::Variable(Variable::Direct(dv)) = key_expr {
                    let name = dv.name.strip_prefix('$').unwrap_or(dv.name).to_string();
                    let offset = dv.span.start.offset;
                    var_defs.push(VarDefSite {
                        offset,
                        name,
                        kind: VarDefKind::Foreach,
                        scope_start,
                        effective_from: offset,
                    });
                }
            }
            let value_expr = foreach_stmt.target.value();
            extract_from_expression(
                value_expr,
                spans,
                var_defs,
                scopes,
                template_defs,
                call_sites,
                trivias,
                content,
                scope_start,
            );
            // Emit VarDefSite for foreach value variable.
            if let Expression::Variable(Variable::Direct(dv)) = value_expr {
                let name = dv.name.strip_prefix('$').unwrap_or(dv.name).to_string();
                let offset = dv.span.start.offset;
                var_defs.push(VarDefSite {
                    offset,
                    name,
                    kind: VarDefKind::Foreach,
                    scope_start,
                    effective_from: offset,
                });
            }
            for inner in foreach_stmt.body.statements() {
                extract_from_statement(
                    inner,
                    spans,
                    var_defs,
                    scopes,
                    template_defs,
                    call_sites,
                    trivias,
                    content,
                    scope_start,
                );
            }
        }
        Statement::Switch(switch_stmt) => {
            extract_from_expression(
                switch_stmt.expression,
                spans,
                var_defs,
                scopes,
                template_defs,
                call_sites,
                trivias,
                content,
                scope_start,
            );
            extract_from_switch_body(
                &switch_stmt.body,
                spans,
                var_defs,
                scopes,
                template_defs,
                call_sites,
                trivias,
                content,
                scope_start,
            );
        }
        Statement::Try(try_stmt) => {
            for s in try_stmt.block.statements.iter() {
                extract_from_statement(
                    s,
                    spans,
                    var_defs,
                    scopes,
                    template_defs,
                    call_sites,
                    trivias,
                    content,
                    scope_start,
                );
            }
            for catch in try_stmt.catch_clauses.iter() {
                // Catch type hint is a navigable class reference.
                extract_from_hint(&catch.hint, spans);
                // The caught variable.
                if let Some(ref var) = catch.variable {
                    let var_name = var.name.strip_prefix('$').unwrap_or(var.name).to_string();
                    spans.push(SymbolSpan {
                        start: var.span.start.offset,
                        end: var.span.end.offset,
                        kind: SymbolKind::Variable {
                            name: var_name.clone(),
                        },
                    });
                    // Emit VarDefSite for catch variable.
                    let catch_var_offset = var.span.start.offset;
                    var_defs.push(VarDefSite {
                        offset: catch_var_offset,
                        name: var_name,
                        kind: VarDefKind::Catch,
                        scope_start,
                        effective_from: catch_var_offset,
                    });
                }
                for s in catch.block.statements.iter() {
                    extract_from_statement(
                        s,
                        spans,
                        var_defs,
                        scopes,
                        template_defs,
                        call_sites,
                        trivias,
                        content,
                        scope_start,
                    );
                }
            }
            if let Some(ref finally) = try_stmt.finally_clause {
                for s in finally.block.statements.iter() {
                    extract_from_statement(
                        s,
                        spans,
                        var_defs,
                        scopes,
                        template_defs,
                        call_sites,
                        trivias,
                        content,
                        scope_start,
                    );
                }
            }
        }
        Statement::Block(block) => {
            for s in block.statements.iter() {
                extract_from_statement(
                    s,
                    spans,
                    var_defs,
                    scopes,
                    template_defs,
                    call_sites,
                    trivias,
                    content,
                    scope_start,
                );
            }
        }
        Statement::Global(global) => {
            for var in global.variables.iter() {
                if let Variable::Direct(dv) = var {
                    let name = dv.name.strip_prefix('$').unwrap_or(dv.name).to_string();
                    spans.push(SymbolSpan {
                        start: dv.span.start.offset,
                        end: dv.span.end.offset,
                        kind: SymbolKind::Variable { name: name.clone() },
                    });
                    // Emit VarDefSite for global variable.
                    let global_offset = dv.span.start.offset;
                    var_defs.push(VarDefSite {
                        offset: global_offset,
                        name,
                        kind: VarDefKind::GlobalDecl,
                        scope_start,
                        effective_from: global_offset,
                    });
                }
            }
        }
        Statement::Static(static_stmt) => {
            for item in static_stmt.items.iter() {
                let dv = item.variable();
                let name = dv.name.strip_prefix('$').unwrap_or(dv.name).to_string();
                spans.push(SymbolSpan {
                    start: dv.span.start.offset,
                    end: dv.span.end.offset,
                    kind: SymbolKind::Variable { name: name.clone() },
                });
                // Emit VarDefSite for static variable.
                let static_offset = dv.span.start.offset;
                var_defs.push(VarDefSite {
                    offset: static_offset,
                    name,
                    kind: VarDefKind::StaticDecl,
                    scope_start,
                    effective_from: static_offset,
                });
            }
        }
        Statement::Unset(unset_stmt) => {
            for val in unset_stmt.values.iter() {
                extract_from_expression(
                    val,
                    spans,
                    var_defs,
                    scopes,
                    template_defs,
                    call_sites,
                    trivias,
                    content,
                    scope_start,
                );
            }
        }
        _ => {}
    }
}

// ─── If / While / For / Switch body helpers ─────────────────────────────────

#[allow(clippy::too_many_arguments)]
fn extract_from_if_body<'a>(
    body: &IfBody<'a>,
    spans: &mut Vec<SymbolSpan>,
    var_defs: &mut Vec<VarDefSite>,
    scopes: &mut Vec<(u32, u32)>,
    template_defs: &mut Vec<TemplateParamDef>,
    call_sites: &mut Vec<CallSite>,
    trivias: &[Trivia<'a>],
    content: &str,
    scope_start: u32,
) {
    match body {
        IfBody::Statement(stmt_body) => {
            extract_from_statement(
                stmt_body.statement,
                spans,
                var_defs,
                scopes,
                template_defs,
                call_sites,
                trivias,
                content,
                scope_start,
            );
            for else_if in stmt_body.else_if_clauses.iter() {
                extract_from_expression(
                    else_if.condition,
                    spans,
                    var_defs,
                    scopes,
                    template_defs,
                    call_sites,
                    trivias,
                    content,
                    scope_start,
                );
                extract_from_statement(
                    else_if.statement,
                    spans,
                    var_defs,
                    scopes,
                    template_defs,
                    call_sites,
                    trivias,
                    content,
                    scope_start,
                );
            }
            if let Some(ref else_clause) = stmt_body.else_clause {
                extract_from_statement(
                    else_clause.statement,
                    spans,
                    var_defs,
                    scopes,
                    template_defs,
                    call_sites,
                    trivias,
                    content,
                    scope_start,
                );
            }
        }
        IfBody::ColonDelimited(colon_body) => {
            for inner in colon_body.statements.iter() {
                extract_from_statement(
                    inner,
                    spans,
                    var_defs,
                    scopes,
                    template_defs,
                    call_sites,
                    trivias,
                    content,
                    scope_start,
                );
            }
            for else_if in colon_body.else_if_clauses.iter() {
                extract_from_expression(
                    else_if.condition,
                    spans,
                    var_defs,
                    scopes,
                    template_defs,
                    call_sites,
                    trivias,
                    content,
                    scope_start,
                );
                for inner in else_if.statements.iter() {
                    extract_from_statement(
                        inner,
                        spans,
                        var_defs,
                        scopes,
                        template_defs,
                        call_sites,
                        trivias,
                        content,
                        scope_start,
                    );
                }
            }
            if let Some(ref else_clause) = colon_body.else_clause {
                for inner in else_clause.statements.iter() {
                    extract_from_statement(
                        inner,
                        spans,
                        var_defs,
                        scopes,
                        template_defs,
                        call_sites,
                        trivias,
                        content,
                        scope_start,
                    );
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn extract_from_while_body<'a>(
    body: &WhileBody<'a>,
    spans: &mut Vec<SymbolSpan>,
    var_defs: &mut Vec<VarDefSite>,
    scopes: &mut Vec<(u32, u32)>,
    template_defs: &mut Vec<TemplateParamDef>,
    call_sites: &mut Vec<CallSite>,
    trivias: &[Trivia<'a>],
    content: &str,
    scope_start: u32,
) {
    match body {
        WhileBody::Statement(inner) => {
            extract_from_statement(
                inner,
                spans,
                var_defs,
                scopes,
                template_defs,
                call_sites,
                trivias,
                content,
                scope_start,
            );
        }
        WhileBody::ColonDelimited(colon_body) => {
            for inner in colon_body.statements.iter() {
                extract_from_statement(
                    inner,
                    spans,
                    var_defs,
                    scopes,
                    template_defs,
                    call_sites,
                    trivias,
                    content,
                    scope_start,
                );
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn extract_from_for_body<'a>(
    body: &ForBody<'a>,
    spans: &mut Vec<SymbolSpan>,
    var_defs: &mut Vec<VarDefSite>,
    scopes: &mut Vec<(u32, u32)>,
    template_defs: &mut Vec<TemplateParamDef>,
    call_sites: &mut Vec<CallSite>,
    trivias: &[Trivia<'a>],
    content: &str,
    scope_start: u32,
) {
    match body {
        ForBody::Statement(inner) => {
            extract_from_statement(
                inner,
                spans,
                var_defs,
                scopes,
                template_defs,
                call_sites,
                trivias,
                content,
                scope_start,
            );
        }
        ForBody::ColonDelimited(colon_body) => {
            for inner in colon_body.statements.iter() {
                extract_from_statement(
                    inner,
                    spans,
                    var_defs,
                    scopes,
                    template_defs,
                    call_sites,
                    trivias,
                    content,
                    scope_start,
                );
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn extract_from_switch_body<'a>(
    body: &SwitchBody<'a>,
    spans: &mut Vec<SymbolSpan>,
    var_defs: &mut Vec<VarDefSite>,
    scopes: &mut Vec<(u32, u32)>,
    template_defs: &mut Vec<TemplateParamDef>,
    call_sites: &mut Vec<CallSite>,
    trivias: &[Trivia<'a>],
    content: &str,
    scope_start: u32,
) {
    let cases = match body {
        SwitchBody::BraceDelimited(b) => &b.cases,
        SwitchBody::ColonDelimited(b) => &b.cases,
    };
    for case in cases.iter() {
        for inner in case.statements().iter() {
            extract_from_statement(
                inner,
                spans,
                var_defs,
                scopes,
                template_defs,
                call_sites,
                trivias,
                content,
                scope_start,
            );
        }
    }
}

// ─── Class-like extractors ──────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
fn extract_from_class<'a>(
    class: &Class<'a>,
    spans: &mut Vec<SymbolSpan>,
    var_defs: &mut Vec<VarDefSite>,
    scopes: &mut Vec<(u32, u32)>,
    template_defs: &mut Vec<TemplateParamDef>,
    call_sites: &mut Vec<CallSite>,
    trivias: &[Trivia<'a>],
    content: &str,
) {
    // Class name — declaration site, not a reference.
    let name = class.name.value.to_string();
    spans.push(SymbolSpan {
        start: class.name.span.start.offset,
        end: class.name.span.end.offset,
        kind: SymbolKind::ClassDeclaration { name },
    });

    // Attributes (PHP 8).
    extract_from_attribute_lists(
        &class.attribute_lists,
        spans,
        var_defs,
        scopes,
        template_defs,
        call_sites,
        trivias,
        content,
        0,
    );

    // Extends.
    if let Some(ref extends) = class.extends {
        for ident in extends.types.iter() {
            let raw = ident.value().to_string();
            spans.push(class_ref_span(
                ident.span().start.offset,
                ident.span().end.offset,
                &raw,
            ));
        }
    }

    // Implements.
    if let Some(ref implements) = class.implements {
        for ident in implements.types.iter() {
            let raw = ident.value().to_string();
            spans.push(class_ref_span(
                ident.span().start.offset,
                ident.span().end.offset,
                &raw,
            ));
        }
    }

    // Docblock.
    if let Some((doc_text, doc_offset)) = get_docblock_text_with_offset(trivias, content, class) {
        let tpl_params = extract_docblock_symbols(doc_text, doc_offset, spans);
        let scope_end = class.right_brace.end.offset;
        for (name, name_offset) in tpl_params {
            template_defs.push(TemplateParamDef {
                name_offset,
                name,
                scope_start: doc_offset,
                scope_end,
            });
        }
    }

    // Members.
    for member in class.members.iter() {
        extract_from_class_member(
            member,
            spans,
            var_defs,
            scopes,
            template_defs,
            call_sites,
            trivias,
            content,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn extract_from_interface<'a>(
    iface: &Interface<'a>,
    spans: &mut Vec<SymbolSpan>,
    var_defs: &mut Vec<VarDefSite>,
    scopes: &mut Vec<(u32, u32)>,
    template_defs: &mut Vec<TemplateParamDef>,
    call_sites: &mut Vec<CallSite>,
    trivias: &[Trivia<'a>],
    content: &str,
) {
    // Interface name — declaration site, not a reference.
    let name = iface.name.value.to_string();
    spans.push(SymbolSpan {
        start: iface.name.span.start.offset,
        end: iface.name.span.end.offset,
        kind: SymbolKind::ClassDeclaration { name },
    });

    // Attributes (PHP 8).
    extract_from_attribute_lists(
        &iface.attribute_lists,
        spans,
        var_defs,
        scopes,
        template_defs,
        call_sites,
        trivias,
        content,
        0,
    );

    if let Some(ref extends) = iface.extends {
        for ident in extends.types.iter() {
            let raw = ident.value().to_string();
            spans.push(class_ref_span(
                ident.span().start.offset,
                ident.span().end.offset,
                &raw,
            ));
        }
    }

    if let Some((doc_text, doc_offset)) = get_docblock_text_with_offset(trivias, content, iface) {
        let tpl_params = extract_docblock_symbols(doc_text, doc_offset, spans);
        let scope_end = iface.right_brace.end.offset;
        for (name, name_offset) in tpl_params {
            template_defs.push(TemplateParamDef {
                name_offset,
                name,
                scope_start: doc_offset,
                scope_end,
            });
        }
    }

    for member in iface.members.iter() {
        extract_from_class_member(
            member,
            spans,
            var_defs,
            scopes,
            template_defs,
            call_sites,
            trivias,
            content,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn extract_from_trait<'a>(
    trait_def: &Trait<'a>,
    spans: &mut Vec<SymbolSpan>,
    var_defs: &mut Vec<VarDefSite>,
    scopes: &mut Vec<(u32, u32)>,
    template_defs: &mut Vec<TemplateParamDef>,
    call_sites: &mut Vec<CallSite>,
    trivias: &[Trivia<'a>],
    content: &str,
) {
    // Trait name — declaration site, not a reference.
    let name = trait_def.name.value.to_string();
    spans.push(SymbolSpan {
        start: trait_def.name.span.start.offset,
        end: trait_def.name.span.end.offset,
        kind: SymbolKind::ClassDeclaration { name },
    });

    // Attributes (PHP 8).
    extract_from_attribute_lists(
        &trait_def.attribute_lists,
        spans,
        var_defs,
        scopes,
        template_defs,
        call_sites,
        trivias,
        content,
        0,
    );

    if let Some((doc_text, doc_offset)) = get_docblock_text_with_offset(trivias, content, trait_def)
    {
        let tpl_params = extract_docblock_symbols(doc_text, doc_offset, spans);
        let scope_end = trait_def.right_brace.end.offset;
        for (name, name_offset) in tpl_params {
            template_defs.push(TemplateParamDef {
                name_offset,
                name,
                scope_start: doc_offset,
                scope_end,
            });
        }
    }

    for member in trait_def.members.iter() {
        extract_from_class_member(
            member,
            spans,
            var_defs,
            scopes,
            template_defs,
            call_sites,
            trivias,
            content,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn extract_from_enum<'a>(
    enum_def: &Enum<'a>,
    spans: &mut Vec<SymbolSpan>,
    var_defs: &mut Vec<VarDefSite>,
    scopes: &mut Vec<(u32, u32)>,
    template_defs: &mut Vec<TemplateParamDef>,
    call_sites: &mut Vec<CallSite>,
    trivias: &[Trivia<'a>],
    content: &str,
) {
    // Enum name — declaration site, not a reference.
    let name = enum_def.name.value.to_string();
    spans.push(SymbolSpan {
        start: enum_def.name.span.start.offset,
        end: enum_def.name.span.end.offset,
        kind: SymbolKind::ClassDeclaration { name },
    });

    // Attributes (PHP 8).
    extract_from_attribute_lists(
        &enum_def.attribute_lists,
        spans,
        var_defs,
        scopes,
        template_defs,
        call_sites,
        trivias,
        content,
        0,
    );

    if let Some(ref implements) = enum_def.implements {
        for ident in implements.types.iter() {
            let raw = ident.value().to_string();
            spans.push(class_ref_span(
                ident.span().start.offset,
                ident.span().end.offset,
                &raw,
            ));
        }
    }

    if let Some((doc_text, doc_offset)) = get_docblock_text_with_offset(trivias, content, enum_def)
    {
        let tpl_params = extract_docblock_symbols(doc_text, doc_offset, spans);
        let scope_end = enum_def.right_brace.end.offset;
        for (name, name_offset) in tpl_params {
            template_defs.push(TemplateParamDef {
                name_offset,
                name,
                scope_start: doc_offset,
                scope_end,
            });
        }
    }

    for member in enum_def.members.iter() {
        extract_from_class_member(
            member,
            spans,
            var_defs,
            scopes,
            template_defs,
            call_sites,
            trivias,
            content,
        );
    }
}

// ─── Class member extractors ────────────────────────────────────────────────

/// Extract symbols from PHP 8 attribute lists (`#[Attr(...)]`).
///
/// Emits a `ClassReference` for the attribute class name and recurses
/// into argument expressions.
#[allow(clippy::too_many_arguments)]
fn extract_from_attribute_lists<'a>(
    attribute_lists: &mago_syntax::ast::sequence::Sequence<
        'a,
        mago_syntax::ast::attribute::AttributeList<'a>,
    >,
    spans: &mut Vec<SymbolSpan>,
    var_defs: &mut Vec<VarDefSite>,
    scopes: &mut Vec<(u32, u32)>,
    template_defs: &mut Vec<TemplateParamDef>,
    call_sites: &mut Vec<CallSite>,
    trivias: &[Trivia<'a>],
    content: &str,
    scope_start: u32,
) {
    for attr_list in attribute_lists.iter() {
        for attr in attr_list.attributes.iter() {
            // The attribute name (e.g. `\Illuminate\...\CollectedBy`).
            let raw = attr.name.value().to_string();
            spans.push(class_ref_span(
                attr.name.span().start.offset,
                attr.name.span().end.offset,
                &raw,
            ));

            // Attribute arguments.
            if let Some(ref arg_list) = attr.argument_list {
                extract_from_arguments(
                    &arg_list.arguments,
                    spans,
                    var_defs,
                    scopes,
                    template_defs,
                    call_sites,
                    trivias,
                    content,
                    scope_start,
                );
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn extract_from_class_member<'a>(
    member: &ClassLikeMember<'a>,
    spans: &mut Vec<SymbolSpan>,
    var_defs: &mut Vec<VarDefSite>,
    scopes: &mut Vec<(u32, u32)>,
    template_defs: &mut Vec<TemplateParamDef>,
    call_sites: &mut Vec<CallSite>,
    trivias: &[Trivia<'a>],
    content: &str,
) {
    match member {
        ClassLikeMember::Method(method) => {
            extract_from_method(
                method,
                spans,
                var_defs,
                scopes,
                template_defs,
                call_sites,
                trivias,
                content,
            );
        }
        ClassLikeMember::Property(property) => {
            extract_from_property(property, spans, var_defs, scopes, trivias, content);
        }
        ClassLikeMember::Constant(constant) => {
            extract_from_class_constant(
                constant,
                spans,
                var_defs,
                scopes,
                template_defs,
                call_sites,
                trivias,
                content,
            );
        }
        ClassLikeMember::TraitUse(trait_use) => {
            for ident in trait_use.trait_names.iter() {
                let raw = ident.value().to_string();
                spans.push(class_ref_span(
                    ident.span().start.offset,
                    ident.span().end.offset,
                    &raw,
                ));
            }
        }
        ClassLikeMember::EnumCase(enum_case) => {
            // Enum case values (backed enums).
            if let EnumCaseItem::Backed(backed) = &enum_case.item {
                extract_from_expression(
                    backed.value,
                    spans,
                    var_defs,
                    scopes,
                    template_defs,
                    call_sites,
                    trivias,
                    content,
                    0,
                );
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn extract_from_method<'a>(
    method: &Method<'a>,
    spans: &mut Vec<SymbolSpan>,
    var_defs: &mut Vec<VarDefSite>,
    scopes: &mut Vec<(u32, u32)>,
    template_defs: &mut Vec<TemplateParamDef>,
    call_sites: &mut Vec<CallSite>,
    trivias: &[Trivia<'a>],
    content: &str,
) {
    // Attributes (PHP 8) on the method.
    extract_from_attribute_lists(
        &method.attribute_lists,
        spans,
        var_defs,
        scopes,
        template_defs,
        call_sites,
        trivias,
        content,
        0,
    );

    // Docblock on the method.
    if let Some((doc_text, doc_offset)) = get_docblock_text_with_offset(trivias, content, method) {
        let tpl_params = extract_docblock_symbols(doc_text, doc_offset, spans);
        // Method-level template params: scope extends from the docblock to
        // the end of the method body (or the end of the docblock for
        // abstract methods without a body).
        let scope_end = if let MethodBody::Concrete(body) = &method.body {
            body.right_brace.end.offset
        } else {
            // Abstract / interface method — scope is just the docblock + signature.
            // Use the method span end as a reasonable bound.
            method.span().end.offset
        };
        for (name, name_offset) in tpl_params {
            template_defs.push(TemplateParamDef {
                name_offset,
                name,
                scope_start: doc_offset,
                scope_end,
            });
        }
    }

    // Determine scope_start for this method body.
    let method_scope_start = if let MethodBody::Concrete(body) = &method.body {
        let s = body.left_brace.start.offset;
        let e = body.right_brace.end.offset;
        scopes.push((s, e));
        s
    } else {
        0
    };

    // Parameter type hints and variable definition sites.
    for param in method.parameter_list.parameters.iter() {
        if let Some(ref hint) = param.hint {
            extract_from_hint(hint, spans);
        }
        let name = param
            .variable
            .name
            .strip_prefix('$')
            .unwrap_or(param.variable.name)
            .to_string();
        let param_offset = param.variable.span.start.offset;
        var_defs.push(VarDefSite {
            offset: param_offset,
            name,
            kind: VarDefKind::Parameter,
            scope_start: method_scope_start,
            effective_from: param_offset,
        });
        if let Some(ref default) = param.default_value {
            extract_from_expression(
                default.value,
                spans,
                var_defs,
                scopes,
                template_defs,
                call_sites,
                trivias,
                content,
                method_scope_start,
            );
        }
    }

    // Return type hint.
    if let Some(ref return_type) = method.return_type_hint {
        extract_from_hint(&return_type.hint, spans);
    }

    // Method body.
    if let MethodBody::Concrete(body) = &method.body {
        for stmt in body.statements.iter() {
            extract_from_statement(
                stmt,
                spans,
                var_defs,
                scopes,
                template_defs,
                call_sites,
                trivias,
                content,
                method_scope_start,
            );
        }
    }
}

/// Extract docblock symbols from an inline `/** @var ... */` comment
/// attached to a body-level statement (expression, return, echo, etc.).
///
/// These comments are stored as trivia preceding the statement token.
/// Unlike class/method docblocks, inline `@var` annotations don't define
/// template parameters — we only care about the type spans they contain.
fn extract_inline_docblock<'a>(
    node: &impl HasSpan,
    trivias: &[Trivia<'a>],
    content: &str,
    spans: &mut Vec<SymbolSpan>,
) {
    if let Some((doc_text, doc_offset)) = get_docblock_text_with_offset(trivias, content, node) {
        let _tpl = extract_docblock_symbols(doc_text, doc_offset, spans);
    }
}

fn extract_from_property<'a>(
    property: &Property<'a>,
    spans: &mut Vec<SymbolSpan>,
    var_defs: &mut Vec<VarDefSite>,
    _scopes: &mut Vec<(u32, u32)>,
    trivias: &[Trivia<'a>],
    content: &str,
) {
    // NOTE: Property attributes (PHP 8) are not extracted here because
    // `Property` is an enum without a direct `attribute_lists` field.
    // This can be added later by matching on the property variant.

    // Docblock.
    if let Some((doc_text, doc_offset)) = get_docblock_text_with_offset(trivias, content, property)
    {
        // Property docblocks don't define template params, but we still
        // need to consume the return value.
        let _tpl = extract_docblock_symbols(doc_text, doc_offset, spans);
    }

    // Property type hint.
    if let Some(hint) = property.hint() {
        extract_from_hint(hint, spans);
    }

    // Property variable names.
    for var in property.variables().iter() {
        let name = var.name.strip_prefix('$').unwrap_or(var.name).to_string();
        let var_offset = var.span.start.offset;
        spans.push(SymbolSpan {
            start: var_offset,
            end: var.span.end.offset,
            kind: SymbolKind::Variable { name: name.clone() },
        });
        var_defs.push(VarDefSite {
            offset: var_offset,
            name,
            kind: VarDefKind::Property,
            scope_start: 0,
            effective_from: var_offset,
        });
    }
}

#[allow(clippy::too_many_arguments)]
fn extract_from_class_constant<'a>(
    constant: &ClassLikeConstant<'a>,
    spans: &mut Vec<SymbolSpan>,
    var_defs: &mut Vec<VarDefSite>,
    scopes: &mut Vec<(u32, u32)>,
    template_defs: &mut Vec<TemplateParamDef>,
    call_sites: &mut Vec<CallSite>,
    trivias: &[Trivia<'a>],
    content: &str,
) {
    // Docblock.
    if let Some((doc_text, doc_offset)) = get_docblock_text_with_offset(trivias, content, constant)
    {
        let _tpl = extract_docblock_symbols(doc_text, doc_offset, spans);
    }

    // Type hint on constant (PHP 8.3+).
    if let Some(ref hint) = constant.hint {
        extract_from_hint(hint, spans);
    }

    // Constant value expressions.
    for item in constant.items.iter() {
        extract_from_expression(
            item.value,
            spans,
            var_defs,
            scopes,
            template_defs,
            call_sites,
            trivias,
            content,
            0,
        );
    }
}

// ─── Function extractor ─────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
fn extract_from_function<'a>(
    func: &Function<'a>,
    spans: &mut Vec<SymbolSpan>,
    var_defs: &mut Vec<VarDefSite>,
    scopes: &mut Vec<(u32, u32)>,
    template_defs: &mut Vec<TemplateParamDef>,
    call_sites: &mut Vec<CallSite>,
    trivias: &[Trivia<'a>],
    content: &str,
) {
    // Attributes (PHP 8) on the function.
    extract_from_attribute_lists(
        &func.attribute_lists,
        spans,
        var_defs,
        scopes,
        template_defs,
        call_sites,
        trivias,
        content,
        0,
    );

    // Function name as a navigable reference.
    let name = func.name.value.to_string();
    spans.push(SymbolSpan {
        start: func.name.span.start.offset,
        end: func.name.span.end.offset,
        kind: SymbolKind::FunctionCall { name },
    });

    // Docblock.
    if let Some((doc_text, doc_offset)) = get_docblock_text_with_offset(trivias, content, func) {
        let tpl_params = extract_docblock_symbols(doc_text, doc_offset, spans);
        let scope_end = func.body.right_brace.end.offset;
        for (name, name_offset) in tpl_params {
            template_defs.push(TemplateParamDef {
                name_offset,
                name,
                scope_start: doc_offset,
                scope_end,
            });
        }
    }

    // Determine scope_start for this function body.
    let func_scope_start = func.body.left_brace.start.offset;
    let func_scope_end = func.body.right_brace.end.offset;
    scopes.push((func_scope_start, func_scope_end));

    // Parameter type hints and variable definition sites.
    for param in func.parameter_list.parameters.iter() {
        if let Some(ref hint) = param.hint {
            extract_from_hint(hint, spans);
        }
        // Emit VarDefSite for each parameter.
        let pname = param
            .variable
            .name
            .strip_prefix('$')
            .unwrap_or(param.variable.name)
            .to_string();
        let param_offset = param.variable.span.start.offset;
        var_defs.push(VarDefSite {
            offset: param_offset,
            name: pname,
            kind: VarDefKind::Parameter,
            scope_start: func_scope_start,
            effective_from: param_offset,
        });
        if let Some(ref default) = param.default_value {
            extract_from_expression(
                default.value,
                spans,
                var_defs,
                scopes,
                template_defs,
                call_sites,
                trivias,
                content,
                func_scope_start,
            );
        }
    }

    // Return type hint.
    if let Some(ref return_type) = func.return_type_hint {
        extract_from_hint(&return_type.hint, spans);
    }

    // Function body.
    for stmt in func.body.statements.iter() {
        extract_from_statement(
            stmt,
            spans,
            var_defs,
            scopes,
            template_defs,
            call_sites,
            trivias,
            content,
            func_scope_start,
        );
    }
}

// ─── Use statement extractor ────────────────────────────────────────────────

fn extract_from_use_statement(use_stmt: &Use<'_>, spans: &mut Vec<SymbolSpan>) {
    fn register_use_item(item: &UseItem<'_>, spans: &mut Vec<SymbolSpan>) {
        let raw = item.name.value().to_string();
        spans.push(class_ref_span(
            item.name.span().start.offset,
            item.name.span().end.offset,
            &raw,
        ));
    }

    match &use_stmt.items {
        UseItems::Sequence(seq) => {
            for use_item in seq.items.iter() {
                register_use_item(use_item, spans);
            }
        }
        UseItems::TypedSequence(typed_seq) => {
            // Only class imports (not function/const).
            if !typed_seq.r#type.is_function() && !typed_seq.r#type.is_const() {
                for use_item in typed_seq.items.iter() {
                    register_use_item(use_item, spans);
                }
            }
        }
        UseItems::TypedList(list) => {
            if !list.r#type.is_function() && !list.r#type.is_const() {
                for use_item in list.items.iter() {
                    register_use_item(use_item, spans);
                }
            }
        }
        UseItems::MixedList(list) => {
            for use_item in list.items.iter() {
                // MixedList items are MaybeTypedUseItem — skip function/const.
                if let Some(ref typ) = use_item.r#type
                    && (typ.is_function() || typ.is_const())
                {
                    continue;
                }
                register_use_item(&use_item.item, spans);
            }
        }
    }
}

// ─── Type hint extractor ────────────────────────────────────────────────────

fn extract_from_hint(hint: &Hint<'_>, spans: &mut Vec<SymbolSpan>) {
    match hint {
        Hint::Identifier(ident) => {
            let raw = ident.value().to_string();
            let name_clean = raw.strip_prefix('\\').unwrap_or(&raw).to_string();
            if is_navigable_type(&name_clean) {
                spans.push(class_ref_span(
                    ident.span().start.offset,
                    ident.span().end.offset,
                    &raw,
                ));
            }
        }
        Hint::Nullable(nullable) => {
            extract_from_hint(nullable.hint, spans);
        }
        Hint::Union(union) => {
            extract_from_hint(union.left, spans);
            extract_from_hint(union.right, spans);
        }
        Hint::Intersection(intersection) => {
            extract_from_hint(intersection.left, spans);
            extract_from_hint(intersection.right, spans);
        }
        Hint::Parenthesized(paren) => {
            extract_from_hint(paren.hint, spans);
        }
        Hint::Self_(kw) => {
            spans.push(SymbolSpan {
                start: kw.span.start.offset,
                end: kw.span.end.offset,
                kind: SymbolKind::SelfStaticParent {
                    keyword: "self".to_string(),
                },
            });
        }
        Hint::Static(kw) => {
            spans.push(SymbolSpan {
                start: kw.span.start.offset,
                end: kw.span.end.offset,
                kind: SymbolKind::SelfStaticParent {
                    keyword: "static".to_string(),
                },
            });
        }
        Hint::Parent(kw) => {
            spans.push(SymbolSpan {
                start: kw.span.start.offset,
                end: kw.span.end.offset,
                kind: SymbolKind::SelfStaticParent {
                    keyword: "parent".to_string(),
                },
            });
        }
        // Scalar / built-in type hints are not navigable.
        _ => {}
    }
}

// ─── Expression extractor ───────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
fn extract_from_expression<'a>(
    expr: &'a Expression<'a>,
    spans: &mut Vec<SymbolSpan>,
    var_defs: &mut Vec<VarDefSite>,
    scopes: &mut Vec<(u32, u32)>,
    template_defs: &mut Vec<TemplateParamDef>,
    call_sites: &mut Vec<CallSite>,
    trivias: &[Trivia<'a>],
    content: &str,
    scope_start: u32,
) {
    match expr {
        // ── Variables ──
        Expression::Variable(Variable::Direct(dv)) => {
            let raw = dv.name;
            if raw == "$this" {
                // `$this` is semantically equivalent to `static` for
                // go-to-definition — resolve it to the enclosing class.
                spans.push(SymbolSpan {
                    start: dv.span.start.offset,
                    end: dv.span.end.offset,
                    kind: SymbolKind::SelfStaticParent {
                        keyword: "static".to_string(),
                    },
                });
            } else {
                let name = raw.strip_prefix('$').unwrap_or(raw).to_string();
                spans.push(SymbolSpan {
                    start: dv.span.start.offset,
                    end: dv.span.end.offset,
                    kind: SymbolKind::Variable { name },
                });
            }
        }

        // ── self / static / parent keywords ──
        Expression::Self_(kw) => {
            spans.push(SymbolSpan {
                start: kw.span.start.offset,
                end: kw.span.end.offset,
                kind: SymbolKind::SelfStaticParent {
                    keyword: "self".to_string(),
                },
            });
        }
        Expression::Static(kw) => {
            spans.push(SymbolSpan {
                start: kw.span.start.offset,
                end: kw.span.end.offset,
                kind: SymbolKind::SelfStaticParent {
                    keyword: "static".to_string(),
                },
            });
        }
        Expression::Parent(kw) => {
            spans.push(SymbolSpan {
                start: kw.span.start.offset,
                end: kw.span.end.offset,
                kind: SymbolKind::SelfStaticParent {
                    keyword: "parent".to_string(),
                },
            });
        }

        // ── Identifiers (standalone class/constant references) ──
        Expression::Identifier(ident) => {
            let name = ident.value().to_string();
            let name_clean = name.strip_prefix('\\').unwrap_or(&name).to_string();
            if is_navigable_type(&name_clean) {
                spans.push(class_ref_span(
                    ident.span().start.offset,
                    ident.span().end.offset,
                    &name,
                ));
            }
        }

        // ── Instantiation: `new Foo(...)` ──
        Expression::Instantiation(inst) => {
            match inst.class {
                Expression::Identifier(ident) => {
                    let raw = ident.value().to_string();
                    spans.push(class_ref_span(
                        ident.span().start.offset,
                        ident.span().end.offset,
                        &raw,
                    ));
                }
                Expression::Self_(kw) => {
                    spans.push(SymbolSpan {
                        start: kw.span.start.offset,
                        end: kw.span.end.offset,
                        kind: SymbolKind::SelfStaticParent {
                            keyword: "self".to_string(),
                        },
                    });
                }
                Expression::Static(kw) => {
                    spans.push(SymbolSpan {
                        start: kw.span.start.offset,
                        end: kw.span.end.offset,
                        kind: SymbolKind::SelfStaticParent {
                            keyword: "static".to_string(),
                        },
                    });
                }
                Expression::Parent(kw) => {
                    spans.push(SymbolSpan {
                        start: kw.span.start.offset,
                        end: kw.span.end.offset,
                        kind: SymbolKind::SelfStaticParent {
                            keyword: "parent".to_string(),
                        },
                    });
                }
                _ => {
                    extract_from_expression(
                        inst.class,
                        spans,
                        var_defs,
                        scopes,
                        template_defs,
                        call_sites,
                        trivias,
                        content,
                        scope_start,
                    );
                }
            }
            if let Some(ref args) = inst.argument_list {
                // Emit call site for constructor: `new ClassName(...)`
                let class_text = expr_to_subject_text(inst.class);
                if !class_text.is_empty() {
                    emit_call_site(format!("new {}", class_text), args, call_sites);
                }
                extract_from_arguments(
                    &args.arguments,
                    spans,
                    var_defs,
                    scopes,
                    template_defs,
                    call_sites,
                    trivias,
                    content,
                    scope_start,
                );
            }
        }

        // ── Function calls ──
        Expression::Call(call) => match call {
            Call::Function(func_call) => {
                match func_call.function {
                    Expression::Identifier(ident) => {
                        let name = ident.value().to_string();
                        let name_clean = name.strip_prefix('\\').unwrap_or(&name).to_string();
                        spans.push(SymbolSpan {
                            start: ident.span().start.offset,
                            end: ident.span().end.offset,
                            kind: SymbolKind::FunctionCall { name: name_clean },
                        });
                    }
                    _ => {
                        extract_from_expression(
                            func_call.function,
                            spans,
                            var_defs,
                            scopes,
                            template_defs,
                            call_sites,
                            trivias,
                            content,
                            scope_start,
                        );
                    }
                }
                // Emit call site for function call
                let func_text = expr_to_subject_text(func_call.function);
                if !func_text.is_empty() {
                    emit_call_site(func_text, &func_call.argument_list, call_sites);
                }
                extract_from_arguments(
                    &func_call.argument_list.arguments,
                    spans,
                    var_defs,
                    scopes,
                    template_defs,
                    call_sites,
                    trivias,
                    content,
                    scope_start,
                );
            }
            Call::Method(method_call) => {
                let subject_text = expr_to_subject_text(method_call.object);
                extract_from_expression(
                    method_call.object,
                    spans,
                    var_defs,
                    scopes,
                    template_defs,
                    call_sites,
                    trivias,
                    content,
                    scope_start,
                );

                if let ClassLikeMemberSelector::Identifier(ident) = &method_call.method {
                    let member_name = ident.value.to_string();
                    // Emit call site for method call: `$subject->method(...)`
                    emit_call_site(
                        format!("{}->{}", &subject_text, &member_name),
                        &method_call.argument_list,
                        call_sites,
                    );
                    spans.push(SymbolSpan {
                        start: ident.span.start.offset,
                        end: ident.span.end.offset,
                        kind: SymbolKind::MemberAccess {
                            subject_text,
                            member_name,
                            is_static: false,
                            is_method_call: true,
                        },
                    });
                }
                extract_from_arguments(
                    &method_call.argument_list.arguments,
                    spans,
                    var_defs,
                    scopes,
                    template_defs,
                    call_sites,
                    trivias,
                    content,
                    scope_start,
                );
            }
            Call::NullSafeMethod(method_call) => {
                let subject_text = expr_to_subject_text(method_call.object);
                extract_from_expression(
                    method_call.object,
                    spans,
                    var_defs,
                    scopes,
                    template_defs,
                    call_sites,
                    trivias,
                    content,
                    scope_start,
                );

                if let ClassLikeMemberSelector::Identifier(ident) = &method_call.method {
                    let member_name = ident.value.to_string();
                    // Emit call site for null-safe method call.
                    // Use `->` so resolve_callable handles it the same
                    // as regular method calls.
                    emit_call_site(
                        format!("{}->{}", &subject_text, &member_name),
                        &method_call.argument_list,
                        call_sites,
                    );
                    spans.push(SymbolSpan {
                        start: ident.span.start.offset,
                        end: ident.span.end.offset,
                        kind: SymbolKind::MemberAccess {
                            subject_text,
                            member_name,
                            is_static: false,
                            is_method_call: true,
                        },
                    });
                }
                extract_from_arguments(
                    &method_call.argument_list.arguments,
                    spans,
                    var_defs,
                    scopes,
                    template_defs,
                    call_sites,
                    trivias,
                    content,
                    scope_start,
                );
            }
            Call::StaticMethod(static_call) => {
                let subject_text = expr_to_subject_text(static_call.class);
                emit_class_expr_span(
                    static_call.class,
                    spans,
                    var_defs,
                    scopes,
                    template_defs,
                    call_sites,
                    trivias,
                    content,
                    scope_start,
                );

                if let ClassLikeMemberSelector::Identifier(ident) = &static_call.method {
                    let member_name = ident.value.to_string();
                    // Emit call site for static method call: `Class::method(...)`
                    emit_call_site(
                        format!("{}::{}", &subject_text, &member_name),
                        &static_call.argument_list,
                        call_sites,
                    );
                    spans.push(SymbolSpan {
                        start: ident.span.start.offset,
                        end: ident.span.end.offset,
                        kind: SymbolKind::MemberAccess {
                            subject_text,
                            member_name,
                            is_static: true,
                            is_method_call: true,
                        },
                    });
                }
                extract_from_arguments(
                    &static_call.argument_list.arguments,
                    spans,
                    var_defs,
                    scopes,
                    template_defs,
                    call_sites,
                    trivias,
                    content,
                    scope_start,
                );
            }
        },

        // ── Property / constant access ──
        Expression::Access(access) => {
            match access {
                Access::Property(pa) => {
                    let subject_text = expr_to_subject_text(pa.object);
                    extract_from_expression(
                        pa.object,
                        spans,
                        var_defs,
                        scopes,
                        template_defs,
                        call_sites,
                        trivias,
                        content,
                        scope_start,
                    );

                    if let ClassLikeMemberSelector::Identifier(ident) = &pa.property {
                        let member_name = ident.value.to_string();
                        spans.push(SymbolSpan {
                            start: ident.span.start.offset,
                            end: ident.span.end.offset,
                            kind: SymbolKind::MemberAccess {
                                subject_text,
                                member_name,
                                is_static: false,
                                is_method_call: false,
                            },
                        });
                    }
                }
                Access::NullSafeProperty(pa) => {
                    let subject_text = expr_to_subject_text(pa.object);
                    extract_from_expression(
                        pa.object,
                        spans,
                        var_defs,
                        scopes,
                        template_defs,
                        call_sites,
                        trivias,
                        content,
                        scope_start,
                    );

                    if let ClassLikeMemberSelector::Identifier(ident) = &pa.property {
                        let member_name = ident.value.to_string();
                        spans.push(SymbolSpan {
                            start: ident.span.start.offset,
                            end: ident.span.end.offset,
                            kind: SymbolKind::MemberAccess {
                                subject_text,
                                member_name,
                                is_static: false,
                                is_method_call: false,
                            },
                        });
                    }
                }
                Access::StaticProperty(spa) => {
                    let subject_text = expr_to_subject_text(spa.class);
                    emit_class_expr_span(
                        spa.class,
                        spans,
                        var_defs,
                        scopes,
                        template_defs,
                        call_sites,
                        trivias,
                        content,
                        scope_start,
                    );

                    if let Variable::Direct(dv) = &spa.property {
                        let prop_name = dv.name.strip_prefix('$').unwrap_or(dv.name).to_string();
                        spans.push(SymbolSpan {
                            start: dv.span.start.offset,
                            end: dv.span.end.offset,
                            kind: SymbolKind::MemberAccess {
                                subject_text,
                                member_name: prop_name,
                                is_static: true,
                                is_method_call: false,
                            },
                        });
                    }
                }
                Access::ClassConstant(cca) => {
                    let subject_text = expr_to_subject_text(cca.class);
                    emit_class_expr_span(
                        cca.class,
                        spans,
                        var_defs,
                        scopes,
                        template_defs,
                        call_sites,
                        trivias,
                        content,
                        scope_start,
                    );

                    if let ClassLikeConstantSelector::Identifier(ident) = &cca.constant {
                        let const_name = ident.value.to_string();
                        if const_name == "class" {
                            // `Foo::class` — the navigable part is `Foo`.
                        } else {
                            spans.push(SymbolSpan {
                                start: ident.span.start.offset,
                                end: ident.span.end.offset,
                                kind: SymbolKind::MemberAccess {
                                    subject_text,
                                    member_name: const_name,
                                    is_static: true,
                                    is_method_call: false,
                                },
                            });
                        }
                    }
                }
            }
        }

        // ── Assignment ──
        Expression::Assignment(assign) => {
            extract_from_expression(
                assign.lhs,
                spans,
                var_defs,
                scopes,
                template_defs,
                call_sites,
                trivias,
                content,
                scope_start,
            );
            extract_from_expression(
                assign.rhs,
                spans,
                var_defs,
                scopes,
                template_defs,
                call_sites,
                trivias,
                content,
                scope_start,
            );

            // The definition only becomes visible *after* the entire
            // assignment expression — the RHS still sees the previous
            // definition of the variable.
            let effective = assign.span().end.offset;

            // Emit VarDefSite for simple variable assignments: `$var = ...`
            match assign.lhs {
                Expression::Variable(Variable::Direct(dv)) => {
                    let name = dv.name.strip_prefix('$').unwrap_or(dv.name).to_string();
                    var_defs.push(VarDefSite {
                        offset: dv.span.start.offset,
                        name,
                        kind: VarDefKind::Assignment,
                        scope_start,
                        effective_from: effective,
                    });
                }
                // Array destructuring: `[$a, $b] = ...`
                Expression::Array(arr) => {
                    collect_destructuring_var_defs(
                        &arr.elements,
                        var_defs,
                        scope_start,
                        VarDefKind::ArrayDestructuring,
                        effective,
                    );
                }
                // List destructuring: `list($a, $b) = ...`
                Expression::List(list) => {
                    collect_destructuring_var_defs(
                        &list.elements,
                        var_defs,
                        scope_start,
                        VarDefKind::ListDestructuring,
                        effective,
                    );
                }
                _ => {}
            }
        }

        // ── Binary operations ──
        Expression::Binary(bin) => {
            extract_from_expression(
                bin.lhs,
                spans,
                var_defs,
                scopes,
                template_defs,
                call_sites,
                trivias,
                content,
                scope_start,
            );
            extract_from_expression(
                bin.rhs,
                spans,
                var_defs,
                scopes,
                template_defs,
                call_sites,
                trivias,
                content,
                scope_start,
            );
        }

        // ── Unary operations ──
        Expression::UnaryPrefix(un) => {
            extract_from_expression(
                un.operand,
                spans,
                var_defs,
                scopes,
                template_defs,
                call_sites,
                trivias,
                content,
                scope_start,
            );
        }
        Expression::UnaryPostfix(un) => {
            extract_from_expression(
                un.operand,
                spans,
                var_defs,
                scopes,
                template_defs,
                call_sites,
                trivias,
                content,
                scope_start,
            );
        }

        // ── Parenthesized ──
        Expression::Parenthesized(paren) => {
            extract_from_expression(
                paren.expression,
                spans,
                var_defs,
                scopes,
                template_defs,
                call_sites,
                trivias,
                content,
                scope_start,
            );
        }

        // ── Ternary ──
        Expression::Conditional(ternary) => {
            extract_from_expression(
                ternary.condition,
                spans,
                var_defs,
                scopes,
                template_defs,
                call_sites,
                trivias,
                content,
                scope_start,
            );
            if let Some(then_branch) = ternary.then {
                extract_from_expression(
                    then_branch,
                    spans,
                    var_defs,
                    scopes,
                    template_defs,
                    call_sites,
                    trivias,
                    content,
                    scope_start,
                );
            }
            extract_from_expression(
                ternary.r#else,
                spans,
                var_defs,
                scopes,
                template_defs,
                call_sites,
                trivias,
                content,
                scope_start,
            );
        }

        // ── Array ──
        Expression::Array(array) => {
            extract_from_array_elements(
                &array.elements,
                spans,
                var_defs,
                scopes,
                template_defs,
                call_sites,
                trivias,
                content,
                scope_start,
            );
        }
        Expression::LegacyArray(array) => {
            extract_from_array_elements(
                &array.elements,
                spans,
                var_defs,
                scopes,
                template_defs,
                call_sites,
                trivias,
                content,
                scope_start,
            );
        }
        Expression::List(list) => {
            extract_from_array_elements(
                &list.elements,
                spans,
                var_defs,
                scopes,
                template_defs,
                call_sites,
                trivias,
                content,
                scope_start,
            );
        }

        // ── Array access ──
        Expression::ArrayAccess(access) => {
            extract_from_expression(
                access.array,
                spans,
                var_defs,
                scopes,
                template_defs,
                call_sites,
                trivias,
                content,
                scope_start,
            );
            extract_from_expression(
                access.index,
                spans,
                var_defs,
                scopes,
                template_defs,
                call_sites,
                trivias,
                content,
                scope_start,
            );
        }

        // ── Closures / arrow functions ──
        Expression::Closure(closure) => {
            // Closure introduces a new scope.
            let closure_scope_start = closure.body.left_brace.start.offset;
            let closure_scope_end = closure.body.right_brace.end.offset;
            scopes.push((closure_scope_start, closure_scope_end));

            for param in closure.parameter_list.parameters.iter() {
                if let Some(ref hint) = param.hint {
                    extract_from_hint(hint, spans);
                }
                let name = param
                    .variable
                    .name
                    .strip_prefix('$')
                    .unwrap_or(param.variable.name)
                    .to_string();
                spans.push(SymbolSpan {
                    start: param.variable.span.start.offset,
                    end: param.variable.span.end.offset,
                    kind: SymbolKind::Variable { name: name.clone() },
                });
                // Emit VarDefSite for closure parameter.
                let cp_offset = param.variable.span.start.offset;
                var_defs.push(VarDefSite {
                    offset: cp_offset,
                    name,
                    kind: VarDefKind::Parameter,
                    scope_start: closure_scope_start,
                    effective_from: cp_offset,
                });
                if let Some(ref default) = param.default_value {
                    extract_from_expression(
                        default.value,
                        spans,
                        var_defs,
                        scopes,
                        template_defs,
                        call_sites,
                        trivias,
                        content,
                        closure_scope_start,
                    );
                }
            }
            if let Some(ref use_clause) = closure.use_clause {
                for var in use_clause.variables.iter() {
                    let name = var
                        .variable
                        .name
                        .strip_prefix('$')
                        .unwrap_or(var.variable.name)
                        .to_string();
                    spans.push(SymbolSpan {
                        start: var.variable.span.start.offset,
                        end: var.variable.span.end.offset,
                        kind: SymbolKind::Variable { name },
                    });
                }
            }
            if let Some(ref return_type) = closure.return_type_hint {
                extract_from_hint(&return_type.hint, spans);
            }
            for s in closure.body.statements.iter() {
                extract_from_statement(
                    s,
                    spans,
                    var_defs,
                    scopes,
                    template_defs,
                    call_sites,
                    trivias,
                    content,
                    closure_scope_start,
                );
            }
        }
        Expression::ArrowFunction(arrow) => {
            // Arrow functions introduce a new scope for their parameters.
            // They don't have braces, so use the span of the arrow function itself.
            let arrow_scope_start = arrow.span().start.offset;
            let arrow_scope_end = arrow.span().end.offset;
            scopes.push((arrow_scope_start, arrow_scope_end));

            for param in arrow.parameter_list.parameters.iter() {
                if let Some(ref hint) = param.hint {
                    extract_from_hint(hint, spans);
                }
                let name = param
                    .variable
                    .name
                    .strip_prefix('$')
                    .unwrap_or(param.variable.name)
                    .to_string();
                spans.push(SymbolSpan {
                    start: param.variable.span.start.offset,
                    end: param.variable.span.end.offset,
                    kind: SymbolKind::Variable { name: name.clone() },
                });
                // Emit VarDefSite for arrow function parameter.
                let ap_offset = param.variable.span.start.offset;
                var_defs.push(VarDefSite {
                    offset: ap_offset,
                    name,
                    kind: VarDefKind::Parameter,
                    scope_start: arrow_scope_start,
                    effective_from: ap_offset,
                });
                if let Some(ref default) = param.default_value {
                    extract_from_expression(
                        default.value,
                        spans,
                        var_defs,
                        scopes,
                        template_defs,
                        call_sites,
                        trivias,
                        content,
                        arrow_scope_start,
                    );
                }
            }
            if let Some(ref return_type) = arrow.return_type_hint {
                extract_from_hint(&return_type.hint, spans);
            }
            extract_from_expression(
                arrow.expression,
                spans,
                var_defs,
                scopes,
                template_defs,
                call_sites,
                trivias,
                content,
                arrow_scope_start,
            );
        }

        // ── Match expression ──
        Expression::Match(match_expr) => {
            extract_from_expression(
                match_expr.expression,
                spans,
                var_defs,
                scopes,
                template_defs,
                call_sites,
                trivias,
                content,
                scope_start,
            );
            for arm in match_expr.arms.iter() {
                match arm {
                    MatchArm::Expression(arm) => {
                        for cond in arm.conditions.iter() {
                            extract_from_expression(
                                cond,
                                spans,
                                var_defs,
                                scopes,
                                template_defs,
                                call_sites,
                                trivias,
                                content,
                                scope_start,
                            );
                        }
                        extract_from_expression(
                            arm.expression,
                            spans,
                            var_defs,
                            scopes,
                            template_defs,
                            call_sites,
                            trivias,
                            content,
                            scope_start,
                        );
                    }
                    MatchArm::Default(arm) => {
                        extract_from_expression(
                            arm.expression,
                            spans,
                            var_defs,
                            scopes,
                            template_defs,
                            call_sites,
                            trivias,
                            content,
                            scope_start,
                        );
                    }
                }
            }
        }

        // ── Throw expression (PHP 8) ──
        Expression::Throw(throw_expr) => {
            extract_from_expression(
                throw_expr.exception,
                spans,
                var_defs,
                scopes,
                template_defs,
                call_sites,
                trivias,
                content,
                scope_start,
            );
        }

        // ── Yield ──
        Expression::Yield(yield_expr) => match yield_expr {
            Yield::Value(yv) => {
                if let Some(value) = yv.value {
                    extract_from_expression(
                        value,
                        spans,
                        var_defs,
                        scopes,
                        template_defs,
                        call_sites,
                        trivias,
                        content,
                        scope_start,
                    );
                }
            }
            Yield::Pair(yp) => {
                extract_from_expression(
                    yp.key,
                    spans,
                    var_defs,
                    scopes,
                    template_defs,
                    call_sites,
                    trivias,
                    content,
                    scope_start,
                );
                extract_from_expression(
                    yp.value,
                    spans,
                    var_defs,
                    scopes,
                    template_defs,
                    call_sites,
                    trivias,
                    content,
                    scope_start,
                );
            }
            Yield::From(yf) => {
                extract_from_expression(
                    yf.iterator,
                    spans,
                    var_defs,
                    scopes,
                    template_defs,
                    call_sites,
                    trivias,
                    content,
                    scope_start,
                );
            }
        },

        // ── Clone ──
        Expression::Clone(clone) => {
            extract_from_expression(
                clone.object,
                spans,
                var_defs,
                scopes,
                template_defs,
                call_sites,
                trivias,
                content,
                scope_start,
            );
        }

        // Non-navigable expressions (literals, etc.) are intentionally ignored.
        _ => {}
    }
}

/// Collect variable definition sites from a destructuring pattern
/// (`[$a, $b] = ...` or `list($a, $b) = ...`).
fn collect_destructuring_var_defs(
    elements: &TokenSeparatedSequence<'_, ArrayElement<'_>>,
    var_defs: &mut Vec<VarDefSite>,
    scope_start: u32,
    kind: VarDefKind,
    effective_from: u32,
) {
    for element in elements.iter() {
        let value_expr = match element {
            ArrayElement::KeyValue(kv) => kv.value,
            ArrayElement::Value(val) => val.value,
            _ => continue,
        };
        match value_expr {
            Expression::Variable(Variable::Direct(dv)) => {
                let name = dv.name.strip_prefix('$').unwrap_or(dv.name).to_string();
                var_defs.push(VarDefSite {
                    offset: dv.span.start.offset,
                    name,
                    kind: kind.clone(),
                    scope_start,
                    effective_from,
                });
            }
            // Nested destructuring: `[[$a, $b], $c] = ...`
            Expression::Array(arr) => {
                collect_destructuring_var_defs(
                    &arr.elements,
                    var_defs,
                    scope_start,
                    kind.clone(),
                    effective_from,
                );
            }
            Expression::List(list) => {
                collect_destructuring_var_defs(
                    &list.elements,
                    var_defs,
                    scope_start,
                    kind.clone(),
                    effective_from,
                );
            }
            _ => {}
        }
    }
}

// ─── Shared helpers ─────────────────────────────────────────────────────────

/// Walk an argument list and extract symbols from each argument expression.
#[allow(clippy::too_many_arguments)]
fn extract_from_arguments<'a>(
    args: &TokenSeparatedSequence<'a, Argument<'a>>,
    spans: &mut Vec<SymbolSpan>,
    var_defs: &mut Vec<VarDefSite>,
    scopes: &mut Vec<(u32, u32)>,
    template_defs: &mut Vec<TemplateParamDef>,
    call_sites: &mut Vec<CallSite>,
    trivias: &[Trivia<'a>],
    content: &str,
    scope_start: u32,
) {
    for arg in args.iter() {
        let arg_expr = match arg {
            Argument::Positional(pos) => pos.value,
            Argument::Named(named) => named.value,
        };
        extract_from_expression(
            arg_expr,
            spans,
            var_defs,
            scopes,
            template_defs,
            call_sites,
            trivias,
            content,
            scope_start,
        );
    }
}

/// Walk array elements and extract symbols from each element expression.
#[allow(clippy::too_many_arguments)]
fn extract_from_array_elements<'a>(
    elements: &TokenSeparatedSequence<'a, ArrayElement<'a>>,
    spans: &mut Vec<SymbolSpan>,
    var_defs: &mut Vec<VarDefSite>,
    scopes: &mut Vec<(u32, u32)>,
    template_defs: &mut Vec<TemplateParamDef>,
    call_sites: &mut Vec<CallSite>,
    trivias: &[Trivia<'a>],
    content: &str,
    scope_start: u32,
) {
    for element in elements.iter() {
        match element {
            ArrayElement::KeyValue(kv) => {
                extract_from_expression(
                    kv.key,
                    spans,
                    var_defs,
                    scopes,
                    template_defs,
                    call_sites,
                    trivias,
                    content,
                    scope_start,
                );
                extract_from_expression(
                    kv.value,
                    spans,
                    var_defs,
                    scopes,
                    template_defs,
                    call_sites,
                    trivias,
                    content,
                    scope_start,
                );
            }
            ArrayElement::Value(val) => {
                extract_from_expression(
                    val.value,
                    spans,
                    var_defs,
                    scopes,
                    template_defs,
                    call_sites,
                    trivias,
                    content,
                    scope_start,
                );
            }
            ArrayElement::Variadic(variadic) => {
                extract_from_expression(
                    variadic.value,
                    spans,
                    var_defs,
                    scopes,
                    template_defs,
                    call_sites,
                    trivias,
                    content,
                    scope_start,
                );
            }
            _ => {}
        }
    }
}

/// For the class part of a static call/property/constant access, emit
/// the appropriate span (ClassReference, SelfStaticParent, or recurse).
#[allow(clippy::too_many_arguments)]
fn emit_class_expr_span<'a>(
    expr: &Expression<'a>,
    spans: &mut Vec<SymbolSpan>,
    var_defs: &mut Vec<VarDefSite>,
    scopes: &mut Vec<(u32, u32)>,
    template_defs: &mut Vec<TemplateParamDef>,
    call_sites: &mut Vec<CallSite>,
    trivias: &[Trivia<'a>],
    content: &str,
    scope_start: u32,
) {
    match expr {
        Expression::Identifier(ident) => {
            let raw = ident.value().to_string();
            spans.push(class_ref_span(
                ident.span().start.offset,
                ident.span().end.offset,
                &raw,
            ));
        }
        Expression::Self_(kw) => {
            spans.push(SymbolSpan {
                start: kw.span.start.offset,
                end: kw.span.end.offset,
                kind: SymbolKind::SelfStaticParent {
                    keyword: "self".to_string(),
                },
            });
        }
        Expression::Static(kw) => {
            spans.push(SymbolSpan {
                start: kw.span.start.offset,
                end: kw.span.end.offset,
                kind: SymbolKind::SelfStaticParent {
                    keyword: "static".to_string(),
                },
            });
        }
        Expression::Parent(kw) => {
            spans.push(SymbolSpan {
                start: kw.span.start.offset,
                end: kw.span.end.offset,
                kind: SymbolKind::SelfStaticParent {
                    keyword: "parent".to_string(),
                },
            });
        }
        _ => {
            extract_from_expression(
                expr,
                spans,
                var_defs,
                scopes,
                template_defs,
                call_sites,
                trivias,
                content,
                scope_start,
            );
        }
    }
}

// ─── Call site emission ─────────────────────────────────────────────────────

/// Build and push a [`CallSite`] from an argument list and its call expression string.
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

// ─── Expression to subject text ─────────────────────────────────────────────

/// Convert an AST expression to the subject text string that
/// `resolve_target_classes` expects.
fn expr_to_subject_text(expr: &Expression<'_>) -> String {
    match expr {
        Expression::Variable(Variable::Direct(dv)) => dv.name.to_string(),
        Expression::Self_(_) => "self".to_string(),
        Expression::Static(_) => "static".to_string(),
        Expression::Parent(_) => "parent".to_string(),
        Expression::Identifier(ident) => ident.value().to_string(),

        Expression::Access(Access::Property(pa)) => {
            let obj = expr_to_subject_text(pa.object);
            if let ClassLikeMemberSelector::Identifier(ident) = &pa.property {
                format!("{}->{}", obj, ident.value)
            } else {
                obj
            }
        }
        Expression::Access(Access::NullSafeProperty(pa)) => {
            let obj = expr_to_subject_text(pa.object);
            if let ClassLikeMemberSelector::Identifier(ident) = &pa.property {
                format!("{}?->{}", obj, ident.value)
            } else {
                obj
            }
        }
        Expression::Access(Access::StaticProperty(spa)) => {
            let class_text = expr_to_subject_text(spa.class);
            if let Variable::Direct(dv) = &spa.property {
                format!("{}::{}", class_text, dv.name)
            } else {
                class_text
            }
        }
        Expression::Access(Access::ClassConstant(cca)) => {
            let class_text = expr_to_subject_text(cca.class);
            match &cca.constant {
                ClassLikeConstantSelector::Identifier(ident) => {
                    format!("{}::{}", class_text, ident.value)
                }
                _ => class_text,
            }
        }

        Expression::Call(Call::Method(mc)) => {
            let obj = expr_to_subject_text(mc.object);
            if let ClassLikeMemberSelector::Identifier(ident) = &mc.method {
                let args_text = format_first_class_arg(&mc.argument_list.arguments);
                format!("{}->{}({})", obj, ident.value, args_text)
            } else {
                format!("{}->?()", obj)
            }
        }
        Expression::Call(Call::NullSafeMethod(mc)) => {
            let obj = expr_to_subject_text(mc.object);
            if let ClassLikeMemberSelector::Identifier(ident) = &mc.method {
                let args_text = format_first_class_arg(&mc.argument_list.arguments);
                format!("{}?->{}({})", obj, ident.value, args_text)
            } else {
                format!("{}?->?()", obj)
            }
        }
        Expression::Call(Call::StaticMethod(sc)) => {
            let class_text = expr_to_subject_text(sc.class);
            if let ClassLikeMemberSelector::Identifier(ident) = &sc.method {
                let args_text = format_first_class_arg(&sc.argument_list.arguments);
                format!("{}::{}({})", class_text, ident.value, args_text)
            } else {
                format!("{}::?()", class_text)
            }
        }
        Expression::Call(Call::Function(fc)) => {
            let func_text = expr_to_subject_text(fc.function);
            let args_text = format_first_class_arg(&fc.argument_list.arguments);
            format!("{}({})", func_text, args_text)
        }

        Expression::Instantiation(inst) => expr_to_subject_text(inst.class),

        Expression::Parenthesized(paren) => expr_to_subject_text(paren.expression),

        Expression::ArrayAccess(access) => {
            let base = expr_to_subject_text(access.array);
            if base.is_empty() {
                return String::new();
            }
            // Preserve string keys for array-shape resolution;
            // collapse everything else to `[]` (generic element access),
            // matching the convention used by `extract_arrow_subject`.
            let bracket = match access.index {
                Expression::Literal(Literal::String(s)) => {
                    // `s.raw` includes surrounding quotes (e.g. `'key'`).
                    // Strip them to get the bare key, then re-wrap in
                    // single quotes for the subject format.
                    let raw = s.raw;
                    let inner = raw
                        .strip_prefix('\'')
                        .and_then(|r| r.strip_suffix('\''))
                        .or_else(|| raw.strip_prefix('"').and_then(|r| r.strip_suffix('"')))
                        .unwrap_or(raw);
                    format!("['{}']", inner)
                }
                _ => "[]".to_string(),
            };
            format!("{}{}", base, bracket)
        }

        _ => String::new(),
    }
}

/// Format the first argument of a call expression as source text.
///
/// Preserves `Foo::class`, string/integer/float literals, `null`,
/// `true`, `false`, and `$variable` references so that conditional
/// return-type resolution (e.g. `$guard is null ? Factory : Guard`)
/// can inspect the argument value.  Returns an empty string when the
/// first argument cannot be represented as simple text.
fn format_first_class_arg(args: &TokenSeparatedSequence<'_, Argument<'_>>) -> String {
    if let Some(first) = args.iter().next() {
        let arg_expr = match first {
            Argument::Positional(pos) => pos.value,
            Argument::Named(named) => named.value,
        };
        match arg_expr {
            // Foo::class
            Expression::Access(Access::ClassConstant(cca)) => {
                if let ClassLikeConstantSelector::Identifier(ident) = &cca.constant
                    && ident.value == "class"
                {
                    let class_text = expr_to_subject_text(cca.class);
                    return format!("{}::class", class_text);
                }
            }
            // String literals: 'web', "guard"
            Expression::Literal(Literal::String(lit_str)) => {
                return lit_str.raw.to_string();
            }
            // Integer literals: 0, 42
            Expression::Literal(Literal::Integer(lit_int)) => {
                return lit_int.raw.to_string();
            }
            // Float literals: 3.14
            Expression::Literal(Literal::Float(lit_float)) => {
                return lit_float.raw.to_string();
            }
            // null
            Expression::Literal(Literal::Null(_)) => {
                return "null".to_string();
            }
            // true
            Expression::Literal(Literal::True(_)) => {
                return "true".to_string();
            }
            // false
            Expression::Literal(Literal::False(_)) => {
                return "false".to_string();
            }
            // $variable
            Expression::Variable(Variable::Direct(dv)) => {
                return dv.name.to_string();
            }
            // new ClassName(…) → "new ClassName()"
            Expression::Instantiation(inst) => {
                let class_text = expr_to_subject_text(inst.class);
                if !class_text.is_empty() {
                    return format!("new {}()", class_text);
                }
            }
            _ => {}
        }
    }
    String::new()
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── SymbolMap::lookup tests ─────────────────────────────────────────

    fn make_span(start: u32, end: u32, name: &str) -> SymbolSpan {
        SymbolSpan {
            start,
            end,
            kind: SymbolKind::ClassReference {
                name: name.to_string(),
                is_fqn: false,
            },
        }
    }

    #[test]
    fn lookup_empty_map_returns_none() {
        let map = SymbolMap::default();
        assert!(map.lookup(0).is_none());
        assert!(map.lookup(100).is_none());
    }

    #[test]
    fn lookup_hit_at_start() {
        let map = SymbolMap {
            spans: vec![make_span(10, 15, "Foo")],
            ..Default::default()
        };
        assert!(map.lookup(10).is_some());
        assert_eq!(map.lookup(10).unwrap().start, 10);
    }

    #[test]
    fn lookup_hit_at_end_minus_one() {
        let map = SymbolMap {
            spans: vec![make_span(10, 15, "Foo")],
            ..Default::default()
        };
        assert!(map.lookup(14).is_some());
    }

    #[test]
    fn lookup_miss_at_end() {
        let map = SymbolMap {
            spans: vec![make_span(10, 15, "Foo")],
            ..Default::default()
        };
        assert!(map.lookup(15).is_none());
    }

    #[test]
    fn lookup_miss_before_first_span() {
        let map = SymbolMap {
            spans: vec![make_span(10, 15, "Foo")],
            ..Default::default()
        };
        assert!(map.lookup(5).is_none());
    }

    #[test]
    fn lookup_miss_in_gap() {
        let map = SymbolMap {
            spans: vec![make_span(10, 15, "Foo"), make_span(20, 25, "Bar")],
            ..Default::default()
        };
        assert!(map.lookup(17).is_none());
    }

    #[test]
    fn lookup_correct_span_in_sequence() {
        let map = SymbolMap {
            spans: vec![
                make_span(10, 15, "Foo"),
                make_span(20, 25, "Bar"),
                make_span(30, 35, "Baz"),
            ],
            ..Default::default()
        };
        let result = map.lookup(22).unwrap();
        if let SymbolKind::ClassReference { ref name, .. } = result.kind {
            assert_eq!(name, "Bar");
        } else {
            panic!("Expected ClassReference");
        }
    }

    // ── is_navigable_type tests ─────────────────────────────────────────

    #[test]
    fn scalar_types_are_not_navigable() {
        assert!(!is_navigable_type("int"));
        assert!(!is_navigable_type("string"));
        assert!(!is_navigable_type("bool"));
        assert!(!is_navigable_type("void"));
        assert!(!is_navigable_type("null"));
        assert!(!is_navigable_type("mixed"));
        assert!(!is_navigable_type("array"));
        assert!(!is_navigable_type("callable"));
        assert!(!is_navigable_type("float"));
        assert!(!is_navigable_type("never"));
        assert!(!is_navigable_type("iterable"));
        assert!(!is_navigable_type("true"));
        assert!(!is_navigable_type("false"));
        assert!(!is_navigable_type("resource"));
        assert!(!is_navigable_type("object"));
    }

    #[test]
    fn class_names_are_navigable() {
        assert!(is_navigable_type("Foo"));
        assert!(is_navigable_type("Collection"));
        assert!(is_navigable_type("App\\Models\\User"));
        assert!(is_navigable_type("ResponseInterface"));
    }

    #[test]
    fn case_insensitive_scalar_check() {
        assert!(!is_navigable_type("INT"));
        assert!(!is_navigable_type("String"));
        assert!(!is_navigable_type("BOOL"));
    }

    #[test]
    fn empty_name_is_not_navigable() {
        assert!(!is_navigable_type(""));
    }

    // ── extract_symbol_map integration tests ────────────────────────────

    fn parse_and_extract(php: &str) -> SymbolMap {
        let arena = bumpalo::Bump::new();
        let file_id = mago_database::file::FileId::new("test.php");
        let program = mago_syntax::parser::parse_file_content(&arena, file_id, php);
        extract_symbol_map(program, php)
    }

    #[test]
    fn class_declaration_produces_class_declaration() {
        let php = "<?php\nclass Foo {}\n";
        let map = parse_and_extract(php);
        let hit = map.lookup(php.find("Foo").unwrap() as u32);
        assert!(hit.is_some());
        if let SymbolKind::ClassDeclaration { ref name } = hit.unwrap().kind {
            assert_eq!(name, "Foo");
        } else {
            panic!("Expected ClassDeclaration, got {:?}", hit.unwrap().kind);
        }
    }

    #[test]
    fn extends_produces_class_reference() {
        let php = "<?php\nclass Foo extends Bar {}\n";
        let map = parse_and_extract(php);
        let bar_offset = php.find("Bar").unwrap() as u32;
        let hit = map.lookup(bar_offset);
        assert!(hit.is_some());
        if let SymbolKind::ClassReference { ref name, .. } = hit.unwrap().kind {
            assert_eq!(name, "Bar");
        } else {
            panic!("Expected ClassReference for Bar");
        }
    }

    #[test]
    fn extends_fqn_sets_is_fqn() {
        let php = "<?php\nclass Foo extends \\App\\Bar {}\n";
        let map = parse_and_extract(php);
        // Find "\\App\\Bar" — the `\` at the start
        let fqn_offset = php.find("\\App\\Bar").unwrap() as u32;
        let hit = map.lookup(fqn_offset);
        assert!(hit.is_some(), "Should have a span at the FQN");
        if let SymbolKind::ClassReference { ref name, is_fqn } = hit.unwrap().kind {
            assert_eq!(name, "App\\Bar");
            assert!(is_fqn, "FQN should be marked as is_fqn");
        } else {
            panic!(
                "Expected ClassReference for FQN, got {:?}",
                hit.unwrap().kind
            );
        }
    }

    #[test]
    fn implements_produces_class_reference() {
        let php = "<?php\nclass Foo implements Baz, Qux {}\n";
        let map = parse_and_extract(php);

        let baz_offset = php.find("Baz").unwrap() as u32;
        let hit = map.lookup(baz_offset);
        assert!(hit.is_some());
        if let SymbolKind::ClassReference { ref name, .. } = hit.unwrap().kind {
            assert_eq!(name, "Baz");
        } else {
            panic!("Expected ClassReference for Baz");
        }

        let qux_offset = php.find("Qux").unwrap() as u32;
        let hit = map.lookup(qux_offset);
        assert!(hit.is_some());
        if let SymbolKind::ClassReference { ref name, .. } = hit.unwrap().kind {
            assert_eq!(name, "Qux");
        } else {
            panic!("Expected ClassReference for Qux");
        }
    }

    #[test]
    fn variable_produces_variable_span() {
        let php = "<?php\nfunction test() { $foo = 1; }\n";
        let map = parse_and_extract(php);
        let offset = php.find("$foo").unwrap() as u32;
        let hit = map.lookup(offset);
        assert!(hit.is_some());
        if let SymbolKind::Variable { ref name } = hit.unwrap().kind {
            assert_eq!(name, "foo");
        } else {
            panic!("Expected Variable");
        }
    }

    #[test]
    fn function_call_produces_function_call_span() {
        let php = "<?php\nfunction test() { strlen('hello'); }\n";
        let map = parse_and_extract(php);
        let offset = php.find("strlen").unwrap() as u32;
        let hit = map.lookup(offset);
        assert!(hit.is_some());
        if let SymbolKind::FunctionCall { ref name } = hit.unwrap().kind {
            assert_eq!(name, "strlen");
        } else {
            panic!("Expected FunctionCall, got {:?}", hit.unwrap().kind);
        }
    }

    #[test]
    fn method_call_produces_member_access() {
        let php = "<?php\nclass Foo { function test() { $this->bar(); } }\n";
        let map = parse_and_extract(php);
        let bar_offset = php.find("bar").unwrap() as u32;
        let hit = map.lookup(bar_offset);
        assert!(hit.is_some());
        if let SymbolKind::MemberAccess {
            ref subject_text,
            ref member_name,
            is_static,
            is_method_call,
        } = hit.unwrap().kind
        {
            assert_eq!(member_name, "bar");
            assert_eq!(subject_text, "$this");
            assert!(!is_static);
            assert!(is_method_call);
        } else {
            panic!("Expected MemberAccess");
        }
    }

    #[test]
    fn static_method_call_produces_member_access() {
        let php = "<?php\nclass Foo { function test() { self::create(); } }\n";
        let map = parse_and_extract(php);
        let offset = php.find("create").unwrap() as u32;
        let hit = map.lookup(offset);
        assert!(hit.is_some());
        if let SymbolKind::MemberAccess {
            ref subject_text,
            ref member_name,
            is_static,
            is_method_call,
        } = hit.unwrap().kind
        {
            assert_eq!(member_name, "create");
            assert_eq!(subject_text, "self");
            assert!(is_static);
            assert!(is_method_call);
        } else {
            panic!("Expected MemberAccess");
        }
    }

    #[test]
    fn property_access_produces_member_access() {
        let php = "<?php\nclass Foo { function test() { $this->name; } }\n";
        let map = parse_and_extract(php);
        let arrow_pos = php.find("->name").unwrap();
        let name_offset = (arrow_pos + 2) as u32;
        let hit = map.lookup(name_offset);
        assert!(hit.is_some());
        if let SymbolKind::MemberAccess {
            ref member_name,
            is_method_call,
            ..
        } = hit.unwrap().kind
        {
            assert_eq!(member_name, "name");
            assert!(!is_method_call);
        } else {
            panic!("Expected MemberAccess");
        }
    }

    #[test]
    fn type_hint_produces_class_reference() {
        let php = "<?php\nfunction test(Foo $x): Bar { }\n";
        let map = parse_and_extract(php);

        let foo_offset = php.find("Foo").unwrap() as u32;
        let hit = map.lookup(foo_offset);
        assert!(hit.is_some());
        if let SymbolKind::ClassReference { ref name, .. } = hit.unwrap().kind {
            assert_eq!(name, "Foo");
        } else {
            panic!("Expected ClassReference for Foo");
        }

        let bar_offset = php.find("Bar").unwrap() as u32;
        let hit = map.lookup(bar_offset);
        assert!(hit.is_some());
        if let SymbolKind::ClassReference { ref name, .. } = hit.unwrap().kind {
            assert_eq!(name, "Bar");
        } else {
            panic!("Expected ClassReference for Bar");
        }
    }

    #[test]
    fn scalar_type_hint_not_in_map() {
        let php = "<?php\nfunction test(int $x): string { }\n";
        let map = parse_and_extract(php);

        let int_offset = php.find("int").unwrap() as u32;
        assert!(map.lookup(int_offset).is_none());

        let string_offset = php.find("string").unwrap() as u32;
        assert!(map.lookup(string_offset).is_none());
    }

    #[test]
    fn new_expression_produces_class_reference() {
        let php = "<?php\nfunction test() { $x = new Foo(); }\n";
        let map = parse_and_extract(php);
        let foo_offset = php.find("Foo").unwrap() as u32;
        let hit = map.lookup(foo_offset);
        assert!(hit.is_some());
        if let SymbolKind::ClassReference { ref name, .. } = hit.unwrap().kind {
            assert_eq!(name, "Foo");
        } else {
            panic!("Expected ClassReference for Foo");
        }
    }

    #[test]
    fn catch_type_produces_class_reference() {
        let php = "<?php\ntry {} catch (RuntimeException $e) {}\n";
        let map = parse_and_extract(php);
        let offset = php.find("RuntimeException").unwrap() as u32;
        let hit = map.lookup(offset);
        assert!(hit.is_some());
        if let SymbolKind::ClassReference { ref name, .. } = hit.unwrap().kind {
            assert_eq!(name, "RuntimeException");
        } else {
            panic!("Expected ClassReference");
        }
    }

    #[test]
    fn self_keyword_produces_self_static_parent() {
        let php = "<?php\nclass Foo { function test(): self { } }\n";
        let map = parse_and_extract(php);
        let offset = php.find("self").unwrap() as u32;
        let hit = map.lookup(offset);
        assert!(hit.is_some());
        if let SymbolKind::SelfStaticParent { ref keyword } = hit.unwrap().kind {
            assert_eq!(keyword, "self");
        } else {
            panic!("Expected SelfStaticParent");
        }
    }

    #[test]
    fn whitespace_offset_returns_none() {
        let php = "<?php\nclass Foo    {}\n";
        let map = parse_and_extract(php);
        let foo_end = php.find("Foo").unwrap() + 3;
        let hit = map.lookup((foo_end + 1) as u32);
        assert!(hit.is_none());
    }

    #[test]
    fn string_interior_not_navigable() {
        let php = "<?php\n$x = 'SomeClass';\n";
        let map = parse_and_extract(php);
        let some_offset = php.find("SomeClass").unwrap() as u32;
        let hit = map.lookup(some_offset);
        if let Some(span) = hit
            && let SymbolKind::ClassReference { .. } = &span.kind
        {
            panic!("Should not produce ClassReference inside a string literal");
        }
    }

    #[test]
    fn chained_method_call_subject_text() {
        let php = "<?php\nclass Foo { function test() { $this->getService()->find(); } }\n";
        let map = parse_and_extract(php);
        let find_offset = php.find("find").unwrap() as u32;
        let hit = map.lookup(find_offset);
        assert!(hit.is_some());
        if let SymbolKind::MemberAccess {
            ref subject_text,
            ref member_name,
            ..
        } = hit.unwrap().kind
        {
            assert_eq!(member_name, "find");
            assert_eq!(subject_text, "$this->getService()");
        } else {
            panic!("Expected MemberAccess");
        }
    }

    #[test]
    fn class_constant_access_produces_member_access() {
        let php = "<?php\nclass Foo { function test() { self::MY_CONST; } }\n";
        let map = parse_and_extract(php);
        let offset = php.find("MY_CONST").unwrap() as u32;
        let hit = map.lookup(offset);
        assert!(hit.is_some());
        if let SymbolKind::MemberAccess {
            ref member_name,
            is_static,
            ..
        } = hit.unwrap().kind
        {
            assert_eq!(member_name, "MY_CONST");
            assert!(is_static);
        } else {
            panic!("Expected MemberAccess");
        }
    }

    #[test]
    fn trait_use_produces_class_reference() {
        let php = "<?php\nclass Foo { use SomeTrait; }\n";
        let map = parse_and_extract(php);
        let offset = php.find("SomeTrait").unwrap() as u32;
        let hit = map.lookup(offset);
        assert!(hit.is_some());
        if let SymbolKind::ClassReference { ref name, .. } = hit.unwrap().kind {
            assert_eq!(name, "SomeTrait");
        } else {
            panic!("Expected ClassReference");
        }
    }

    #[test]
    fn docblock_param_class_reference() {
        let php = concat!(
            "<?php\n",
            "class Foo {\n",
            "    /**\n",
            "     * @param UserService $service\n",
            "     * @return ResponseInterface\n",
            "     */\n",
            "    public function test($service) {}\n",
            "}\n",
        );
        let map = parse_and_extract(php);

        let user_service_offset = php.find("UserService").unwrap() as u32;
        let hit = map.lookup(user_service_offset);
        assert!(hit.is_some(), "Should find UserService in docblock");
        if let SymbolKind::ClassReference { ref name, .. } = hit.unwrap().kind {
            assert_eq!(name, "UserService");
        } else {
            panic!("Expected ClassReference for UserService");
        }

        let response_offset = php.find("ResponseInterface").unwrap() as u32;
        let hit = map.lookup(response_offset);
        assert!(hit.is_some(), "Should find ResponseInterface in docblock");
        if let SymbolKind::ClassReference { ref name, .. } = hit.unwrap().kind {
            assert_eq!(name, "ResponseInterface");
        } else {
            panic!("Expected ClassReference for ResponseInterface");
        }
    }

    #[test]
    fn docblock_scalar_param_not_navigable() {
        let php = concat!(
            "<?php\n",
            "/**\n",
            " * @param string $name\n",
            " */\n",
            "function test($name) {}\n",
        );
        let map = parse_and_extract(php);
        let string_offset = php.find("string").unwrap() as u32;
        let hit = map.lookup(string_offset);
        if let Some(span) = hit
            && let SymbolKind::ClassReference { .. } = &span.kind
        {
            panic!("Scalar type 'string' should not produce a ClassReference");
        }
    }

    #[test]
    fn nullable_type_hint_produces_class_reference() {
        let php = "<?php\nfunction test(?Foo $x) {}\n";
        let map = parse_and_extract(php);
        let foo_offset = php.find("Foo").unwrap() as u32;
        let hit = map.lookup(foo_offset);
        assert!(hit.is_some());
        if let SymbolKind::ClassReference { ref name, .. } = hit.unwrap().kind {
            assert_eq!(name, "Foo");
        } else {
            panic!("Expected ClassReference for nullable Foo");
        }
    }

    #[test]
    fn interface_declaration_produces_declaration() {
        let php = "<?php\ninterface Serializable {}\n";
        let map = parse_and_extract(php);
        let offset = php.find("Serializable").unwrap() as u32;
        let hit = map.lookup(offset);
        assert!(hit.is_some());
        if let SymbolKind::ClassDeclaration { ref name } = hit.unwrap().kind {
            assert_eq!(name, "Serializable");
        } else {
            panic!("Expected ClassDeclaration, got {:?}", hit.unwrap().kind);
        }
    }

    #[test]
    fn enum_declaration_produces_declaration() {
        let php = "<?php\nenum Color { case Red; case Blue; }\n";
        let map = parse_and_extract(php);
        let offset = php.find("Color").unwrap() as u32;
        let hit = map.lookup(offset);
        assert!(hit.is_some());
        if let SymbolKind::ClassDeclaration { ref name } = hit.unwrap().kind {
            assert_eq!(name, "Color");
        } else {
            panic!("Expected ClassDeclaration, got {:?}", hit.unwrap().kind);
        }
    }

    #[test]
    fn closure_param_type_hint() {
        let php = "<?php\n$f = function(Foo $x): Bar {};\n";
        let map = parse_and_extract(php);

        let foo_offset = php.find("Foo").unwrap() as u32;
        let hit = map.lookup(foo_offset);
        assert!(hit.is_some());
        if let SymbolKind::ClassReference { ref name, .. } = hit.unwrap().kind {
            assert_eq!(name, "Foo");
        } else {
            panic!("Expected ClassReference for Foo");
        }
    }

    #[test]
    fn instanceof_rhs_produces_class_reference() {
        let php = "<?php\nfunction test($x) { if ($x instanceof Foo) {} }\n";
        let map = parse_and_extract(php);
        let foo_offset = php.find("Foo").unwrap() as u32;
        let hit = map.lookup(foo_offset);
        assert!(hit.is_some());
        if let SymbolKind::ClassReference { ref name, .. } = hit.unwrap().kind {
            assert_eq!(name, "Foo");
        } else {
            panic!("Expected ClassReference for instanceof Foo");
        }
    }

    #[test]
    fn docblock_union_type_produces_multiple_references() {
        let php = concat!(
            "<?php\n",
            "/**\n",
            " * @return Foo|Bar\n",
            " */\n",
            "function test() {}\n",
        );
        let map = parse_and_extract(php);

        let foo_offset = php.find("Foo").unwrap() as u32;
        let hit = map.lookup(foo_offset);
        assert!(hit.is_some(), "Should find Foo in union return type");
        if let SymbolKind::ClassReference { ref name, .. } = hit.unwrap().kind {
            assert_eq!(name, "Foo");
        }

        let bar_offset = php.find("Bar").unwrap() as u32;
        let hit = map.lookup(bar_offset);
        assert!(hit.is_some(), "Should find Bar in union return type");
        if let SymbolKind::ClassReference { ref name, .. } = hit.unwrap().kind {
            assert_eq!(name, "Bar");
        }
    }

    #[test]
    fn docblock_nullable_type() {
        let php = concat!(
            "<?php\n",
            "/**\n",
            " * @return ?Foo\n",
            " */\n",
            "function test() {}\n",
        );
        let map = parse_and_extract(php);
        let foo_offset = php.find("Foo").unwrap() as u32;
        let hit = map.lookup(foo_offset);
        assert!(hit.is_some(), "Should find Foo in nullable return type");
        if let SymbolKind::ClassReference { ref name, .. } = hit.unwrap().kind {
            assert_eq!(name, "Foo");
        }
    }

    #[test]
    fn docblock_fqn_type() {
        let php = concat!(
            "<?php\n",
            "/**\n",
            " * @return \\App\\Models\\User\n",
            " */\n",
            "function test() {}\n",
        );
        let map = parse_and_extract(php);
        let user_offset = php.find("\\App\\Models\\User").unwrap() as u32;
        let hit = map.lookup(user_offset);
        assert!(hit.is_some(), "Should find FQN type in docblock");
        if let SymbolKind::ClassReference { ref name, is_fqn } = hit.unwrap().kind {
            assert_eq!(name, "App\\Models\\User");
            assert!(is_fqn, "Docblock FQN type should have is_fqn = true");
        } else {
            panic!("Expected ClassReference, got {:?}", hit.unwrap().kind);
        }
    }

    #[test]
    fn docblock_this_produces_self_static_parent() {
        let php = concat!(
            "<?php\n",
            "class Foo {\n",
            "    /**\n",
            "     * @return Collection<Item, $this>\n",
            "     */\n",
            "    public function items() {}\n",
            "}\n",
        );
        let map = parse_and_extract(php);
        let this_offset = php.find("$this").unwrap() as u32;
        let hit = map.lookup(this_offset);
        assert!(hit.is_some(), "Should find $this in docblock generic arg");
        if let SymbolKind::SelfStaticParent { ref keyword } = hit.unwrap().kind {
            assert_eq!(keyword, "static");
        } else {
            panic!(
                "Expected SelfStaticParent for $this, got {:?}",
                hit.unwrap().kind
            );
        }
    }

    #[test]
    fn attribute_class_reference() {
        let php = concat!(
            "<?php\n",
            "#[\\Illuminate\\Database\\Eloquent\\Attributes\\CollectedBy(ReviewCollection::class)]\n",
            "class Review {}\n",
        );
        let map = parse_and_extract(php);

        // The attribute class name should be a ClassReference.
        let attr_offset = php
            .find("\\Illuminate\\Database\\Eloquent\\Attributes\\CollectedBy")
            .unwrap() as u32;
        let hit = map.lookup(attr_offset);
        assert!(hit.is_some(), "Should find attribute class reference");
        if let SymbolKind::ClassReference { ref name, is_fqn } = hit.unwrap().kind {
            assert_eq!(
                name,
                "Illuminate\\Database\\Eloquent\\Attributes\\CollectedBy"
            );
            assert!(is_fqn, "Attribute FQN should have is_fqn = true");
        } else {
            panic!(
                "Expected ClassReference for attribute, got {:?}",
                hit.unwrap().kind
            );
        }

        // The argument `ReviewCollection::class` should produce a ClassReference for ReviewCollection.
        let rc_offset = php.find("ReviewCollection").unwrap() as u32;
        let hit = map.lookup(rc_offset);
        assert!(
            hit.is_some(),
            "Should find ReviewCollection in attribute argument"
        );
        if let SymbolKind::ClassReference { ref name, .. } = hit.unwrap().kind {
            assert_eq!(name, "ReviewCollection");
        } else {
            panic!(
                "Expected ClassReference for ReviewCollection, got {:?}",
                hit.unwrap().kind
            );
        }

        // The class declaration name `Review` should be ClassDeclaration, not ClassReference.
        let review_offset = php.find("class Review").unwrap() as u32 + 6; // skip "class "
        let hit = map.lookup(review_offset);
        assert!(hit.is_some(), "Should find Review declaration");
        if let SymbolKind::ClassDeclaration { ref name } = hit.unwrap().kind {
            assert_eq!(name, "Review");
        } else {
            panic!(
                "Expected ClassDeclaration for Review, got {:?}",
                hit.unwrap().kind
            );
        }
    }

    #[test]
    fn fqn_type_hint_in_parameter() {
        let php = "<?php\nfunction test(\\Illuminate\\Support\\Collection $c) {}\n";
        let map = parse_and_extract(php);
        let fqn_offset = php.find("\\Illuminate\\Support\\Collection").unwrap() as u32;
        let hit = map.lookup(fqn_offset);
        assert!(hit.is_some(), "Should find FQN type hint in parameter");
        if let SymbolKind::ClassReference { ref name, is_fqn } = hit.unwrap().kind {
            assert_eq!(name, "Illuminate\\Support\\Collection");
            assert!(is_fqn, "FQN parameter type hint should have is_fqn = true");
        } else {
            panic!(
                "Expected ClassReference for FQN param hint, got {:?}",
                hit.unwrap().kind
            );
        }
    }

    #[test]
    fn fqn_extends_class_reference() {
        let php = "<?php\nclass Review extends \\Illuminate\\Database\\Eloquent\\Model {}\n";
        let map = parse_and_extract(php);
        let fqn_offset = php.find("\\Illuminate\\Database\\Eloquent\\Model").unwrap() as u32;
        let hit = map.lookup(fqn_offset);
        assert!(hit.is_some(), "Should find FQN in extends clause");
        if let SymbolKind::ClassReference { ref name, is_fqn } = hit.unwrap().kind {
            assert_eq!(name, "Illuminate\\Database\\Eloquent\\Model");
            assert!(is_fqn, "FQN extends should have is_fqn = true");
        } else {
            panic!(
                "Expected ClassReference for FQN extends, got {:?}",
                hit.unwrap().kind
            );
        }
    }

    #[test]
    fn fqn_lookup_at_middle_of_name() {
        // Verify that the symbol span covers the ENTIRE FQN so that
        // clicking anywhere within it (not just at the leading `\`)
        // resolves correctly.
        let php = concat!(
            "<?php\n",
            "#[\\Illuminate\\Database\\Eloquent\\Attributes\\CollectedBy(ReviewCollection::class)]\n",
            "class Review extends \\Illuminate\\Database\\Eloquent\\Model\n",
            "{\n",
            "    /** @return \\Illuminate\\Database\\Eloquent\\Relations\\HasMany<Review, $this> */\n",
            "    public function replies(): mixed { return $this->hasMany(Review::class); }\n",
            "}\n",
        );
        let map = parse_and_extract(php);

        // ── Attribute FQN: click on "CollectedBy" (last segment) ──
        let cb_offset = php.find("CollectedBy").unwrap() as u32;
        let hit = map.lookup(cb_offset);
        assert!(
            hit.is_some(),
            "Should find attribute FQN when cursor is on 'CollectedBy'"
        );
        if let SymbolKind::ClassReference { ref name, is_fqn } = hit.unwrap().kind {
            assert_eq!(
                name,
                "Illuminate\\Database\\Eloquent\\Attributes\\CollectedBy"
            );
            assert!(is_fqn);
        } else {
            panic!("Expected ClassReference, got {:?}", hit.unwrap().kind);
        }

        // ── Attribute FQN: click on "Database" (middle segment) ──
        // Find the first "Database" which is inside the attribute
        let db_attr_offset = php.find("Database").unwrap() as u32;
        let hit = map.lookup(db_attr_offset);
        assert!(
            hit.is_some(),
            "Should find attribute FQN when cursor is on 'Database'"
        );
        if let SymbolKind::ClassReference { ref name, is_fqn } = hit.unwrap().kind {
            assert_eq!(
                name,
                "Illuminate\\Database\\Eloquent\\Attributes\\CollectedBy"
            );
            assert!(is_fqn);
        } else {
            panic!("Expected ClassReference, got {:?}", hit.unwrap().kind);
        }

        // ── Extends FQN: click on "Model" (last segment) ──
        let model_offset = php.find("Model\n").unwrap() as u32;
        let hit = map.lookup(model_offset);
        assert!(
            hit.is_some(),
            "Should find extends FQN when cursor is on 'Model'"
        );
        if let SymbolKind::ClassReference { ref name, is_fqn } = hit.unwrap().kind {
            assert_eq!(name, "Illuminate\\Database\\Eloquent\\Model");
            assert!(is_fqn);
        } else {
            panic!("Expected ClassReference, got {:?}", hit.unwrap().kind);
        }

        // ── Extends FQN: click on "Eloquent" (middle segment) ──
        // The second "Eloquent" is in the extends clause
        let extends_line_start = php.find("class Review extends").unwrap();
        let eloquent_in_extends =
            php[extends_line_start..].find("Eloquent").unwrap() + extends_line_start;
        let hit = map.lookup(eloquent_in_extends as u32);
        assert!(
            hit.is_some(),
            "Should find extends FQN when cursor is on 'Eloquent'"
        );
        if let SymbolKind::ClassReference { ref name, is_fqn } = hit.unwrap().kind {
            assert_eq!(name, "Illuminate\\Database\\Eloquent\\Model");
            assert!(is_fqn);
        } else {
            panic!("Expected ClassReference, got {:?}", hit.unwrap().kind);
        }

        // ── Docblock FQN: click on "HasMany" (last segment) ──
        let hm_offset = php.find("HasMany").unwrap() as u32;
        let hit = map.lookup(hm_offset);
        assert!(
            hit.is_some(),
            "Should find docblock FQN when cursor is on 'HasMany'"
        );
        if let SymbolKind::ClassReference { ref name, is_fqn } = hit.unwrap().kind {
            assert_eq!(name, "Illuminate\\Database\\Eloquent\\Relations\\HasMany");
            assert!(is_fqn);
        } else {
            panic!("Expected ClassReference, got {:?}", hit.unwrap().kind);
        }

        // ── Docblock FQN: click on "Relations" (middle segment) ──
        let rel_offset = php.find("Relations").unwrap() as u32;
        let hit = map.lookup(rel_offset);
        assert!(
            hit.is_some(),
            "Should find docblock FQN when cursor is on 'Relations'"
        );
        if let SymbolKind::ClassReference { ref name, is_fqn } = hit.unwrap().kind {
            assert_eq!(name, "Illuminate\\Database\\Eloquent\\Relations\\HasMany");
            assert!(is_fqn);
        } else {
            panic!("Expected ClassReference, got {:?}", hit.unwrap().kind);
        }

        // ── Docblock $this inside generic args ──
        let docblock_start = php.find("/** @return").unwrap();
        let this_in_doc = php[docblock_start..].find("$this").unwrap() + docblock_start;
        let hit = map.lookup(this_in_doc as u32);
        assert!(hit.is_some(), "Should find $this in docblock generic arg");
        if let SymbolKind::SelfStaticParent { ref keyword } = hit.unwrap().kind {
            assert_eq!(keyword, "static");
        } else {
            panic!(
                "Expected SelfStaticParent for $this, got {:?}",
                hit.unwrap().kind
            );
        }
    }

    // ── @template tag tests ─────────────────────────────────────────────

    #[test]
    fn template_tag_bound_type_produces_class_reference() {
        let php = concat!(
            "<?php\n",
            "/**\n",
            " * @template-covariant TNode of AstNode\n",
            " */\n",
            "class Foo {}\n",
        );
        let map = parse_and_extract(php);
        let ast_offset = php.find("AstNode").unwrap() as u32;
        let hit = map.lookup(ast_offset);
        assert!(
            hit.is_some(),
            "Should find bound type AstNode in @template tag"
        );
        if let SymbolKind::ClassReference { ref name, is_fqn } = hit.unwrap().kind {
            assert_eq!(name, "AstNode");
            assert!(!is_fqn);
        } else {
            panic!(
                "Expected ClassReference for AstNode, got {:?}",
                hit.unwrap().kind
            );
        }
    }

    #[test]
    fn template_tag_without_bound_produces_no_span() {
        let php = concat!(
            "<?php\n",
            "/**\n",
            " * @template T\n",
            " */\n",
            "class Foo {}\n",
        );
        let map = parse_and_extract(php);
        // "T" should NOT produce a ClassReference — it's a parameter name.
        let t_offset = php.find(" T\n").unwrap() as u32 + 1; // offset of 'T'
        let hit = map.lookup(t_offset);
        assert!(
            hit.is_none(),
            "Template parameter name should not be navigable"
        );
    }

    #[test]
    fn template_tag_fqn_bound() {
        let php = concat!(
            "<?php\n",
            "/**\n",
            " * @template T of \\App\\Contracts\\Renderable\n",
            " */\n",
            "class Foo {}\n",
        );
        let map = parse_and_extract(php);
        let r_offset = php.find("\\App\\Contracts\\Renderable").unwrap() as u32;
        let hit = map.lookup(r_offset);
        assert!(hit.is_some(), "Should find FQN bound type");
        if let SymbolKind::ClassReference { ref name, is_fqn } = hit.unwrap().kind {
            assert_eq!(name, "App\\Contracts\\Renderable");
            assert!(is_fqn);
        } else {
            panic!("Expected ClassReference, got {:?}", hit.unwrap().kind);
        }
    }

    #[test]
    fn template_covariant_and_contravariant_tags() {
        let php = concat!(
            "<?php\n",
            "/**\n",
            " * @template-covariant TOut of Output\n",
            " * @template-contravariant TIn of Input\n",
            " */\n",
            "class Foo {}\n",
        );
        let map = parse_and_extract(php);

        let out_offset = php.find("Output").unwrap() as u32;
        let hit = map.lookup(out_offset);
        assert!(hit.is_some(), "Should find Output bound");
        if let SymbolKind::ClassReference { ref name, .. } = hit.unwrap().kind {
            assert_eq!(name, "Output");
        } else {
            panic!("Expected ClassReference for Output");
        }

        let in_offset = php.find("Input").unwrap() as u32;
        let hit = map.lookup(in_offset);
        assert!(hit.is_some(), "Should find Input bound");
        if let SymbolKind::ClassReference { ref name, .. } = hit.unwrap().kind {
            assert_eq!(name, "Input");
        } else {
            panic!("Expected ClassReference for Input");
        }
    }

    #[test]
    fn phpstan_template_tag() {
        let php = concat!(
            "<?php\n",
            "/**\n",
            " * @phpstan-template T of Collection\n",
            " */\n",
            "class Foo {}\n",
        );
        let map = parse_and_extract(php);
        let c_offset = php.find("Collection").unwrap() as u32;
        let hit = map.lookup(c_offset);
        assert!(
            hit.is_some(),
            "Should find Collection bound from @phpstan-template"
        );
        if let SymbolKind::ClassReference { ref name, .. } = hit.unwrap().kind {
            assert_eq!(name, "Collection");
        } else {
            panic!("Expected ClassReference for Collection");
        }
    }

    // ── VarDefSite extraction tests ─────────────────────────────────────

    #[test]
    fn var_def_assignment_in_function() {
        let php = "<?php\nfunction foo() {\n    $x = 42;\n}\n";
        let map = parse_and_extract(php);
        assert!(
            !map.var_defs.is_empty(),
            "Should have at least one VarDefSite"
        );
        let def = map
            .var_defs
            .iter()
            .find(|d| d.name == "x")
            .expect("Should find $x def");
        assert_eq!(def.kind, VarDefKind::Assignment);
        // The definition should be inside the function scope.
        assert_ne!(
            def.scope_start, 0,
            "scope_start should be function body brace, not top-level"
        );
    }

    #[test]
    fn var_def_parameter_in_function() {
        let php = "<?php\nfunction greet(string $name) {\n    echo $name;\n}\n";
        let map = parse_and_extract(php);
        let def = map
            .var_defs
            .iter()
            .find(|d| d.name == "name" && d.kind == VarDefKind::Parameter);
        assert!(def.is_some(), "Should find parameter $name as VarDefSite");
    }

    #[test]
    fn var_def_foreach_key_and_value() {
        let php = "<?php\nfunction f() {\n    foreach ($items as $key => $val) { }\n}\n";
        let map = parse_and_extract(php);
        let key_def = map
            .var_defs
            .iter()
            .find(|d| d.name == "key" && d.kind == VarDefKind::Foreach);
        let val_def = map
            .var_defs
            .iter()
            .find(|d| d.name == "val" && d.kind == VarDefKind::Foreach);
        assert!(key_def.is_some(), "Should find foreach key $key");
        assert!(val_def.is_some(), "Should find foreach value $val");
    }

    #[test]
    fn var_def_catch_variable() {
        let php = "<?php\nfunction f() {\n    try { } catch (Exception $e) { }\n}\n";
        let map = parse_and_extract(php);
        let def = map
            .var_defs
            .iter()
            .find(|d| d.name == "e" && d.kind == VarDefKind::Catch);
        assert!(def.is_some(), "Should find catch variable $e");
    }

    #[test]
    fn var_def_static_variable() {
        let php = "<?php\nfunction f() {\n    static $counter = 0;\n}\n";
        let map = parse_and_extract(php);
        let def = map
            .var_defs
            .iter()
            .find(|d| d.name == "counter" && d.kind == VarDefKind::StaticDecl);
        assert!(def.is_some(), "Should find static variable $counter");
    }

    #[test]
    fn var_def_global_variable() {
        let php = "<?php\nfunction f() {\n    global $db;\n}\n";
        let map = parse_and_extract(php);
        let def = map
            .var_defs
            .iter()
            .find(|d| d.name == "db" && d.kind == VarDefKind::GlobalDecl);
        assert!(def.is_some(), "Should find global variable $db");
    }

    #[test]
    fn var_def_array_destructuring() {
        let php = "<?php\nfunction f() {\n    [$a, $b] = explode(',', $str);\n}\n";
        let map = parse_and_extract(php);
        let a_def = map
            .var_defs
            .iter()
            .find(|d| d.name == "a" && d.kind == VarDefKind::ArrayDestructuring);
        let b_def = map
            .var_defs
            .iter()
            .find(|d| d.name == "b" && d.kind == VarDefKind::ArrayDestructuring);
        assert!(a_def.is_some(), "Should find $a from array destructuring");
        assert!(b_def.is_some(), "Should find $b from array destructuring");
    }

    #[test]
    fn var_def_list_destructuring() {
        let php = "<?php\nfunction f() {\n    list($a, $b) = func();\n}\n";
        let map = parse_and_extract(php);
        let a_def = map
            .var_defs
            .iter()
            .find(|d| d.name == "a" && d.kind == VarDefKind::ListDestructuring);
        let b_def = map
            .var_defs
            .iter()
            .find(|d| d.name == "b" && d.kind == VarDefKind::ListDestructuring);
        assert!(a_def.is_some(), "Should find $a from list destructuring");
        assert!(b_def.is_some(), "Should find $b from list destructuring");
    }

    #[test]
    fn var_def_method_parameter() {
        let php =
            "<?php\nclass Foo {\n    public function bar(int $x) {\n        return $x;\n    }\n}\n";
        let map = parse_and_extract(php);
        let def = map
            .var_defs
            .iter()
            .find(|d| d.name == "x" && d.kind == VarDefKind::Parameter);
        assert!(def.is_some(), "Should find method parameter $x");
    }

    #[test]
    fn var_def_closure_parameter() {
        let php = "<?php\nfunction f() {\n    $fn = function (string $s) { return $s; };\n}\n";
        let map = parse_and_extract(php);
        let def = map
            .var_defs
            .iter()
            .find(|d| d.name == "s" && d.kind == VarDefKind::Parameter);
        assert!(def.is_some(), "Should find closure parameter $s");
    }

    #[test]
    fn var_def_arrow_function_parameter() {
        let php = "<?php\nfunction f() {\n    $fn = fn(int $n) => $n * 2;\n}\n";
        let map = parse_and_extract(php);
        let def = map
            .var_defs
            .iter()
            .find(|d| d.name == "n" && d.kind == VarDefKind::Parameter);
        assert!(def.is_some(), "Should find arrow function parameter $n");
    }

    // ── Scope tracking tests ────────────────────────────────────────────

    #[test]
    fn scopes_populated_for_function() {
        let php = "<?php\nfunction foo() {\n    $x = 1;\n}\n";
        let map = parse_and_extract(php);
        assert!(
            !map.scopes.is_empty(),
            "Should have at least one scope for the function body"
        );
    }

    #[test]
    fn scopes_populated_for_method() {
        let php = "<?php\nclass A {\n    public function m() {\n        $y = 2;\n    }\n}\n";
        let map = parse_and_extract(php);
        assert!(
            !map.scopes.is_empty(),
            "Should have at least one scope for the method body"
        );
    }

    #[test]
    fn scopes_populated_for_closure() {
        let php = "<?php\nfunction f() {\n    $fn = function () { $z = 3; };\n}\n";
        let map = parse_and_extract(php);
        // One for the outer function, one for the closure.
        assert!(
            map.scopes.len() >= 2,
            "Should have scopes for both function and closure"
        );
    }

    #[test]
    fn find_enclosing_scope_top_level() {
        let php = "<?php\n$x = 1;\n";
        let map = parse_and_extract(php);
        // Top-level offset should return scope_start 0.
        assert_eq!(map.find_enclosing_scope(7), 0);
    }

    #[test]
    fn find_enclosing_scope_inside_function() {
        let php = "<?php\nfunction foo() {\n    $x = 1;\n}\n";
        let map = parse_and_extract(php);
        // Offset inside the function body should return the function's scope_start.
        let body_offset = php.find('{').unwrap() as u32;
        let x_offset = php.find("$x").unwrap() as u32;
        let scope = map.find_enclosing_scope(x_offset);
        assert_eq!(
            scope, body_offset,
            "Should find the function body as the enclosing scope"
        );
    }

    // ── find_var_definition tests ───────────────────────────────────────

    #[test]
    fn find_var_definition_returns_most_recent() {
        let php = "<?php\nfunction f() {\n    $x = 1;\n    $x = 2;\n    echo $x;\n}\n";
        let map = parse_and_extract(php);
        let echo_x_offset = php.rfind("$x").unwrap() as u32;
        let scope = map.find_enclosing_scope(echo_x_offset);
        let def = map.find_var_definition("x", echo_x_offset, scope);
        assert!(def.is_some(), "Should find a definition for $x");
        // The most recent definition should be `$x = 2;` not `$x = 1;`
        let second_assign_offset = php.find("$x = 2").unwrap() as u32;
        assert_eq!(
            def.unwrap().offset,
            second_assign_offset,
            "Should find the second assignment"
        );
    }

    #[test]
    fn find_var_definition_parameter_found() {
        let php = "<?php\nfunction greet(string $name) {\n    echo $name;\n}\n";
        let map = parse_and_extract(php);
        let echo_name_offset = php.rfind("$name").unwrap() as u32;
        let scope = map.find_enclosing_scope(echo_name_offset);
        let def = map.find_var_definition("name", echo_name_offset, scope);
        assert!(def.is_some(), "Should find parameter $name");
        assert_eq!(def.unwrap().kind, VarDefKind::Parameter);
    }

    #[test]
    fn find_var_definition_none_when_no_def() {
        let php = "<?php\nfunction f() {\n    echo $undefined;\n}\n";
        let map = parse_and_extract(php);
        let offset = php.find("$undefined").unwrap() as u32;
        let scope = map.find_enclosing_scope(offset);
        let def = map.find_var_definition("undefined", offset, scope);
        assert!(def.is_none(), "Should return None for undefined variable");
    }

    #[test]
    fn find_var_definition_respects_scope() {
        let php = concat!(
            "<?php\n",
            "function outer() {\n",
            "    $x = 'outer';\n",
            "    $fn = function () {\n",
            "        echo $x;\n", // $x not defined in closure scope
            "    };\n",
            "}\n",
        );
        let map = parse_and_extract(php);
        let echo_x_offset = php.rfind("$x").unwrap() as u32;
        let scope = map.find_enclosing_scope(echo_x_offset);
        let def = map.find_var_definition("x", echo_x_offset, scope);
        // $x is defined in outer scope, not closure scope, so should be None.
        assert!(def.is_none(), "Should not find $x from a different scope");
    }

    #[test]
    fn assignment_effective_from_excludes_rhs() {
        // In `$x = $x + 1;`, the RHS $x should see the *previous* definition,
        // not the one being written.
        let php = concat!(
            "<?php\n",
            "function f() {\n",
            "    $x = 10;\n",
            "    $x = $x + 1;\n",
            "}\n",
        );
        let map = parse_and_extract(php);
        // The RHS `$x` in `$x = $x + 1;`
        let rhs_x_offset = php.rfind("$x + 1").unwrap() as u32;
        let scope = map.find_enclosing_scope(rhs_x_offset);
        let def = map.find_var_definition("x", rhs_x_offset, scope);
        assert!(def.is_some(), "Should find a definition for RHS $x");
        // Should point to `$x = 10;`, not `$x = $x + 1;`
        let first_assign_offset = php.find("$x = 10").unwrap() as u32;
        assert_eq!(
            def.unwrap().offset,
            first_assign_offset,
            "RHS $x should see the first assignment, not the one being written"
        );
    }

    // ── is_at_var_definition tests ──────────────────────────────────────

    #[test]
    fn is_at_var_definition_on_assignment_lhs() {
        let php = "<?php\nfunction f() {\n    $x = 42;\n}\n";
        let map = parse_and_extract(php);
        let x_offset = php.find("$x = 42").unwrap() as u32;
        assert!(
            map.is_at_var_definition("x", x_offset),
            "Should detect cursor on assignment LHS as at-definition"
        );
        // One byte into the token (on the 'x')
        assert!(
            map.is_at_var_definition("x", x_offset + 1),
            "Should detect cursor on 'x' of '$x' as at-definition"
        );
    }

    #[test]
    fn is_at_var_definition_on_parameter() {
        let php = "<?php\nfunction greet(string $name) {\n    echo $name;\n}\n";
        let map = parse_and_extract(php);
        let param_offset = php.find("$name)").unwrap() as u32;
        assert!(
            map.is_at_var_definition("name", param_offset),
            "Should detect cursor on parameter as at-definition"
        );
    }

    #[test]
    fn is_at_var_definition_false_on_usage() {
        let php = "<?php\nfunction f() {\n    $x = 42;\n    echo $x;\n}\n";
        let map = parse_and_extract(php);
        let echo_x_offset = php.rfind("$x").unwrap() as u32;
        assert!(
            !map.is_at_var_definition("x", echo_x_offset),
            "Should NOT detect cursor on variable usage as at-definition"
        );
    }

    #[test]
    fn nested_array_destructuring_var_defs() {
        let php = "<?php\nfunction f() {\n    [[$a, $b], $c] = getData();\n}\n";
        let map = parse_and_extract(php);
        let a_def = map
            .var_defs
            .iter()
            .find(|d| d.name == "a" && d.kind == VarDefKind::ArrayDestructuring);
        let b_def = map
            .var_defs
            .iter()
            .find(|d| d.name == "b" && d.kind == VarDefKind::ArrayDestructuring);
        let c_def = map
            .var_defs
            .iter()
            .find(|d| d.name == "c" && d.kind == VarDefKind::ArrayDestructuring);
        assert!(a_def.is_some(), "Should find $a from nested destructuring");
        assert!(b_def.is_some(), "Should find $b from nested destructuring");
        assert!(c_def.is_some(), "Should find $c from outer destructuring");
    }

    #[test]
    fn var_defs_sorted_by_scope_start_then_offset() {
        let php = concat!(
            "<?php\n",
            "function a() {\n",
            "    $x = 1;\n",
            "    $y = 2;\n",
            "}\n",
            "function b() {\n",
            "    $z = 3;\n",
            "}\n",
        );
        let map = parse_and_extract(php);
        // Verify the var_defs are sorted by (scope_start, offset).
        for window in map.var_defs.windows(2) {
            let (a, b) = (&window[0], &window[1]);
            assert!(
                (a.scope_start, a.offset) <= (b.scope_start, b.offset),
                "var_defs should be sorted by (scope_start, offset): ({}, {}) vs ({}, {})",
                a.scope_start,
                a.offset,
                b.scope_start,
                b.offset,
            );
        }
    }

    #[test]
    fn top_level_var_def_has_scope_start_zero() {
        let php = "<?php\n$global = 'hello';\n";
        let map = parse_and_extract(php);
        let def = map.var_defs.iter().find(|d| d.name == "global");
        assert!(def.is_some(), "Should find top-level $global");
        assert_eq!(
            def.unwrap().scope_start,
            0,
            "Top-level definitions should have scope_start 0"
        );
    }

    // ── Template param definition lookup tests ──────────────────────────

    #[test]
    fn template_param_def_recorded_for_class() {
        let php = concat!(
            "<?php\n",
            "/**\n",
            " * @template TKey of array-key\n",
            " * @template TModel\n",
            " */\n",
            "class Collection {\n",
            "    /** @return array<TKey, TModel> */\n",
            "    public function all(): array { return []; }\n",
            "}\n",
        );
        let map = parse_and_extract(php);

        assert!(
            map.template_defs.len() >= 2,
            "Should record at least 2 template defs, got {}",
            map.template_defs.len()
        );

        let tkey = map.template_defs.iter().find(|d| d.name == "TKey");
        assert!(tkey.is_some(), "Should find TKey template def");
        let tkey = tkey.unwrap();
        assert_eq!(
            &php[tkey.name_offset as usize..(tkey.name_offset + 4) as usize],
            "TKey",
            "name_offset should point to the TKey text"
        );

        let tmodel = map.template_defs.iter().find(|d| d.name == "TModel");
        assert!(tmodel.is_some(), "Should find TModel template def");
    }

    #[test]
    fn template_param_def_lookup_in_same_class() {
        let php = concat!(
            "<?php\n",
            "/**\n",
            " * @template TKey of array-key\n",
            " * @template TModel\n",
            " */\n",
            "class Collection {\n",
            "    /** @return array<TKey, TModel> */\n",
            "    public function all(): array { return []; }\n",
            "}\n",
        );
        let map = parse_and_extract(php);

        // Cursor inside the class body (on the @return line) should find TKey
        let return_line_offset = php.find("@return").unwrap() as u32;
        let found = map.find_template_def("TKey", return_line_offset);
        assert!(
            found.is_some(),
            "Should find TKey from within the class body"
        );
        assert_eq!(found.unwrap().name, "TKey");

        let found = map.find_template_def("TModel", return_line_offset);
        assert!(
            found.is_some(),
            "Should find TModel from within the class body"
        );
    }

    #[test]
    fn template_param_def_not_found_outside_scope() {
        let php = concat!(
            "<?php\n",
            "/**\n",
            " * @template T\n",
            " */\n",
            "class Foo {}\n",
            "class Bar {}\n",
        );
        let map = parse_and_extract(php);

        let bar_offset = php.find("class Bar").unwrap() as u32;
        let found = map.find_template_def("T", bar_offset);
        assert!(found.is_none(), "T should NOT be found outside Foo's scope");
    }

    #[test]
    fn template_param_def_method_level() {
        let php = concat!(
            "<?php\n",
            "class Mapper {\n",
            "    /**\n",
            "     * @template T\n",
            "     * @param T $item\n",
            "     * @return T\n",
            "     */\n",
            "    public function wrap(object $item): object { return $item; }\n",
            "}\n",
        );
        let map = parse_and_extract(php);

        let t_def = map.template_defs.iter().find(|d| d.name == "T");
        assert!(t_def.is_some(), "Should find method-level template T");

        // Should be findable from within the method's docblock
        let param_line = php.find("@param T").unwrap() as u32;
        let found = map.find_template_def("T", param_line);
        assert!(
            found.is_some(),
            "Should find T from within the method docblock"
        );
    }

    // ── $this as SelfStaticParent tests ─────────────────────────────────

    #[test]
    fn this_variable_emits_self_static_parent() {
        let php = concat!(
            "<?php\n",
            "class Foo {\n",
            "    public function bar(): void {\n",
            "        $this->baz();\n",
            "    }\n",
            "}\n",
        );
        let map = parse_and_extract(php);

        // Find the $this token (not the one inside ->baz subject_text)
        let this_offset = php.find("$this->baz").unwrap() as u32;
        let hit = map.lookup(this_offset);
        assert!(hit.is_some(), "Should find a span at $this");
        match &hit.unwrap().kind {
            SymbolKind::SelfStaticParent { keyword } => {
                assert_eq!(keyword, "static", "$this should map to 'static' keyword");
            }
            other => panic!("Expected SelfStaticParent for $this, got {:?}", other),
        }
    }

    #[test]
    fn this_variable_standalone_emits_self_static_parent() {
        // `$this` on its own (not as part of ->)
        let php = concat!(
            "<?php\n",
            "class Foo {\n",
            "    public function bar(): self {\n",
            "        return $this;\n",
            "    }\n",
            "}\n",
        );
        let map = parse_and_extract(php);

        let this_offset = php.find("$this;").unwrap() as u32;
        let hit = map.lookup(this_offset);
        assert!(hit.is_some(), "Should find a span at standalone $this");
        match &hit.unwrap().kind {
            SymbolKind::SelfStaticParent { keyword } => {
                assert_eq!(keyword, "static");
            }
            other => panic!(
                "Expected SelfStaticParent for standalone $this, got {:?}",
                other
            ),
        }
    }

    #[test]
    fn regular_variable_still_emits_variable() {
        let php = "<?php\nfunction f() { $x = 1; }\n";
        let map = parse_and_extract(php);

        let x_offset = php.find("$x").unwrap() as u32;
        let hit = map.lookup(x_offset);
        assert!(hit.is_some());
        match &hit.unwrap().kind {
            SymbolKind::Variable { name } => {
                assert_eq!(name, "x", "$x should still emit Variable");
            }
            other => panic!("Expected Variable for $x, got {:?}", other),
        }
    }

    // ── Array suffix stripping tests ────────────────────────────────────

    #[test]
    fn docblock_array_suffix_type_produces_class_reference() {
        let php = concat!(
            "<?php\n",
            "class AstNode {}\n",
            "class Foo {\n",
            "    /** @return AstNode[] */\n",
            "    public function getChildren(): array { return []; }\n",
            "}\n",
        );
        let map = parse_and_extract(php);

        // Find the AstNode in the @return tag (not the class declaration)
        let docblock_start = php.find("/** @return").unwrap();
        let ast_in_doc = php[docblock_start..].find("AstNode").unwrap() + docblock_start;
        let hit = map.lookup(ast_in_doc as u32);
        assert!(hit.is_some(), "Should find AstNode in @return AstNode[]");
        if let SymbolKind::ClassReference { ref name, .. } = hit.unwrap().kind {
            assert_eq!(
                name, "AstNode",
                "Name should be 'AstNode' without [] suffix"
            );
        } else {
            panic!(
                "Expected ClassReference for AstNode[], got {:?}",
                hit.unwrap().kind
            );
        }
    }

    #[test]
    fn docblock_array_suffix_span_excludes_brackets() {
        let php = concat!(
            "<?php\n",
            "class Item {}\n",
            "class Holder {\n",
            "    /** @var Item[] */\n",
            "    public array $items = [];\n",
            "}\n",
        );
        let map = parse_and_extract(php);

        let docblock_start = php.find("/** @var").unwrap();
        let item_in_doc = php[docblock_start..].find("Item").unwrap() + docblock_start;
        let hit = map.lookup(item_in_doc as u32);
        assert!(hit.is_some(), "Should find Item in @var Item[]");
        let span = hit.unwrap();
        let span_text = &php[span.start as usize..span.end as usize];
        assert_eq!(
            span_text, "Item",
            "Span should cover 'Item' only, not 'Item[]'"
        );
    }

    // ── Conditional return type tests ───────────────────────────────────

    #[test]
    fn conditional_return_type_all_parts_get_spans() {
        // PHPStan conditional return type:
        //   ($abstract is class-string<TClass> ? TClass : Container)
        // All three type positions (class-string<TClass>, TClass, Container)
        // should produce ClassReference spans.
        let php = concat!(
            "<?php\n",
            "class Container {\n",
            "    /**\n",
            "     * @template TClass\n",
            "     * @param string|null $abstract\n",
            "     * @return ($abstract is class-string<TClass> ? TClass : Container)\n",
            "     */\n",
            "    public function make($abstract) {}\n",
            "}\n",
        );
        let map = parse_and_extract(php);

        // Find offsets of each TClass and Container in the @return line.
        let return_line_start = php.find("@return").unwrap();

        // First TClass — inside class-string<TClass>
        let first_tclass_offset =
            php[return_line_start..].find("TClass").unwrap() + return_line_start;
        let hit1 = map.lookup(first_tclass_offset as u32);
        assert!(
            hit1.is_some(),
            "Should find first TClass (inside class-string<TClass>)"
        );
        let span1 = hit1.unwrap();
        assert_eq!(&php[span1.start as usize..span1.end as usize], "TClass");

        // Second TClass — the true branch of the conditional
        let after_first = first_tclass_offset + "TClass".len();
        let second_tclass_offset = php[after_first..].find("TClass").unwrap() + after_first;
        let hit2 = map.lookup(second_tclass_offset as u32);
        assert!(
            hit2.is_some(),
            "Should find second TClass (true branch of conditional)"
        );
        let span2 = hit2.unwrap();
        assert_eq!(&php[span2.start as usize..span2.end as usize], "TClass");
        assert_ne!(
            span1.start, span2.start,
            "The two TClass spans should be at different offsets"
        );

        // Container — the false branch of the conditional
        let container_in_return =
            php[return_line_start..].find("Container").unwrap() + return_line_start;
        let hit3 = map.lookup(container_in_return as u32);
        assert!(
            hit3.is_some(),
            "Should find Container (false branch of conditional)"
        );
        let span3 = hit3.unwrap();
        assert_eq!(&php[span3.start as usize..span3.end as usize], "Container");
    }

    #[test]
    fn conditional_return_type_with_not_keyword() {
        let php = concat!(
            "<?php\n",
            "class Foo {\n",
            "    /**\n",
            "     * @return ($x is not null ? Foo : Bar)\n",
            "     */\n",
            "    public function test($x) {}\n",
            "}\n",
            "class Bar {}\n",
        );
        let map = parse_and_extract(php);

        let return_start = php.find("@return").unwrap();
        let foo_in_return = php[return_start..].find("Foo").unwrap() + return_start;
        let bar_in_return = php[return_start..].find("Bar").unwrap() + return_start;

        let hit_foo = map.lookup(foo_in_return as u32);
        assert!(
            hit_foo.is_some(),
            "Should find Foo in true branch of conditional with 'is not'"
        );

        let hit_bar = map.lookup(bar_in_return as u32);
        assert!(
            hit_bar.is_some(),
            "Should find Bar in false branch of conditional with 'is not'"
        );
    }

    // ── var_def_kind_at tests ───────────────────────────────────────────

    #[test]
    fn var_def_kind_at_returns_parameter() {
        let php = concat!(
            "<?php\n",
            "class Ctrl {\n",
            "    public function handle(Request $req) {}\n",
            "}\n",
        );
        let map = parse_and_extract(php);

        let dollar_offset = php.find("$req").unwrap() as u32;
        let kind = map.var_def_kind_at("req", dollar_offset);
        assert_eq!(
            kind,
            Some(&VarDefKind::Parameter),
            "Should detect $req as Parameter"
        );

        let kind2 = map.var_def_kind_at("req", dollar_offset + 1);
        assert_eq!(
            kind2,
            Some(&VarDefKind::Parameter),
            "Should detect cursor on 'r' as Parameter"
        );
    }

    #[test]
    fn var_def_kind_at_returns_catch() {
        let php = concat!(
            "<?php\n",
            "function f() {\n",
            "    try {} catch (\\Exception $e) {}\n",
            "}\n",
        );
        let map = parse_and_extract(php);

        let dollar_offset = php.find("$e)").unwrap() as u32;
        let kind = map.var_def_kind_at("e", dollar_offset);
        assert_eq!(kind, Some(&VarDefKind::Catch), "Should detect $e as Catch");
    }

    #[test]
    fn var_def_kind_at_returns_foreach() {
        let php = concat!(
            "<?php\n",
            "function f() {\n",
            "    foreach ($items as $item) {}\n",
            "}\n",
        );
        let map = parse_and_extract(php);

        let dollar_offset = php.find("$item)").unwrap() as u32;
        let kind = map.var_def_kind_at("item", dollar_offset);
        assert_eq!(
            kind,
            Some(&VarDefKind::Foreach),
            "Should detect $item as Foreach"
        );
    }

    #[test]
    fn var_def_kind_at_returns_none_on_usage() {
        let php = concat!(
            "<?php\n",
            "function f() {\n",
            "    $x = 1;\n",
            "    echo $x;\n",
            "}\n",
        );
        let map = parse_and_extract(php);

        let echo_x_offset = php.rfind("$x").unwrap() as u32;
        let kind = map.var_def_kind_at("x", echo_x_offset);
        assert!(kind.is_none(), "Should return None for variable usage site");
    }
}
