/// Go-to-implementation support (`textDocument/implementation`).
///
/// When the cursor is on an interface name, abstract class name, or a method
/// call where the owning type is an interface or abstract class, this module
/// finds all concrete implementations and returns their locations.
///
/// # Resolution strategy
///
/// 1. **Determine the target symbol** — consult the precomputed `SymbolMap`
///    for the word under the cursor.
/// 2. **Identify the target type** — resolve the symbol to a `ClassInfo` and
///    check whether it is an interface or abstract class.
/// 3. **Scan for implementors** — walk all classes known to the server
///    (`ast_map`, `class_index`, `classmap`, PSR-4 directories) and collect
///    those whose `interfaces` list or `parent_class` matches the target type.
/// 4. **Return locations** — for class-level requests, return the class
///    declaration position; for method-level requests, return the method
///    position in each implementing class.
use std::collections::HashSet;
use std::path::PathBuf;

use tower_lsp::lsp_types::*;

use super::member::MemberKind;
use super::point_location;
use crate::Backend;
use crate::completion::resolver::ResolutionCtx;
use crate::symbol_map::SymbolKind;
use crate::types::{ClassInfo, ClassLikeKind, FileContext, MAX_INHERITANCE_DEPTH};
use crate::util::{collect_php_files, find_class_at_offset, position_to_offset, short_name};

impl Backend {
    /// Entry point for `textDocument/implementation`.
    ///
    /// Returns a list of locations where the symbol under the cursor is
    /// concretely implemented.  Returns `None` if the cursor is not on a
    /// resolvable interface/abstract symbol.
    pub(crate) fn resolve_implementation(
        &self,
        uri: &str,
        content: &str,
        position: Position,
    ) -> Option<Vec<Location>> {
        // ── 1. Extract the word under the cursor ────────────────────────
        // Primary path: consult the precomputed symbol map.
        let offset = position_to_offset(content, position);
        let symbol = self.lookup_symbol_map(uri, offset).or_else(|| {
            if offset > 0 {
                self.lookup_symbol_map(uri, offset - 1)
            } else {
                None
            }
        });

        if let Some(ref sym) = symbol {
            match &sym.kind {
                // Member access — delegate directly to member implementation
                // resolution using the structured symbol information.
                SymbolKind::MemberAccess { member_name, .. } => {
                    let ctx = self.file_context(uri);
                    return self.resolve_member_implementations(
                        uri,
                        content,
                        position,
                        member_name.as_str(),
                        &ctx,
                    );
                }
                // Class reference or declaration — resolve as a class/interface name.
                SymbolKind::ClassReference { name, .. } | SymbolKind::ClassDeclaration { name } => {
                    let ctx = self.file_context(uri);
                    return self.resolve_class_implementation(uri, content, name, &ctx);
                }
                // self/static/parent — resolve the keyword to the current
                // class and check whether it is an interface/abstract.
                SymbolKind::SelfStaticParent { keyword } => {
                    let ctx = self.file_context(uri);
                    let class_loader = self.class_loader(&ctx);
                    let current_class = find_class_at_offset(&ctx.classes, offset);
                    let target = match keyword.as_str() {
                        "parent" => current_class
                            .and_then(|cc| cc.parent_class.as_ref())
                            .and_then(|p| class_loader(p)),
                        _ => current_class.cloned(),
                    };
                    if let Some(ref t) = target {
                        return self.resolve_class_implementation(uri, content, &t.name, &ctx);
                    }
                    return None;
                }
                // Other symbol kinds (variables, function calls, etc.)
                // are not meaningful for go-to-implementation.
                _ => return None,
            }
        }

        // No symbol map span covers the cursor — nothing to resolve.
        None
    }

    /// Resolve go-to-implementation for a class/interface name.
    ///
    /// Resolves `name` to a fully-qualified class, checks that it is an
    /// interface or abstract class, finds all concrete implementors, and
    /// returns their declaration locations.
    fn resolve_class_implementation(
        &self,
        uri: &str,
        content: &str,
        name: &str,
        ctx: &FileContext,
    ) -> Option<Vec<Location>> {
        let class_loader = self.class_loader(ctx);

        let fqn = Self::resolve_to_fqn(name, &ctx.use_map, &ctx.namespace);
        let target = class_loader(&fqn).or_else(|| class_loader(name))?;

        // Only interfaces and abstract classes are meaningful targets.
        if target.kind != ClassLikeKind::Interface && !target.is_abstract {
            return None;
        }

        let target_short = target.name.clone();
        let target_fqn = self
            .class_fqn_for_short(&target_short)
            .unwrap_or(target_short.clone());

        let implementors = self.find_implementors(&target_short, &target_fqn, &class_loader);

        if implementors.is_empty() {
            return None;
        }

        let mut locations = Vec::new();
        for imp in &implementors {
            if let Some(loc) = self.locate_class_declaration(imp, uri, content) {
                locations.push(loc);
            }
        }

        if locations.is_empty() {
            None
        } else {
            Some(locations)
        }
    }

