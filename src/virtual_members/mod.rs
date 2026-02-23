//! Virtual member provider abstraction.
//!
//! Virtual members are methods and properties that do not exist as real
//! PHP declarations but are surfaced by magic methods (`__call`, `__get`,
//! `__set`, etc.) or framework conventions.  Three sources produce virtual
//! members today:
//!
//! 1. **PHPDoc tags** (`@method`, `@property`, `@property-read`,
//!    `@property-write`) — document magic members on a class.
//! 2. **Mixins** (`@mixin ClassName`) — proxy all public members of
//!    another class via magic methods.
//! 3. **Framework providers** (e.g. Laravel) — synthesize members from
//!    framework-specific patterns (relationship properties, scope methods,
//!    Builder-as-static forwarding).
//!
//! All three are unified behind the [`VirtualMemberProvider`] trait.
//! Providers are queried in priority order after base resolution
//! (own members + traits + parent chain) is complete.  A member
//! contributed by a higher-priority provider is never overwritten by a
//! lower-priority one, and all virtual members lose to real declared
//! members.
//!
//! # Precedence model
//!
//! ```text
//! 1. Real declared members (in PHP source code)
//! 2. Trait members (real implementations)
//! 3. Parent chain members (real implementations)
//! 4. Virtual member providers (in priority order):
//!    a. Framework provider  — richest type info
//!    b. PHPDoc provider     — @method, @property tags
//!    c. Mixin provider      — @mixin
//! ```

pub mod mixin;
pub mod phpdoc;

use crate::Backend;
use crate::types::{ClassInfo, ConstantInfo, MethodInfo, PropertyInfo};

/// Members synthesized by a provider.
///
/// Merged below real declared members, traits, and the parent chain.
/// Each provider returns a `VirtualMembers` value from its
/// [`provide`](VirtualMemberProvider::provide) method.
pub struct VirtualMembers {
    /// Virtual methods to add to the class.
    pub methods: Vec<MethodInfo>,
    /// Virtual properties to add to the class.
    pub properties: Vec<PropertyInfo>,
    /// Virtual constants to add to the class.
    pub constants: Vec<ConstantInfo>,
}

impl VirtualMembers {
    /// Whether this value contains no methods, properties, or constants.
    pub fn is_empty(&self) -> bool {
        self.methods.is_empty() && self.properties.is_empty() && self.constants.is_empty()
    }
}

/// A provider that contributes virtual members to a class.
///
/// Receives the class with traits and parents already merged (via
/// [`resolve_class_with_inheritance`](Backend::resolve_class_with_inheritance)),
/// but **without** other providers' contributions.  This prevents
/// circular loading when one provider's output would trigger another
/// provider.
///
/// Implementations must be cheap to construct and stateless.  All
/// contextual information is passed through the `class` and
/// `class_loader` arguments.
pub trait VirtualMemberProvider {
    /// Whether this provider has anything to say about this class.
    ///
    /// This is a cheap pre-check so the resolver can skip providers
    /// early without calling [`provide`](Self::provide).  Returning
    /// `false` means [`provide`](Self::provide) will not be called.
    fn applies_to(
        &self,
        class: &ClassInfo,
        class_loader: &dyn Fn(&str) -> Option<ClassInfo>,
    ) -> bool;

    /// Produce virtual members for this class.
    ///
    /// Only called when [`applies_to`](Self::applies_to) returned `true`.
    /// The returned members are merged into the class below all real
    /// declared members (own, trait, and parent chain).
    fn provide(
        &self,
        class: &ClassInfo,
        class_loader: &dyn Fn(&str) -> Option<ClassInfo>,
    ) -> VirtualMembers;
}

/// Merge virtual members into a resolved `ClassInfo`.
///
/// For each method in `virtual.methods`, adds it to `class.methods` only
/// if no method with the same name already exists.  Same for properties.
/// This ensures that real declared members (and contributions from
/// higher-priority providers that were merged earlier) are never
/// overwritten.
pub fn merge_virtual_members(class: &mut ClassInfo, virtual_members: VirtualMembers) {
    for method in virtual_members.methods {
        if !class.methods.iter().any(|m| m.name == method.name) {
            class.methods.push(method);
        }
    }
    for property in virtual_members.properties {
        if !class.properties.iter().any(|p| p.name == property.name) {
            class.properties.push(property);
        }
    }
    for constant in virtual_members.constants {
        if !class.constants.iter().any(|c| c.name == constant.name) {
            class.constants.push(constant);
        }
    }
}

