//! PHPDoc virtual member provider.
//!
//! Extracts `@method`, `@property` / `@property-read` / `@property-write`,
//! and `@mixin` tags from the class-level docblock and presents them as
//! virtual members.  This is the second-highest-priority virtual member
//! provider: framework providers (e.g. Laravel) take precedence, but
//! PHPDoc-sourced members beat all other virtual member sources.
//!
//! Within this provider, `@method` and `@property` tags take precedence
//! over `@mixin` members: if a class declares both `@property int $id`
//! and `@mixin SomeClass` where `SomeClass` also has an `$id` property,
//! the `@property` tag wins.
//!
//! Previously `@method` / `@property` and `@mixin` were handled by two
//! separate providers (`PHPDocProvider` and `MixinProvider`).  Since both
//! are driven by PHPDoc tags, they are now unified into a single provider
//! with internal precedence rules.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::docblock;
use crate::inheritance;
use crate::inheritance::ClassRef;
use crate::php_type::PhpType;
use crate::types::{
    ClassInfo, ConstantInfo, MAX_INHERITANCE_DEPTH, MAX_MIXIN_DEPTH, MethodInfo, PropertyInfo,
    Visibility,
};
use crate::util::short_name;

thread_local! {
    /// Thread-local cache of base-resolved mixin classes.
    ///
    /// Keyed by fully-qualified mixin name, stores the result of
    /// [`resolve_class_with_inheritance`](crate::inheritance::resolve_class_with_inheritance)
    /// so that expensive inheritance walks (e.g. for
    /// `\Illuminate\Database\Eloquent\Builder`) are performed at most
    /// once per thread.
    ///
    /// Must be cleared between test runs via [`clear_mixin_cache`]
    /// because different tests may define classes with the same short
    /// name but different members.
    static MIXIN_CACHE: RefCell<HashMap<String, Arc<ClassInfo>>> =
        RefCell::new(HashMap::new());
}

/// Clear the thread-local mixin resolution cache.
///
/// In production the cache lives for the lifetime of the thread and is
/// safe because the same FQN always maps to the same class.  In tests,
/// however, each test may define classes with identical short names but
/// different members.  Call this function when creating a new test
/// backend so that stale entries from a previous test do not leak.
pub fn clear_mixin_cache() {
    MIXIN_CACHE.with(|cache| cache.borrow_mut().clear());
}

/// Tracks member names already seen during mixin collection.
///
/// Accumulates mixin members during collection, grouping the output
/// vectors and dedup sets into a single value to keep the argument
/// count of [`collect_mixin_members`] within clippy's limit.
struct MixinCollector {
    methods: Vec<MethodInfo>,
    properties: Vec<PropertyInfo>,
    constants: Vec<ConstantInfo>,
    dedup: MixinDedup,
}

/// Passed through [`collect_mixin_members`] (including recursive calls)
/// so that every addition is checked in O(1) instead of scanning the
/// accumulated vectors and base class members.
struct MixinDedup {
    /// Method names from the base class + accumulated virtual methods.
    methods: HashSet<String>,
    /// Property names from the base class + accumulated virtual properties.
    properties: HashSet<String>,
    /// Constant names from the base class + accumulated virtual constants.
    constants: HashSet<String>,
}

use super::{VirtualMemberProvider, VirtualMembers};

/// Virtual member provider for `@method`, `@property`, and `@mixin` docblock tags.
///
/// When a class declares `@method` or `@property` tags in its class-level
/// docblock, those tags describe magic members accessible via `__call`,
/// `__get`, and `__set`.  When a class declares `@mixin ClassName`, all
/// public members of `ClassName` (and its inheritance chain) become
/// available via magic methods.
///
/// Resolution order within this provider:
/// 1. `@method` and `@property` tags (highest precedence)
/// 2. `@mixin` class members (lower precedence, never overwrite tags)
///
/// Mixins are inherited: if `User extends Model` and `Model` has
/// `@mixin Builder`, then `User` also gains Builder's public members.
/// The provider walks the parent chain to collect mixin declarations
/// from ancestors.
///
/// Mixin classes can themselves declare `@mixin`, so the provider
/// recurses up to [`MAX_MIXIN_DEPTH`] levels.
pub struct PHPDocProvider;

