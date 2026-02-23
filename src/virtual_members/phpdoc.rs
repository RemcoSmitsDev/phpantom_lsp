//! PHPDoc virtual member provider.
//!
//! Extracts `@method` and `@property` / `@property-read` / `@property-write`
//! tags from the class-level docblock and presents them as virtual members.
//! This is the second-highest-priority virtual member provider: framework
//! providers (e.g. Laravel) take precedence, but PHPDoc tags beat `@mixin`
//! members.
//!
//! Previously these tags were parsed eagerly during AST extraction in
//! `parser/classes.rs` and stuffed directly into `ClassInfo.methods` /
//! `ClassInfo.properties`.  Moving them behind the
//! [`VirtualMemberProvider`] trait gives them the correct precedence
//! (below real declared members, traits, and parents; above `@mixin`)
//! and defers parsing until someone actually needs completion or
//! go-to-definition on the class.

use crate::docblock;
use crate::types::{ClassInfo, PropertyInfo, Visibility};

use super::{VirtualMemberProvider, VirtualMembers};

/// Virtual member provider for `@method` and `@property` docblock tags.
///
/// When a class declares `@method` or `@property` tags in its class-level
/// docblock, those tags describe magic members accessible via `__call`,
/// `__get`, and `__set`.  This provider parses those tags lazily and
/// returns them as virtual members.
pub struct PHPDocProvider;

impl VirtualMemberProvider for PHPDocProvider {
    /// Returns `true` if the class has a non-empty class-level docblock.
    ///
    /// This is a cheap string-presence check. No parsing is performed.
    fn applies_to(
        &self,
        class: &ClassInfo,
        _class_loader: &dyn Fn(&str) -> Option<ClassInfo>,
    ) -> bool {
        class.class_docblock.as_ref().is_some_and(|d| !d.is_empty())
    }

    /// Parse `@method` and `@property` tags from the class docblock.
    ///
    /// Uses the existing [`docblock::extract_method_tags`] and
    /// [`docblock::extract_property_tags`] functions.  The returned
    /// virtual members are merged below real declared members (own,
    /// trait, and parent chain) but above `@mixin` members.
    fn provide(
        &self,
        class: &ClassInfo,
        _class_loader: &dyn Fn(&str) -> Option<ClassInfo>,
    ) -> VirtualMembers {
        let doc_text = match class.class_docblock.as_deref() {
            Some(t) if !t.is_empty() => t,
            _ => {
                return VirtualMembers {
                    methods: Vec::new(),
                    properties: Vec::new(),
                    constants: Vec::new(),
                };
            }
        };

        let methods = docblock::extract_method_tags(doc_text);

        let properties = docblock::extract_property_tags(doc_text)
            .into_iter()
            .map(|(name, type_str)| PropertyInfo {
                name,
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

        VirtualMembers {
            methods,
            properties,
            constants: Vec::new(),
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
    fn does_not_apply_when_no_docblock() {
        let provider = PHPDocProvider;
        let class = make_class("Foo");
        assert!(!provider.applies_to(&class, &no_loader));
    }

    #[test]
    fn does_not_apply_when_docblock_empty() {
        let provider = PHPDocProvider;
        let mut class = make_class("Foo");
        class.class_docblock = Some(String::new());
        assert!(!provider.applies_to(&class, &no_loader));
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

    // ── provide: no constants ───────────────────────────────────────────

    #[test]
    fn never_produces_constants() {
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

    // ── provide: empty docblock ─────────────────────────────────────────

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

    // ── provide: mixed tags ─────────────────────────────────────────────

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
}