/// Apply all registered providers to a base-resolved class.
///
/// Iterates over `providers` in order (highest priority first) and
/// merges each provider's virtual members into `class`.  Because
/// [`merge_virtual_members`] skips members that already exist,
/// higher-priority providers' contributions shadow lower-priority ones.
pub fn apply_virtual_members(
    class: &mut ClassInfo,
    class_loader: &dyn Fn(&str) -> Option<ClassInfo>,
    providers: &[Box<dyn VirtualMemberProvider>],
) {
    for provider in providers {
        if provider.applies_to(class, class_loader) {
            let virtual_members = provider.provide(class, class_loader);
            if !virtual_members.is_empty() {
                merge_virtual_members(class, virtual_members);
            }
        }
    }
}

/// Return the default set of virtual member providers in priority order.
///
/// Currently returns an empty list.  As providers are implemented
/// (PHPDocProvider, MixinProvider, LaravelModelProvider) they will be
/// added here in the correct priority order:
///
/// 1. Framework provider (highest priority)
/// 2. PHPDoc provider
/// 3. Mixin provider (lowest priority)
pub fn default_providers() -> Vec<Box<dyn VirtualMemberProvider>> {
    vec![
        // PHPDoc provider — @method / @property tags, higher priority than mixin.
        Box::new(phpdoc::PHPDocProvider),
        // Mixin provider — lowest priority among virtual member providers.
        Box::new(mixin::MixinProvider),
    ]
}

// ─── Backend integration ────────────────────────────────────────────────────

impl Backend {
    /// Resolve a class with full inheritance and virtual member providers.
    ///
    /// This is the primary entry point for completion, go-to-definition,
    /// and any other feature that needs the complete set of members
    /// visible on a class instance or static access.
    ///
    /// The resolution proceeds in two phases:
    ///
    /// 1. **Base resolution** via
    ///    [`resolve_class_with_inheritance`](Self::resolve_class_with_inheritance):
    ///    merges own members, trait members, and parent chain members,
    ///    applying generic type substitution along the way.
    ///
    /// 2. **Virtual member providers**: queries each registered provider
    ///    in priority order and merges their contributions.  Virtual
    ///    members never overwrite real declared members or contributions
    ///    from higher-priority providers.
    ///
    /// Code that needs only the base resolution (e.g. providers
    /// themselves, to avoid circular loading) should call
    /// [`resolve_class_with_inheritance`](Self::resolve_class_with_inheritance)
    /// directly.
    pub fn resolve_class_fully(
        class: &ClassInfo,
        class_loader: &dyn Fn(&str) -> Option<ClassInfo>,
    ) -> ClassInfo {
        let mut merged = Self::resolve_class_with_inheritance(class, class_loader);
        let providers = default_providers();
        if !providers.is_empty() {
            apply_virtual_members(&mut merged, class_loader, &providers);
        }
        merged
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ClassLikeKind, Visibility};
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

    /// Helper: create a `MethodInfo` with the given name and return type.
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

    /// Helper: create a `PropertyInfo` with the given name and type hint.
    fn make_property(name: &str, type_hint: Option<&str>) -> PropertyInfo {
        PropertyInfo {
            name: name.to_string(),
            type_hint: type_hint.map(|s| s.to_string()),
            is_static: false,
            visibility: Visibility::Public,
            is_deprecated: false,
        }
    }

    // ── VirtualMembers tests ────────────────────────────────────────────

    #[test]
    fn virtual_members_is_empty() {
        let vm = VirtualMembers {
            methods: Vec::new(),
            properties: Vec::new(),
            constants: Vec::new(),
        };
        assert!(vm.is_empty());
    }

    #[test]
    fn virtual_members_not_empty_with_method() {
        let vm = VirtualMembers {
            methods: vec![make_method("foo", Some("string"))],
            properties: Vec::new(),
            constants: Vec::new(),
        };
        assert!(!vm.is_empty());
    }

    #[test]
    fn virtual_members_not_empty_with_property() {
        let vm = VirtualMembers {
            methods: Vec::new(),
            properties: vec![make_property("bar", Some("int"))],
            constants: Vec::new(),
        };
        assert!(!vm.is_empty());
    }

    #[test]
    fn virtual_members_not_empty_with_constant() {
        let vm = VirtualMembers {
            methods: Vec::new(),
            properties: Vec::new(),
            constants: vec![ConstantInfo {
                name: "FOO".to_string(),
                type_hint: None,
                visibility: Visibility::Public,
                is_deprecated: false,
            }],
        };
        assert!(!vm.is_empty());
    }

    // ── merge_virtual_members tests ─────────────────────────────────────