impl VirtualMemberProvider for PHPDocProvider {
    /// Returns `true` if the class has a non-empty class-level docblock
    /// or declares `@mixin` tags (directly or via ancestors).
    ///
    /// This is a cheap pre-check. No parsing is performed.
    fn applies_to(
        &self,
        class: &ClassInfo,
        class_loader: &dyn Fn(&str) -> Option<Arc<ClassInfo>>,
    ) -> bool {
        // Has a non-empty docblock with potential @method/@property tags.
        if class.class_docblock.as_ref().is_some_and(|d| !d.is_empty()) {
            return true;
        }

        // Has used traits that might have @method/@property tags.
        for trait_name in &class.used_traits {
            if let Some(trait_info) = class_loader(trait_name)
                && trait_info
                    .class_docblock
                    .as_ref()
                    .is_some_and(|d| !d.is_empty())
            {
                return true;
            }
        }

        // Has direct @mixin declarations.
        if !class.mixins.is_empty() {
            return true;
        }

        // Walk the parent chain to check for ancestor mixins or docblocks
        // with @method/@property tags.  Use a cheap Arc handle instead of
        // cloning the entire ClassInfo at each level.
        let mut current_parent = class.parent_class.clone();
        let mut depth = 0u32;
        while let Some(ref parent_name) = current_parent {
            depth += 1;
            if depth > MAX_INHERITANCE_DEPTH {
                break;
            }
            let parent = if let Some(p) = class_loader(parent_name) {
                p
            } else {
                break;
            };
            if !parent.mixins.is_empty() {
                return true;
            }
            if parent
                .class_docblock
                .as_ref()
                .is_some_and(|d| !d.is_empty())
            {
                return true;
            }
            current_parent = parent.parent_class.clone();
        }

        false
    }