    /// Resolve implementations of a method call on an interface/abstract class.
    fn resolve_member_implementations(
        &self,
        uri: &str,
        content: &str,
        position: Position,
        member_name: &str,
        ctx: &FileContext,
    ) -> Option<Vec<Location>> {
        // Extract the subject (left side of -> or ::).
        let (subject, access_kind) = self.lookup_member_access_context(uri, content, position)?;

        let cursor_offset = position_to_offset(content, position);
        let current_class = find_class_at_offset(&ctx.classes, cursor_offset);

        let class_loader = self.class_loader(ctx);
        let function_loader = self.function_loader(ctx);

        // Resolve the subject to candidate classes.
        let rctx = ResolutionCtx {
            current_class,
            all_classes: &ctx.classes,
            content,
            cursor_offset,
            class_loader: &class_loader,
            resolved_class_cache: Some(&self.resolved_class_cache),
            function_loader: Some(&function_loader),
        };
        let candidates =
            crate::completion::resolver::resolve_target_classes(&subject, access_kind, &rctx);

        if candidates.is_empty() {
            return None;
        }

        // Check if ANY candidate is an interface or abstract class with this
        // method.  If so, find all implementors that have the method.
        let mut all_locations = Vec::new();

        for candidate in &candidates {
            if candidate.kind != ClassLikeKind::Interface && !candidate.is_abstract {
                continue;
            }

            // Verify the method exists on this interface/abstract class
            // (directly or inherited).
            let merged = crate::virtual_members::resolve_class_fully_cached(
                candidate,
                &class_loader,
                &self.resolved_class_cache,
            );
            let has_method = merged.methods.iter().any(|m| m.name == member_name);
            let has_property = merged.properties.iter().any(|p| p.name == member_name);

            if !has_method && !has_property {
                continue;
            }

            let member_kind = if has_method {
                MemberKind::Method
            } else {
                MemberKind::Property
            };

            let target_short = candidate.name.clone();
            let target_fqn = self
                .class_fqn_for_short(&target_short)
                .unwrap_or(target_short.clone());

            let implementors = self.find_implementors(&target_short, &target_fqn, &class_loader);

            for imp in &implementors {
                // Check that the implementor actually has this member.
                let imp_merged = crate::virtual_members::resolve_class_fully_cached(
                    imp,
                    &class_loader,
                    &self.resolved_class_cache,
                );
                let imp_has = match member_kind {
                    MemberKind::Method => imp_merged.methods.iter().any(|m| m.name == member_name),
                    MemberKind::Property => {
                        imp_merged.properties.iter().any(|p| p.name == member_name)
                    }
                    MemberKind::Constant => {
                        imp_merged.constants.iter().any(|c| c.name == member_name)
                    }
                };

                if !imp_has {
                    continue;
                }

                // Find the member position in the implementor's file.
                // We want the member defined directly on this class (not
                // inherited), so check the un-merged class first.
                let owns_member = match member_kind {
                    MemberKind::Method => imp.methods.iter().any(|m| m.name == member_name),
                    MemberKind::Property => imp.properties.iter().any(|p| p.name == member_name),
                    MemberKind::Constant => imp.constants.iter().any(|c| c.name == member_name),
                };

                if !owns_member {
                    // The member is inherited — the implementor doesn't
                    // override it, so there's no definition to jump to
                    // in this class.
                    continue;
                }

                if let Some((class_uri, class_content)) =
                    self.find_class_file_content(&imp.name, uri, content)
                    && let Some(member_pos) = Self::find_member_position_in_class(
                        &class_content,
                        member_name,
                        member_kind,
                        imp,
                    )
                    && let Ok(parsed_uri) = Url::parse(&class_uri)
                {
                    let loc = point_location(parsed_uri, member_pos);
                    if !all_locations.contains(&loc) {
                        all_locations.push(loc);
                    }
                }
            }
        }

        // If no interface/abstract candidate was found, try treating the
        // request as a regular "find all overrides" — useful for concrete
        // base-class methods too.
        if all_locations.is_empty() {
            return None;
        }

        Some(all_locations)
    }

