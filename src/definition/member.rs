/// Member-access definition resolution.
///
/// This module handles go-to-definition for member references — methods,
/// properties, and constants accessed via `->`, `?->`, or `::` operators.
///
/// Supported patterns:
///   - `$this->method()`, `$this->property`
///   - `$var->method()`, `$var->property`
///   - `self::method()`, `self::CONST`, `self::$staticProp`
///   - `static::method()`, `parent::method()`
///   - `ClassName::method()`, `ClassName::CONST`, `ClassName::$staticProp`
///   - Chained access: `$this->prop->method()`, `app()->method()`
///
/// Resolution walks the class hierarchy (parent classes, traits, mixins)
/// to find the declaring class and locates the member position in its
/// source file.
use tower_lsp::lsp_types::*;

use super::point_location;
use crate::Backend;
use crate::completion::resolver::ResolutionCtx;
use crate::docblock;
use crate::subject_extraction::{
    collapse_continuation_lines, extract_arrow_subject, extract_double_colon_subject,
};
use crate::types::*;
use crate::util::short_name;
use crate::virtual_members::laravel::{
    ELOQUENT_BUILDER_FQN, accessor_method_candidates, extends_eloquent_model,
};

/// The kind of class member being resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MemberKind {
    Method,
    Property,
    Constant,
}

/// Hint about whether the member access looks like a method call or a property
/// access.  Used to disambiguate when a class has both a method and a property
/// with the same name (e.g. `id()` method vs `$id` property).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MemberAccessHint {
    /// Followed by `(` — looks like a method call.
    MethodCall,
    /// No `(` after the name — looks like a property / constant access.
    PropertyAccess,
    /// Cannot determine (fallback to original order).
    Unknown,
}

impl Backend {
    // ─── Member Definition Resolution ───────────────────────────────────────