    /// Parse `@method`, `@property`, and `@mixin` tags from the class.
    ///
    /// Uses the existing [`docblock::extract_method_tags`] and
    /// [`docblock::extract_property_tags`] functions for tag parsing.
    /// Then collects public members from `@mixin` classes.  Within the
    /// provider, `@method` / `@property` tags take precedence over
    /// `@mixin` members.
    fn provide(
        &self,
        class: &ClassInfo,
        class_loader: &dyn Fn(&str) -> Option<Arc<ClassInfo>>,
        _cache: Option<&super::ResolvedClassCache>,
    ) -> VirtualMembers {
        let mut methods = Vec::new();
        let mut properties = Vec::new();
        let constants = Vec::new();

        // Dedup sets for O(1) membership checks.  Seeded from the
        // base-resolved class members (real + inherited) and updated
        // as virtual members are collected.
        //
        // `seen_props` is NOT seeded from existing class properties.
        // Phase 1 (`@property` tags) always emits its properties so
        // that `merge_virtual_members` can compare type specificity
        // and keep the most specific type (e.g. `array<string>` from
        // `@property` beats bare `array` from `$casts`).  After
        // phase 1 emits, names are added to `seen_props` to prevent
        // lower-priority sources (trait tags, parent tags, `@mixin`
        // members) from overriding them.
        let mut seen_methods: HashSet<String> =
            class.methods.iter().map(|m| m.name.clone()).collect();
        let mut seen_props: HashSet<String> = HashSet::new();
        let seen_consts: HashSet<String> = class.constants.iter().map(|c| c.name.clone()).collect();

        // ── Phase 1: @method and @property tags (higher precedence) ─────

        if let Some(doc_text) = class.class_docblock.as_deref()
            && !doc_text.is_empty()
        {
            for m in docblock::extract_method_tags(doc_text) {
                seen_methods.insert(m.name.clone());
                methods.push(m);
            }

            for (name, type_str) in docblock::extract_property_tags(doc_text) {
                seen_props.insert(name.clone());
                let type_hint: Option<String> = if type_str.is_empty() {
                    None
                } else {
                    Some(type_str)
                };
                properties.push(PropertyInfo {
                    name,
                    name_offset: 0,
                    type_hint: type_hint.clone(),
                    type_hint_parsed: type_hint.as_deref().map(PhpType::parse),
                    native_type_hint: None,
                    description: None,
                    is_static: false,
                    visibility: Visibility::Public,
                    deprecation_message: None,
                    deprecated_replacement: None,
                    see_refs: Vec::new(),
                    is_virtual: true,
                });
            }
        }

        // ── Phase 1b: @method and @property tags from used traits ───────
        //
        // When a class uses a trait that declares `@method` or `@property`
        // tags in its docblock, those virtual members should propagate to
        // the consuming class.  Real trait methods are already merged by
        // `merge_traits_into`, but virtual members from docblock tags are
        // not — they only exist as text in the trait's `class_docblock`.
        for trait_name in &class.used_traits {
            let trait_info = if let Some(t) = class_loader(trait_name) {
                t
            } else {
                continue;
            };

            if let Some(doc_text) = trait_info.class_docblock.as_deref()
                && !doc_text.is_empty()
            {
                for m in docblock::extract_method_tags(doc_text) {
                    if seen_methods.insert(m.name.clone()) {
                        methods.push(m);
                    }
                }

                for (name, type_str) in docblock::extract_property_tags(doc_text) {
                    if seen_props.insert(name.clone()) {
                        let type_hint: Option<String> = if type_str.is_empty() {
                            None
                        } else {
                            Some(type_str)
                        };
                        properties.push(PropertyInfo {
                            name,
                            name_offset: 0,
                            type_hint: type_hint.clone(),
                            type_hint_parsed: type_hint.as_deref().map(PhpType::parse),
                            native_type_hint: None,
                            description: None,
                            is_static: false,
                            visibility: Visibility::Public,
                            deprecation_message: None,
                            deprecated_replacement: None,
                            see_refs: Vec::new(),
                            is_virtual: true,
                        });
                    }
                }
            }
        }

        // ── Phase 1c: @method and @property tags from parent classes ────
        //
        // When a parent class declares `@method` or `@property` tags in
        // its docblock, those virtual members should be visible on child
        // classes.  Real inherited methods are already merged by
        // `resolve_class_with_inheritance`, but virtual members from
        // docblock tags are not — they only exist as text in the parent's
        // `class_docblock`.  Walk the parent chain and collect them.
        // Use a cheap handle instead of cloning ClassInfo at each level.
        {
            let mut current_parent = class.parent_class.clone();
            let mut depth = 0u32;
            while let Some(ref parent_name) = current_parent {
                depth += 1;
                if depth > MAX_INHERITANCE_DEPTH {
                    break;
                }
                let parent = if let Some(p) = class_loader(parent_name) {
                    p
                } else {
                    break;
                };

                if let Some(doc_text) = parent.class_docblock.as_deref()
                    && !doc_text.is_empty()
                {
                    for m in docblock::extract_method_tags(doc_text) {
                        if seen_methods.insert(m.name.clone()) {
                            methods.push(m);
                        }
                    }

                    for (name, type_str) in docblock::extract_property_tags(doc_text) {
                        if seen_props.insert(name.clone()) {
                            let type_hint: Option<String> = if type_str.is_empty() {
                                None
                            } else {
                                Some(type_str)
                            };
                            properties.push(PropertyInfo {
                                name,
                                name_offset: 0,
                                type_hint: type_hint.clone(),
                                type_hint_parsed: type_hint.as_deref().map(PhpType::parse),
                                native_type_hint: None,
                                description: None,
                                is_static: false,
                                visibility: Visibility::Public,
                                deprecation_message: None,
                                deprecated_replacement: None,
                                see_refs: Vec::new(),
                                is_virtual: true,
                            });
                        }
                    }
                }

                current_parent = parent.parent_class.clone();
            }
        }

        // ── Phase 2: @mixin members (lower precedence) ─────────────────

        let mixin_dedup = MixinDedup {
            methods: seen_methods,
            properties: seen_props,
            constants: seen_consts,
        };

        let mut collector = MixinCollector {
            methods,
            properties,
            constants,
            dedup: mixin_dedup,
        };

        // Collect from the class's own mixins.
        collect_mixin_members(
            &class.mixins,
            &class.mixin_generics,
            class_loader,
            &mut collector,
            0,
        );

        // Collect from ancestor mixins.
        //
        // As we walk the parent chain we accumulate a substitution map
        // (template-param → concrete-type) so that mixin generic
        // arguments that reference a parent's template params are
        // resolved to concrete types.  For example, when
        // `BelongsTo extends Relation<Product>` and `Relation` has
        // `@mixin Builder<TRelatedModel>`, the walk builds
        // `{TRelatedModel → Product}` from the child's `@extends`
        // generics and applies it to the mixin's generic args, turning
        // `Builder<TRelatedModel>` into `Builder<Product>`.
        let mut current_ancestor: ClassRef<'_> = ClassRef::Borrowed(class);
        let mut active_subs: HashMap<String, String> = HashMap::new();
        let mut depth = 0u32;
        while let Some(ref parent_name) = current_ancestor.parent_class.clone() {
            depth += 1;
            if depth > MAX_INHERITANCE_DEPTH {
                break;
            }
            let parent = if let Some(p) = class_loader(parent_name) {
                p
            } else {
                break;
            };

            // Build the substitution map for this parent level,
            // analogous to `build_substitution_map` in inheritance.rs.
            let level_subs = build_mixin_substitution_map(&current_ancestor, &parent, &active_subs);

            if !parent.mixins.is_empty() {
                // Apply the accumulated substitution map to the
                // parent's mixin generic arguments so that template
                // param names are replaced with concrete types.
                let resolved_mixin_generics: Vec<(String, Vec<String>)> = if level_subs.is_empty() {
                    parent.mixin_generics.clone()
                } else {
                    parent
                        .mixin_generics
                        .iter()
                        .map(|(name, args)| {
                            let resolved_args: Vec<String> = args
                                .iter()
                                .map(|arg| {
                                    let sub = inheritance::apply_substitution(arg, &level_subs);
                                    sub.into_owned()
                                })
                                .collect();
                            (name.clone(), resolved_args)
                        })
                        .collect()
                };

                collect_mixin_members(
                    &parent.mixins,
                    &resolved_mixin_generics,
                    class_loader,
                    &mut collector,
                    0,
                );
            }
            active_subs = level_subs;
            current_ancestor = ClassRef::Owned(parent);
        }

        VirtualMembers {
            methods: collector.methods,
            properties: collector.properties,
            constants: collector.constants,
        }
    }
}

