//! Mixin virtual member provider.
//!
//! Extracts public members from classes listed in `@mixin` docblock tags
//! and presents them as virtual members on the annotated class.  This is
//! the lowest-priority virtual member provider: real declared members,
//! trait members, parent chain members, framework providers, and PHPDoc
//! providers all take precedence.
//!
//! The logic was originally part of `inheritance.rs`
//! (`merge_mixins_into` / `merge_mixins_into_recursive`).  Moving it
//! behind the [`VirtualMemberProvider`] trait gives it an explicit
//! priority slot and keeps `resolve_class_with_inheritance` focused on
//! base resolution (own + traits + parent chain).

use crate::Backend;
use crate::types::{
    ClassInfo, ConstantInfo, MAX_INHERITANCE_DEPTH, MAX_MIXIN_DEPTH, MethodInfo, PropertyInfo,
    Visibility,
};

use super::{VirtualMemberProvider, VirtualMembers};

/// Virtual member provider for `@mixin` docblock tags.
///
/// When a class declares `@mixin SomeClass`, all public members of
/// `SomeClass` (and its inheritance chain) become available on the
/// annotated class via magic methods.  This provider synthesizes those
/// virtual members.
///
/// Mixins are inherited: if `User extends Model` and `Model` has
/// `@mixin Builder`, then `User` also gains Builder's public members.
/// The provider walks the parent chain to collect mixin declarations
/// from ancestors.
///
/// Mixin classes can themselves declare `@mixin`, so the provider
/// recurses up to [`MAX_MIXIN_DEPTH`] levels.
pub struct MixinProvider;

impl VirtualMemberProvider for MixinProvider {
    /// Returns `true` if the class or any of its ancestors declares
    /// at least one `@mixin` tag.
    fn applies_to(
        &self,
        class: &ClassInfo,
        class_loader: &dyn Fn(&str) -> Option<ClassInfo>,
    ) -> bool {
        if !class.mixins.is_empty() {
            return true;
        }

        // Walk the parent chain to check for ancestor mixins.
        let mut current = class.clone();
        let mut depth = 0u32;
        while let Some(ref parent_name) = current.parent_class {
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
            current = parent;
        }

        false
    }

    /// Produce virtual members from all `@mixin` classes on this class
    /// and its ancestors.
    fn provide(
        &self,
        class: &ClassInfo,
        class_loader: &dyn Fn(&str) -> Option<ClassInfo>,
    ) -> VirtualMembers {
        let mut methods = Vec::new();
        let mut properties = Vec::new();
        let mut constants = Vec::new();

        // Collect from the class's own mixins.
        collect_mixin_members(
            class,
            &class.mixins,
            class_loader,
            &mut methods,
            &mut properties,
            &mut constants,
            0,
        );

        // Collect from ancestor mixins.
        let mut current = class.clone();
        let mut depth = 0u32;
        while let Some(ref parent_name) = current.parent_class {
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
                collect_mixin_members(
                    class,
                    &parent.mixins,
                    class_loader,
                    &mut methods,
                    &mut properties,
                    &mut constants,
                    0,
                );
            }
            current = parent;
        }

        VirtualMembers {
            methods,
            properties,
            constants,
        }
    }
}