    /// Try to resolve a member access pattern and jump to the member's
    /// declaration.
    ///
    /// Detects `::`, `->`, and `?->` before the word under the cursor,
    /// resolves the owning class, and finds the member position in the
    /// class's source file.
    pub(super) fn resolve_member_definition(
        &self,
        uri: &str,
        content: &str,
        position: Position,
        member_name: &str,
    ) -> Option<Location> {
        // 1. Detect the access operator and extract the subject (left side).
        let (subject, access_kind) = Self::extract_member_access_context(content, position)?;

        // 2. Gather context needed for class resolution.
        let cursor_offset = Self::position_to_offset(content, position);
        let ctx = self.file_context(uri);

        let current_class = Self::find_class_at_offset(&ctx.classes, cursor_offset).cloned();

        let class_loader = self.class_loader(&ctx);
        let function_loader = self.function_loader(&ctx);

        // 3. Resolve the subject to all candidate classes.
        //    When a variable is assigned different types in conditional
        //    branches (e.g. if/else), multiple candidates are returned.
        let rctx = ResolutionCtx {
            current_class: current_class.as_ref(),
            all_classes: &ctx.classes,
            content,
            cursor_offset,
            class_loader: &class_loader,
            function_loader: Some(&function_loader),
        };
        let candidates = Self::resolve_target_classes(&subject, access_kind, &rctx);

        if candidates.is_empty() {
            return None;
        }

        // Determine whether this looks like a method call or property access.
        let access_hint = Self::detect_member_access_hint(content, position, member_name);

        // 4. Try each candidate class and pick the first one where the
        //    member actually exists (directly or via inheritance).
        for target_class in &candidates {
            // Candidates from resolve_target_classes may be fully-resolved
            // (merged) classes that include virtual/mixin members directly
            // in their methods list (e.g. when generic args triggered
            // resolve_class_fully inside type_hint_to_classes).
            // find_declaring_class needs the raw (unmerged) class so it
            // can trace the member to the actual declaring class through
            // the real inheritance/mixin chain.
            let raw_class = Self::reload_raw_class(target_class, &ctx.classes, &class_loader);
            let lookup_class = raw_class.as_ref().unwrap_or(target_class);

            // Check if the member name is a trait `as` alias on this class.
            // If so, resolve to the original method name and (optionally) the
            // source trait so we jump to the actual method definition rather
            // than failing to find an alias that only exists after inheritance
            // resolution.
            let (effective_name, alias_trait) =
                Self::resolve_trait_alias(target_class, member_name);

            // If we know the exact source trait from the alias, go directly
            // to that trait's method definition.
            if let Some(ref trait_name) = alias_trait
                && let Some(trait_info) = class_loader(trait_name)
                && Self::classify_member(&trait_info, &effective_name, access_hint).is_some()
                && let Some((class_uri, class_content)) =
                    self.find_class_file_content(trait_name, uri, content)
                && let Some(member_position) = Self::find_member_position_in_range(
                    &class_content,
                    &effective_name,
                    MemberKind::Method,
                    Some((
                        trait_info.start_offset as usize,
                        trait_info.end_offset as usize,
                    )),
                )
                && let Ok(parsed_uri) = Url::parse(&class_uri)
            {
                return Some(point_location(parsed_uri, member_position));
            }

            // ── Scope method mapping ────────────────────────────────
            // Laravel scope methods are defined as `scopeActive()` but
            // invoked as `active()`.  When the effective name doesn't
            // exist as a real member, check if `scopeXxx` does and
            // redirect to that method definition instead.
            let scope_name = Self::scope_method_name(&effective_name);
            let (search_name, declaring_class, declaring_fqn) =
                match Self::find_declaring_class(lookup_class, &effective_name, &class_loader) {
                    Some((cls, fqn)) => (effective_name.clone(), cls, fqn),
                    None => {
                        // Try scope mapping: active → scopeActive
                        match Self::find_declaring_class(lookup_class, &scope_name, &class_loader) {
                            Some((cls, fqn)) => (scope_name.clone(), cls, fqn),
                            None => {
                                // Try scope-on-Builder: when the target
                                // is an Eloquent Builder<Model>, look
                                // for scopeXxx on the model class.
                                match Self::find_scope_on_builder_model(
                                    target_class,
                                    lookup_class,
                                    &effective_name,
                                    &class_loader,
                                ) {
                                    Some((cls, fqn, sname)) => (sname, cls, fqn),
                                    None => {
                                        // Try accessor mapping: display_name →
                                        // getDisplayNameAttribute or avatarUrl
                                        let accessor_match =
                                            accessor_method_candidates(&effective_name)
                                                .into_iter()
                                                .find_map(|candidate| {
                                                    Self::find_declaring_class(
                                                        lookup_class,
                                                        &candidate,
                                                        &class_loader,
                                                    )
                                                    .map(|(cls, fqn)| (candidate, cls, fqn))
                                                });
                                        match accessor_match {
                                            Some((name, cls, fqn)) => (name, cls, fqn),
                                            None => {
                                                // Try builder-forwarded method: Laravel's
                                                // Model::__callStatic delegates to Builder.
                                                // The real Model has no @mixin, so we check
                                                // explicitly.
                                                match Self::find_builder_forwarded_method(
                                                    lookup_class,
                                                    &effective_name,
                                                    &class_loader,
                                                ) {
                                                    Some((cls, fqn)) => {
                                                        (effective_name.clone(), cls, fqn)
                                                    }
                                                    None => (
                                                        effective_name.clone(),
                                                        target_class.clone(),
                                                        target_class.name.clone(),
                                                    ),
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                };

            // Check that the member is actually present on the declaring class.
            let member_kind =
                match Self::classify_member(&declaring_class, &search_name, access_hint) {
                    Some(k) => k,
                    None => continue, // member not on this candidate, try next
                };

            // Locate the file that contains the declaring class.
            if let Some((class_uri, class_content)) =
                self.find_class_file_content(&declaring_fqn, uri, content)
                && let Some(member_position) = Self::find_member_position_in_range(
                    &class_content,
                    &search_name,
                    member_kind,
                    Some((
                        declaring_class.start_offset as usize,
                        declaring_class.end_offset as usize,
                    )),
                )
                && let Ok(parsed_uri) = Url::parse(&class_uri)
            {
                return Some(point_location(parsed_uri, member_position));
            }

            // ── Eloquent array entry fallback ───────────────────────
            // Virtual properties from $casts, $attributes, $fillable,
            // $guarded, and $hidden don't have a method or property
            // declaration.  Jump to the string literal entry inside the
            // array property instead.
            if extends_eloquent_model(lookup_class, &class_loader)
                && let Some((class_uri, class_content)) =
                    self.find_class_file_content(&declaring_fqn, uri, content)
                && let Some(entry_position) = Self::find_eloquent_array_entry(
                    &class_content,
                    &effective_name,
                    Some((
                        declaring_class.start_offset as usize,
                        declaring_class.end_offset as usize,
                    )),
                )
                && let Ok(parsed_uri) = Url::parse(&class_uri)
            {
                return Some(point_location(parsed_uri, entry_position));
            }
        }

        // No candidate had the member — fall back to the first candidate
        // and try the original (non-iterating) logic so we at least get
        // partial results when possible.
        let target_class = &candidates[0];
        let raw_fallback = Self::reload_raw_class(target_class, &ctx.classes, &class_loader);
        let fallback_class = raw_fallback.as_ref().unwrap_or(target_class);

        let (effective_name, alias_trait) = Self::resolve_trait_alias(target_class, member_name);

        // Direct trait lookup for aliased members in the fallback path.
        if let Some(ref trait_name) = alias_trait
            && let Some(ref trait_info) = class_loader(trait_name)
            && let Some((class_uri, class_content)) =
                self.find_class_file_content(trait_name, uri, content)
            && let Some(member_position) = Self::find_member_position_in_range(
                &class_content,
                &effective_name,
                MemberKind::Method,
                Some((
                    trait_info.start_offset as usize,
                    trait_info.end_offset as usize,
                )),
            )
            && let Ok(parsed_uri) = Url::parse(&class_uri)
        {
            return Some(point_location(parsed_uri, member_position));
        }

        // Try with scope mapping in the fallback path too.
        let scope_name = Self::scope_method_name(&effective_name);
        let (search_name, declaring_class, declaring_fqn) =
            match Self::find_declaring_class(fallback_class, &effective_name, &class_loader) {
                Some((cls, fqn)) => (effective_name.clone(), cls, fqn),
                None => {
                    match Self::find_declaring_class(fallback_class, &scope_name, &class_loader) {
                        Some((cls, fqn)) => (scope_name, cls, fqn),
                        None => {
                            // Try scope-on-Builder in the fallback path.
                            match Self::find_scope_on_builder_model(
                                target_class,
                                fallback_class,
                                &effective_name,
                                &class_loader,
                            ) {
                                Some((cls, fqn, sname)) => (sname, cls, fqn),
                                None => {
                                    // Try accessor mapping in the fallback path.
                                    let accessor_match =
                                        accessor_method_candidates(&effective_name)
                                            .into_iter()
                                            .find_map(|candidate| {
                                                Self::find_declaring_class(
                                                    fallback_class,
                                                    &candidate,
                                                    &class_loader,
                                                )
                                                .map(|(cls, fqn)| (candidate, cls, fqn))
                                            });
                                    match accessor_match {
                                        Some((name, cls, fqn)) => (name, cls, fqn),
                                        None => {
                                            match Self::find_builder_forwarded_method(
                                                fallback_class,
                                                &effective_name,
                                                &class_loader,
                                            ) {
                                                Some((cls, fqn)) => {
                                                    (effective_name.clone(), cls, fqn)
                                                }
                                                None => {
                                                    // Last resort: Eloquent array entry.
                                                    if extends_eloquent_model(
                                                        fallback_class,
                                                        &class_loader,
                                                    ) {
                                                        let fqn = fallback_class.name.clone();
                                                        if let Some((class_uri, class_content)) =
                                                            self.find_class_file_content(
                                                                &fqn, uri, content,
                                                            )
                                                            && let Some(entry_position) =
                                                                Self::find_eloquent_array_entry(
                                                                    &class_content,
                                                                    &effective_name,
                                                                    Some((
                                                                        fallback_class.start_offset
                                                                            as usize,
                                                                        fallback_class.end_offset
                                                                            as usize,
                                                                    )),
                                                                )
                                                            && let Ok(parsed_uri) =
                                                                Url::parse(&class_uri)
                                                        {
                                                            return Some(point_location(
                                                                parsed_uri,
                                                                entry_position,
                                                            ));
                                                        }
                                                    }
                                                    return None;
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            };

        let member_kind = Self::classify_member(&declaring_class, &search_name, access_hint)?;

        let (class_uri, class_content) =
            self.find_class_file_content(&declaring_fqn, uri, content)?;

        let member_position = Self::find_member_position_in_range(
            &class_content,
            &search_name,
            member_kind,
            Some((
                declaring_class.start_offset as usize,
                declaring_class.end_offset as usize,
            )),
        )?;

        let parsed_uri = Url::parse(&class_uri).ok()?;
        Some(point_location(parsed_uri, member_position))
    }

    // ─── Member Access Context Extraction ───────────────────────────────────

    /// Check whether the cursor is on the right-hand side of a member
    /// access operator (`->`, `?->`, or `::`).
    ///
    /// Returns `true` when the word under the cursor is preceded by one
    /// of these operators — meaning the word is a member name, NOT a
    /// standalone function / class / constant.  This is used by
    /// [`resolve_definition`](super::resolve) to prevent falling through
    /// to standalone symbol resolution when member resolution fails
    /// (e.g. because the owning class couldn't be determined).
    pub(crate) fn is_member_access_context(content: &str, position: Position) -> bool {
        Self::extract_member_access_context(content, position).is_some()
    }

    /// Detect the access operator (`::`, `->`, `?->`) immediately before the
    /// word under the cursor and extract the subject to its left.
    ///
    /// Returns `(subject, AccessKind)` or `None` if no operator is found.
    ///
    /// This works by:
    ///   1. Finding the start of the identifier under the cursor.
    ///   2. Skipping a `$` prefix if present (for `::$staticProp`).
    ///   3. Checking for `::`, `->`, or `?->` immediately before.
    ///   4. Extracting the subject expression to the left of the operator.
    pub(crate) fn extract_member_access_context(
        content: &str,
        position: Position,
    ) -> Option<(String, AccessKind)> {
        let lines: Vec<&str> = content.lines().collect();
        if position.line as usize >= lines.len() {
            return None;
        }

        // Collapse multi-line method chains so that continuation lines
        // (starting with `->` or `?->`) are joined with preceding lines.
        let (line, col) = collapse_continuation_lines(
            &lines,
            position.line as usize,
            position.character as usize,
        );
        let chars: Vec<char> = line.chars().collect();
        let col = col.min(chars.len());

        if chars.is_empty() {
            return None;
        }

        // Find the start of the identifier under the cursor.
        let mut i = col;

        // If the cursor is on or past the end of a word, adjust.
        if i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
            // on a word char — walk left
        } else if i > 0 && (chars[i - 1].is_alphanumeric() || chars[i - 1] == '_') {
            i -= 1;
        } else {
            return None;
        }

        // Walk left past identifier characters.
        while i > 0 && (chars[i - 1].is_alphanumeric() || chars[i - 1] == '_') {
            i -= 1;
        }

        let mut operator_end = i;

        // Skip `$` prefix (for `Class::$staticProp`).
        if operator_end > 0 && chars[operator_end - 1] == '$' {
            operator_end -= 1;
        }

        // Detect `::`.
        if operator_end >= 2 && chars[operator_end - 2] == ':' && chars[operator_end - 1] == ':' {
            let subject = extract_double_colon_subject(&chars, operator_end - 2);
            if !subject.is_empty() {
                return Some((subject, AccessKind::DoubleColon));
            }
        }

        // Detect `->`.
        if operator_end >= 2 && chars[operator_end - 2] == '-' && chars[operator_end - 1] == '>' {
            let subject = extract_arrow_subject(&chars, operator_end - 2);
            if !subject.is_empty() {
                return Some((subject, AccessKind::Arrow));
            }
        }

        // Detect `?->` (null-safe operator).
        if operator_end >= 3
            && chars[operator_end - 3] == '?'
            && chars[operator_end - 2] == '-'
            && chars[operator_end - 1] == '>'
        {
            let subject = extract_arrow_subject(&chars, operator_end - 3);
            if !subject.is_empty() {
                return Some((subject, AccessKind::Arrow));
            }
        }

        None
    }

    // ─── Member Classification ──────────────────────────────────────────────

    /// Determine the kind of member (method, property, or constant) by
    /// checking the class's parsed information.
    ///
    /// Also checks `@method` and `@property` tags in the class's deferred
    /// docblock, since those are no longer parsed eagerly into
    /// `ClassInfo.methods` / `ClassInfo.properties`.
    ///
    /// Returns `None` if the member is not found in the class.
    fn classify_member(
        class: &ClassInfo,
        member_name: &str,
        hint: MemberAccessHint,
    ) -> Option<MemberKind> {
        let has_method = class.methods.iter().any(|m| m.name == member_name);
        let has_property = class.properties.iter().any(|p| p.name == member_name);
        let has_constant = class.constants.iter().any(|c| c.name == member_name);

        // Also check the deferred class docblock for @method / @property
        // tags that are no longer in the parsed members.
        let (has_virtual_method, has_virtual_property) =
            Self::has_docblock_virtual_member(class, member_name);

        match hint {
            MemberAccessHint::PropertyAccess => {
                // Prefer property/constant over method when there's no `()`.
                if has_property || has_virtual_property {
                    return Some(MemberKind::Property);
                }
                if has_constant {
                    return Some(MemberKind::Constant);
                }
                if has_method || has_virtual_method {
                    return Some(MemberKind::Method);
                }
            }
            MemberAccessHint::MethodCall => {
                // Prefer method when followed by `()`.
                if has_method || has_virtual_method {
                    return Some(MemberKind::Method);
                }
                if has_property || has_virtual_property {
                    return Some(MemberKind::Property);
                }
                if has_constant {
                    return Some(MemberKind::Constant);
                }
            }
            MemberAccessHint::Unknown => {
                // Default order: method, property, constant.
                if has_method || has_virtual_method {
                    return Some(MemberKind::Method);
                }
                if has_property || has_virtual_property {
                    return Some(MemberKind::Property);
                }
                if has_constant {
                    return Some(MemberKind::Constant);
                }
            }
        }
        None
    }

    /// Check if a class's deferred docblock contains `@method` or `@property`
    /// tags that declare the given member name.
    ///
    /// Returns `(has_method, has_property)`.  This is a lazy parse of the
    /// class-level docblock that only runs when the member was not found
    /// among real declared members.
    fn has_docblock_virtual_member(class: &ClassInfo, member_name: &str) -> (bool, bool) {
        let doc_text = match class.class_docblock.as_deref() {
            Some(t) if !t.is_empty() => t,
            _ => return (false, false),
        };

        let has_method = docblock::extract_method_tags(doc_text)
            .iter()
            .any(|m| m.name == member_name);

        let has_property = docblock::extract_property_tags(doc_text)
            .iter()
            .any(|(name, _)| name == member_name);

        (has_method, has_property)
    }

    /// Determine whether the member name at the given position is followed by
    /// `(` (indicating a method call) or not (indicating property / constant
    /// access).
    fn detect_member_access_hint(
        content: &str,
        position: Position,
        member_name: &str,
    ) -> MemberAccessHint {
        let lines: Vec<&str> = content.lines().collect();
        let line = match lines.get(position.line as usize) {
            Some(l) => *l,
            None => return MemberAccessHint::Unknown,
        };
        let chars: Vec<char> = line.chars().collect();
        let col = (position.character as usize).min(chars.len());

        // Find the end of the member name by walking right from the cursor.
        let is_word_char = |c: char| c.is_alphanumeric() || c == '_';

        let mut end = col;
        // If cursor is on a word char, walk right to end of word.
        if end < chars.len() && is_word_char(chars[end]) {
            while end < chars.len() && is_word_char(chars[end]) {
                end += 1;
            }
        } else if end > 0 && is_word_char(chars[end - 1]) {
            // Cursor is just past the word; `end` is already correct.
        } else {
            // Try to find the member name by searching forward from col.
            if let Some(idx) = line[col..].find(member_name) {
                end = col + idx + member_name.len();
            } else {
                return MemberAccessHint::Unknown;
            }
        }

        // Skip whitespace after the word.
        let mut i = end;
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }

        if i < chars.len() && chars[i] == '(' {
            MemberAccessHint::MethodCall
        } else {
            MemberAccessHint::PropertyAccess
        }
    }

    // ─── Inheritance Chain Walking ──────────────────────────────────────────

    /// Walk up the inheritance chain to find the class that actually declares
    /// the given member.
    ///
    /// Returns `Some(ClassInfo)` of the declaring class, or `None` if the
    /// member cannot be found in any ancestor.
    /// Resolve a trait `as` alias on a class.
    ///
    /// If `member_name` matches a trait alias declared on the class, returns
    /// the original method name and (optionally) the source trait name.
    /// Otherwise returns `member_name` unchanged with no trait hint.
    /// Map a virtual scope method name to the underlying `scopeXxx` method.
    ///
    /// Laravel scope methods are defined as `scopeActive(Builder $query)`
    /// but invoked as `active()` (or `BlogAuthor::active()`).  This helper
    /// converts `"active"` → `"scopeActive"` so that go-to-definition can
    /// find the actual method declaration.
    fn scope_method_name(member_name: &str) -> String {
        let mut scope = String::with_capacity("scope".len() + member_name.len());
        scope.push_str("scope");
        let mut chars = member_name.chars();
        if let Some(first) = chars.next() {
            scope.extend(first.to_uppercase());
            scope.extend(chars);
        }
        scope
    }

    /// Check if a method is available on the Eloquent Builder for a Model
    /// subclass.
    ///
    /// Laravel's `Model::__callStatic()` forwards static calls to
    /// `Builder`, but the real `Model` class has no `@mixin Builder`
    /// annotation.  This function bridges that gap for go-to-definition
    /// by loading the Builder and searching its inheritance chain
    /// (including `@mixin Query\Builder` and traits like
    /// `BuildsQueries`) for the requested method.
    ///
    /// Returns `Some((ClassInfo, fqn))` of the declaring class when the
    /// method is found, or `None` if the class is not an Eloquent Model
    /// subclass or the method does not exist on Builder.
    /// Reload the raw (unmerged) `ClassInfo` for a candidate.
    ///
    /// Candidates returned by `resolve_target_classes` may be
    /// fully-resolved classes with virtual/mixin members baked into
    /// their `methods` list (this happens when `type_hint_to_classes`
    /// calls `resolve_class_fully` to apply generic substitutions).
    /// `find_declaring_class` needs the raw class so it can trace
    /// member declarations through the real inheritance and mixin
    /// chain instead of short-circuiting on a merged method.
    ///
    /// Returns `Some(raw)` when a reload succeeds, or `None` when the
    /// class cannot be reloaded (e.g. synthetic/anonymous classes).
    /// Find a string literal entry inside an Eloquent array property.
    ///
    /// Searches for `'member_name'` or `"member_name"` inside `$casts`,
    /// `$attributes`, `$fillable`, `$guarded`, and `$hidden` property
    /// declarations within the given class range.  Returns the position
    /// of the string literal so go-to-definition can jump to it.
    fn find_eloquent_array_entry(
        content: &str,
        member_name: &str,
        class_range: Option<(usize, usize)>,
    ) -> Option<Position> {
        let single_pattern = format!("'{member_name}'");
        let double_pattern = format!("\"{member_name}\"");
        let targets = ["$casts", "$attributes", "$fillable", "$guarded", "$hidden"];

        // Track whether we're inside one of the target property arrays.
        let mut in_target_property = false;
        let mut byte_offset: usize = 0;

        for (line_idx, line) in content.lines().enumerate() {
            let line_len = line.len() + 1;
            let in_range = match class_range {
                Some((start, end)) => byte_offset >= start && byte_offset < end,
                None => true,
            };
            if in_range {
                let trimmed = line.trim();
                // Detect property declarations for target arrays.
                if targets.iter().any(|t| trimmed.contains(t)) {
                    in_target_property = true;
                }
                // Also detect the casts() method body.
                if trimmed.contains("function casts(") {
                    in_target_property = true;
                }

                if in_target_property {
                    // Look for the member name as a string key.
                    if let Some(col) = line.find(&single_pattern) {
                        // Position cursor inside the quotes on the first
                        // letter of the column name.
                        return Some(Position {
                            line: line_idx as u32,
                            character: (col + 1) as u32,
                        });
                    }
                    if let Some(col) = line.find(&double_pattern) {
                        return Some(Position {
                            line: line_idx as u32,
                            character: (col + 1) as u32,
                        });
                    }

                    // A line ending with `];` or just `];` closes the array.
                    if trimmed == "];" || trimmed.ends_with("];") {
                        in_target_property = false;
                    }
                }
            }
            byte_offset += line_len;
        }
        None
    }

    fn reload_raw_class(
        candidate: &ClassInfo,
        all_classes: &[ClassInfo],
        class_loader: &dyn Fn(&str) -> Option<ClassInfo>,
    ) -> Option<ClassInfo> {
        let fqn = match &candidate.file_namespace {
            Some(ns) if !ns.is_empty() => format!("{}\\{}", ns, candidate.name),
            _ => candidate.name.clone(),
        };
        crate::completion::resolver::find_class_by_name(all_classes, &fqn)
            .cloned()
            .or_else(|| class_loader(&fqn))
    }

    fn find_builder_forwarded_method(
        class: &ClassInfo,
        member_name: &str,
        class_loader: &dyn Fn(&str) -> Option<ClassInfo>,
    ) -> Option<(ClassInfo, String)> {
        if !extends_eloquent_model(class, class_loader) {
            return None;
        }
        let builder = class_loader(ELOQUENT_BUILDER_FQN)?;
        let (declaring_class, fqn) =
            Self::find_declaring_class(&builder, member_name, class_loader)?;
        // When the declaring class is the Eloquent Builder itself,
        // find_declaring_class returns the short name ("Builder").
        // Replace it with the fully-qualified name so that
        // find_class_file_content can disambiguate classes that share
        // the same short name (e.g. Eloquent\Builder vs Demo\Builder).
        if !fqn.contains('\\') && fqn == builder.name {
            Some((declaring_class, ELOQUENT_BUILDER_FQN.to_string()))
        } else {
            Some((declaring_class, fqn))
        }
    }

    /// Find a scope method's declaration on the model when the target
    /// class is an Eloquent Builder instance.
    ///
    /// When a variable resolves to `Builder<User>`, completion injects
    /// the model's scope methods onto the Builder.  For go-to-definition,
    /// we need to trace back to the `scopeXxx` method on the model.
    ///
    /// `resolved_candidate` is the fully-resolved Builder (with scope
    /// methods injected by `type_hint_to_classes_depth`).  We use it to
    /// confirm the member exists and to extract the model name from the
    /// scope method's return type.
    ///
    /// Returns `Some((declaring_class, fqn, scope_method_name))` when
    /// the scope is found on the model, or `None` otherwise.
    fn find_scope_on_builder_model(
        resolved_candidate: &ClassInfo,
        raw_class: &ClassInfo,
        member_name: &str,
        class_loader: &dyn Fn(&str) -> Option<ClassInfo>,
    ) -> Option<(ClassInfo, String, String)> {
        // Only applies to the Eloquent Builder class.
        let raw_fqn = match &raw_class.file_namespace {
            Some(ns) if !ns.is_empty() => format!("{}\\{}", ns, raw_class.name),
            _ => raw_class.name.clone(),
        };
        let raw_clean = raw_fqn.strip_prefix('\\').unwrap_or(&raw_fqn);
        if raw_clean != ELOQUENT_BUILDER_FQN {
            return None;
        }

        // Check if the resolved (scope-injected) candidate has this
        // method.  If not, the member is not a scope.
        let scope_method = resolved_candidate
            .methods
            .iter()
            .find(|m| m.name == member_name && !m.is_static)?;

        // Extract the model name from the scope method's return type.
        // The return type is typically
        // `\Illuminate\Database\Eloquent\Builder<App\Models\User>`.
        let ret = scope_method.return_type.as_deref()?;
        let model_name = crate::docblock::types::parse_generic_args(ret)
            .1
            .into_iter()
            .next()
            .map(|s| s.strip_prefix('\\').unwrap_or(s).to_string())?;

        // Load the model and verify it extends Eloquent Model.
        let model = class_loader(&model_name)?;
        if !extends_eloquent_model(&model, class_loader) {
            return None;
        }

        // Look for `scopeXxx` on the model's inheritance chain.
        let scope_name = Self::scope_method_name(member_name);
        let (declaring, fqn) = Self::find_declaring_class(&model, &scope_name, class_loader)?;
        Some((declaring, fqn, scope_name))
    }

    fn resolve_trait_alias(class: &ClassInfo, member_name: &str) -> (String, Option<String>) {
        for alias in &class.trait_aliases {
            if alias.alias.as_deref() == Some(member_name) {
                return (alias.method_name.clone(), alias.trait_name.clone());
            }
        }
        (member_name.to_string(), None)
    }

    /// Walk up the inheritance chain to find the class that actually declares
    /// the given member and the FQN (or best-known name) used to load it.
    ///
    /// Returns `Some((ClassInfo, fqn))` of the declaring class, or `None` if
    /// the member cannot be found in any ancestor.  The `fqn` is the name
    /// that was passed to `class_loader` to obtain the `ClassInfo`, which is
    /// a fully-qualified name for parents and traits.  For the class itself
    /// (when the member is declared directly), the FQN is reconstructed
    /// from `file_namespace` + `name` when a namespace is available so
    /// that `find_class_file_content` can disambiguate classes that share
    /// the same short name (e.g. `Eloquent\Builder` vs `Query\Builder`).
    fn find_declaring_class(
        class: &ClassInfo,
        member_name: &str,
        class_loader: &dyn Fn(&str) -> Option<ClassInfo>,
    ) -> Option<(ClassInfo, String)> {
        // Check if this class directly declares the member.
        if Self::classify_member(class, member_name, MemberAccessHint::Unknown).is_some() {
            let fqn = match &class.file_namespace {
                Some(ns) if !ns.is_empty() => format!("{}\\{}", ns, class.name),
                _ => class.name.clone(),
            };
            return Some((class.clone(), fqn));
        }

        // Check traits used by this class.
        if let Some(found) =
            Self::find_declaring_in_traits(&class.used_traits, member_name, class_loader, 0)
        {
            return Some(found);
        }

        // Walk up the parent chain.
        let mut current = class.clone();
        for _ in 0..MAX_INHERITANCE_DEPTH {
            let parent_name = match current.parent_class.as_ref() {
                Some(name) => name.clone(),
                None => break,
            };
            let parent = match class_loader(&parent_name) {
                Some(p) => p,
                None => break,
            };
            if Self::classify_member(&parent, member_name, MemberAccessHint::Unknown).is_some() {
                return Some((parent, parent_name));
            }
            // Check traits used by the parent class.
            if let Some(found) =
                Self::find_declaring_in_traits(&parent.used_traits, member_name, class_loader, 0)
            {
                return Some(found);
            }
            current = parent;
        }

        // Check @mixin classes — these have the lowest precedence.
        if let Some(found) =
            Self::find_declaring_in_mixins(&class.mixins, member_name, class_loader, 0)
        {
            return Some(found);
        }

        // Also check @mixin classes declared on ancestor classes.
        // e.g. `User extends Model` where `Model` has `@mixin Builder`.
        let mut ancestor = class.clone();
        for _ in 0..MAX_INHERITANCE_DEPTH {
            let parent_name = match ancestor.parent_class.as_ref() {
                Some(name) => name.clone(),
                None => break,
            };
            let parent = match class_loader(&parent_name) {
                Some(p) => p,
                None => break,
            };
            if !parent.mixins.is_empty()
                && let Some(found) =
                    Self::find_declaring_in_mixins(&parent.mixins, member_name, class_loader, 0)
            {
                return Some(found);
            }
            ancestor = parent;
        }

        None
    }

    /// Search through a list of trait names for one that declares `member_name`.
    ///
    /// Traits can themselves `use` other traits, so this recurses up to a
    /// depth limit to handle trait composition.
    ///
    /// Returns `(ClassInfo, fqn)` where `fqn` is the fully-qualified name
    /// that was used to load the declaring class from `class_loader`.
    fn find_declaring_in_traits(
        trait_names: &[String],
        member_name: &str,
        class_loader: &dyn Fn(&str) -> Option<ClassInfo>,
        depth: usize,
    ) -> Option<(ClassInfo, String)> {
        if depth > MAX_TRAIT_DEPTH as usize {
            return None;
        }

        for trait_name in trait_names {
            let trait_info = if let Some(t) = class_loader(trait_name) {
                t
            } else {
                continue;
            };
            if Self::classify_member(&trait_info, member_name, MemberAccessHint::Unknown).is_some()
            {
                return Some((trait_info, trait_name.clone()));
            }
            // Recurse into traits used by this trait.
            if let Some(found) = Self::find_declaring_in_traits(
                &trait_info.used_traits,
                member_name,
                class_loader,
                depth + 1,
            ) {
                return Some(found);
            }
            // Walk the parent_class (extends) chain so that interface
            // inheritance is resolved.  For example, BackedEnum extends
            // UnitEnum — looking up `cases` on BackedEnum should find
            // the declaring UnitEnum interface.
            let mut current = trait_info;
            let mut parent_depth = depth;
            while let Some(ref parent_name) = current.parent_class {
                parent_depth += 1;
                if parent_depth > MAX_TRAIT_DEPTH as usize {
                    break;
                }
                let parent = if let Some(p) = class_loader(parent_name) {
                    p
                } else {
                    break;
                };
                if Self::classify_member(&parent, member_name, MemberAccessHint::Unknown).is_some()
                {
                    return Some((parent, parent_name.clone()));
                }
                if let Some(found) = Self::find_declaring_in_traits(
                    &parent.used_traits,
                    member_name,
                    class_loader,
                    parent_depth + 1,
                ) {
                    return Some(found);
                }
                current = parent;
            }
        }

        None
    }

    /// Search through `@mixin` class names for one that declares `member_name`.
    ///
    /// Mixin classes are resolved with their full inheritance chain (parent
    /// classes, traits) so that inherited members are found.  Only public
    /// members are considered since mixins proxy via magic methods.
    /// Mixin classes can themselves declare `@mixin`, so this recurses up
    /// to a depth limit.
    ///
    /// Returns `(ClassInfo, fqn)` where `fqn` is the fully-qualified name
    /// that was used to load the declaring class from `class_loader`.
    fn find_declaring_in_mixins(
        mixin_names: &[String],
        member_name: &str,
        class_loader: &dyn Fn(&str) -> Option<ClassInfo>,
        depth: usize,
    ) -> Option<(ClassInfo, String)> {
        if depth > MAX_MIXIN_DEPTH as usize {
            return None;
        }

        for mixin_name in mixin_names {
            let mixin_class = if let Some(c) = class_loader(mixin_name) {
                c
            } else {
                continue;
            };

            // Try to find the declaring class within the mixin's own
            // hierarchy (itself, its traits, its parents).
            if let Some((declaring_class, fqn)) =
                Self::find_declaring_class(&mixin_class, member_name, class_loader)
            {
                // When find_declaring_class finds the member directly on
                // the mixin class, it returns the short name (e.g.
                // "Builder") because ClassInfo.name is always short.
                // Replace it with the fully-qualified mixin_name so that
                // find_class_file_content can disambiguate classes that
                // share the same short name (e.g. Eloquent\Builder vs
                // Query\Builder).
                if !fqn.contains('\\') && fqn == mixin_class.name {
                    return Some((declaring_class, mixin_name.clone()));
                }
                return Some((declaring_class, fqn));
            }

            // Recurse into mixins declared by this mixin class.
            if !mixin_class.mixins.is_empty()
                && let Some(found) = Self::find_declaring_in_mixins(
                    &mixin_class.mixins,
                    member_name,
                    class_loader,
                    depth + 1,
                )
            {
                return Some(found);
            }
        }

        None
    }

    // ─── File & Position Lookup ─────────────────────────────────────────────

    /// Find the file URI and content for the file that contains a given class.
    ///
    /// `class_name` can be a short name (e.g. `"Kernel"`) or a
    /// fully-qualified name (e.g. `"Illuminate\\Foundation\\Console\\Kernel"`).
    /// When a namespace prefix is present the file's namespace (from
    /// `namespace_map`) must match for the class to be returned.  This
    /// prevents short-name collisions when a child class and its parent
    /// share the same simple name but live in different namespaces.
    ///
    /// Searches the `ast_map` (which includes files loaded via PSR-4 by
    /// `find_or_load_class`) and returns `(uri, content)`.
    pub(crate) fn find_class_file_content(
        &self,
        class_name: &str,
        current_uri: &str,
        current_content: &str,
    ) -> Option<(String, String)> {
        let normalized = class_name.strip_prefix('\\').unwrap_or(class_name);
        let last_segment = short_name(normalized);
        let expected_ns: Option<&str> = if normalized.contains('\\') {
            Some(&normalized[..normalized.len() - last_segment.len() - 1])
        } else {
            None
        };

        // Search the ast_map for the file containing this class.
        let uri = {
            let map = self.ast_map.lock().ok()?;
            let nmap = self.namespace_map.lock().ok();

            // Check whether a class with the right short name and
            // namespace lives in this file.  Uses the per-class
            // `file_namespace` field first (correct for multi-namespace
            // files like example.php), falling back to the file-level
            // `namespace_map` for single-namespace files.
            let class_in_file = |file_uri: &str, classes: &[ClassInfo]| -> bool {
                match expected_ns {
                    None => classes.iter().any(|c| c.name == last_segment),
                    Some(exp) => {
                        // Prefer per-class file_namespace (handles
                        // multi-namespace files correctly).
                        let found_via_class_ns = classes.iter().any(|c| {
                            c.name == last_segment && c.file_namespace.as_deref() == Some(exp)
                        });
                        if found_via_class_ns {
                            return true;
                        }
                        // Fall back to file-level namespace_map for
                        // classes that don't have file_namespace set
                        // (e.g. single-namespace files, stubs).
                        let file_ns = nmap
                            .as_ref()
                            .and_then(|nm| nm.get(file_uri))
                            .and_then(|opt| opt.as_deref());
                        file_ns == Some(exp) && classes.iter().any(|c| c.name == last_segment)
                    }
                }
            };

            // Check the current file first (common case: $this->method).
            if let Some(classes) = map.get(current_uri) {
                if class_in_file(current_uri, classes) {
                    Some(current_uri.to_string())
                } else {
                    // Search other files.
                    map.iter()
                        .find(|(u, classes)| class_in_file(u, classes))
                        .map(|(u, _)| u.clone())
                }
            } else {
                map.iter()
                    .find(|(u, classes)| class_in_file(u, classes))
                    .map(|(u, _)| u.clone())
            }
        }?;

        // Get the file content.
        let file_content = if uri == current_uri {
            current_content.to_string()
        } else if uri.starts_with("phpantom-stub://") {
            // Embedded stubs are stored under synthetic URIs and have no
            // on-disk file.  Retrieve the raw stub source from the
            // stub_index instead.
            self.stub_index.get(last_segment).map(|s| s.to_string())?
        } else {
            self.get_file_content(&uri)?
        };

        Some((uri, file_content))
    }

    /// Find the position of a member declaration (method, property, or constant)
    /// inside a PHP file.
    ///
    /// Searches line by line for the declaration pattern corresponding to the
    /// member kind, with word-boundary checks to avoid partial matches.
    pub(crate) fn find_member_position(
        content: &str,
        member_name: &str,
        kind: MemberKind,
    ) -> Option<Position> {
        Self::find_member_position_in_range(content, member_name, kind, None)
    }

    /// Find the position of a member declaration, optionally scoped to a
    /// byte range within the file.  When `class_range` is `Some((start, end))`,
    /// only lines whose starting byte offset falls within `[start, end)` are
    /// considered.  This prevents jumping to the wrong class when multiple
    /// classes in the same file declare a member with the same name (e.g.
    /// `Demo\Builder::where` vs `Illuminate\Database\Eloquent\Builder::where`).
    pub(crate) fn find_member_position_in_range(
        content: &str,
        member_name: &str,
        kind: MemberKind,
        class_range: Option<(usize, usize)>,
    ) -> Option<Position> {
        let is_word_boundary = |c: u8| {
            let ch = c as char;
            !ch.is_alphanumeric() && ch != '_'
        };

        let mut byte_offset: usize = 0;
        for (line_idx, line) in content.lines().enumerate() {
            let line_len = line.len() + 1; // +1 for the newline
            let in_range = match class_range {
                Some((start, end)) => byte_offset >= start && byte_offset < end,
                None => true,
            };
            if !in_range {
                byte_offset += line_len;
                continue;
            }
            match kind {
                MemberKind::Method => {
                    // Look for `function methodName` with word boundaries.
                    let pattern = format!("function {}", member_name);
                    if let Some(col) = line.find(&pattern) {
                        let after_pos = col + pattern.len();
                        let after_ok =
                            after_pos >= line.len() || is_word_boundary(line.as_bytes()[after_pos]);
                        if after_ok {
                            return Some(Position {
                                line: line_idx as u32,
                                character: col as u32,
                            });
                        }
                    }
                }
                MemberKind::Constant => {
                    // Look for the constant name on a line containing `const`.
                    // Handles both untyped (`const NAME`) and typed
                    // (`const string NAME`, PHP 8.3+) declarations.
                    if !line.contains("const ") {
                        // Fast reject — skip lines without `const` entirely
                        // before checking for the constant name.
                    } else if let Some(col) = line.find(member_name) {
                        let before_ok = col == 0 || is_word_boundary(line.as_bytes()[col - 1]);
                        let after_pos = col + member_name.len();
                        let after_ok =
                            after_pos >= line.len() || is_word_boundary(line.as_bytes()[after_pos]);
                        if before_ok && after_ok {
                            return Some(Position {
                                line: line_idx as u32,
                                character: col as u32,
                            });
                        }
                    }
                    // Also look for `case CASE_NAME` (enum cases are stored
                    // as constants but declared with `case` keyword).
                    let case_pattern = format!("case {}", member_name);
                    if let Some(col) = line.find(&case_pattern) {
                        let before_ok = col == 0 || is_word_boundary(line.as_bytes()[col - 1]);
                        let after_pos = col + case_pattern.len();
                        let after_ok =
                            after_pos >= line.len() || is_word_boundary(line.as_bytes()[after_pos]);
                        if before_ok && after_ok {
                            return Some(Position {
                                line: line_idx as u32,
                                character: col as u32,
                            });
                        }
                    }
                }
                MemberKind::Property => {
                    // Look for `$propertyName` on a line that looks like a
                    // property declaration (has a visibility keyword, `var`,
                    // or `readonly`).
                    let var_pattern = format!("${}", member_name);
                    if let Some(col) = line.find(&var_pattern) {
                        let after_pos = col + var_pattern.len();
                        let after_ok =
                            after_pos >= line.len() || is_word_boundary(line.as_bytes()[after_pos]);

                        if after_ok {
                            let trimmed = line.trim_start();
                            // A line starting with a visibility keyword is a
                            // property declaration UNLESS it also contains
                            // `function` before the `$` — in that case it is
                            // a method whose parameter happens to share the
                            // property name (e.g.
                            // `public static function from(int|string $value)`
                            // vs `public readonly int|string $value;`).
                            let prefix = &line[..col];
                            let is_method_param = prefix.contains("function");
                            let is_declaration = !is_method_param
                                && (trimmed.starts_with("public")
                                    || trimmed.starts_with("protected")
                                    || trimmed.starts_with("private")
                                    || trimmed.starts_with("var ")
                                    || trimmed.starts_with("readonly")
                                    || trimmed.starts_with("static"));

                            // Also detect promoted constructor properties:
                            // `public function __construct(private Type $prop)`
                            // In this case the visibility keyword appears
                            // inside the parameter list on the same line.
                            // Only applies to `__construct` — regular method
                            // parameters like `function from(int|string $value)`
                            // must not be mistaken for property declarations.
                            let is_promoted = !is_declaration
                                && !is_method_param
                                && prefix.contains("__construct")
                                && {
                                    // Check if visibility keyword appears before
                                    // the `$prop` on the same line (inside parens).
                                    prefix.contains("public")
                                        || prefix.contains("protected")
                                        || prefix.contains("private")
                                        || prefix.contains("readonly")
                                };

                            if is_declaration || is_promoted {
                                // Place the cursor on the first letter after
                                // `$` so that a second go-to-definition
                                // triggers type-hint resolution.
                                return Some(Position {
                                    line: line_idx as u32,
                                    character: (col + 1) as u32,
                                });
                            }
                        }
                    }
                }
            }
            byte_offset += line_len;
        }

        // Fallback: for properties, check if this is a magic property
        // declared via a `@property` tag in the class docblock.
        // Lines look like: ` * @property Type $propertyName`
        // NOTE: docblock tags precede the class body, so they fall
        // outside `[start_offset, end_offset)`.  Don't scope these
        // fallback searches by class_range.
        if kind == MemberKind::Property {
            let var_pattern = format!("${}", member_name);
            for (line_idx, line) in content.lines().enumerate() {
                if let Some(col) = line.find(&var_pattern) {
                    let after_pos = col + var_pattern.len();
                    let after_ok =
                        after_pos >= line.len() || is_word_boundary(line.as_bytes()[after_pos]);
                    if !after_ok {
                        continue;
                    }

                    let trimmed = line.trim().trim_start_matches('*').trim();
                    if trimmed.starts_with("@property-read")
                        || trimmed.starts_with("@property-write")
                        || trimmed.starts_with("@property")
                    {
                        return Some(Position {
                            line: line_idx as u32,
                            character: (col + 1) as u32,
                        });
                    }
                }
            }
        }

        // Fallback: for methods, check if this is a magic method
        // declared via a `@method` tag in the class docblock.
        // Lines look like: ` * @method ReturnType methodName(params...)`
        // NOTE: same as above — docblock tags are outside the class body
        // range, so don't scope by class_range.
        if kind == MemberKind::Method {
            // The method name is followed by `(` in a @method tag.
            let method_pattern = member_name;
            for (line_idx, line) in content.lines().enumerate() {
                // Search for ALL occurrences of the pattern within the line,
                // not just the first one.  This is important when the method
                // name collides with a type keyword (e.g. `string`) that also
                // appears as the return type on the same line.
                let mut search_start = 0;
                while let Some(offset) = line[search_start..].find(method_pattern) {
                    let col = search_start + offset;
                    search_start = col + method_pattern.len();

                    // Verify the character after the name is `(` (method call syntax).
                    let after_pos = col + method_pattern.len();
                    if after_pos >= line.len() {
                        continue;
                    }
                    let after_char = line.as_bytes()[after_pos];
                    if after_char != b'(' {
                        continue;
                    }

                    // Verify the character before is a word boundary (whitespace)
                    // to avoid matching partial names.
                    if col > 0 && !is_word_boundary(line.as_bytes()[col - 1]) {
                        continue;
                    }

                    let trimmed = line.trim().trim_start_matches('*').trim();
                    if trimmed.starts_with("@method") {
                        return Some(Position {
                            line: line_idx as u32,
                            character: col as u32,
                        });
                    }
                }
            }
        }

        None
    }
}