    #[test]
    fn merge_adds_new_methods() {
        let mut class = make_class("Foo");
        class.methods.push(make_method("existing", Some("string")));

        let virtual_members = VirtualMembers {
            methods: vec![make_method("new_method", Some("int"))],
            properties: Vec::new(),
            constants: Vec::new(),
        };

        merge_virtual_members(&mut class, virtual_members);

        assert_eq!(class.methods.len(), 2);
        assert!(class.methods.iter().any(|m| m.name == "existing"));
        assert!(class.methods.iter().any(|m| m.name == "new_method"));
    }

    #[test]
    fn merge_adds_new_properties() {
        let mut class = make_class("Foo");
        class
            .properties
            .push(make_property("existing", Some("string")));

        let virtual_members = VirtualMembers {
            methods: Vec::new(),
            properties: vec![make_property("new_prop", Some("int"))],
            constants: Vec::new(),
        };

        merge_virtual_members(&mut class, virtual_members);

        assert_eq!(class.properties.len(), 2);
        assert!(class.properties.iter().any(|p| p.name == "existing"));
        assert!(class.properties.iter().any(|p| p.name == "new_prop"));
    }

    #[test]
    fn merge_does_not_overwrite_existing_method() {
        let mut class = make_class("Foo");
        class.methods.push(make_method("doStuff", Some("string")));

        let virtual_members = VirtualMembers {
            methods: vec![make_method("doStuff", Some("int"))],
            properties: Vec::new(),
            constants: Vec::new(),
        };

        merge_virtual_members(&mut class, virtual_members);

        assert_eq!(class.methods.len(), 1);
        assert_eq!(
            class.methods[0].return_type.as_deref(),
            Some("string"),
            "existing method should not be overwritten"
        );
    }

    #[test]
    fn merge_does_not_overwrite_existing_property() {
        let mut class = make_class("Foo");
        class
            .properties
            .push(make_property("value", Some("string")));

        let virtual_members = VirtualMembers {
            methods: Vec::new(),
            properties: vec![make_property("value", Some("int"))],
            constants: Vec::new(),
        };

        merge_virtual_members(&mut class, virtual_members);

        assert_eq!(class.properties.len(), 1);
        assert_eq!(
            class.properties[0].type_hint.as_deref(),
            Some("string"),
            "existing property should not be overwritten"
        );
    }

    #[test]
    fn merge_handles_empty_virtual_members() {
        let mut class = make_class("Foo");
        class.methods.push(make_method("foo", Some("void")));
        class.properties.push(make_property("bar", Some("int")));

        merge_virtual_members(
            &mut class,
            VirtualMembers {
                methods: Vec::new(),
                properties: Vec::new(),
                constants: Vec::new(),
            },
        );

        assert_eq!(class.methods.len(), 1);
        assert_eq!(class.properties.len(), 1);
    }

    // ── apply_virtual_members / provider priority tests ─────────────────

    /// A test provider that always applies and contributes fixed members.
    struct TestProvider {
        methods: Vec<MethodInfo>,
        properties: Vec<PropertyInfo>,
    }

    impl VirtualMemberProvider for TestProvider {
        fn applies_to(
            &self,
            _class: &ClassInfo,
            _class_loader: &dyn Fn(&str) -> Option<ClassInfo>,
        ) -> bool {
            true
        }

        fn provide(
            &self,
            _class: &ClassInfo,
            _class_loader: &dyn Fn(&str) -> Option<ClassInfo>,
        ) -> VirtualMembers {
            VirtualMembers {
                methods: self.methods.clone(),
                properties: self.properties.clone(),
                constants: Vec::new(),
            }
        }
    }

    /// A test provider that never applies.
    struct NeverProvider;

    impl VirtualMemberProvider for NeverProvider {
        fn applies_to(
            &self,
            _class: &ClassInfo,
            _class_loader: &dyn Fn(&str) -> Option<ClassInfo>,
        ) -> bool {
            false
        }

        fn provide(
            &self,
            _class: &ClassInfo,
            _class_loader: &dyn Fn(&str) -> Option<ClassInfo>,
        ) -> VirtualMembers {
            panic!("provide should not be called when applies_to returns false")
        }
    }