/// Recursively collect public members from mixin classes.
///
/// For each mixin name, loads the class via `class_loader`, resolves its
/// full inheritance chain (via [`Backend::resolve_class_with_inheritance`]),
/// and adds its public members to the output vectors.  Only members whose
/// names are not already present in `class` (the target class with base
/// resolution already applied) or in the output vectors are added.
///
/// Recurses into mixins declared on the mixin classes themselves, up to
/// [`MAX_MIXIN_DEPTH`] levels.
fn collect_mixin_members(
    class: &ClassInfo,
    mixin_names: &[String],
    class_loader: &dyn Fn(&str) -> Option<ClassInfo>,
    methods: &mut Vec<MethodInfo>,
    properties: &mut Vec<PropertyInfo>,
    constants: &mut Vec<ConstantInfo>,
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

        // Resolve the mixin class with its own inheritance so we see
        // all of its inherited/trait members too.  Use base resolution
        // (not resolve_class_fully) to avoid circular provider calls.
        let resolved_mixin = Backend::resolve_class_with_inheritance(&mixin_class, class_loader);

        // Only merge public members — mixins proxy via magic methods
        // which only expose public API.
        for method in &resolved_mixin.methods {
            if method.visibility != Visibility::Public {
                continue;
            }
            // Skip if the base-resolved class already has this method,
            // or if a previous mixin already contributed it.
            if class.methods.iter().any(|m| m.name == method.name) {
                continue;
            }
            if methods.iter().any(|m| m.name == method.name) {
                continue;
            }
            let mut method = method.clone();
            // `@return $this` in the mixin class refers to the mixin
            // instance, NOT the consuming class.  Rewrite the return
            // type to the concrete mixin class name so that resolution
            // produces the mixin class rather than the consumer.
            if matches!(
                method.return_type.as_deref(),
                Some("$this" | "self" | "static")
            ) {
                method.return_type = Some(mixin_class.name.clone());
            }
            methods.push(method);
        }

        for property in &resolved_mixin.properties {
            if property.visibility != Visibility::Public {
                continue;
            }
            if class.properties.iter().any(|p| p.name == property.name) {
                continue;
            }
            if properties.iter().any(|p| p.name == property.name) {
                continue;
            }
            properties.push(property.clone());
        }

        for constant in &resolved_mixin.constants {
            if constant.visibility != Visibility::Public {
                continue;
            }
            if class.constants.iter().any(|c| c.name == constant.name) {
                continue;
            }
            if constants.iter().any(|c| c.name == constant.name) {
                continue;
            }
            constants.push(constant.clone());
        }

        // Recurse into mixins declared by the mixin class itself.
        if !mixin_class.mixins.is_empty() {
            collect_mixin_members(
                class,
                &mixin_class.mixins,
                class_loader,
                methods,
                properties,
                constants,
                depth + 1,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ClassLikeKind;
    use std::collections::HashMap;

    /// Helper: create a minimal `ClassInfo` with the given name.
    fn make_class(name: &str) -> ClassInfo {
        ClassInfo {
            kind: ClassLikeKind::Class,
            name: name.to_string(),
            methods: Vec::new(),
            properties: Vec::new(),
            constants: Vec::new(),
            start_offset: 0,
            end_offset: 0,
            parent_class: None,
            interfaces: Vec::new(),
            used_traits: Vec::new(),
            mixins: Vec::new(),
            is_final: false,
            is_abstract: false,
            is_deprecated: false,
            template_params: Vec::new(),
            template_param_bounds: HashMap::new(),
            extends_generics: Vec::new(),
            implements_generics: Vec::new(),
            use_generics: Vec::new(),
            type_aliases: HashMap::new(),
            trait_precedences: Vec::new(),
            trait_aliases: Vec::new(),
            class_docblock: None,
        }
    }

    fn make_method(name: &str, return_type: Option<&str>) -> MethodInfo {
        MethodInfo {
            name: name.to_string(),
            parameters: Vec::new(),
            return_type: return_type.map(|s| s.to_string()),
            is_static: false,
            visibility: Visibility::Public,
            conditional_return: None,
            is_deprecated: false,
            template_params: Vec::new(),
            template_bindings: Vec::new(),
        }
    }

    fn make_property(name: &str, type_hint: Option<&str>) -> PropertyInfo {
        PropertyInfo {
            name: name.to_string(),
            type_hint: type_hint.map(|s| s.to_string()),
            is_static: false,
            visibility: Visibility::Public,
            is_deprecated: false,
        }
    }

    fn make_constant(name: &str) -> ConstantInfo {
        ConstantInfo {
            name: name.to_string(),
            type_hint: None,
            visibility: Visibility::Public,
            is_deprecated: false,
        }
    }

    // ── applies_to tests ────────────────────────────────────────────────

    #[test]
    fn applies_when_class_has_mixins() {
        let provider = MixinProvider;
        let mut class = make_class("Foo");
        class.mixins = vec!["Bar".to_string()];

        let class_loader = |_: &str| -> Option<ClassInfo> { None };
        assert!(provider.applies_to(&class, &class_loader));
    }

    #[test]
    fn does_not_apply_when_no_mixins() {
        let provider = MixinProvider;
        let class = make_class("Foo");

        let class_loader = |_: &str| -> Option<ClassInfo> { None };
        assert!(!provider.applies_to(&class, &class_loader));
    }

    #[test]
    fn applies_when_ancestor_has_mixins() {
        let provider = MixinProvider;
        let mut class = make_class("Child");
        class.parent_class = Some("Parent".to_string());

        let mut parent = make_class("Parent");
        parent.mixins = vec!["Mixin".to_string()];

        let class_loader = move |name: &str| -> Option<ClassInfo> {
            if name == "Parent" {
                Some(parent.clone())
            } else {
                None
            }
        };
        assert!(provider.applies_to(&class, &class_loader));
    }

    // ── provide tests ───────────────────────────────────────────────────

    #[test]
    fn provides_public_methods_from_mixin() {
        let provider = MixinProvider;
        let mut class = make_class("Foo");
        class.mixins = vec!["Bar".to_string()];

        let mut bar = make_class("Bar");
        bar.methods.push(make_method("doStuff", Some("string")));
        let mut private_method = make_method("secret", Some("void"));
        private_method.visibility = Visibility::Private;
        bar.methods.push(private_method);

        let class_loader = move |name: &str| -> Option<ClassInfo> {
            if name == "Bar" {
                Some(bar.clone())
            } else {
                None
            }
        };

        let result = provider.provide(&class, &class_loader);
        assert_eq!(result.methods.len(), 1);
        assert_eq!(result.methods[0].name, "doStuff");
    }

    #[test]
    fn provides_public_properties_from_mixin() {
        let provider = MixinProvider;
        let mut class = make_class("Foo");
        class.mixins = vec!["Bar".to_string()];

        let mut bar = make_class("Bar");
        bar.properties.push(make_property("name", Some("string")));
        let mut protected_prop = make_property("internal", Some("int"));
        protected_prop.visibility = Visibility::Protected;
        bar.properties.push(protected_prop);

        let class_loader = move |name: &str| -> Option<ClassInfo> {
            if name == "Bar" {
                Some(bar.clone())
            } else {
                None
            }
        };

        let result = provider.provide(&class, &class_loader);
        assert_eq!(result.properties.len(), 1);
        assert_eq!(result.properties[0].name, "name");
    }

    #[test]
    fn provides_public_constants_from_mixin() {
        let provider = MixinProvider;
        let mut class = make_class("Foo");
        class.mixins = vec!["Bar".to_string()];

        let mut bar = make_class("Bar");
        bar.constants.push(make_constant("MAX_SIZE"));
        let mut private_const = make_constant("INTERNAL");
        private_const.visibility = Visibility::Private;
        bar.constants.push(private_const);

        let class_loader = move |name: &str| -> Option<ClassInfo> {
            if name == "Bar" {
                Some(bar.clone())
            } else {
                None
            }
        };

        let result = provider.provide(&class, &class_loader);
        assert_eq!(result.constants.len(), 1);
        assert_eq!(result.constants[0].name, "MAX_SIZE");
    }

    #[test]
    fn does_not_overwrite_existing_class_members() {
        let provider = MixinProvider;
        let mut class = make_class("Foo");
        class.mixins = vec!["Bar".to_string()];
        class.methods.push(make_method("doStuff", Some("int")));

        let mut bar = make_class("Bar");
        bar.methods.push(make_method("doStuff", Some("string")));
        bar.methods.push(make_method("barOnly", Some("void")));

        let class_loader = move |name: &str| -> Option<ClassInfo> {
            if name == "Bar" {
                Some(bar.clone())
            } else {
                None
            }
        };

        let result = provider.provide(&class, &class_loader);
        // "doStuff" is already on the class, so only "barOnly" should appear
        assert_eq!(result.methods.len(), 1);
        assert_eq!(result.methods[0].name, "barOnly");
    }

    #[test]
    fn rewrites_this_return_type_to_mixin_class() {
        let provider = MixinProvider;
        let mut class = make_class("Foo");
        class.mixins = vec!["Bar".to_string()];

        let mut bar = make_class("Bar");
        bar.methods.push(make_method("fluent", Some("$this")));
        bar.methods.push(make_method("selfRef", Some("self")));
        bar.methods.push(make_method("staticRef", Some("static")));

        let class_loader = move |name: &str| -> Option<ClassInfo> {
            if name == "Bar" {
                Some(bar.clone())
            } else {
                None
            }
        };

        let result = provider.provide(&class, &class_loader);
        assert_eq!(result.methods.len(), 3);
        for method in &result.methods {
            assert_eq!(
                method.return_type.as_deref(),
                Some("Bar"),
                "method '{}' should have return type rewritten to mixin class name",
                method.name
            );
        }
    }

    #[test]
    fn collects_from_ancestor_mixins() {
        let provider = MixinProvider;
        let mut class = make_class("Child");
        class.parent_class = Some("Parent".to_string());

        let mut parent = make_class("Parent");
        parent.mixins = vec!["Mixin".to_string()];

        let mut mixin = make_class("Mixin");
        mixin.methods.push(make_method("mixinMethod", Some("void")));

        let class_loader = move |name: &str| -> Option<ClassInfo> {
            match name {
                "Parent" => Some(parent.clone()),
                "Mixin" => Some(mixin.clone()),
                _ => None,
            }
        };

        let result = provider.provide(&class, &class_loader);
        assert_eq!(result.methods.len(), 1);
        assert_eq!(result.methods[0].name, "mixinMethod");
    }

    #[test]
    fn recurses_into_mixin_mixins() {
        let provider = MixinProvider;
        let mut class = make_class("Foo");
        class.mixins = vec!["Bar".to_string()];

        let mut bar = make_class("Bar");
        bar.mixins = vec!["Baz".to_string()];
        bar.methods.push(make_method("barMethod", Some("void")));

        let mut baz = make_class("Baz");
        baz.methods.push(make_method("bazMethod", Some("void")));

        let class_loader = move |name: &str| -> Option<ClassInfo> {
            match name {
                "Bar" => Some(bar.clone()),
                "Baz" => Some(baz.clone()),
                _ => None,
            }
        };

        let result = provider.provide(&class, &class_loader);
        assert_eq!(result.methods.len(), 2);
        assert!(result.methods.iter().any(|m| m.name == "barMethod"));
        assert!(result.methods.iter().any(|m| m.name == "bazMethod"));
    }

    #[test]
    fn multiple_mixins() {
        let provider = MixinProvider;
        let mut class = make_class("Foo");
        class.mixins = vec!["Bar".to_string(), "Baz".to_string()];

        let mut bar = make_class("Bar");
        bar.methods.push(make_method("barMethod", Some("void")));

        let mut baz = make_class("Baz");
        baz.methods.push(make_method("bazMethod", Some("void")));

        let class_loader = move |name: &str| -> Option<ClassInfo> {
            match name {
                "Bar" => Some(bar.clone()),
                "Baz" => Some(baz.clone()),
                _ => None,
            }
        };

        let result = provider.provide(&class, &class_loader);
        assert_eq!(result.methods.len(), 2);
        assert!(result.methods.iter().any(|m| m.name == "barMethod"));
        assert!(result.methods.iter().any(|m| m.name == "bazMethod"));
    }

    #[test]
    fn first_mixin_wins_on_name_collision() {
        let provider = MixinProvider;
        let mut class = make_class("Foo");
        class.mixins = vec!["Bar".to_string(), "Baz".to_string()];

        let mut bar = make_class("Bar");
        bar.methods.push(make_method("shared", Some("string")));

        let mut baz = make_class("Baz");
        baz.methods.push(make_method("shared", Some("int")));

        let class_loader = move |name: &str| -> Option<ClassInfo> {
            match name {
                "Bar" => Some(bar.clone()),
                "Baz" => Some(baz.clone()),
                _ => None,
            }
        };

        let result = provider.provide(&class, &class_loader);
        assert_eq!(result.methods.len(), 1);
        assert_eq!(
            result.methods[0].return_type.as_deref(),
            Some("string"),
            "first mixin should win"
        );
    }
}