/// Recursively collect public members from mixin classes.
///
/// For each mixin name, loads the class via `class_loader`, resolves its
/// full inheritance chain (via [`crate::inheritance::resolve_class_with_inheritance`]),
/// and adds its public members to the output vectors.  Only members whose
/// names are not already present in `class` (the target class with base
/// resolution already applied) or in the output vectors are added.
/// This means `@method` / `@property` tags collected before this function
/// is called take precedence over mixin members.
///
/// Recurses into mixins declared on the mixin classes themselves, up to
/// [`MAX_MIXIN_DEPTH`] levels.
///
/// Uses a thread-local cache so that `resolve_class_with_inheritance` is
/// called at most once per unique mixin FQN across all `provide` calls
/// within the same thread.  Without this cache, a mixin like
/// `\Illuminate\Database\Eloquent\Builder` was fully re-resolved for
/// every Eloquent model class (very expensive: deep inheritance chain
/// with dozens of traits).
fn collect_mixin_members(
    mixin_names: &[String],
    mixin_generics: &[(String, Vec<String>)],
    class_loader: &dyn Fn(&str) -> Option<Arc<ClassInfo>>,
    collector: &mut MixinCollector,
    depth: u32,
) {
    if depth > MAX_MIXIN_DEPTH {
        return;
    }

    for mixin_name in mixin_names {
        let mixin_class = if let Some(c) = class_loader(mixin_name) {
            c
        } else {
            continue;
        };

        // Find generic args for this mixin from the @mixin tag.
        let mixin_short = short_name(mixin_name);
        let generic_args: Option<&[String]> = mixin_generics
            .iter()
            .find(|(name, _)| name == mixin_name || short_name(name) == mixin_short)
            .map(|(_, args)| args.as_slice());

        // Resolve the mixin class with its own inheritance so we see
        // all of its inherited/trait members too.  Use base resolution
        // (not resolve_class_fully) to avoid circular provider calls.
        //
        // Results are cached in a thread-local map so that the same
        // mixin (e.g. Builder) is only resolved once per thread.
        let resolved_mixin = MIXIN_CACHE.with(|cache| {
            let mut map = cache.borrow_mut();
            Arc::clone(map.entry(mixin_name.clone()).or_insert_with(|| {
                Arc::new(crate::inheritance::resolve_class_with_inheritance(
                    &mixin_class,
                    class_loader,
                ))
            }))
        });

        // Build a substitution map from the mixin class's template params
        // to the concrete types provided in the @mixin tag's generic args.
        let subs: HashMap<String, String> = if let Some(args) = generic_args {
            let mut map = HashMap::new();
            for (i, param_name) in mixin_class.template_params.iter().enumerate() {
                if let Some(arg) = args.get(i) {
                    map.insert(param_name.clone(), arg.clone());
                }
            }
            map
        } else {
            HashMap::new()
        };

        // Only merge public members — mixins proxy via magic methods
        // which only expose public API.
        for method in &resolved_mixin.methods {
            if method.visibility != Visibility::Public {
                continue;
            }
            // Skip if the base-resolved class already has this method,
            // or if a previous @method tag or mixin already contributed it.
            if !collector.dedup.methods.insert(method.name.clone()) {
                continue;
            }
            let mut method = method.clone();
            if !subs.is_empty() {
                inheritance::apply_substitution_to_method(&mut method, &subs);
            }
            // `@return $this` / `self` / `static` in mixin methods are
            // left as-is.  When the method is later called on the
            // consuming class, `$this` resolves to the consumer (not the
            // mixin), which is the correct semantic: fluent chains
            // continue with the consumer's full API (own methods + all
            // mixin methods).  In the builder-as-static forwarding path,
            // the substitution map rewrites `$this` to
            // `\Illuminate\Database\Eloquent\Builder<Model>`, so the
            // return type must still be the raw keyword at this stage.
            collector.methods.push(method);
        }

        for property in &resolved_mixin.properties {
            if property.visibility != Visibility::Public {
                continue;
            }
            if !collector.dedup.properties.insert(property.name.clone()) {
                continue;
            }
            let mut property = property.clone();
            if !subs.is_empty() {
                inheritance::apply_substitution_to_property(&mut property, &subs);
            }
            collector.properties.push(property);
        }

        for constant in &resolved_mixin.constants {
            if constant.visibility != Visibility::Public {
                continue;
            }
            if !collector.dedup.constants.insert(constant.name.clone()) {
                continue;
            }
            collector.constants.push(constant.clone());
        }

        // Recurse into mixins declared by the mixin class itself.
        if !mixin_class.mixins.is_empty() {
            collect_mixin_members(
                &mixin_class.mixins,
                &mixin_class.mixin_generics,
                class_loader,
                collector,
                depth + 1,
            );
        }
    }
}