    #[test]
    fn apply_providers_in_priority_order() {
        let mut class = make_class("Foo");

        // Higher priority provider contributes "doStuff" returning "string"
        let high_priority = Box::new(TestProvider {
            methods: vec![make_method("doStuff", Some("string"))],
            properties: Vec::new(),
        }) as Box<dyn VirtualMemberProvider>;

        // Lower priority provider contributes "doStuff" returning "int"
        // (should be shadowed) and "other" returning "bool" (should be added)
        let low_priority = Box::new(TestProvider {
            methods: vec![
                make_method("doStuff", Some("int")),
                make_method("other", Some("bool")),
            ],
            properties: Vec::new(),
        }) as Box<dyn VirtualMemberProvider>;

        let providers: Vec<Box<dyn VirtualMemberProvider>> = vec![high_priority, low_priority];
        let class_loader = |_: &str| -> Option<ClassInfo> { None };

        apply_virtual_members(&mut class, &class_loader, &providers);

        assert_eq!(class.methods.len(), 2);

        let do_stuff = class.methods.iter().find(|m| m.name == "doStuff").unwrap();
        assert_eq!(
            do_stuff.return_type.as_deref(),
            Some("string"),
            "higher-priority provider should win"
        );

        let other = class.methods.iter().find(|m| m.name == "other").unwrap();
        assert_eq!(other.return_type.as_deref(), Some("bool"));
    }

    #[test]
    fn apply_providers_skips_non_applicable() {
        let mut class = make_class("Foo");

        let providers: Vec<Box<dyn VirtualMemberProvider>> = vec![Box::new(NeverProvider)];
        let class_loader = |_: &str| -> Option<ClassInfo> { None };

        apply_virtual_members(&mut class, &class_loader, &providers);

        assert!(class.methods.is_empty());
        assert!(class.properties.is_empty());
    }

    #[test]
    fn apply_providers_real_members_beat_virtual() {
        let mut class = make_class("Foo");
        class
            .methods
            .push(make_method("realMethod", Some("string")));

        let provider = Box::new(TestProvider {
            methods: vec![make_method("realMethod", Some("int"))],
            properties: Vec::new(),
        }) as Box<dyn VirtualMemberProvider>;

        let providers: Vec<Box<dyn VirtualMemberProvider>> = vec![provider];
        let class_loader = |_: &str| -> Option<ClassInfo> { None };

        apply_virtual_members(&mut class, &class_loader, &providers);

        assert_eq!(class.methods.len(), 1);
        assert_eq!(
            class.methods[0].return_type.as_deref(),
            Some("string"),
            "real declared method should not be overwritten by virtual"
        );
    }

    #[test]
    fn apply_providers_property_priority() {
        let mut class = make_class("Foo");

        let high_priority = Box::new(TestProvider {
            methods: Vec::new(),
            properties: vec![make_property("name", Some("string"))],
        }) as Box<dyn VirtualMemberProvider>;

        let low_priority = Box::new(TestProvider {
            methods: Vec::new(),
            properties: vec![
                make_property("name", Some("mixed")),
                make_property("email", Some("string")),
            ],
        }) as Box<dyn VirtualMemberProvider>;

        let providers: Vec<Box<dyn VirtualMemberProvider>> = vec![high_priority, low_priority];
        let class_loader = |_: &str| -> Option<ClassInfo> { None };

        apply_virtual_members(&mut class, &class_loader, &providers);

        assert_eq!(class.properties.len(), 2);

        let name = class.properties.iter().find(|p| p.name == "name").unwrap();
        assert_eq!(
            name.type_hint.as_deref(),
            Some("string"),
            "higher-priority provider property should win"
        );

        let email = class.properties.iter().find(|p| p.name == "email").unwrap();
        assert_eq!(email.type_hint.as_deref(), Some("string"));
    }

    #[test]
    fn default_providers_has_phpdoc_and_mixin() {
        let providers = default_providers();
        assert_eq!(
            providers.len(),
            2,
            "should have PHPDocProvider and MixinProvider registered"
        );
    }

    // ── resolve_class_fully tests ───────────────────────────────────────

    #[test]
    fn resolve_class_fully_returns_same_as_base_when_no_providers() {
        // With no providers registered, resolve_class_fully should produce
        // the same result as resolve_class_with_inheritance.
        let mut class = make_class("Child");
        class.methods.push(make_method("childMethod", Some("void")));
        class.parent_class = Some("Parent".to_string());

        let mut parent = make_class("Parent");
        parent
            .methods
            .push(make_method("parentMethod", Some("string")));

        let class_loader = move |name: &str| -> Option<ClassInfo> {
            if name == "Parent" {
                Some(parent.clone())
            } else {
                None
            }
        };

        let base = Backend::resolve_class_with_inheritance(&class, &class_loader);
        let full = Backend::resolve_class_fully(&class, &class_loader);

        assert_eq!(base.methods.len(), full.methods.len());
        assert_eq!(base.properties.len(), full.properties.len());
        for base_method in &base.methods {
            assert!(
                full.methods.iter().any(|m| m.name == base_method.name),
                "full resolution should contain all base methods"
            );
        }
    }
}