    /// Find all classes that implement a given interface or extend a given
    /// abstract class.
    ///
    /// Scans:
    /// 1. All classes already in `ast_map` (open files + autoload-discovered)
    /// 2. All classes loadable via `class_index`
    /// 3. Classmap files not yet loaded — string pre-filter then parse
    /// 4. Embedded PHP stubs — string pre-filter then lazy parse
    /// 5. User PSR-4 directories — walk for `.php` files not covered by
    ///    the classmap, string pre-filter then parse.  Vendor PSR-4 roots
    ///    are skipped because vendor classes are assumed complete in the
    ///    classmap (Phase 3).
    ///
    /// Returns the list of concrete `ClassInfo` values (non-interface,
    /// non-abstract).
    fn find_implementors(
        &self,
        target_short: &str,
        target_fqn: &str,
        class_loader: &dyn Fn(&str) -> Option<ClassInfo>,
    ) -> Vec<ClassInfo> {
        let mut result: Vec<ClassInfo> = Vec::new();
        let mut seen_names = HashSet::new();

        // ── Phase 1: scan ast_map ───────────────────────────────────────
        // Collect all candidate classes first, then drop the lock before
        // calling class_loader (which may re-lock ast_map).
        let ast_candidates: Vec<ClassInfo> = if let Ok(map) = self.ast_map.lock() {
            map.values()
                .flat_map(|classes| classes.iter().cloned())
                .collect()
        } else {
            Vec::new()
        };

        for cls in &ast_candidates {
            if self.class_implements_or_extends(cls, target_short, target_fqn, class_loader)
                && seen_names.insert(cls.name.clone())
            {
                result.push(cls.clone());
            }
        }

        // ── Phase 2: scan class_index for classes not yet in ast_map ────
        let index_entries: Vec<(String, String)> = self
            .class_index
            .lock()
            .ok()
            .map(|idx| {
                idx.iter()
                    .map(|(fqn, uri)| (fqn.clone(), uri.clone()))
                    .collect()
            })
            .unwrap_or_default();

        for (fqn, _uri) in &index_entries {
            let short = short_name(fqn);
            if seen_names.contains(short) {
                continue;
            }
            if let Some(cls) = class_loader(fqn)
                && self.class_implements_or_extends(&cls, target_short, target_fqn, class_loader)
                && seen_names.insert(cls.name.clone())
            {
                result.push(cls);
            }
        }

        // ── Phase 3: scan classmap files with string pre-filter ─────────
        // Collect unique file paths from the classmap (one file may define
        // multiple classes, so we de-duplicate by path and scan each file
        // at most once).  Files already present in ast_map were covered by
        // Phase 1 and can be skipped.
        let classmap_paths: HashSet<PathBuf> = self
            .classmap
            .lock()
            .ok()
            .map(|cm| cm.values().cloned().collect())
            .unwrap_or_default();

        let loaded_uris: HashSet<String> = self
            .ast_map
            .lock()
            .ok()
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default();

        for path in &classmap_paths {
            let uri = format!("file://{}", path.display());
            if loaded_uris.contains(&uri) {
                continue;
            }

            // Cheap pre-filter: read the raw file and skip it if the
            // source doesn't mention the target name at all.
            let raw = match std::fs::read_to_string(path) {
                Ok(r) => r,
                Err(_) => continue,
            };
            if !raw.contains(target_short) {
                continue;
            }

            // Parse the file, cache it, and check every class it defines.
            if let Some(classes) = self.parse_and_cache_file(path) {
                for cls in &classes {
                    if seen_names.contains(&cls.name) {
                        continue;
                    }
                    if self.class_implements_or_extends(cls, target_short, target_fqn, class_loader)
                    {
                        seen_names.insert(cls.name.clone());
                        result.push(cls.clone());
                    }
                }
            }
        }

        // ── Phase 4: scan embedded stubs with string pre-filter ─────────
        // Stubs are static strings baked into the binary.  A cheap text
        // search for the target name narrows candidates before we parse.
        // Parsing is lazy and cached in ast_map, so subsequent lookups
        // hit Phase 1.
        for (&stub_name, &stub_source) in &self.stub_index {
            if seen_names.contains(stub_name) {
                continue;
            }
            // Cheap pre-filter: skip stubs whose source doesn't mention
            // the target name at all.
            if !stub_source.contains(target_short) {
                continue;
            }
            if let Some(cls) = class_loader(stub_name)
                && self.class_implements_or_extends(&cls, target_short, target_fqn, class_loader)
                && seen_names.insert(cls.name.clone())
            {
                result.push(cls);
            }
        }

        // ── Phase 5: scan user PSR-4 directories for files not in classmap
        // The user may have created classes that are not yet in the
        // classmap.  Walk user PSR-4 roots only — vendor classes are
        // assumed complete in the classmap (Phase 3) and should not
        // require a filesystem walk.
        if let Some(workspace_root) = self
            .workspace_root
            .lock()
            .ok()
            .and_then(|guard| guard.clone())
        {
            // The vendor dir name is needed by collect_php_files even
            // though we only walk user PSR-4 roots.  A fallback mapping
            // like `"" => "."` resolves to the workspace root, so the
            // walk must still skip the vendor directory (and hidden
            // directories like .git).
            let vendor_dir_name = self
                .vendor_dir_name
                .lock()
                .ok()
                .map(|v| v.clone())
                .unwrap_or_else(|| "vendor".to_string());

            let psr4_dirs: Vec<PathBuf> = self
                .psr4_mappings
                .lock()
                .ok()
                .map(|mappings| {
                    mappings
                        .iter()
                        .map(|m| workspace_root.join(&m.base_path))
                        .filter(|p| p.is_dir())
                        .collect()
                })
                .unwrap_or_default();

            // Refresh loaded URIs — Phase 3 may have added entries.
            let loaded_uris_p5: HashSet<String> = self
                .ast_map
                .lock()
                .ok()
                .map(|m| m.keys().cloned().collect())
                .unwrap_or_default();

            for dir in &psr4_dirs {
                for php_file in collect_php_files(dir, &vendor_dir_name) {
                    // Skip files already covered by the classmap (Phase 3).
                    if classmap_paths.contains(&php_file) {
                        continue;
                    }

                    let uri = format!("file://{}", php_file.display());
                    if loaded_uris_p5.contains(&uri) {
                        continue;
                    }

                    let raw = match std::fs::read_to_string(&php_file) {
                        Ok(r) => r,
                        Err(_) => continue,
                    };
                    if !raw.contains(target_short) {
                        continue;
                    }

                    if let Some(classes) = self.parse_and_cache_file(&php_file) {
                        for cls in &classes {
                            if seen_names.contains(&cls.name) {
                                continue;
                            }
                            if self.class_implements_or_extends(
                                cls,
                                target_short,
                                target_fqn,
                                class_loader,
                            ) {
                                seen_names.insert(cls.name.clone());
                                result.push(cls.clone());
                            }
                        }
                    }
                }
            }
        }

        result
    }