/// Build a substitution map for mixin generic resolution by zipping the
/// parent class's `@template` parameters with the type arguments provided
/// by the child's `@extends` / `@implements` generics.
///
/// This mirrors [`crate::inheritance::build_substitution_map`] but is
/// scoped to the virtual-member provider so it does not need to be public
/// on the inheritance module.
fn build_mixin_substitution_map(
    current: &ClassInfo,
    parent: &ClassInfo,
    active_subs: &HashMap<String, String>,
) -> HashMap<String, String> {
    if parent.template_params.is_empty() {
        return active_subs.clone();
    }

    let parent_short = short_name(&parent.name);

    // Find `@extends`/`@implements` generics matching this parent.
    let type_args = current
        .extends_generics
        .iter()
        .chain(current.implements_generics.iter())
        .find(|(name, _)| {
            let name_short = short_name(name);
            name_short == parent_short
        })
        .map(|(_, args)| args);

    let type_args = match type_args {
        Some(args) => args,
        None => return active_subs.clone(),
    };

    let mut map = HashMap::new();
    for (i, param_name) in parent.template_params.iter().enumerate() {
        if let Some(arg) = type_args.get(i) {
            let resolved = if active_subs.is_empty() {
                arg.clone()
            } else {
                inheritance::apply_substitution(arg, active_subs).into_owned()
            };
            map.insert(param_name.clone(), resolved);
        }
    }

    map
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
#[path = "phpdoc_tests.rs"]
mod tests;
