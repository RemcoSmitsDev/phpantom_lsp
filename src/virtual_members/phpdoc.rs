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

use crate::Backend;
use crate::docblock;
use crate::types::{
    ClassInfo, ConstantInfo, MAX_INHERITANCE_DEPTH, MAX_MIXIN_DEPTH, MethodInfo, PropertyInfo,
    Visibility,
};

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
        class_loader: &dyn Fn(&str) -> Option<ClassInfo>,
    ) -> bool {
        // Has a non-empty docblock with potential @method/@property tags.
        if class.class_docblock.as_ref().is_some_and(|d| !d.is_empty()) {
            return true;
        }

        // Has direct @mixin declarations.
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
        class_loader: &dyn Fn(&str) -> Option<ClassInfo>,
    ) -> VirtualMembers {
        let mut methods = Vec::new();
        let mut properties = Vec::new();
        let mut constants = Vec::new();

        // ── Phase 1: @method and @property tags (higher precedence) ─────

        if let Some(doc_text) = class.class_docblock.as_deref()
            && !doc_text.is_empty()
        {
            methods = docblock::extract_method_tags(doc_text);

            properties = docblock::extract_property_tags(doc_text)
                .into_iter()
                .map(|(name, type_str)| PropertyInfo {
                    name,
                    name_offset: 0,
                    type_hint: if type_str.is_empty() {
                        None
                    } else {
                        Some(type_str)
                    },
                    is_static: false,
                    visibility: Visibility::Public,
                    is_deprecated: false,
                })
                .collect();
        }

        // ── Phase 2: @mixin members (lower precedence) ─────────────────

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
/// This means `@method` / `@property` tags collected before this function
/// is called take precedence over mixin members.
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
            // or if a previous @method tag or mixin already contributed it.
            if class.methods.iter().any(|m| m.name == method.name) {
                continue;
            }
            if methods.iter().any(|m| m.name == method.name) {
                continue;
            }
            let method = method.clone();
            // `@return $this` / `self` / `static` in mixin methods are
            // left as-is.  When the method is later called on the
            // consuming class, `$this` resolves to the consumer (not the
            // mixin), which is the correct semantic: fluent chains
            // continue with the consumer's full API (own methods + all
            // mixin methods).  In the builder-as-static forwarding path,
            // the substitution map rewrites `$this` to
            // `\Illuminate\Database\Eloquent\Builder<Model>`, so the
            // return type must still be the raw keyword at this stage.
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

// ─── Tests ──────────────────────────────────────────────────────────────────

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
            keyword_offset: 0,
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
            file_namespace: None,
            custom_collection: None,
            casts_definitions: Vec::new(),
            attributes_definitions: Vec::new(),
            column_names: Vec::new(),
        }
    }

    fn make_method(name: &str, return_type: Option<&str>) -> MethodInfo {
        MethodInfo {
            name: name.to_string(),
            name_offset: 0,
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
            name_offset: 0,
            type_hint: type_hint.map(|s| s.to_string()),
            is_static: false,
            visibility: Visibility::Public,
            is_deprecated: false,
        }
    }

    fn make_constant(name: &str) -> ConstantInfo {
        ConstantInfo {
            name: name.to_string(),
            name_offset: 0,
            type_hint: None,
            visibility: Visibility::Public,
            is_deprecated: false,
        }
    }

    fn no_loader(_name: &str) -> Option<ClassInfo> {
        None
    }

    // ── applies_to ──────────────────────────────────────────────────────

    #[test]
    fn applies_when_docblock_present() {
        let provider = PHPDocProvider;
        let mut class = make_class("Foo");
        class.class_docblock = Some("/** @method void bar() */".to_string());
        assert!(provider.applies_to(&class, &no_loader));
    }

    #[test]
    fn does_not_apply_when_no_docblock_and_no_mixins() {
        let provider = PHPDocProvider;
        let class = make_class("Foo");
        assert!(!provider.applies_to(&class, &no_loader));
    }

    #[test]
    fn does_not_apply_when_docblock_empty_and_no_mixins() {
        let provider = PHPDocProvider;
        let mut class = make_class("Foo");
        class.class_docblock = Some(String::new());
        assert!(!provider.applies_to(&class, &no_loader));
    }

    #[test]
    fn applies_when_class_has_mixins() {
        let provider = PHPDocProvider;
        let mut class = make_class("Foo");
        class.mixins = vec!["Bar".to_string()];

        let class_loader = |_: &str| -> Option<ClassInfo> { None };
        assert!(provider.applies_to(&class, &class_loader));
    }

    #[test]
    fn applies_when_ancestor_has_mixins() {
        let provider = PHPDocProvider;
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

    // ── provide: @method ────────────────────────────────────────────────

    #[test]
    fn provides_method_tags() {
        let provider = PHPDocProvider;
        let mut class = make_class("Cart");
        class.class_docblock = Some(
            concat!(
                "/**\n",
                " * @method string getName()\n",
                " * @method void setName(string $name)\n",
                " */",
            )
            .to_string(),
        );

        let result = provider.provide(&class, &no_loader);
        assert_eq!(result.methods.len(), 2);
        assert!(result.methods.iter().any(|m| m.name == "getName"));
        assert!(result.methods.iter().any(|m| m.name == "setName"));
    }

    #[test]
    fn provides_static_method_tags() {
        let provider = PHPDocProvider;
        let mut class = make_class("Facade");
        class.class_docblock =
            Some(concat!("/**\n", " * @method static int count()\n", " */",).to_string());

        let result = provider.provide(&class, &no_loader);
        assert_eq!(result.methods.len(), 1);
        assert!(result.methods[0].is_static);
        assert_eq!(result.methods[0].name, "count");
        assert_eq!(result.methods[0].return_type.as_deref(), Some("int"));
    }

    #[test]
    fn method_tag_preserves_return_type() {
        let provider = PHPDocProvider;
        let mut class = make_class("TestCase");
        class.class_docblock = Some(
            concat!(
                "/**\n",
                " * @method \\Mockery\\MockInterface mock(string $abstract)\n",
                " */",
            )
            .to_string(),
        );

        let result = provider.provide(&class, &no_loader);
        assert_eq!(result.methods.len(), 1);
        assert_eq!(
            result.methods[0].return_type.as_deref(),
            Some("\\Mockery\\MockInterface")
        );
    }

    #[test]
    fn method_tag_parses_parameters() {
        let provider = PHPDocProvider;
        let mut class = make_class("DB");
        class.class_docblock = Some(concat!(
            "/**\n",
            " * @method void assertDatabaseHas(string $table, array $data, string $connection = null)\n",
            " */",
        ).to_string());

        let result = provider.provide(&class, &no_loader);
        assert_eq!(result.methods.len(), 1);
        let method = &result.methods[0];
        assert_eq!(method.parameters.len(), 3);
        assert!(method.parameters[0].is_required);
        assert!(method.parameters[1].is_required);
        assert!(!method.parameters[2].is_required, "$connection has default");
    }

    // ── provide: @property ──────────────────────────────────────────────

    #[test]
    fn provides_property_tags() {
        let provider = PHPDocProvider;
        let mut class = make_class("Customer");
        class.class_docblock = Some(
            concat!(
                "/**\n",
                " * @property int $id\n",
                " * @property string $name\n",
                " */",
            )
            .to_string(),
        );

        let result = provider.provide(&class, &no_loader);
        assert_eq!(result.properties.len(), 2);
        assert!(result.properties.iter().any(|p| p.name == "id"));
        assert!(result.properties.iter().any(|p| p.name == "name"));
    }

    #[test]
    fn provides_property_read_and_write_tags() {
        let provider = PHPDocProvider;
        let mut class = make_class("Controller");
        class.class_docblock = Some(
            concat!(
                "/**\n",
                " * @property-read Session $session\n",
                " * @property-write string $title\n",
                " */",
            )
            .to_string(),
        );

        let result = provider.provide(&class, &no_loader);
        assert_eq!(result.properties.len(), 2);
        let session = result
            .properties
            .iter()
            .find(|p| p.name == "session")
            .unwrap();
        assert_eq!(session.type_hint.as_deref(), Some("Session"));
        let title = result
            .properties
            .iter()
            .find(|p| p.name == "title")
            .unwrap();
        assert_eq!(title.type_hint.as_deref(), Some("string"));
    }

    #[test]
    fn property_tags_are_public_and_non_static() {
        let provider = PHPDocProvider;
        let mut class = make_class("Model");
        class.class_docblock = Some("/** @property int $id */".to_string());

        let result = provider.provide(&class, &no_loader);
        assert_eq!(result.properties.len(), 1);
        assert_eq!(result.properties[0].visibility, Visibility::Public);
        assert!(!result.properties[0].is_static);
    }

    #[test]
    fn nullable_type_cleaned() {
        let provider = PHPDocProvider;
        let mut class = make_class("Customer");
        class.class_docblock = Some("/** @property null|int $agreement_id */".to_string());

        let result = provider.provide(&class, &no_loader);
        assert_eq!(result.properties.len(), 1);
        assert_eq!(
            result.properties[0].type_hint.as_deref(),
            Some("int"),
            "null|int should resolve to int via clean_type"
        );
    }

    // ── provide: no constants from tags ─────────────────────────────────

    #[test]
    fn tags_never_produce_constants() {
        let provider = PHPDocProvider;
        let mut class = make_class("Foo");
        class.class_docblock = Some(
            concat!(
                "/**\n",
                " * @method void bar()\n",
                " * @property int $baz\n",
                " */",
            )
            .to_string(),
        );

        let result = provider.provide(&class, &no_loader);
        assert!(result.constants.is_empty());
    }

    // ── provide: empty / missing docblock ───────────────────────────────

    #[test]
    fn empty_docblock_returns_empty() {
        let provider = PHPDocProvider;
        let mut class = make_class("Foo");
        class.class_docblock = Some("/** */".to_string());

        let result = provider.provide(&class, &no_loader);
        assert!(result.methods.is_empty());
        assert!(result.properties.is_empty());
        assert!(result.constants.is_empty());
    }

    #[test]
    fn no_docblock_returns_empty() {
        let provider = PHPDocProvider;
        let class = make_class("Foo");

        let result = provider.provide(&class, &no_loader);
        assert!(result.is_empty());
    }

    // ── provide: mixed @method and @property tags ───────────────────────

    #[test]
    fn provides_both_methods_and_properties() {
        let provider = PHPDocProvider;
        let mut class = make_class("Model");
        class.class_docblock = Some(
            concat!(
                "/**\n",
                " * @property string $name\n",
                " * @method static Model find(int $id)\n",
                " * @property-read int $id\n",
                " * @method void save()\n",
                " */",
            )
            .to_string(),
        );

        let result = provider.provide(&class, &no_loader);
        assert_eq!(result.methods.len(), 2);
        assert_eq!(result.properties.len(), 2);
    }

    // ── provide: @mixin members ─────────────────────────────────────────

    #[test]
    fn provides_public_methods_from_mixin() {
        let provider = PHPDocProvider;
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
        let provider = PHPDocProvider;
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
        let provider = PHPDocProvider;
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
    fn mixin_does_not_overwrite_existing_class_members() {
        let provider = PHPDocProvider;
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
    fn mixin_leaves_this_return_type_as_is_for_consumer_resolution() {
        let provider = PHPDocProvider;
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
        // Return types are left as-is so that $this/self/static resolve
        // to the consuming class when the method is called on it.
        let expected = [
            ("fluent", "$this"),
            ("selfRef", "self"),
            ("staticRef", "static"),
        ];
        for (name, expected_ret) in &expected {
            let method = result.methods.iter().find(|m| m.name == *name).unwrap();
            assert_eq!(
                method.return_type.as_deref(),
                Some(*expected_ret),
                "method '{}' should keep its original return type for consumer resolution",
                name
            );
        }
    }

    #[test]
    fn mixin_collects_from_ancestor_mixins() {
        let provider = PHPDocProvider;
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
    fn mixin_recurses_into_mixin_mixins() {
        let provider = PHPDocProvider;
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
        let provider = PHPDocProvider;
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
        let provider = PHPDocProvider;
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

    // ── @method / @property tags take precedence over @mixin ────────────

    #[test]
    fn method_tag_beats_mixin_method() {
        let provider = PHPDocProvider;
        let mut class = make_class("Foo");
        class.class_docblock = Some("/** @method string doStuff() */".to_string());
        class.mixins = vec!["Bar".to_string()];

        let mut bar = make_class("Bar");
        bar.methods.push(make_method("doStuff", Some("int")));
        bar.methods.push(make_method("barOnly", Some("void")));

        let class_loader = move |name: &str| -> Option<ClassInfo> {
            if name == "Bar" {
                Some(bar.clone())
            } else {
                None
            }
        };

        let result = provider.provide(&class, &class_loader);
        assert_eq!(result.methods.len(), 2);
        let do_stuff = result.methods.iter().find(|m| m.name == "doStuff").unwrap();
        assert_eq!(
            do_stuff.return_type.as_deref(),
            Some("string"),
            "@method tag should take precedence over mixin method"
        );
        assert!(
            result.methods.iter().any(|m| m.name == "barOnly"),
            "non-conflicting mixin method should still appear"
        );
    }

    #[test]
    fn property_tag_beats_mixin_property() {
        let provider = PHPDocProvider;
        let mut class = make_class("Foo");
        class.class_docblock = Some("/** @property string $name */".to_string());
        class.mixins = vec!["Bar".to_string()];

        let mut bar = make_class("Bar");
        bar.properties.push(make_property("name", Some("int")));
        bar.properties.push(make_property("email", Some("string")));

        let class_loader = move |name: &str| -> Option<ClassInfo> {
            if name == "Bar" {
                Some(bar.clone())
            } else {
                None
            }
        };

        let result = provider.provide(&class, &class_loader);
        assert_eq!(result.properties.len(), 2);
        let name = result.properties.iter().find(|p| p.name == "name").unwrap();
        assert_eq!(
            name.type_hint.as_deref(),
            Some("string"),
            "@property tag should take precedence over mixin property"
        );
        assert!(
            result.properties.iter().any(|p| p.name == "email"),
            "non-conflicting mixin property should still appear"
        );
    }

    #[test]
    fn mixin_only_no_docblock() {
        let provider = PHPDocProvider;
        let mut class = make_class("Foo");
        class.mixins = vec!["Bar".to_string()];

        let mut bar = make_class("Bar");
        bar.methods.push(make_method("barMethod", Some("void")));
        bar.properties.push(make_property("barProp", Some("int")));

        let class_loader = move |name: &str| -> Option<ClassInfo> {
            if name == "Bar" {
                Some(bar.clone())
            } else {
                None
            }
        };

        let result = provider.provide(&class, &class_loader);
        assert_eq!(result.methods.len(), 1);
        assert_eq!(result.methods[0].name, "barMethod");
        assert_eq!(result.properties.len(), 1);
        assert_eq!(result.properties[0].name, "barProp");
    }
}