    /// Check whether `cls` implements the target interface or extends the
    /// target abstract class (directly or transitively through its parent
    /// chain).
    fn class_implements_or_extends(
        &self,
        cls: &ClassInfo,
        target_short: &str,
        target_fqn: &str,
        class_loader: &dyn Fn(&str) -> Option<ClassInfo>,
    ) -> bool {
        // Skip the target class itself.
        if cls.name == target_short {
            return false;
        }

        // Skip interfaces and abstract classes — we want concrete implementations.
        if cls.kind == ClassLikeKind::Interface || cls.is_abstract {
            return false;
        }

        // Direct `implements` match.
        for iface in &cls.interfaces {
            let iface_short = short_name(iface);
            if iface_short == target_short || iface == target_fqn {
                return true;
            }
        }

        // Direct `extends` match (for abstract class implementations).
        if let Some(ref parent) = cls.parent_class {
            let parent_short = short_name(parent);
            if parent_short == target_short || parent == target_fqn {
                return true;
            }
        }

        // ── Transitive check: walk the interface-extends chains ─────────
        // If ClassC implements InterfaceB, and InterfaceB extends
        // InterfaceA, a go-to-implementation on InterfaceA should find
        // ClassC.  Load each directly-implemented interface and
        // recursively check whether it extends the target.
        for iface in &cls.interfaces {
            if Self::interface_extends_target(iface, target_short, target_fqn, class_loader, 0) {
                return true;
            }
        }

        // ── Transitive check: walk the parent class chain ───────────────
        // A class might extend another class that implements the target
        // interface.  Walk up to a bounded depth to find it.
        let mut current = cls.parent_class.clone();
        let mut depth = 0u32;

        while let Some(ref parent_name) = current {
            if depth >= MAX_INHERITANCE_DEPTH {
                break;
            }
            depth += 1;

            if let Some(parent_cls) = class_loader(parent_name) {
                // Check if the parent implements the target interface.
                for iface in &parent_cls.interfaces {
                    let iface_short = short_name(iface);
                    if iface_short == target_short || iface == target_fqn {
                        return true;
                    }
                    // Also walk the interface's own extends chain.
                    if Self::interface_extends_target(
                        iface,
                        target_short,
                        target_fqn,
                        class_loader,
                        0,
                    ) {
                        return true;
                    }
                }

                // Check if the parent IS the target (for abstract class chains).
                let pshort = parent_cls.name.as_str();
                if pshort == target_short {
                    return true;
                }

                current = parent_cls.parent_class.clone();
            } else {
                break;
            }
        }

        false
    }

    /// Check whether `iface_name` transitively extends the target interface.
    ///
    /// Loads the interface via `class_loader`, then checks its
    /// `parent_class` (single-extends) and `interfaces` (multi-extends)
    /// lists recursively up to [`MAX_INHERITANCE_DEPTH`].
    fn interface_extends_target(
        iface_name: &str,
        target_short: &str,
        target_fqn: &str,
        class_loader: &dyn Fn(&str) -> Option<ClassInfo>,
        depth: u32,
    ) -> bool {
        if depth >= MAX_INHERITANCE_DEPTH {
            return false;
        }

        let Some(iface_cls) = class_loader(iface_name) else {
            return false;
        };

        // Check `parent_class` (first extended interface stored here for
        // backward compatibility).
        if let Some(ref parent) = iface_cls.parent_class {
            let parent_short = short_name(parent);
            if parent_short == target_short || parent == target_fqn {
                return true;
            }
            if Self::interface_extends_target(
                parent,
                target_short,
                target_fqn,
                class_loader,
                depth + 1,
            ) {
                return true;
            }
        }

        // Check all entries in `interfaces` (covers multi-extends for
        // interfaces that extend more than one parent).
        for parent_iface in &iface_cls.interfaces {
            let parent_short = short_name(parent_iface);
            if parent_short == target_short || parent_iface == target_fqn {
                return true;
            }
            if Self::interface_extends_target(
                parent_iface,
                target_short,
                target_fqn,
                class_loader,
                depth + 1,
            ) {
                return true;
            }
        }

        false
    }

    /// Find a member position scoped to a specific class body.
    ///
    /// When multiple classes in the same file define a method with the same
    /// name, [`find_member_position`](Self::find_member_position) would
    /// always return the first match.  This variant restricts the search
    /// to lines that fall within the class's `start_offset..end_offset`
    /// byte range so that each implementing class resolves to its own
    /// definition.
    fn find_member_position_in_class(
        content: &str,
        member_name: &str,
        kind: MemberKind,
        cls: &ClassInfo,
    ) -> Option<Position> {
        // Fast path: use stored AST offset when available.
        let name_offset = cls.member_name_offset(member_name, kind.as_str());
        if name_offset.is_some() {
            return Self::find_member_position(content, member_name, kind, name_offset);
        }

        // Convert byte offsets to line numbers.
        let start_line = content
            .get(..cls.start_offset as usize)
            .map(|s| s.matches('\n').count())
            .unwrap_or(0);
        let end_line = content
            .get(..cls.end_offset as usize)
            .map(|s| s.matches('\n').count())
            .unwrap_or(usize::MAX);

        // Build a sub-content containing only the class body lines and
        // delegate to the existing searcher, adjusting the result line.
        let class_lines: Vec<&str> = content
            .lines()
            .skip(start_line)
            .take(end_line - start_line + 1)
            .collect();
        let class_body = class_lines.join("\n");

        Self::find_member_position(&class_body, member_name, kind, None).map(|pos| Position {
            line: pos.line + start_line as u32,
            character: pos.character,
        })
    }

    /// Get the FQN for a class given its short name, by looking it up in
    /// the `class_index`.
    fn class_fqn_for_short(&self, target_short: &str) -> Option<String> {
        let idx = self.class_index.lock().ok()?;
        // Look for an entry whose short name matches.
        for fqn in idx.keys() {
            let short = short_name(fqn);
            if short == target_short {
                return Some(fqn.clone());
            }
        }
        None
    }

    /// Find the location of a class declaration for an implementor.
    fn locate_class_declaration(
        &self,
        cls: &ClassInfo,
        current_uri: &str,
        current_content: &str,
    ) -> Option<Location> {
        let (class_uri, class_content) =
            self.find_class_file_content(&cls.name, current_uri, current_content)?;

        if cls.keyword_offset == 0 {
            return None;
        }
        let position = crate::util::offset_to_position(&class_content, cls.keyword_offset as usize);
        let parsed_uri = Url::parse(&class_uri).ok()?;

        Some(point_location(parsed_uri, position))
    }
}
