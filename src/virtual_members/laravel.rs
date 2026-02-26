//! Laravel Eloquent Model virtual member provider.
//!
//! Synthesizes virtual members for classes that extend
//! `Illuminate\Database\Eloquent\Model`.  This is the highest-priority
//! virtual member provider: its contributions beat `@method` /
//! `@property` / `@mixin` members (PHPDocProvider).
//!
//! Currently implements:
//!
//! - **Relationship properties.** Methods returning a known Eloquent
//!   relationship type (e.g. `HasOne`, `HasMany`, `BelongsTo`) produce
//!   a virtual property with the same name.  The property type is
//!   inferred from the relationship's generic parameters (Larastan-style
//!   `@return HasMany<Post, $this>` annotations) or, as a fallback,
//!   from the first `::class` argument in the method body text.
//!
//! - **Scope methods.** Methods whose name starts with `scope` (e.g.
//!   `scopeActive`, `scopeVerified`) produce a virtual method with the
//!   `scope` prefix stripped and the first letter lowercased (e.g.
//!   `active`, `verified`).  The first `$query` parameter is removed.
//!   Scope methods are available as both static and instance methods
//!   so they resolve for `User::active()` and `$user->active()`.
//!
//! - **Builder-as-static forwarding.** Laravel's `Model::__callStatic()`
//!   forwards static calls to `static::query()`, which returns an
//!   Eloquent Builder.  This provider loads
//!   `\Illuminate\Database\Eloquent\Builder`, fully resolves it
//!   (including its `@mixin` on `Query\Builder`), and presents its
//!   public instance methods as static virtual methods on the model.
//!   Return types are mapped so that `static`/`$this`/`self` resolve
//!   to `Builder<ConcreteModel>` (the chain continues on the builder)
//!   and template parameters like `TModel` resolve to the concrete
//!   model class.  This makes `User::where(...)->orderBy(...)->get()`
//!   resolve end-to-end.
//!
//! - **Cast properties.** Entries in the `$casts` property array or
//!   `casts()` method body produce typed virtual properties.  Cast type
//!   strings are mapped to PHP types (e.g. `datetime` → `\Carbon\Carbon`,
//!   `boolean` → `bool`, `decimal:2` → `float`).  Custom cast classes
//!   are resolved by loading the class and inspecting the `get()`
//!   method's return type.  When the `get()` method has no return type,
//!   the resolver falls back to the first generic argument from an
//!   `@implements CastsAttributes<TGet, TSet>` annotation on the cast
//!   class.  Enum casts resolve to the enum class itself.  Classes
//!   implementing `Castable` also resolve to themselves.  A `:argument`
//!   suffix (e.g. `Address::class.':nullable'`) is stripped before
//!   resolution.
//!
//! - **Attribute default properties.** Entries in the `$attributes`
//!   property array produce typed virtual properties as a fallback.
//!   Types are inferred from the literal default values: strings,
//!   booleans, integers, floats, `null`, and arrays.  Columns that
//!   already have a `$casts` entry are skipped, so casts always take
//!   priority.
//!
//! - **Column name properties.** Column names from `$fillable`,
//!   `$guarded`, and `$hidden` produce `mixed`-typed virtual
//!   properties as a last-resort fallback.  Columns already covered
//!   by `$casts` or `$attributes` are skipped.

use std::collections::HashMap;

use crate::Backend;
use crate::docblock::types::parse_generic_args;
use crate::inheritance::{apply_substitution, apply_substitution_to_conditional};
use crate::types::ELOQUENT_COLLECTION_FQN;
use crate::types::{
    ClassInfo, ClassLikeKind, MAX_INHERITANCE_DEPTH, MethodInfo, PropertyInfo, Visibility,
};

use super::{VirtualMemberProvider, VirtualMembers};

/// The fully-qualified name of the Eloquent base model.
const ELOQUENT_MODEL_FQN: &str = "Illuminate\\Database\\Eloquent\\Model";

/// Maps Eloquent relationship builder method names to their corresponding
/// relationship class short names.  Used by [`infer_relationship_from_body`]
/// to synthesize a return type from the method body when no `@return`
/// annotation is present.
const RELATIONSHIP_METHOD_MAP: &[(&str, &str)] = &[
    ("hasOne", "HasOne"),
    ("hasMany", "HasMany"),
    ("belongsTo", "BelongsTo"),
    ("belongsToMany", "BelongsToMany"),
    ("morphOne", "MorphOne"),
    ("morphMany", "MorphMany"),
    ("morphTo", "MorphTo"),
    ("morphToMany", "MorphToMany"),
    ("hasManyThrough", "HasManyThrough"),
    ("hasOneThrough", "HasOneThrough"),
];

/// Known Eloquent relationship class short names that yield a single
/// (nullable) related model instance when accessed as a property.
const SINGULAR_RELATIONSHIPS: &[&str] = &["HasOne", "MorphOne", "BelongsTo", "HasOneThrough"];

/// Known Eloquent relationship class short names that yield a
/// `Collection<TRelated>` when accessed as a property.
const COLLECTION_RELATIONSHIPS: &[&str] = &[
    "HasMany",
    "MorphMany",
    "BelongsToMany",
    "HasManyThrough",
    "MorphToMany",
];

/// The `MorphTo` relationship resolves to the generic `Model` base class
/// because the concrete related type is determined at runtime.
const MORPH_TO: &str = "MorphTo";

/// The fully-qualified name of the `Attribute` cast class used by
/// Laravel 9+ modern accessors/mutators.
const ATTRIBUTE_CAST_FQN: &str = "Illuminate\\Database\\Eloquent\\Casts\\Attribute";

/// The default return type for scope methods that don't declare a return
/// type or return `void`.
const DEFAULT_SCOPE_RETURN_TYPE: &str = "\\Illuminate\\Database\\Eloquent\\Builder<static>";

/// The fully-qualified name of the Eloquent Builder class.
pub const ELOQUENT_BUILDER_FQN: &str = "Illuminate\\Database\\Eloquent\\Builder";

/// The fully-qualified name of the `Factory` base class.
const FACTORY_FQN: &str = "Illuminate\\Database\\Eloquent\\Factories\\Factory";

/// The short name of the `CastsAttributes` interface, used to look up
/// `@implements` generic arguments on custom cast classes.
const CASTS_ATTRIBUTES_SHORT: &str = "CastsAttributes";

/// The fully-qualified name of the `CastsAttributes` interface.
const CASTS_ATTRIBUTES_FQN: &str = "Illuminate\\Contracts\\Database\\Eloquent\\CastsAttributes";

/// Maps Eloquent cast type strings to their corresponding PHP types.
///
/// When a model declares `protected $casts = ['col' => 'datetime']`, the
/// column is treated as `\Carbon\Carbon` in completions.  This table
/// covers all built-in Laravel cast types.
const CAST_TYPE_MAP: &[(&str, &str)] = &[
    ("datetime", "\\Carbon\\Carbon"),
    ("date", "\\Carbon\\Carbon"),
    ("timestamp", "int"),
    ("immutable_datetime", "\\Carbon\\CarbonImmutable"),
    ("immutable_date", "\\Carbon\\CarbonImmutable"),
    ("boolean", "bool"),
    ("bool", "bool"),
    ("integer", "int"),
    ("int", "int"),
    ("float", "float"),
    ("double", "float"),
    ("real", "float"),
    ("string", "string"),
    ("array", "array"),
    ("json", "array"),
    ("object", "object"),
    ("collection", "\\Illuminate\\Support\\Collection"),
    ("encrypted", "string"),
    ("encrypted:array", "array"),
    ("encrypted:collection", "\\Illuminate\\Support\\Collection"),
    ("encrypted:object", "object"),
    ("hashed", "string"),
];

/// The fully-qualified name of the `Castable` contract.
const CASTABLE_FQN: &str = "Illuminate\\Contracts\\Database\\Eloquent\\Castable";

/// Map an Eloquent cast type string to a PHP type.
///
/// Handles built-in cast strings (`datetime`, `boolean`, `array`, etc.),
/// `decimal:N` variants (e.g. `decimal:2` → `float`), custom cast
/// classes (inspects the `get()` return type), enum classes (the
/// property type is the enum itself), and `Castable` implementations
/// (the property type is the class itself).
///
/// When a custom cast class's `get()` method has no return type (native
/// or docblock), the resolver falls back to the first generic argument
/// from an `@implements CastsAttributes<TGet, TSet>` annotation on the
/// cast class.
///
/// Class-based cast types may carry a `:argument` suffix (e.g.
/// `Address::class.':nullable'`).  The suffix is stripped before
/// resolving the class.
fn cast_type_to_php_type(
    cast_type: &str,
    class_loader: &dyn Fn(&str) -> Option<ClassInfo>,
) -> String {
    // 1. Check the built-in mapping table.
    let lower = cast_type.to_lowercase();
    for &(key, php_type) in CAST_TYPE_MAP {
        if lower == key {
            return php_type.to_string();
        }
    }

    // 2. Handle `decimal:N` variants (e.g. `decimal:2`, `decimal:8`).
    if lower.starts_with("decimal:") || lower == "decimal" {
        return "float".to_string();
    }

    // 3. Handle `datetime:format` variants (e.g. `datetime:Y-m-d`).
    if lower.starts_with("datetime:") {
        return "\\Carbon\\Carbon".to_string();
    }

    // 4. Handle `date:format` variants.
    if lower.starts_with("date:") {
        return "\\Carbon\\Carbon".to_string();
    }

    // 5. Handle `immutable_datetime:format` variants.
    if lower.starts_with("immutable_datetime:") {
        return "\\Carbon\\CarbonImmutable".to_string();
    }

    // 6. Handle `immutable_date:format` variants.
    if lower.starts_with("immutable_date:") {
        return "\\Carbon\\CarbonImmutable".to_string();
    }

    // 7. Assume it's a class-based cast.  Strip any `:argument` suffix
    //    (e.g. `App\Casts\Address:nullable` → `App\Casts\Address`).
    let class_name = cast_type.split(':').next().unwrap_or(cast_type);
    let clean = class_name.strip_prefix('\\').unwrap_or(class_name);

    if let Some(cast_class) = class_loader(clean) {
        // 7a. Enums — the property type is the enum itself.
        if cast_class.kind == ClassLikeKind::Enum {
            return format!("\\{clean}");
        }

        // 7b. Castable implementations — the property type is the
        //     class itself.  Castable classes declare `castUsing()`
        //     which returns a CastsAttributes instance, but the
        //     developer-facing type is the Castable class.
        if is_castable(&cast_class) {
            return format!("\\{clean}");
        }

        // 7c. CastsAttributes / custom cast class — inspect `get()`.
        if let Some(get_method) = cast_class.methods.iter().find(|m| m.name == "get")
            && let Some(ref rt) = get_method.return_type
        {
            return rt.clone();
        }

        // 7d. Fallback: extract TGet from `@implements CastsAttributes<TGet, TSet>`.
        //     When the `get()` method has no return type (native or docblock),
        //     the type may be declared via the class-level `@implements`
        //     annotation on the `CastsAttributes` interface.  The first
        //     generic argument corresponds to `TGet`.
        if let Some(tget) = extract_tget_from_implements_generics(&cast_class) {
            return tget;
        }
    }

    // 8. Fallback: unknown cast type.
    "mixed".to_string()
}

/// Extract the `TGet` type from a cast class's `@implements CastsAttributes<TGet, TSet>`.
///
/// Returns the first generic argument if the class declares an
/// `@implements` annotation for `CastsAttributes` (matched by short
/// name or FQN, with or without leading backslash).
fn extract_tget_from_implements_generics(class: &ClassInfo) -> Option<String> {
    for (name, args) in &class.implements_generics {
        let stripped = name.strip_prefix('\\').unwrap_or(name);
        if (stripped == CASTS_ATTRIBUTES_FQN
            || stripped == CASTS_ATTRIBUTES_SHORT
            || extract_short_name(stripped) == CASTS_ATTRIBUTES_SHORT)
            && let Some(tget) = args.first()
            && !tget.is_empty()
        {
            return Some(tget.clone());
        }
    }
    None
}

/// Check whether a class implements the `Castable` contract.
///
/// Looks for `Illuminate\Contracts\Database\Eloquent\Castable` in the
/// class's `interfaces` list (with or without leading backslash, and
/// also matches the short name `Castable`).
fn is_castable(class: &ClassInfo) -> bool {
    class.interfaces.iter().any(|iface| {
        let stripped = iface.strip_prefix('\\').unwrap_or(iface);
        stripped == CASTABLE_FQN || stripped == "Castable"
    })
}

/// Virtual member provider for Laravel Eloquent models.
///
/// When a class extends `Illuminate\Database\Eloquent\Model` (directly
/// or through an intermediate parent), this provider scans its methods
/// for Eloquent relationship return types and synthesizes virtual
/// properties for each one.
///
/// For example, a method `posts()` returning `HasMany<Post, $this>`
/// produces a virtual property `$posts` with type
/// `\Illuminate\Database\Eloquent\Collection<Post>`.
pub struct LaravelModelProvider;

/// Virtual member provider for Laravel Eloquent factory classes.
///
/// When a class extends `Illuminate\Database\Eloquent\Factories\Factory`
/// and does not already have an `@extends Factory<Model>` annotation
/// that would let the generics system resolve `TModel`, this provider
/// uses the naming convention (`Database\Factories\UserFactory` maps to
/// `App\Models\User`) to synthesize `create()` and `make()` methods
/// that return the correct model type.
pub struct LaravelFactoryProvider;

/// Determine whether `class_name` is the Eloquent Model base class.
///
/// Checks against the FQN with and without a leading backslash, and
/// also against the short name `Model` (which may appear in stubs or
/// in same-file test setups).
fn is_eloquent_model(class_name: &str) -> bool {
    let stripped = class_name.strip_prefix('\\').unwrap_or(class_name);
    stripped == ELOQUENT_MODEL_FQN
}

/// Derive the conventional factory FQN from a model FQN.
///
/// Follows Laravel's default convention:
/// - `App\Models\User` → `Database\Factories\UserFactory`
/// - `App\Models\Admin\SuperUser` → `Database\Factories\Admin\SuperUserFactory`
///
/// The rule: strip the `Models\` segment from the namespace, replace
/// the root with `Database\Factories\`, and append `Factory` to the
/// class short name.
pub(crate) fn model_to_factory_fqn(model_fqn: &str) -> String {
    let clean = model_fqn.strip_prefix('\\').unwrap_or(model_fqn);

    // Split into namespace + short name.
    let (ns, short) = match clean.rsplit_once('\\') {
        Some((ns, short)) => (ns, short),
        None => return format!("Database\\Factories\\{clean}Factory"),
    };

    // Check for `X\Models\Sub` pattern → `Database\Factories\Sub`
    if let Some((_prefix, suffix)) = ns.split_once("\\Models\\") {
        return format!("Database\\Factories\\{suffix}\\{short}Factory");
    }

    // Check for `X\Models` pattern (model directly in Models namespace)
    if ns.ends_with("\\Models") || ns == "Models" {
        return format!("Database\\Factories\\{short}Factory");
    }

    // No `Models` segment — put factory in `Database\Factories`
    format!("Database\\Factories\\{short}Factory")
}

/// Derive the conventional model FQN from a factory FQN.
///
/// Reverse of [`model_to_factory_fqn`]:
/// - `Database\Factories\UserFactory` → `App\Models\User`
/// - `Database\Factories\Admin\SuperUserFactory` → `App\Models\Admin\SuperUser`
pub(crate) fn factory_to_model_fqn(factory_fqn: &str) -> Option<String> {
    let clean = factory_fqn.strip_prefix('\\').unwrap_or(factory_fqn);

    // The short name must end with `Factory`.
    let short = clean.rsplit('\\').next().unwrap_or(clean);
    let model_short = short.strip_suffix("Factory")?;
    if model_short.is_empty() {
        return None;
    }

    // Extract the namespace after `Database\Factories\`.
    let ns = clean.rsplit_once('\\').map(|(ns, _)| ns).unwrap_or("");

    let sub_ns = if let Some(after) = ns.strip_prefix("Database\\Factories\\") {
        Some(after)
    } else if ns == "Database\\Factories" {
        None
    } else {
        // Not in the standard factory namespace — still try to strip
        // any `Factories` segment.
        None
    };

    match sub_ns {
        Some(sub) => Some(format!("App\\Models\\{sub}\\{model_short}")),
        None => Some(format!("App\\Models\\{model_short}")),
    }
}

/// Determine whether `class_name` is the Eloquent Factory base class.
fn is_eloquent_factory(class_name: &str) -> bool {
    let stripped = class_name.strip_prefix('\\').unwrap_or(class_name);
    stripped == FACTORY_FQN
}

/// Walk the parent chain of `class` looking for
/// `Illuminate\Database\Eloquent\Factories\Factory`.
///
/// Returns `true` if the class itself is `Factory` or any ancestor is.
fn extends_eloquent_factory(
    class: &ClassInfo,
    class_loader: &dyn Fn(&str) -> Option<ClassInfo>,
) -> bool {
    if is_eloquent_factory(&class.name) {
        return true;
    }

    let mut current = class.clone();
    let mut depth = 0u32;
    while let Some(ref parent_name) = current.parent_class {
        depth += 1;
        if depth > MAX_INHERITANCE_DEPTH {
            break;
        }
        if is_eloquent_factory(parent_name) {
            return true;
        }
        match class_loader(parent_name) {
            Some(parent) => {
                if is_eloquent_factory(&parent.name) {
                    return true;
                }
                current = parent;
            }
            None => break,
        }
    }

    false
}

/// Check whether a factory class already has `@extends Factory<Model>`
/// that would let the generics system resolve `TModel`.
fn has_factory_extends_generic(class: &ClassInfo) -> bool {
    class.extends_generics.iter().any(|(name, args)| {
        let short = name.rsplit('\\').next().unwrap_or(name);
        short == "Factory" && !args.is_empty()
    })
}

/// Build virtual `create()` and `make()` methods for a factory class
/// that does not have `@extends Factory<Model>`.
///
/// The model type is derived from the naming convention (e.g.
/// `Database\Factories\UserFactory` → `App\Models\User`).
fn build_factory_model_methods(
    class: &ClassInfo,
    class_loader: &dyn Fn(&str) -> Option<ClassInfo>,
) -> Vec<MethodInfo> {
    let model_fqn = match factory_to_model_fqn(&class.name) {
        Some(fqn) => fqn,
        None => return Vec::new(),
    };

    // Verify the model class actually exists.
    if class_loader(&model_fqn).is_none() {
        return Vec::new();
    }

    let model_type = format!("\\{model_fqn}");

    vec![
        MethodInfo {
            name: "create".to_string(),
            parameters: Vec::new(),
            return_type: Some(model_type.clone()),
            is_static: false,
            visibility: Visibility::Public,
            conditional_return: None,
            is_deprecated: false,
            template_params: Vec::new(),
            template_bindings: Vec::new(),
        },
        MethodInfo {
            name: "make".to_string(),
            parameters: Vec::new(),
            return_type: Some(model_type),
            is_static: false,
            visibility: Visibility::Public,
            conditional_return: None,
            is_deprecated: false,
            template_params: Vec::new(),
            template_bindings: Vec::new(),
        },
    ]
}

/// Walk the parent chain of `class` looking for
/// `Illuminate\Database\Eloquent\Model`.
///
/// Returns `true` if the class itself is `Model` or any ancestor is.
pub fn extends_eloquent_model(
    class: &ClassInfo,
    class_loader: &dyn Fn(&str) -> Option<ClassInfo>,
) -> bool {
    if is_eloquent_model(&class.name) {
        return true;
    }

    let mut current = class.clone();
    let mut depth = 0u32;
    while let Some(ref parent_name) = current.parent_class {
        depth += 1;
        if depth > MAX_INHERITANCE_DEPTH {
            break;
        }
        if is_eloquent_model(parent_name) {
            return true;
        }
        match class_loader(parent_name) {
            Some(parent) => {
                if is_eloquent_model(&parent.name) {
                    return true;
                }
                current = parent;
            }
            None => break,
        }
    }

    false
}

/// The category of a relationship return type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RelationshipKind {
    /// HasOne, MorphOne, BelongsTo — singular nullable model.
    Singular,
    /// HasMany, MorphMany, BelongsToMany, HasManyThrough, MorphToMany — Collection.
    Collection,
    /// MorphTo — generic Model.
    MorphTo,
}

/// Try to classify a return type string as a known Eloquent relationship.
///
/// Accepts both short names (`HasMany`) and fully-qualified names
/// (`\Illuminate\Database\Eloquent\Relations\HasMany`).  Generic
/// parameters are stripped before matching.
fn classify_relationship(return_type: &str) -> Option<RelationshipKind> {
    let (base, _) = parse_generic_args(return_type);
    let short_name = extract_short_name(base);

    if SINGULAR_RELATIONSHIPS.contains(&short_name) {
        return Some(RelationshipKind::Singular);
    }
    if COLLECTION_RELATIONSHIPS.contains(&short_name) {
        return Some(RelationshipKind::Collection);
    }
    if short_name == MORPH_TO {
        return Some(RelationshipKind::MorphTo);
    }

    None
}

/// Extract the short (unqualified) class name from a potentially
/// fully-qualified name.
///
/// `"\\Illuminate\\Database\\Eloquent\\Relations\\HasMany"` → `"HasMany"`
/// `"HasMany"` → `"HasMany"`
fn extract_short_name(fqn: &str) -> &str {
    fqn.rsplit('\\').next().unwrap_or(fqn)
}

/// Extract the `TRelated` type from a relationship return type's
/// generic parameters.
///
/// Given `"HasMany<Post, $this>"`, returns `Some("Post")`.
/// Given `"HasOne<\\App\\Models\\Post, $this>"`, returns
/// `Some("\\App\\Models\\Post")`.
///
/// Returns `None` if no generic parameters are present.
fn extract_related_type(return_type: &str) -> Option<String> {
    let (_, args) = parse_generic_args(return_type);
    let first = args.first()?;
    let trimmed = first.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.to_string())
}

/// Build the virtual property type for a relationship.
/// Build the property type string for a relationship.
///
/// - Singular relationships → the related type as-is (nullable).
/// - Collection relationships → the custom collection class (if set) or
///   `\Illuminate\Database\Eloquent\Collection`, parameterised with `<TRelated>`.
/// - MorphTo → `\Illuminate\Database\Eloquent\Model`.
fn build_property_type(
    kind: RelationshipKind,
    related_type: Option<&str>,
    custom_collection: Option<&str>,
) -> Option<String> {
    match kind {
        RelationshipKind::Singular => related_type.map(|t| t.to_string()),
        RelationshipKind::Collection => {
            let inner = related_type.unwrap_or("\\Illuminate\\Database\\Eloquent\\Model");
            let collection_class = custom_collection
                .map(|c| format!("\\{}", c.strip_prefix('\\').unwrap_or(c)))
                .unwrap_or_else(|| format!("\\{ELOQUENT_COLLECTION_FQN}"));
            Some(format!("{collection_class}<{inner}>"))
        }
        RelationshipKind::MorphTo => Some("\\Illuminate\\Database\\Eloquent\\Model".to_string()),
    }
}

/// Determine whether a method is a legacy Eloquent accessor.
///
/// Legacy accessors follow the `getXAttribute()` naming convention where
/// `X` starts with an uppercase letter.  For example,
/// `getFullNameAttribute()` is a legacy accessor that produces a virtual
/// property `$full_name`.
fn is_legacy_accessor(method: &MethodInfo) -> bool {
    let name = &method.name;
    if !name.starts_with("get") || !name.ends_with("Attribute") {
        return false;
    }
    // Must have at least one character between "get" and "Attribute".
    // "getAttribute" itself (len 12) is a real Eloquent method, not an accessor.
    let middle = &name[3..name.len() - 9]; // strip "get" (3) and "Attribute" (9)
    if middle.is_empty() {
        return false;
    }
    // The first character of the middle portion must be uppercase.
    middle.starts_with(|c: char| c.is_uppercase())
}

/// Extract the virtual property name from a legacy accessor method name.
///
/// Strips `get` prefix and `Attribute` suffix, then converts the
/// remaining CamelCase portion to snake_case.
///
/// `getFullNameAttribute` → `full_name`
/// `getNameAttribute` → `name`
fn legacy_accessor_property_name(method_name: &str) -> String {
    let middle = &method_name[3..method_name.len() - 9];
    camel_to_snake(middle)
}

/// Determine whether a method is a modern Eloquent accessor (Laravel 9+).
///
/// Modern accessors are methods that return
/// `Illuminate\Database\Eloquent\Casts\Attribute`.  The method name
/// is in camelCase and the virtual property name is the snake_case
/// equivalent.  For example, `fullName(): Attribute` produces
/// `$full_name`.
fn is_modern_accessor(method: &MethodInfo) -> bool {
    match method.return_type.as_deref() {
        Some(rt) => {
            let clean = rt.strip_prefix('\\').unwrap_or(rt);
            // Strip generic parameters (e.g. Attribute<string, never>)
            let base = clean.split('<').next().unwrap_or(clean).trim();
            base == ATTRIBUTE_CAST_FQN || base == "Attribute"
        }
        None => false,
    }
}

/// Convert a camelCase or PascalCase string to snake_case.
///
/// Inserts an underscore before each uppercase letter that follows a
/// lowercase letter or digit, and before an uppercase letter that is
/// followed by a lowercase letter when preceded by another uppercase
/// letter (to handle acronyms like `URL` → `u_r_l`).
///
/// `FullName` → `full_name`
/// `firstName` → `first_name`
/// `isAdmin` → `is_admin`
pub(crate) fn camel_to_snake(s: &str) -> String {
    let mut result = String::with_capacity(s.len() + 4);
    let chars: Vec<char> = s.chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        if c.is_uppercase() {
            if i > 0 {
                let prev = chars[i - 1];
                // Insert underscore when: lowercase/digit → uppercase,
                // or uppercase → uppercase followed by lowercase (acronym boundary).
                if prev.is_lowercase() || prev.is_ascii_digit() {
                    result.push('_');
                } else if prev.is_uppercase() {
                    // Check next char for acronym boundary: "URL" + "Name" → "u_r_l_name"
                    if let Some(&next) = chars.get(i + 1)
                        && next.is_lowercase()
                    {
                        result.push('_');
                    }
                }
            }
            for lc in c.to_lowercase() {
                result.push(lc);
            }
        } else {
            result.push(c);
        }
    }
    result
}

/// Convert a snake_case string to camelCase.
///
/// `full_name` → `fullName`
/// `avatar_url` → `avatarUrl`
/// `name` → `name`
pub(crate) fn snake_to_camel(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut capitalize_next = false;
    for c in s.chars() {
        if c == '_' {
            capitalize_next = true;
        } else if capitalize_next {
            for uc in c.to_uppercase() {
                result.push(uc);
            }
            capitalize_next = false;
        } else {
            result.push(c);
        }
    }
    result
}

/// Convert a snake_case string to PascalCase.
///
/// `full_name` → `FullName`
/// `avatar_url` → `AvatarUrl`
/// `name` → `Name`
fn snake_to_pascal(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut capitalize_next = true;
    for c in s.chars() {
        if c == '_' {
            capitalize_next = true;
        } else if capitalize_next {
            for uc in c.to_uppercase() {
                result.push(uc);
            }
            capitalize_next = false;
        } else {
            result.push(c);
        }
    }
    result
}

/// Build the legacy accessor method name from a virtual property name.
///
/// `display_name` → `getDisplayNameAttribute`
/// `name` → `getNameAttribute`
pub(crate) fn legacy_accessor_method_name(property_name: &str) -> String {
    let pascal = snake_to_pascal(property_name);
    format!("get{pascal}Attribute")
}

/// Return candidate accessor method names for a virtual property name.
///
/// Go-to-definition uses this to map a snake_case virtual property back
/// to the method that produces it.  Returns both the legacy
/// (`getDisplayNameAttribute`) and modern (`displayName`) forms so the
/// caller can try each one.
pub(crate) fn accessor_method_candidates(property_name: &str) -> Vec<String> {
    vec![
        legacy_accessor_method_name(property_name),
        snake_to_camel(property_name),
    ]
}

/// Infer a relationship return type from a method's body text.
///
/// When a relationship method has no `@return` annotation, this function
/// scans the body for patterns like `$this->hasMany(Post::class)` and
/// synthesizes a return type string (e.g. `HasMany<Post>`).
///
/// Supports all standard Eloquent relationship builder methods:
/// `hasOne`, `hasMany`, `belongsTo`, `belongsToMany`, `morphOne`,
/// `morphMany`, `morphTo`, `morphToMany`, `hasManyThrough`, and
/// `hasOneThrough`.
///
/// Returns `None` if no recognisable pattern is found.
pub fn infer_relationship_from_body(body_text: &str) -> Option<String> {
    for &(method_name, class_name) in RELATIONSHIP_METHOD_MAP {
        // Look for `$this->methodName(` in the body text.
        let needle = format!("$this->{method_name}(");
        let Some(call_pos) = body_text.find(&needle) else {
            continue;
        };

        // `morphTo` never carries a related-model generic parameter;
        // the concrete type is determined at runtime.
        if method_name == "morphTo" {
            return Some(class_name.to_string());
        }

        // Extract the first argument from the call.  We look for
        // `SomeName::class` as the first positional argument.
        let args_start = call_pos + needle.len();
        let after_paren = &body_text[args_start..];

        if let Some(class_arg) = extract_class_argument(after_paren) {
            return Some(format!("{class_name}<{class_arg}>"));
        }

        // No `::class` argument found — return the bare relationship
        // name without generics.  The provider will handle it the same
        // way it handles annotated relationships without generics.
        return Some(class_name.to_string());
    }

    None
}

/// Extract a class name from the first `X::class` argument in a
/// parenthesised argument list.
///
/// Given the text after the opening `(`, e.g. `Post::class, 'user_id')`,
/// returns `Some("Post")`.  Also handles fully-qualified names like
/// `\App\Models\Post::class` and `self::class` / `static::class`.
///
/// Returns `None` if no `::class` token is found before the closing `)`.
fn extract_class_argument(after_paren: &str) -> Option<String> {
    // Find the closing paren to bound our search.
    let end = after_paren.find(')')?;
    let args_region = &after_paren[..end];

    // Isolate the first argument (before the first comma) and look for
    // `X::class` within it.
    let first_arg = args_region.split(',').next().unwrap_or(args_region);
    let class_pos = first_arg.find("::class")?;
    let before = first_arg[..class_pos].trim();

    if before.is_empty() {
        return None;
    }

    // Strip leading backslash for FQNs and extract the short name.
    let name = before.strip_prefix('\\').unwrap_or(before);
    let short_name = name.rsplit('\\').next().unwrap_or(name);

    if short_name.is_empty() {
        return None;
    }

    Some(short_name.to_string())
}

/// Determine whether a method is an Eloquent scope.
///
/// Scopes are methods whose name starts with `scope` (case-sensitive)
/// and have at least five characters (the prefix plus at least one
/// character for the scope name).  For example, `scopeActive` is a
/// scope, but `scope` alone is not.
fn is_scope_method(method: &MethodInfo) -> bool {
    method.name.starts_with("scope") && method.name.len() > 5
}

/// Transform a scope method name into the public-facing scope name.
///
/// Strips the `scope` prefix and lowercases the first character:
/// `scopeActive` → `active`, `scopeVerified` → `verified`.
fn scope_name(method_name: &str) -> String {
    let after_prefix = &method_name[5..]; // skip "scope"
    let mut chars = after_prefix.chars();
    match chars.next() {
        Some(c) => {
            let lower: String = c.to_lowercase().collect();
            format!("{lower}{}", chars.as_str())
        }
        None => String::new(),
    }
}

/// Determine the return type for a synthesized scope method.
///
/// Uses the scope method's declared return type.  If the return type is
/// `void` or absent, defaults to
/// `\Illuminate\Database\Eloquent\Builder<static>`.
fn scope_return_type(method: &MethodInfo) -> String {
    match method.return_type.as_deref() {
        Some("void") | None => DEFAULT_SCOPE_RETURN_TYPE.to_string(),
        Some(rt) => rt.to_string(),
    }
}

/// Build virtual methods for a scope method.
///
/// Returns two `MethodInfo` values: one static and one instance.  Both
/// have the `scope` prefix stripped, the first letter lowercased, and
/// the first `$query` parameter removed.  This makes scope methods
/// accessible via both `User::active()` (static) and `$user->active()`
/// (instance).
fn build_scope_methods(method: &MethodInfo) -> [MethodInfo; 2] {
    let name = scope_name(&method.name);
    let return_type = Some(scope_return_type(method));

    // Strip the first parameter ($query / $builder) that Laravel injects.
    let parameters: Vec<_> = if method.parameters.is_empty() {
        Vec::new()
    } else {
        method.parameters[1..].to_vec()
    };

    let instance_method = MethodInfo {
        name: name.clone(),
        parameters: parameters.clone(),
        return_type: return_type.clone(),
        is_static: false,
        visibility: Visibility::Public,
        conditional_return: None,
        is_deprecated: method.is_deprecated,
        template_params: Vec::new(),
        template_bindings: Vec::new(),
    };

    let static_method = MethodInfo {
        name,
        parameters,
        return_type,
        is_static: true,
        visibility: Visibility::Public,
        conditional_return: None,
        is_deprecated: method.is_deprecated,
        template_params: Vec::new(),
        template_bindings: Vec::new(),
    };

    [instance_method, static_method]
}

/// Build static virtual methods by forwarding Eloquent Builder's public
/// instance methods onto the model class.
///
/// Laravel's `Model::__callStatic()` delegates static calls to
/// `static::query()`, which returns a `Builder<static>`.  This function
/// loads the Builder class, fully resolves it (including `@mixin`
/// `Query\Builder` members), and converts each public instance method
/// into a static virtual method on the model.
///
/// Return type mapping:
/// - `static`, `$this`, `self` → `\Illuminate\Database\Eloquent\Builder<ConcreteModel>`
///   (the chain continues on the builder, not the model).
/// - Template parameters (e.g. `TModel`) → the concrete model class name.
///
/// Methods whose name starts with `__` (magic methods) are skipped.
/// Replace `\Illuminate\Database\Eloquent\Collection` with a custom
/// collection class in a type string, preserving generic parameters.
fn replace_eloquent_collection(type_str: &str, custom_collection: &str) -> String {
    let fqn_prefixed = format!("\\{ELOQUENT_COLLECTION_FQN}");
    let bare_fqn = ELOQUENT_COLLECTION_FQN;
    let replacement = if custom_collection.starts_with('\\') {
        custom_collection.to_string()
    } else {
        format!("\\{custom_collection}")
    };

    // Replace both `\Illuminate\...\Collection` and `Illuminate\...\Collection`
    // (with and without leading backslash).
    let result = type_str.replace(&fqn_prefixed, &replacement);
    result.replace(bare_fqn, replacement.trim_start_matches('\\'))
}

fn build_builder_forwarded_methods(
    class: &ClassInfo,
    class_loader: &dyn Fn(&str) -> Option<ClassInfo>,
) -> Vec<MethodInfo> {
    // Load the Eloquent Builder class.
    let builder_class = match class_loader(ELOQUENT_BUILDER_FQN) {
        Some(c) => c,
        None => return Vec::new(),
    };

    // Fully resolve Builder (own + traits + parents + virtual members
    // including @mixin Query\Builder).  This is safe because Builder
    // does not extend Model, so the LaravelModelProvider will not
    // recurse.
    let resolved_builder = Backend::resolve_class_fully(&builder_class, class_loader);

    // Build a substitution map: TModel → concrete model class name,
    // and static/$this/self → Builder<ConcreteModel>.
    let builder_self_type = format!("\\{ELOQUENT_BUILDER_FQN}<{}>", class.name);
    let mut subs = HashMap::new();
    for param in &builder_class.template_params {
        subs.insert(param.clone(), class.name.clone());
    }
    subs.insert("static".to_string(), builder_self_type.clone());
    subs.insert("$this".to_string(), builder_self_type.clone());
    subs.insert("self".to_string(), builder_self_type.clone());

    let mut methods = Vec::new();

    for method in &resolved_builder.methods {
        if method.visibility != Visibility::Public {
            continue;
        }
        // Skip magic methods (__construct, __call, etc.).
        if method.name.starts_with("__") {
            continue;
        }
        // Skip methods already present on the model (real methods,
        // scope methods, etc.).  The merge logic in
        // `merge_virtual_members` would also skip them, but filtering
        // here avoids unnecessary cloning and substitution work.
        if class
            .methods
            .iter()
            .any(|m| m.name == method.name && m.is_static)
        {
            continue;
        }

        let mut forwarded = method.clone();
        forwarded.is_static = true;

        // Apply template and self-type substitutions.
        if !subs.is_empty() {
            if let Some(ref mut ret) = forwarded.return_type {
                *ret = apply_substitution(ret, &subs);
            }
            if let Some(ref mut cond) = forwarded.conditional_return {
                apply_substitution_to_conditional(cond, &subs);
            }
            for param in &mut forwarded.parameters {
                if let Some(ref mut hint) = param.type_hint {
                    *hint = apply_substitution(hint, &subs);
                }
            }
        }

        // Replace Eloquent Collection with custom collection class.
        if let Some(ref coll) = class.custom_collection
            && let Some(ref mut ret) = forwarded.return_type
        {
            *ret = replace_eloquent_collection(ret, coll);
        }

        methods.push(forwarded);
    }

    methods
}

/// Inject scope methods from a concrete model onto a resolved Builder.
///
/// When a type resolves to `Builder<User>`, the generic substitution
/// replaces `TModel` with `User` but does not add `User`'s scope
/// methods.  This function loads the concrete model, scans for scope
/// methods, and returns them as **instance** methods on the Builder so
/// that `$query->active()` and `Brand::where(...)->isActive()` both
/// resolve.
///
/// Return types are mapped so that `static` (from the default scope
/// return type `Builder<static>`) becomes `Builder<ConcreteModel>`,
/// keeping the chain on the Builder rather than jumping to the model.
pub fn build_scope_methods_for_builder(
    model_name: &str,
    class_loader: &dyn Fn(&str) -> Option<ClassInfo>,
) -> Vec<MethodInfo> {
    let model_class = match class_loader(model_name) {
        Some(c) => c,
        None => return Vec::new(),
    };

    // Only synthesize scopes for actual Eloquent models.
    if !extends_eloquent_model(&model_class, class_loader) {
        return Vec::new();
    }

    // Fully resolve the model to pick up scope methods defined in
    // traits and parent classes, not just the model itself.
    let resolved_model = Backend::resolve_class_fully(&model_class, class_loader);

    // Build a substitution map so that `static`, `$this`, and `self`
    // in scope return types resolve to the concrete model name.
    // The default scope return type is `\...\Builder<static>` where
    // `static` means the model, so substituting `static` → `User`
    // produces `\...\Builder<User>`, keeping the chain on the builder.
    let mut subs = HashMap::new();
    subs.insert("static".to_string(), model_name.to_string());
    subs.insert("$this".to_string(), model_name.to_string());
    subs.insert("self".to_string(), model_name.to_string());

    let mut methods = Vec::new();

    for method in &resolved_model.methods {
        if !is_scope_method(method) {
            continue;
        }

        // Build an instance method (scopes are called as instance
        // methods on Builder, not static).
        let [instance_method, _static_method] = build_scope_methods(method);

        let mut m = instance_method;

        // Apply substitutions to the return type.
        if let Some(ref mut ret) = m.return_type {
            *ret = apply_substitution(ret, &subs);
        }

        methods.push(m);
    }

    methods
}

impl VirtualMemberProvider for LaravelModelProvider {
    /// Returns `true` if the class extends `Illuminate\Database\Eloquent\Model`.
    fn applies_to(
        &self,
        class: &ClassInfo,
        class_loader: &dyn Fn(&str) -> Option<ClassInfo>,
    ) -> bool {
        extends_eloquent_model(class, class_loader)
    }

    /// Scan the class's methods for Eloquent relationship return types,
    /// scope methods, Builder-as-static forwarded methods, `$casts`
    /// definitions, `$attributes` defaults, and `$fillable`/`$guarded`/
    /// `$hidden` column names.
    fn provide(
        &self,
        class: &ClassInfo,
        class_loader: &dyn Fn(&str) -> Option<ClassInfo>,
    ) -> VirtualMembers {
        let mut properties = Vec::new();
        let mut methods = Vec::new();

        // ── Cast properties ─────────────────────────────────────────
        for (column, cast_type) in &class.casts_definitions {
            let php_type = cast_type_to_php_type(cast_type, class_loader);
            properties.push(PropertyInfo {
                name: column.clone(),
                type_hint: Some(php_type),
                is_static: false,
                visibility: Visibility::Public,
                is_deprecated: false,
            });
        }

        // ── Attribute default properties (fallback) ─────────────────
        // Only add properties for columns not already covered by $casts.
        for (column, php_type) in &class.attributes_definitions {
            if properties.iter().any(|p| p.name == *column) {
                continue;
            }
            properties.push(PropertyInfo {
                name: column.clone(),
                type_hint: Some(php_type.clone()),
                is_static: false,
                visibility: Visibility::Public,
                is_deprecated: false,
            });
        }

        // ── Column name properties (last-resort fallback) ───────────
        // $fillable, $guarded, and $hidden provide column names without
        // type information.  Only add for columns not already covered.
        for column in &class.column_names {
            if properties.iter().any(|p| p.name == *column) {
                continue;
            }
            properties.push(PropertyInfo {
                name: column.clone(),
                type_hint: Some("mixed".to_string()),
                is_static: false,
                visibility: Visibility::Public,
                is_deprecated: false,
            });
        }

        for method in &class.methods {
            // ── Scope methods ───────────────────────────────────────
            if is_scope_method(method) {
                let [instance_method, static_method] = build_scope_methods(method);
                methods.push(instance_method);
                methods.push(static_method);
                continue;
            }

            // ── Legacy accessors (getXAttribute) ────────────────────
            if is_legacy_accessor(method) {
                let prop_name = legacy_accessor_property_name(&method.name);
                properties.push(PropertyInfo {
                    name: prop_name,
                    type_hint: method.return_type.clone(),
                    is_static: false,
                    visibility: Visibility::Public,
                    is_deprecated: method.is_deprecated,
                });
                continue;
            }

            // ── Modern accessors (Laravel 9+ Attribute casts) ───────
            if is_modern_accessor(method) {
                let prop_name = camel_to_snake(&method.name);
                properties.push(PropertyInfo {
                    name: prop_name,
                    type_hint: Some("mixed".to_string()),
                    is_static: false,
                    visibility: Visibility::Public,
                    is_deprecated: method.is_deprecated,
                });
                continue;
            }

            // ── Relationship properties ─────────────────────────────
            let return_type = match method.return_type.as_deref() {
                Some(rt) => rt,
                None => continue,
            };

            let kind = match classify_relationship(return_type) {
                Some(k) => k,
                None => continue,
            };

            let related_type = extract_related_type(return_type);

            // For collection relationships, use the *related* model's
            // custom_collection, not the owning model's.  For example,
            // if Product has `#[CollectedBy(ProductCollection)]` and
            // Review has `#[CollectedBy(ReviewCollection)]`, then
            // `Product::reviews()` returning `HasMany<Review, $this>`
            // should produce `ReviewCollection<Review>`, not
            // `ProductCollection<Review>`.
            let custom_collection = if kind == RelationshipKind::Collection {
                related_type
                    .as_deref()
                    .and_then(|rt| {
                        let clean = rt.strip_prefix('\\').unwrap_or(rt);
                        class_loader(clean)
                    })
                    .and_then(|related_class| related_class.custom_collection)
            } else {
                None
            };

            let type_hint =
                build_property_type(kind, related_type.as_deref(), custom_collection.as_deref());

            if type_hint.is_some() {
                properties.push(PropertyInfo {
                    name: method.name.clone(),
                    type_hint,
                    is_static: false,
                    visibility: Visibility::Public,
                    is_deprecated: false,
                });
            }
        }

        // ── Builder-as-static forwarding ────────────────────────────
        let forwarded = build_builder_forwarded_methods(class, class_loader);
        methods.extend(forwarded);

        VirtualMembers {
            methods,
            properties,
            constants: Vec::new(),
        }
    }
}

impl VirtualMemberProvider for LaravelFactoryProvider {
    /// Returns `true` if the class extends
    /// `Illuminate\Database\Eloquent\Factories\Factory` and does not
    /// already have `@extends Factory<Model>` generics.
    fn applies_to(
        &self,
        class: &ClassInfo,
        class_loader: &dyn Fn(&str) -> Option<ClassInfo>,
    ) -> bool {
        !is_eloquent_factory(&class.name)
            && !has_factory_extends_generic(class)
            && extends_eloquent_factory(class, class_loader)
    }

    /// Synthesize `create()` and `make()` methods that return the model
    /// type derived from the naming convention.
    fn provide(
        &self,
        class: &ClassInfo,
        class_loader: &dyn Fn(&str) -> Option<ClassInfo>,
    ) -> VirtualMembers {
        let methods = build_factory_model_methods(class, class_loader);
        VirtualMembers {
            methods,
            properties: Vec::new(),
            constants: Vec::new(),
        }
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ClassLikeKind, MethodInfo, ParameterInfo};
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
            file_namespace: None,
            custom_collection: None,
            casts_definitions: Vec::new(),
            attributes_definitions: Vec::new(),
            column_names: Vec::new(),
        }
    }

    /// Helper: create a `MethodInfo` with a given return type.
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

    /// Helper: create a `MethodInfo` with parameters.
    fn make_method_with_params(
        name: &str,
        return_type: Option<&str>,
        params: Vec<ParameterInfo>,
    ) -> MethodInfo {
        MethodInfo {
            name: name.to_string(),
            parameters: params,
            return_type: return_type.map(|s| s.to_string()),
            is_static: false,
            visibility: Visibility::Public,
            conditional_return: None,
            is_deprecated: false,
            template_params: Vec::new(),
            template_bindings: Vec::new(),
        }
    }

    /// Helper: create a `ParameterInfo`.
    fn make_param(name: &str, type_hint: Option<&str>, is_required: bool) -> ParameterInfo {
        ParameterInfo {
            name: name.to_string(),
            is_required,
            type_hint: type_hint.map(|s| s.to_string()),
            is_variadic: false,
            is_reference: false,
        }
    }

    fn no_loader(_name: &str) -> Option<ClassInfo> {
        None
    }

    // ── is_eloquent_model ───────────────────────────────────────────────

    #[test]
    fn recognises_fqn() {
        assert!(is_eloquent_model("Illuminate\\Database\\Eloquent\\Model"));
    }

    #[test]
    fn recognises_fqn_with_leading_backslash() {
        assert!(is_eloquent_model("\\Illuminate\\Database\\Eloquent\\Model"));
    }

    #[test]
    fn rejects_unrelated_class() {
        assert!(!is_eloquent_model("App\\Models\\User"));
    }

    // ── extends_eloquent_model ──────────────────────────────────────────

    #[test]
    fn direct_child_of_model() {
        let mut user = make_class("App\\Models\\User");
        user.parent_class = Some("Illuminate\\Database\\Eloquent\\Model".to_string());

        let model = make_class("Illuminate\\Database\\Eloquent\\Model");
        let loader = |name: &str| -> Option<ClassInfo> {
            if name == "Illuminate\\Database\\Eloquent\\Model" {
                Some(model.clone())
            } else {
                None
            }
        };

        assert!(extends_eloquent_model(&user, &loader));
    }

    #[test]
    fn indirect_child_of_model() {
        let mut user = make_class("App\\Models\\User");
        user.parent_class = Some("App\\Models\\BaseModel".to_string());

        let mut base_model = make_class("App\\Models\\BaseModel");
        base_model.parent_class = Some("Illuminate\\Database\\Eloquent\\Model".to_string());

        let model = make_class("Illuminate\\Database\\Eloquent\\Model");

        let loader = |name: &str| -> Option<ClassInfo> {
            match name {
                "App\\Models\\BaseModel" => Some(base_model.clone()),
                "Illuminate\\Database\\Eloquent\\Model" => Some(model.clone()),
                _ => None,
            }
        };

        assert!(extends_eloquent_model(&user, &loader));
    }

    #[test]
    fn not_a_model() {
        let service = make_class("App\\Services\\UserService");
        assert!(!extends_eloquent_model(&service, &no_loader));
    }

    // ── classify_relationship ───────────────────────────────────────────

    #[test]
    fn classify_has_one() {
        assert_eq!(
            classify_relationship("HasOne<Profile, $this>"),
            Some(RelationshipKind::Singular)
        );
    }

    #[test]
    fn classify_has_many() {
        assert_eq!(
            classify_relationship("HasMany<Post, $this>"),
            Some(RelationshipKind::Collection)
        );
    }

    #[test]
    fn classify_belongs_to() {
        assert_eq!(
            classify_relationship("BelongsTo<User, $this>"),
            Some(RelationshipKind::Singular)
        );
    }

    #[test]
    fn classify_belongs_to_many() {
        assert_eq!(
            classify_relationship("BelongsToMany<Role, $this>"),
            Some(RelationshipKind::Collection)
        );
    }

    #[test]
    fn classify_morph_one() {
        assert_eq!(
            classify_relationship("MorphOne<Image, $this>"),
            Some(RelationshipKind::Singular)
        );
    }

    #[test]
    fn classify_morph_many() {
        assert_eq!(
            classify_relationship("MorphMany<Comment, $this>"),
            Some(RelationshipKind::Collection)
        );
    }

    #[test]
    fn classify_morph_to() {
        assert_eq!(
            classify_relationship("MorphTo"),
            Some(RelationshipKind::MorphTo)
        );
    }

    #[test]
    fn classify_morph_to_many() {
        assert_eq!(
            classify_relationship("MorphToMany<Tag, $this>"),
            Some(RelationshipKind::Collection)
        );
    }

    #[test]
    fn classify_has_many_through() {
        assert_eq!(
            classify_relationship("HasManyThrough<Post, Country>"),
            Some(RelationshipKind::Collection)
        );
    }

    #[test]
    fn classify_fqn_relationship() {
        assert_eq!(
            classify_relationship(
                "\\Illuminate\\Database\\Eloquent\\Relations\\HasMany<Post, $this>"
            ),
            Some(RelationshipKind::Collection)
        );
    }

    #[test]
    fn classify_non_relationship() {
        assert_eq!(classify_relationship("string"), None);
        assert_eq!(classify_relationship("Collection<User>"), None);
    }

    #[test]
    fn classify_bare_name_without_generics() {
        assert_eq!(
            classify_relationship("HasMany"),
            Some(RelationshipKind::Collection)
        );
    }

    // ── extract_related_type ────────────────────────────────────────────

    #[test]
    fn extracts_first_generic_arg() {
        assert_eq!(
            extract_related_type("HasMany<Post, $this>"),
            Some("Post".to_string())
        );
    }

    #[test]
    fn extracts_fqn_related_type() {
        assert_eq!(
            extract_related_type("HasOne<\\App\\Models\\Profile, $this>"),
            Some("\\App\\Models\\Profile".to_string())
        );
    }

    #[test]
    fn returns_none_without_generics() {
        assert_eq!(extract_related_type("HasMany"), None);
    }

    // ── build_property_type ─────────────────────────────────────────────

    #[test]
    fn singular_with_related() {
        assert_eq!(
            build_property_type(RelationshipKind::Singular, Some("App\\Models\\Post"), None),
            Some("App\\Models\\Post".to_string())
        );
    }

    #[test]
    fn singular_without_related() {
        assert_eq!(
            build_property_type(RelationshipKind::Singular, None, None),
            None
        );
    }

    #[test]
    fn collection_with_related() {
        assert_eq!(
            build_property_type(
                RelationshipKind::Collection,
                Some("App\\Models\\Post"),
                None
            ),
            Some("\\Illuminate\\Database\\Eloquent\\Collection<App\\Models\\Post>".to_string())
        );
    }

    #[test]
    fn collection_without_related_uses_model() {
        assert_eq!(
            build_property_type(RelationshipKind::Collection, None, None),
            Some(
                "\\Illuminate\\Database\\Eloquent\\Collection<\\Illuminate\\Database\\Eloquent\\Model>"
                    .to_string()
            )
        );
    }

    #[test]
    fn morph_to_always_returns_model() {
        assert_eq!(
            build_property_type(RelationshipKind::MorphTo, Some("App\\Models\\Foo"), None),
            Some("\\Illuminate\\Database\\Eloquent\\Model".to_string())
        );
    }

    #[test]
    fn collection_with_custom_collection() {
        assert_eq!(
            build_property_type(
                RelationshipKind::Collection,
                Some("App\\Models\\Post"),
                Some("App\\Collections\\PostCollection")
            ),
            Some("\\App\\Collections\\PostCollection<App\\Models\\Post>".to_string())
        );
    }

    #[test]
    fn collection_custom_collection_with_leading_backslash() {
        assert_eq!(
            build_property_type(
                RelationshipKind::Collection,
                Some("App\\Models\\Post"),
                Some("\\App\\Collections\\PostCollection")
            ),
            Some("\\App\\Collections\\PostCollection<App\\Models\\Post>".to_string())
        );
    }

    #[test]
    fn singular_ignores_custom_collection() {
        assert_eq!(
            build_property_type(
                RelationshipKind::Singular,
                Some("App\\Models\\Post"),
                Some("App\\Collections\\PostCollection")
            ),
            Some("App\\Models\\Post".to_string())
        );
    }

    #[test]
    fn morph_to_ignores_custom_collection() {
        assert_eq!(
            build_property_type(
                RelationshipKind::MorphTo,
                Some("App\\Models\\Foo"),
                Some("App\\Collections\\FooCollection")
            ),
            Some("\\Illuminate\\Database\\Eloquent\\Model".to_string())
        );
    }

    #[test]
    fn replace_eloquent_collection_in_return_type() {
        let result = replace_eloquent_collection(
            "\\Illuminate\\Database\\Eloquent\\Collection<int, App\\Models\\User>",
            "App\\Collections\\UserCollection",
        );
        assert_eq!(
            result,
            "\\App\\Collections\\UserCollection<int, App\\Models\\User>"
        );
    }

    #[test]
    fn replace_eloquent_collection_preserves_other_types() {
        let result = replace_eloquent_collection(
            "\\Illuminate\\Support\\Collection<int, string>",
            "App\\Collections\\UserCollection",
        );
        assert_eq!(result, "\\Illuminate\\Support\\Collection<int, string>");
    }

    #[test]
    fn replace_eloquent_collection_in_union() {
        let result = replace_eloquent_collection(
            "\\Illuminate\\Database\\Eloquent\\Collection<int, App\\Models\\User>|null",
            "App\\Collections\\UserCollection",
        );
        assert_eq!(
            result,
            "\\App\\Collections\\UserCollection<int, App\\Models\\User>|null"
        );
    }

    // ── applies_to ──────────────────────────────────────────────────────

    #[test]
    fn applies_to_model_subclass() {
        let provider = LaravelModelProvider;
        let mut user = make_class("App\\Models\\User");
        user.parent_class = Some("Illuminate\\Database\\Eloquent\\Model".to_string());

        let model = make_class("Illuminate\\Database\\Eloquent\\Model");
        let loader = |name: &str| -> Option<ClassInfo> {
            if name == "Illuminate\\Database\\Eloquent\\Model" {
                Some(model.clone())
            } else {
                None
            }
        };

        assert!(provider.applies_to(&user, &loader));
    }

    #[test]
    fn does_not_apply_to_non_model() {
        let provider = LaravelModelProvider;
        let service = make_class("App\\Services\\UserService");
        assert!(!provider.applies_to(&service, &no_loader));
    }

    // ── provide: relationship properties ────────────────────────────────

    #[test]
    fn synthesizes_has_many_property() {
        let provider = LaravelModelProvider;
        let mut user = make_class("App\\Models\\User");
        user.parent_class = Some("Illuminate\\Database\\Eloquent\\Model".to_string());
        user.methods
            .push(make_method("posts", Some("HasMany<Post, $this>")));

        let result = provider.provide(&user, &no_loader);
        assert_eq!(result.properties.len(), 1);
        assert_eq!(result.properties[0].name, "posts");
        assert_eq!(
            result.properties[0].type_hint.as_deref(),
            Some("\\Illuminate\\Database\\Eloquent\\Collection<Post>")
        );
        assert_eq!(result.properties[0].visibility, Visibility::Public);
        assert!(!result.properties[0].is_static);
    }

    #[test]
    fn synthesizes_has_one_property() {
        let provider = LaravelModelProvider;
        let mut user = make_class("App\\Models\\User");
        user.parent_class = Some("Illuminate\\Database\\Eloquent\\Model".to_string());
        user.methods
            .push(make_method("profile", Some("HasOne<Profile, $this>")));

        let result = provider.provide(&user, &no_loader);
        assert_eq!(result.properties.len(), 1);
        assert_eq!(result.properties[0].name, "profile");
        assert_eq!(result.properties[0].type_hint.as_deref(), Some("Profile"));
    }

    #[test]
    fn synthesizes_belongs_to_property() {
        let provider = LaravelModelProvider;
        let mut post = make_class("App\\Models\\Post");
        post.parent_class = Some("Illuminate\\Database\\Eloquent\\Model".to_string());
        post.methods
            .push(make_method("author", Some("BelongsTo<User, $this>")));

        let result = provider.provide(&post, &no_loader);
        assert_eq!(result.properties.len(), 1);
        assert_eq!(result.properties[0].name, "author");
        assert_eq!(result.properties[0].type_hint.as_deref(), Some("User"));
    }

    #[test]
    fn synthesizes_morph_to_property() {
        let provider = LaravelModelProvider;
        let mut comment = make_class("App\\Models\\Comment");
        comment.parent_class = Some("Illuminate\\Database\\Eloquent\\Model".to_string());
        comment
            .methods
            .push(make_method("commentable", Some("MorphTo")));

        let result = provider.provide(&comment, &no_loader);
        assert_eq!(result.properties.len(), 1);
        assert_eq!(result.properties[0].name, "commentable");
        assert_eq!(
            result.properties[0].type_hint.as_deref(),
            Some("\\Illuminate\\Database\\Eloquent\\Model")
        );
    }

    #[test]
    fn synthesizes_belongs_to_many_property() {
        let provider = LaravelModelProvider;
        let mut user = make_class("App\\Models\\User");
        user.parent_class = Some("Illuminate\\Database\\Eloquent\\Model".to_string());
        user.methods
            .push(make_method("roles", Some("BelongsToMany<Role, $this>")));

        let result = provider.provide(&user, &no_loader);
        assert_eq!(result.properties.len(), 1);
        assert_eq!(result.properties[0].name, "roles");
        assert_eq!(
            result.properties[0].type_hint.as_deref(),
            Some("\\Illuminate\\Database\\Eloquent\\Collection<Role>")
        );
    }

    #[test]
    fn synthesizes_multiple_relationship_properties() {
        let provider = LaravelModelProvider;
        let mut user = make_class("App\\Models\\User");
        user.parent_class = Some("Illuminate\\Database\\Eloquent\\Model".to_string());
        user.methods
            .push(make_method("posts", Some("HasMany<Post, $this>")));
        user.methods
            .push(make_method("profile", Some("HasOne<Profile, $this>")));
        user.methods
            .push(make_method("roles", Some("BelongsToMany<Role, $this>")));

        let result = provider.provide(&user, &no_loader);
        assert_eq!(result.properties.len(), 3);

        let names: Vec<&str> = result.properties.iter().map(|p| p.name.as_str()).collect();
        assert!(names.contains(&"posts"));
        assert!(names.contains(&"profile"));
        assert!(names.contains(&"roles"));
    }

    #[test]
    fn skips_non_relationship_methods() {
        let provider = LaravelModelProvider;
        let mut user = make_class("App\\Models\\User");
        user.parent_class = Some("Illuminate\\Database\\Eloquent\\Model".to_string());
        user.methods
            .push(make_method("getFullName", Some("string")));
        user.methods.push(make_method("save", Some("bool")));
        user.methods.push(make_method("toArray", Some("array")));

        let result = provider.provide(&user, &no_loader);
        assert!(result.properties.is_empty());
    }

    #[test]
    fn skips_methods_without_return_type() {
        let provider = LaravelModelProvider;
        let mut user = make_class("App\\Models\\User");
        user.parent_class = Some("Illuminate\\Database\\Eloquent\\Model".to_string());
        user.methods.push(make_method("posts", None));

        let result = provider.provide(&user, &no_loader);
        assert!(result.properties.is_empty());
    }

    #[test]
    fn handles_fqn_relationship_return_types() {
        let provider = LaravelModelProvider;
        let mut user = make_class("App\\Models\\User");
        user.parent_class = Some("Illuminate\\Database\\Eloquent\\Model".to_string());
        user.methods.push(make_method(
            "posts",
            Some("\\Illuminate\\Database\\Eloquent\\Relations\\HasMany<Post, $this>"),
        ));

        let result = provider.provide(&user, &no_loader);
        assert_eq!(result.properties.len(), 1);
        assert_eq!(result.properties[0].name, "posts");
        assert_eq!(
            result.properties[0].type_hint.as_deref(),
            Some("\\Illuminate\\Database\\Eloquent\\Collection<Post>")
        );
    }

    #[test]
    fn relationship_without_generics_and_singular_produces_nothing() {
        // A singular relationship without generics has no TRelated,
        // so we cannot determine the property type.
        let provider = LaravelModelProvider;
        let mut user = make_class("App\\Models\\User");
        user.parent_class = Some("Illuminate\\Database\\Eloquent\\Model".to_string());
        user.methods.push(make_method("profile", Some("HasOne")));

        let result = provider.provide(&user, &no_loader);
        assert!(
            result.properties.is_empty(),
            "Singular relationship without generics should not produce a property"
        );
    }

    #[test]
    fn collection_relationship_without_generics_uses_model_fallback() {
        // A collection relationship without generics defaults to
        // Collection<Model>.
        let provider = LaravelModelProvider;
        let mut user = make_class("App\\Models\\User");
        user.parent_class = Some("Illuminate\\Database\\Eloquent\\Model".to_string());
        user.methods.push(make_method("posts", Some("HasMany")));

        let result = provider.provide(&user, &no_loader);
        assert_eq!(result.properties.len(), 1);
        assert_eq!(
            result.properties[0].type_hint.as_deref(),
            Some(
                "\\Illuminate\\Database\\Eloquent\\Collection<\\Illuminate\\Database\\Eloquent\\Model>"
            )
        );
    }

    #[test]
    fn relationships_produce_no_virtual_methods_or_constants() {
        let provider = LaravelModelProvider;
        let mut user = make_class("App\\Models\\User");
        user.parent_class = Some("Illuminate\\Database\\Eloquent\\Model".to_string());
        user.methods
            .push(make_method("posts", Some("HasMany<Post, $this>")));

        let result = provider.provide(&user, &no_loader);
        assert!(
            result.methods.is_empty(),
            "Relationship methods should not produce virtual methods"
        );
        assert!(result.constants.is_empty());
    }

    #[test]
    fn provides_fqn_related_type_in_collection() {
        let provider = LaravelModelProvider;
        let mut user = make_class("App\\Models\\User");
        user.parent_class = Some("Illuminate\\Database\\Eloquent\\Model".to_string());
        user.methods.push(make_method(
            "posts",
            Some("HasMany<\\App\\Models\\Post, $this>"),
        ));

        let result = provider.provide(&user, &no_loader);
        assert_eq!(result.properties.len(), 1);
        assert_eq!(
            result.properties[0].type_hint.as_deref(),
            Some("\\Illuminate\\Database\\Eloquent\\Collection<\\App\\Models\\Post>")
        );
    }

    #[test]
    fn provides_fqn_related_type_singular() {
        let provider = LaravelModelProvider;
        let mut user = make_class("App\\Models\\User");
        user.parent_class = Some("Illuminate\\Database\\Eloquent\\Model".to_string());
        user.methods.push(make_method(
            "profile",
            Some("HasOne<\\App\\Models\\Profile, $this>"),
        ));

        let result = provider.provide(&user, &no_loader);
        assert_eq!(result.properties.len(), 1);
        assert_eq!(
            result.properties[0].type_hint.as_deref(),
            Some("\\App\\Models\\Profile")
        );
    }

    // ── extract_short_name ──────────────────────────────────────────────

    #[test]
    fn short_name_from_fqn() {
        assert_eq!(
            extract_short_name("\\Illuminate\\Database\\Eloquent\\Relations\\HasMany"),
            "HasMany"
        );
    }

    #[test]
    fn short_name_already_short() {
        assert_eq!(extract_short_name("HasMany"), "HasMany");
    }

    #[test]
    fn short_name_no_backslash_prefix() {
        assert_eq!(
            extract_short_name("Illuminate\\Database\\Eloquent\\Relations\\HasOne"),
            "HasOne"
        );
    }

    // ── is_scope_method ─────────────────────────────────────────────────

    #[test]
    fn scope_method_detected() {
        let method = make_method("scopeActive", Some("void"));
        assert!(is_scope_method(&method));
    }

    #[test]
    fn scope_method_multi_word() {
        let method = make_method("scopeRecentlyVerified", Some("void"));
        assert!(is_scope_method(&method));
    }

    #[test]
    fn not_a_scope_bare_scope_name() {
        // "scope" alone with no suffix is not a scope
        let method = make_method("scope", Some("void"));
        assert!(!is_scope_method(&method));
    }

    #[test]
    fn not_a_scope_different_prefix() {
        let method = make_method("getActive", Some("void"));
        assert!(!is_scope_method(&method));
    }

    #[test]
    fn not_a_scope_lowercase_prefix() {
        // Must be exactly "scope" not "Scope"
        let method = make_method("ScopeActive", Some("void"));
        assert!(!is_scope_method(&method));
    }

    // ── scope_name ──────────────────────────────────────────────────────

    #[test]
    fn scope_name_simple() {
        assert_eq!(scope_name("scopeActive"), "active");
    }

    #[test]
    fn scope_name_multi_word() {
        assert_eq!(scope_name("scopeRecentlyVerified"), "recentlyVerified");
    }

    #[test]
    fn scope_name_single_char() {
        assert_eq!(scope_name("scopeA"), "a");
    }

    #[test]
    fn scope_name_already_lowercase() {
        assert_eq!(scope_name("scopeactive"), "active");
    }

    // ── scope_return_type ───────────────────────────────────────────────

    #[test]
    fn scope_return_type_void_defaults() {
        let method = make_method("scopeActive", Some("void"));
        assert_eq!(
            scope_return_type(&method),
            "\\Illuminate\\Database\\Eloquent\\Builder<static>"
        );
    }

    #[test]
    fn scope_return_type_none_defaults() {
        let method = make_method("scopeActive", None);
        assert_eq!(
            scope_return_type(&method),
            "\\Illuminate\\Database\\Eloquent\\Builder<static>"
        );
    }

    #[test]
    fn scope_return_type_explicit() {
        let method = make_method(
            "scopeActive",
            Some("\\Illuminate\\Database\\Eloquent\\Builder<static>"),
        );
        assert_eq!(
            scope_return_type(&method),
            "\\Illuminate\\Database\\Eloquent\\Builder<static>"
        );
    }

    #[test]
    fn scope_return_type_custom() {
        let method = make_method("scopeActive", Some("\\App\\Builders\\UserBuilder"));
        assert_eq!(scope_return_type(&method), "\\App\\Builders\\UserBuilder");
    }

    // ── build_scope_methods ─────────────────────────────────────────────

    #[test]
    fn build_scope_methods_strips_query_param() {
        let method = make_method_with_params(
            "scopeActive",
            Some("void"),
            vec![make_param(
                "$query",
                Some("\\Illuminate\\Database\\Eloquent\\Builder"),
                true,
            )],
        );

        let [instance, static_m] = build_scope_methods(&method);
        assert!(instance.parameters.is_empty());
        assert!(static_m.parameters.is_empty());
    }

    #[test]
    fn build_scope_methods_preserves_extra_params() {
        let method = make_method_with_params(
            "scopeOfType",
            Some("void"),
            vec![
                make_param(
                    "$query",
                    Some("\\Illuminate\\Database\\Eloquent\\Builder"),
                    true,
                ),
                make_param("$type", Some("string"), true),
                make_param("$strict", Some("bool"), false),
            ],
        );

        let [instance, static_m] = build_scope_methods(&method);
        assert_eq!(instance.parameters.len(), 2);
        assert_eq!(instance.parameters[0].name, "$type");
        assert!(instance.parameters[0].is_required);
        assert_eq!(instance.parameters[1].name, "$strict");
        assert!(!instance.parameters[1].is_required);

        assert_eq!(static_m.parameters.len(), 2);
        assert_eq!(static_m.parameters[0].name, "$type");
        assert_eq!(static_m.parameters[1].name, "$strict");
    }

    #[test]
    fn build_scope_methods_creates_instance_and_static() {
        let method = make_method("scopeActive", Some("void"));
        let [instance, static_m] = build_scope_methods(&method);

        assert_eq!(instance.name, "active");
        assert!(!instance.is_static);
        assert_eq!(instance.visibility, Visibility::Public);

        assert_eq!(static_m.name, "active");
        assert!(static_m.is_static);
        assert_eq!(static_m.visibility, Visibility::Public);
    }

    #[test]
    fn build_scope_methods_default_return_type() {
        let method = make_method("scopeActive", None);
        let [instance, static_m] = build_scope_methods(&method);

        assert_eq!(
            instance.return_type.as_deref(),
            Some("\\Illuminate\\Database\\Eloquent\\Builder<static>")
        );
        assert_eq!(
            static_m.return_type.as_deref(),
            Some("\\Illuminate\\Database\\Eloquent\\Builder<static>")
        );
    }

    #[test]
    fn build_scope_methods_void_return_type() {
        let method = make_method("scopeActive", Some("void"));
        let [instance, _] = build_scope_methods(&method);

        assert_eq!(
            instance.return_type.as_deref(),
            Some("\\Illuminate\\Database\\Eloquent\\Builder<static>")
        );
    }

    #[test]
    fn build_scope_methods_with_no_params() {
        // Scope method without any parameters (unusual but valid)
        let method = make_method("scopeActive", Some("void"));
        let [instance, static_m] = build_scope_methods(&method);

        assert!(instance.parameters.is_empty());
        assert!(static_m.parameters.is_empty());
    }

    #[test]
    fn build_scope_methods_preserves_deprecated() {
        let mut method = make_method("scopeOld", Some("void"));
        method.is_deprecated = true;

        let [instance, static_m] = build_scope_methods(&method);
        assert!(instance.is_deprecated);
        assert!(static_m.is_deprecated);
    }

    // ── provide: scope methods (integration) ────────────────────────────

    #[test]
    fn synthesizes_scope_as_both_static_and_instance() {
        let provider = LaravelModelProvider;
        let mut user = make_class("App\\Models\\User");
        user.parent_class = Some("Illuminate\\Database\\Eloquent\\Model".to_string());
        user.methods.push(make_method_with_params(
            "scopeActive",
            Some("void"),
            vec![make_param(
                "$query",
                Some("\\Illuminate\\Database\\Eloquent\\Builder"),
                true,
            )],
        ));

        let result = provider.provide(&user, &no_loader);
        assert_eq!(result.methods.len(), 2, "Expected both static and instance");

        let instance = result.methods.iter().find(|m| !m.is_static).unwrap();
        assert_eq!(instance.name, "active");
        assert!(instance.parameters.is_empty());
        assert_eq!(
            instance.return_type.as_deref(),
            Some("\\Illuminate\\Database\\Eloquent\\Builder<static>")
        );

        let static_m = result.methods.iter().find(|m| m.is_static).unwrap();
        assert_eq!(static_m.name, "active");
        assert!(static_m.parameters.is_empty());
        assert_eq!(
            static_m.return_type.as_deref(),
            Some("\\Illuminate\\Database\\Eloquent\\Builder<static>")
        );
    }

    #[test]
    fn synthesizes_scope_with_extra_params() {
        let provider = LaravelModelProvider;
        let mut user = make_class("App\\Models\\User");
        user.parent_class = Some("Illuminate\\Database\\Eloquent\\Model".to_string());
        user.methods.push(make_method_with_params(
            "scopeOfType",
            Some("void"),
            vec![
                make_param(
                    "$query",
                    Some("\\Illuminate\\Database\\Eloquent\\Builder"),
                    true,
                ),
                make_param("$type", Some("string"), true),
            ],
        ));

        let result = provider.provide(&user, &no_loader);
        assert_eq!(result.methods.len(), 2);

        let instance = result.methods.iter().find(|m| !m.is_static).unwrap();
        assert_eq!(instance.name, "ofType");
        assert_eq!(instance.parameters.len(), 1);
        assert_eq!(instance.parameters[0].name, "$type");
        assert_eq!(instance.parameters[0].type_hint.as_deref(), Some("string"));
    }

    #[test]
    fn synthesizes_multiple_scopes() {
        let provider = LaravelModelProvider;
        let mut user = make_class("App\\Models\\User");
        user.parent_class = Some("Illuminate\\Database\\Eloquent\\Model".to_string());
        user.methods.push(make_method_with_params(
            "scopeActive",
            Some("void"),
            vec![make_param("$query", Some("Builder"), true)],
        ));
        user.methods.push(make_method_with_params(
            "scopeVerified",
            Some("void"),
            vec![make_param("$query", Some("Builder"), true)],
        ));

        let result = provider.provide(&user, &no_loader);
        // 2 scopes × 2 variants (static + instance) = 4
        assert_eq!(result.methods.len(), 4);

        let names: Vec<&str> = result
            .methods
            .iter()
            .filter(|m| !m.is_static)
            .map(|m| m.name.as_str())
            .collect();
        assert!(names.contains(&"active"));
        assert!(names.contains(&"verified"));
    }

    #[test]
    fn scope_and_relationship_coexist() {
        let provider = LaravelModelProvider;
        let mut user = make_class("App\\Models\\User");
        user.parent_class = Some("Illuminate\\Database\\Eloquent\\Model".to_string());
        user.methods
            .push(make_method("posts", Some("HasMany<Post, $this>")));
        user.methods.push(make_method_with_params(
            "scopeActive",
            Some("void"),
            vec![make_param("$query", Some("Builder"), true)],
        ));

        let result = provider.provide(&user, &no_loader);
        assert_eq!(result.properties.len(), 1, "One relationship property");
        assert_eq!(result.properties[0].name, "posts");
        assert_eq!(
            result.methods.len(),
            2,
            "Two scope methods (static + instance)"
        );
        let instance = result.methods.iter().find(|m| !m.is_static).unwrap();
        assert_eq!(instance.name, "active");
    }

    #[test]
    fn scope_method_not_treated_as_relationship() {
        // scopeActive's return type is "void", not a relationship type.
        // It should be treated as a scope, not produce a property.
        let provider = LaravelModelProvider;
        let mut user = make_class("App\\Models\\User");
        user.parent_class = Some("Illuminate\\Database\\Eloquent\\Model".to_string());
        user.methods.push(make_method_with_params(
            "scopeActive",
            Some("void"),
            vec![make_param("$query", Some("Builder"), true)],
        ));

        let result = provider.provide(&user, &no_loader);
        assert!(
            result.properties.is_empty(),
            "Scope methods should not produce relationship properties"
        );
        assert_eq!(result.methods.len(), 2);
    }

    #[test]
    fn scope_with_custom_return_type() {
        let provider = LaravelModelProvider;
        let mut user = make_class("App\\Models\\User");
        user.parent_class = Some("Illuminate\\Database\\Eloquent\\Model".to_string());
        user.methods.push(make_method_with_params(
            "scopeActive",
            Some("\\App\\Builders\\UserBuilder"),
            vec![make_param("$query", Some("Builder"), true)],
        ));

        let result = provider.provide(&user, &no_loader);
        let instance = result.methods.iter().find(|m| !m.is_static).unwrap();
        assert_eq!(
            instance.return_type.as_deref(),
            Some("\\App\\Builders\\UserBuilder")
        );
    }

    // ── Builder-as-static forwarding (unit tests) ───────────────────────

    /// Helper: create a minimal Builder class with template params and methods.
    fn make_builder(methods: Vec<MethodInfo>) -> ClassInfo {
        let mut builder = make_class(ELOQUENT_BUILDER_FQN);
        builder.template_params = vec!["TModel".to_string()];
        builder.methods = methods;
        builder
    }

    #[test]
    fn builder_forwarding_returns_empty_when_builder_not_found() {
        let class = make_class("App\\Models\\User");
        let result = build_builder_forwarded_methods(&class, &no_loader);
        assert!(result.is_empty());
    }

    #[test]
    fn builder_forwarding_converts_instance_to_static() {
        let mut builder = make_builder(vec![make_method("where", Some("static"))]);
        builder.methods[0].is_static = false;

        let user = make_class("App\\Models\\User");

        let loader = |name: &str| -> Option<ClassInfo> {
            if name == ELOQUENT_BUILDER_FQN {
                Some(builder.clone())
            } else {
                None
            }
        };

        let result = build_builder_forwarded_methods(&user, &loader);
        assert_eq!(result.len(), 1);
        assert!(result[0].is_static, "Forwarded method should be static");
        assert_eq!(result[0].name, "where");
    }

    #[test]
    fn builder_forwarding_maps_static_to_builder_self_type() {
        let builder = make_builder(vec![make_method("where", Some("static"))]);
        let user = make_class("App\\Models\\User");

        let loader = |name: &str| -> Option<ClassInfo> {
            if name == ELOQUENT_BUILDER_FQN {
                Some(builder.clone())
            } else {
                None
            }
        };

        let result = build_builder_forwarded_methods(&user, &loader);
        assert_eq!(result.len(), 1);
        assert_eq!(
            result[0].return_type.as_deref(),
            Some("\\Illuminate\\Database\\Eloquent\\Builder<App\\Models\\User>"),
            "static should map to Builder<ConcreteModel>"
        );
    }

    #[test]
    fn builder_forwarding_maps_this_to_builder_self_type() {
        let builder = make_builder(vec![make_method("orderBy", Some("$this"))]);
        let user = make_class("App\\Models\\User");

        let loader = |name: &str| -> Option<ClassInfo> {
            if name == ELOQUENT_BUILDER_FQN {
                Some(builder.clone())
            } else {
                None
            }
        };

        let result = build_builder_forwarded_methods(&user, &loader);
        assert_eq!(result.len(), 1);
        assert_eq!(
            result[0].return_type.as_deref(),
            Some("\\Illuminate\\Database\\Eloquent\\Builder<App\\Models\\User>"),
            "$this should map to Builder<ConcreteModel>"
        );
    }

    #[test]
    fn builder_forwarding_maps_self_to_builder_self_type() {
        let builder = make_builder(vec![make_method("limit", Some("self"))]);
        let user = make_class("App\\Models\\User");

        let loader = |name: &str| -> Option<ClassInfo> {
            if name == ELOQUENT_BUILDER_FQN {
                Some(builder.clone())
            } else {
                None
            }
        };

        let result = build_builder_forwarded_methods(&user, &loader);
        assert_eq!(result.len(), 1);
        assert_eq!(
            result[0].return_type.as_deref(),
            Some("\\Illuminate\\Database\\Eloquent\\Builder<App\\Models\\User>"),
            "self should map to Builder<ConcreteModel>"
        );
    }

    #[test]
    fn builder_forwarding_maps_tmodel_to_concrete_class() {
        let builder = make_builder(vec![make_method("first", Some("TModel|null"))]);
        let user = make_class("App\\Models\\User");

        let loader = |name: &str| -> Option<ClassInfo> {
            if name == ELOQUENT_BUILDER_FQN {
                Some(builder.clone())
            } else {
                None
            }
        };

        let result = build_builder_forwarded_methods(&user, &loader);
        assert_eq!(result.len(), 1);
        assert_eq!(
            result[0].return_type.as_deref(),
            Some("App\\Models\\User|null"),
            "TModel should map to the concrete model class"
        );
    }

    #[test]
    fn builder_forwarding_maps_generic_collection_return() {
        let builder = make_builder(vec![make_method(
            "get",
            Some("\\Illuminate\\Database\\Eloquent\\Collection<int, TModel>"),
        )]);
        let user = make_class("App\\Models\\User");

        let loader = |name: &str| -> Option<ClassInfo> {
            if name == ELOQUENT_BUILDER_FQN {
                Some(builder.clone())
            } else {
                None
            }
        };

        let result = build_builder_forwarded_methods(&user, &loader);
        assert_eq!(result.len(), 1);
        assert_eq!(
            result[0].return_type.as_deref(),
            Some("\\Illuminate\\Database\\Eloquent\\Collection<int, App\\Models\\User>"),
            "Collection<int, TModel> should become Collection<int, User>"
        );
    }

    #[test]
    fn builder_forwarding_maps_static_in_union() {
        let builder = make_builder(vec![make_method("whereNull", Some("static|null"))]);
        let user = make_class("App\\Models\\User");

        let loader = |name: &str| -> Option<ClassInfo> {
            if name == ELOQUENT_BUILDER_FQN {
                Some(builder.clone())
            } else {
                None
            }
        };

        let result = build_builder_forwarded_methods(&user, &loader);
        assert_eq!(result.len(), 1);
        assert_eq!(
            result[0].return_type.as_deref(),
            Some("\\Illuminate\\Database\\Eloquent\\Builder<App\\Models\\User>|null"),
            "static|null should become Builder<User>|null"
        );
    }

    #[test]
    fn builder_forwarding_skips_magic_methods() {
        let builder = make_builder(vec![
            make_method("where", Some("static")),
            make_method("__construct", None),
            make_method("__call", Some("mixed")),
        ]);
        let user = make_class("App\\Models\\User");

        let loader = |name: &str| -> Option<ClassInfo> {
            if name == ELOQUENT_BUILDER_FQN {
                Some(builder.clone())
            } else {
                None
            }
        };

        let result = build_builder_forwarded_methods(&user, &loader);
        assert_eq!(
            result.len(),
            1,
            "Only non-magic methods should be forwarded"
        );
        assert_eq!(result[0].name, "where");
    }

    #[test]
    fn builder_forwarding_skips_non_public_methods() {
        let mut builder = make_builder(vec![
            make_method("where", Some("static")),
            make_method("internalHelper", Some("void")),
        ]);
        builder.methods[1].visibility = Visibility::Protected;
        let user = make_class("App\\Models\\User");

        let loader = |name: &str| -> Option<ClassInfo> {
            if name == ELOQUENT_BUILDER_FQN {
                Some(builder.clone())
            } else {
                None
            }
        };

        let result = build_builder_forwarded_methods(&user, &loader);
        assert_eq!(result.len(), 1, "Only public methods should be forwarded");
        assert_eq!(result[0].name, "where");
    }

    #[test]
    fn builder_forwarding_skips_methods_already_on_model() {
        let builder = make_builder(vec![
            make_method("where", Some("static")),
            make_method("myMethod", Some("void")),
        ]);
        let mut user = make_class("App\\Models\\User");
        // The model has a static method named "myMethod" already.
        let mut existing = make_method("myMethod", Some("string"));
        existing.is_static = true;
        user.methods.push(existing);

        let loader = |name: &str| -> Option<ClassInfo> {
            if name == ELOQUENT_BUILDER_FQN {
                Some(builder.clone())
            } else {
                None
            }
        };

        let result = build_builder_forwarded_methods(&user, &loader);
        assert_eq!(
            result.len(),
            1,
            "Should skip 'myMethod' because the model already has it as static"
        );
        assert_eq!(result[0].name, "where");
    }

    #[test]
    fn builder_forwarding_does_not_skip_instance_method_with_same_name() {
        // If the model has an instance method named "where", the static
        // forwarded Builder method should still appear since they differ
        // in staticness.
        let builder = make_builder(vec![make_method("where", Some("static"))]);
        let mut user = make_class("App\\Models\\User");
        let mut existing = make_method("where", Some("string"));
        existing.is_static = false;
        user.methods.push(existing);

        let loader = |name: &str| -> Option<ClassInfo> {
            if name == ELOQUENT_BUILDER_FQN {
                Some(builder.clone())
            } else {
                None
            }
        };

        let result = build_builder_forwarded_methods(&user, &loader);
        assert_eq!(
            result.len(),
            1,
            "Static forwarded method should be added even when an instance method with the same name exists"
        );
        assert!(result[0].is_static);
    }

    #[test]
    fn builder_forwarding_maps_parameter_types() {
        let builder = make_builder(vec![make_method_with_params(
            "find",
            Some("TModel|null"),
            vec![make_param("$id", Some("TModel"), true)],
        )]);
        let user = make_class("App\\Models\\User");

        let loader = |name: &str| -> Option<ClassInfo> {
            if name == ELOQUENT_BUILDER_FQN {
                Some(builder.clone())
            } else {
                None
            }
        };

        let result = build_builder_forwarded_methods(&user, &loader);
        assert_eq!(result.len(), 1);
        assert_eq!(
            result[0].parameters[0].type_hint.as_deref(),
            Some("App\\Models\\User"),
            "Parameter TModel should map to the concrete model class"
        );
    }

    #[test]
    fn builder_forwarding_preserves_method_metadata() {
        let mut builder = make_builder(vec![make_method_with_params(
            "where",
            Some("static"),
            vec![
                make_param("$column", Some("string"), true),
                make_param("$value", Some("mixed"), false),
            ],
        )]);
        builder.methods[0].is_deprecated = true;

        let user = make_class("App\\Models\\User");

        let loader = |name: &str| -> Option<ClassInfo> {
            if name == ELOQUENT_BUILDER_FQN {
                Some(builder.clone())
            } else {
                None
            }
        };

        let result = build_builder_forwarded_methods(&user, &loader);
        assert_eq!(result.len(), 1);
        assert!(
            result[0].is_deprecated,
            "Deprecated flag should be preserved"
        );
        assert_eq!(result[0].parameters.len(), 2);
        assert_eq!(result[0].parameters[0].name, "$column");
        assert!(!result[0].parameters[1].is_required);
    }

    #[test]
    fn builder_forwarding_multiple_methods() {
        let builder = make_builder(vec![
            make_method("where", Some("static")),
            make_method("orderBy", Some("static")),
            make_method(
                "get",
                Some("\\Illuminate\\Database\\Eloquent\\Collection<int, TModel>"),
            ),
            make_method("first", Some("TModel|null")),
        ]);
        let user = make_class("App\\Models\\User");

        let loader = |name: &str| -> Option<ClassInfo> {
            if name == ELOQUENT_BUILDER_FQN {
                Some(builder.clone())
            } else {
                None
            }
        };

        let result = build_builder_forwarded_methods(&user, &loader);
        assert_eq!(result.len(), 4);
        let names: Vec<&str> = result.iter().map(|m| m.name.as_str()).collect();
        assert!(names.contains(&"where"));
        assert!(names.contains(&"orderBy"));
        assert!(names.contains(&"get"));
        assert!(names.contains(&"first"));
        assert!(result.iter().all(|m| m.is_static));
    }

    #[test]
    fn provide_includes_builder_forwarded_methods() {
        let provider = LaravelModelProvider;
        let mut user = make_class("App\\Models\\User");
        user.parent_class = Some("Illuminate\\Database\\Eloquent\\Model".to_string());

        let model = make_class("Illuminate\\Database\\Eloquent\\Model");
        let builder = make_builder(vec![
            make_method("where", Some("static")),
            make_method(
                "get",
                Some("\\Illuminate\\Database\\Eloquent\\Collection<int, TModel>"),
            ),
        ]);

        let loader = |name: &str| -> Option<ClassInfo> {
            if name == "Illuminate\\Database\\Eloquent\\Model" {
                Some(model.clone())
            } else if name == ELOQUENT_BUILDER_FQN {
                Some(builder.clone())
            } else {
                None
            }
        };

        let result = provider.provide(&user, &loader);

        let static_methods: Vec<&str> = result
            .methods
            .iter()
            .filter(|m| m.is_static)
            .map(|m| m.name.as_str())
            .collect();
        assert!(
            static_methods.contains(&"where"),
            "Builder's where() should be forwarded as static, got: {:?}",
            static_methods
        );
        assert!(
            static_methods.contains(&"get"),
            "Builder's get() should be forwarded as static, got: {:?}",
            static_methods
        );
    }

    #[test]
    fn provide_scope_beats_builder_method_with_same_name() {
        // If the model has a scopeWhere method AND Builder has a where
        // method, both produce static methods named "where". The scope's
        // version is added first, and merge_virtual_members would
        // deduplicate. But within the provider itself, the scope method
        // is added first, and build_builder_forwarded_methods skips
        // methods already on the class. However, scope methods are added
        // to the `methods` vec, not to the class itself, so the builder
        // dedup is based on class.methods (real methods + inherited).
        // The merge_virtual_members in mod.rs handles the final dedup.
        //
        // Here we just verify that both are produced (the dedup happens
        // at the merge layer).
        let provider = LaravelModelProvider;
        let mut user = make_class("App\\Models\\User");
        user.parent_class = Some("Illuminate\\Database\\Eloquent\\Model".to_string());
        user.methods.push(make_method_with_params(
            "scopeWhere",
            Some("void"),
            vec![make_param("$query", Some("Builder"), true)],
        ));

        let model = make_class("Illuminate\\Database\\Eloquent\\Model");
        let builder = make_builder(vec![make_method("where", Some("static"))]);

        let loader = |name: &str| -> Option<ClassInfo> {
            if name == "Illuminate\\Database\\Eloquent\\Model" {
                Some(model.clone())
            } else if name == ELOQUENT_BUILDER_FQN {
                Some(builder.clone())
            } else {
                None
            }
        };

        let result = provider.provide(&user, &loader);

        // Scope produces both static and instance "where".
        // Builder forwarding also produces a static "where".
        // merge_virtual_members will keep the first (scope) static one.
        let static_wheres: Vec<_> = result
            .methods
            .iter()
            .filter(|m| m.name == "where" && m.is_static)
            .collect();
        assert!(
            !static_wheres.is_empty(),
            "At least one static 'where' should exist from scope"
        );
        // The scope version has the default builder return type.
        assert_eq!(
            static_wheres[0].return_type.as_deref(),
            Some("\\Illuminate\\Database\\Eloquent\\Builder<static>"),
            "First static 'where' should be from the scope (added first)"
        );
    }

    #[test]
    fn builder_forwarding_with_no_return_type() {
        let builder = make_builder(vec![make_method("doSomething", None)]);
        let user = make_class("App\\Models\\User");

        let loader = |name: &str| -> Option<ClassInfo> {
            if name == ELOQUENT_BUILDER_FQN {
                Some(builder.clone())
            } else {
                None
            }
        };

        let result = build_builder_forwarded_methods(&user, &loader);
        assert_eq!(result.len(), 1);
        assert!(
            result[0].return_type.is_none(),
            "None return type should stay None"
        );
    }

    #[test]
    fn builder_forwarding_preserves_non_template_return_types() {
        let builder = make_builder(vec![
            make_method("toSql", Some("string")),
            make_method("exists", Some("bool")),
        ]);
        let user = make_class("App\\Models\\User");

        let loader = |name: &str| -> Option<ClassInfo> {
            if name == ELOQUENT_BUILDER_FQN {
                Some(builder.clone())
            } else {
                None
            }
        };

        let result = build_builder_forwarded_methods(&user, &loader);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].return_type.as_deref(), Some("string"));
        assert_eq!(result[1].return_type.as_deref(), Some("bool"));
    }

    // ── camel_to_snake ──────────────────────────────────────────────

    #[test]
    fn camel_to_snake_simple() {
        assert_eq!(camel_to_snake("FullName"), "full_name");
    }

    #[test]
    fn camel_to_snake_single_word() {
        assert_eq!(camel_to_snake("Name"), "name");
    }

    #[test]
    fn camel_to_snake_already_lower() {
        assert_eq!(camel_to_snake("name"), "name");
    }

    #[test]
    fn camel_to_snake_camel_case() {
        assert_eq!(camel_to_snake("firstName"), "first_name");
    }

    #[test]
    fn camel_to_snake_multiple_words() {
        assert_eq!(camel_to_snake("isAdminUser"), "is_admin_user");
    }

    #[test]
    fn camel_to_snake_with_digit() {
        assert_eq!(camel_to_snake("item2Name"), "item2_name");
    }

    #[test]
    fn camel_to_snake_acronym() {
        assert_eq!(camel_to_snake("URLName"), "url_name");
    }

    // ── is_legacy_accessor ──────────────────────────────────────────

    #[test]
    fn legacy_accessor_detected() {
        let method = make_method("getFullNameAttribute", Some("string"));
        assert!(is_legacy_accessor(&method));
    }

    #[test]
    fn legacy_accessor_single_word() {
        let method = make_method("getNameAttribute", Some("string"));
        assert!(is_legacy_accessor(&method));
    }

    #[test]
    fn legacy_accessor_get_attribute_itself_not_accessor() {
        // getAttribute() is a real Eloquent method, not an accessor.
        let method = make_method("getAttribute", Some("mixed"));
        assert!(!is_legacy_accessor(&method));
    }

    #[test]
    fn legacy_accessor_wrong_prefix() {
        let method = make_method("setFullNameAttribute", None);
        assert!(!is_legacy_accessor(&method));
    }

    #[test]
    fn legacy_accessor_no_attribute_suffix() {
        let method = make_method("getFullName", Some("string"));
        assert!(!is_legacy_accessor(&method));
    }

    #[test]
    fn legacy_accessor_lowercase_after_get() {
        // getfooAttribute — first char after "get" must be uppercase.
        let method = make_method("getfooAttribute", Some("string"));
        assert!(!is_legacy_accessor(&method));
    }

    // ── legacy_accessor_property_name ───────────────────────────────

    #[test]
    fn legacy_accessor_prop_name_simple() {
        assert_eq!(legacy_accessor_property_name("getNameAttribute"), "name");
    }

    #[test]
    fn legacy_accessor_prop_name_multi_word() {
        assert_eq!(
            legacy_accessor_property_name("getFullNameAttribute"),
            "full_name"
        );
    }

    #[test]
    fn legacy_accessor_prop_name_three_words() {
        assert_eq!(
            legacy_accessor_property_name("getFirstMiddleLastAttribute"),
            "first_middle_last"
        );
    }

    // ── is_modern_accessor ──────────────────────────────────────────

    #[test]
    fn modern_accessor_fqn() {
        let method = make_method(
            "fullName",
            Some("Illuminate\\Database\\Eloquent\\Casts\\Attribute"),
        );
        assert!(is_modern_accessor(&method));
    }

    #[test]
    fn modern_accessor_fqn_with_backslash() {
        let method = make_method(
            "fullName",
            Some("\\Illuminate\\Database\\Eloquent\\Casts\\Attribute"),
        );
        assert!(is_modern_accessor(&method));
    }

    #[test]
    fn modern_accessor_short_name() {
        let method = make_method("fullName", Some("Attribute"));
        assert!(is_modern_accessor(&method));
    }

    #[test]
    fn modern_accessor_with_generics() {
        let method = make_method(
            "fullName",
            Some("Illuminate\\Database\\Eloquent\\Casts\\Attribute<string, never>"),
        );
        assert!(is_modern_accessor(&method));
    }

    #[test]
    fn modern_accessor_not_matching_return_type() {
        let method = make_method("fullName", Some("string"));
        assert!(!is_modern_accessor(&method));
    }

    #[test]
    fn modern_accessor_no_return_type() {
        let method = make_method("fullName", None);
        assert!(!is_modern_accessor(&method));
    }

    // ── provide: accessor integration ───────────────────────────────

    #[test]
    fn synthesizes_legacy_accessor_property() {
        let provider = LaravelModelProvider;
        let mut user = make_class("App\\Models\\User");
        user.parent_class = Some("Illuminate\\Database\\Eloquent\\Model".to_string());
        user.methods
            .push(make_method("getFullNameAttribute", Some("string")));

        let model = make_class("Illuminate\\Database\\Eloquent\\Model");
        let loader = |name: &str| -> Option<ClassInfo> {
            if name == "Illuminate\\Database\\Eloquent\\Model" {
                Some(model.clone())
            } else {
                None
            }
        };

        let result = provider.provide(&user, &loader);
        let prop = result.properties.iter().find(|p| p.name == "full_name");
        assert!(
            prop.is_some(),
            "Legacy accessor getFullNameAttribute should produce property full_name, got: {:?}",
            result
                .properties
                .iter()
                .map(|p| &p.name)
                .collect::<Vec<_>>()
        );
        assert_eq!(prop.unwrap().type_hint.as_deref(), Some("string"));
        assert!(!prop.unwrap().is_static);
    }

    #[test]
    fn synthesizes_modern_accessor_property() {
        let provider = LaravelModelProvider;
        let mut user = make_class("App\\Models\\User");
        user.parent_class = Some("Illuminate\\Database\\Eloquent\\Model".to_string());
        user.methods.push(make_method(
            "fullName",
            Some("Illuminate\\Database\\Eloquent\\Casts\\Attribute"),
        ));

        let model = make_class("Illuminate\\Database\\Eloquent\\Model");
        let loader = |name: &str| -> Option<ClassInfo> {
            if name == "Illuminate\\Database\\Eloquent\\Model" {
                Some(model.clone())
            } else {
                None
            }
        };

        let result = provider.provide(&user, &loader);
        let prop = result.properties.iter().find(|p| p.name == "full_name");
        assert!(
            prop.is_some(),
            "Modern accessor fullName() returning Attribute should produce property full_name, got: {:?}",
            result
                .properties
                .iter()
                .map(|p| &p.name)
                .collect::<Vec<_>>()
        );
        assert_eq!(prop.unwrap().type_hint.as_deref(), Some("mixed"));
    }

    #[test]
    fn accessor_and_relationship_coexist() {
        let provider = LaravelModelProvider;
        let mut user = make_class("App\\Models\\User");
        user.parent_class = Some("Illuminate\\Database\\Eloquent\\Model".to_string());
        user.methods
            .push(make_method("getFullNameAttribute", Some("string")));
        user.methods.push(make_method(
            "posts",
            Some("HasMany<App\\Models\\Post, $this>"),
        ));

        let model = make_class("Illuminate\\Database\\Eloquent\\Model");
        let loader = |name: &str| -> Option<ClassInfo> {
            if name == "Illuminate\\Database\\Eloquent\\Model" {
                Some(model.clone())
            } else {
                None
            }
        };

        let result = provider.provide(&user, &loader);
        let prop_names: Vec<_> = result.properties.iter().map(|p| p.name.as_str()).collect();
        assert!(
            prop_names.contains(&"full_name"),
            "Should have accessor property"
        );
        assert!(
            prop_names.contains(&"posts"),
            "Should have relationship property"
        );
    }

    #[test]
    fn get_attribute_method_not_treated_as_accessor() {
        // getAttribute() is a real Eloquent method, not an accessor.
        let provider = LaravelModelProvider;
        let mut user = make_class("App\\Models\\User");
        user.parent_class = Some("Illuminate\\Database\\Eloquent\\Model".to_string());
        user.methods
            .push(make_method("getAttribute", Some("mixed")));

        let model = make_class("Illuminate\\Database\\Eloquent\\Model");
        let loader = |name: &str| -> Option<ClassInfo> {
            if name == "Illuminate\\Database\\Eloquent\\Model" {
                Some(model.clone())
            } else {
                None
            }
        };

        let result = provider.provide(&user, &loader);
        // getAttribute should not produce any virtual property.
        assert!(
            result.properties.is_empty(),
            "getAttribute() should not be treated as a legacy accessor, got: {:?}",
            result
                .properties
                .iter()
                .map(|p| &p.name)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn accessor_scope_and_relationship_all_coexist() {
        let provider = LaravelModelProvider;
        let mut user = make_class("App\\Models\\User");
        user.parent_class = Some("Illuminate\\Database\\Eloquent\\Model".to_string());
        user.methods
            .push(make_method("getFullNameAttribute", Some("string")));
        user.methods.push(make_method(
            "firstName",
            Some("Illuminate\\Database\\Eloquent\\Casts\\Attribute"),
        ));
        user.methods.push(make_method_with_params(
            "scopeActive",
            Some("void"),
            vec![make_param("$query", Some("Builder"), true)],
        ));
        user.methods.push(make_method(
            "posts",
            Some("HasMany<App\\Models\\Post, $this>"),
        ));

        let model = make_class("Illuminate\\Database\\Eloquent\\Model");
        let loader = |name: &str| -> Option<ClassInfo> {
            if name == "Illuminate\\Database\\Eloquent\\Model" {
                Some(model.clone())
            } else {
                None
            }
        };

        let result = provider.provide(&user, &loader);
        let prop_names: Vec<_> = result.properties.iter().map(|p| p.name.as_str()).collect();
        assert!(
            prop_names.contains(&"full_name"),
            "Legacy accessor property"
        );
        assert!(
            prop_names.contains(&"first_name"),
            "Modern accessor property"
        );
        assert!(prop_names.contains(&"posts"), "Relationship property");

        let method_names: Vec<_> = result.methods.iter().map(|m| m.name.as_str()).collect();
        assert!(method_names.contains(&"active"), "Scope method");
    }

    #[test]
    fn legacy_accessor_preserves_deprecated() {
        let provider = LaravelModelProvider;
        let mut user = make_class("App\\Models\\User");
        user.parent_class = Some("Illuminate\\Database\\Eloquent\\Model".to_string());

        let mut accessor = make_method("getOldNameAttribute", Some("string"));
        accessor.is_deprecated = true;
        user.methods.push(accessor);

        let model = make_class("Illuminate\\Database\\Eloquent\\Model");
        let loader = |name: &str| -> Option<ClassInfo> {
            if name == "Illuminate\\Database\\Eloquent\\Model" {
                Some(model.clone())
            } else {
                None
            }
        };

        let result = provider.provide(&user, &loader);
        let prop = result.properties.iter().find(|p| p.name == "old_name");
        assert!(prop.is_some());
        assert!(
            prop.unwrap().is_deprecated,
            "Deprecated flag should be preserved"
        );
    }

    // ── infer_relationship_from_body ─────────────────────────────────────

    #[test]
    fn infer_has_many_from_body() {
        let body = "{ return $this->hasMany(Post::class); }";
        assert_eq!(
            infer_relationship_from_body(body),
            Some("HasMany<Post>".to_string())
        );
    }

    #[test]
    fn infer_has_one_from_body() {
        let body = "{ return $this->hasOne(Profile::class); }";
        assert_eq!(
            infer_relationship_from_body(body),
            Some("HasOne<Profile>".to_string())
        );
    }

    #[test]
    fn infer_belongs_to_from_body() {
        let body = "{ return $this->belongsTo(User::class); }";
        assert_eq!(
            infer_relationship_from_body(body),
            Some("BelongsTo<User>".to_string())
        );
    }

    #[test]
    fn infer_belongs_to_many_from_body() {
        let body = "{ return $this->belongsToMany(Role::class); }";
        assert_eq!(
            infer_relationship_from_body(body),
            Some("BelongsToMany<Role>".to_string())
        );
    }

    #[test]
    fn infer_morph_one_from_body() {
        let body = "{ return $this->morphOne(Image::class, 'imageable'); }";
        assert_eq!(
            infer_relationship_from_body(body),
            Some("MorphOne<Image>".to_string())
        );
    }

    #[test]
    fn infer_morph_many_from_body() {
        let body = "{ return $this->morphMany(Comment::class, 'commentable'); }";
        assert_eq!(
            infer_relationship_from_body(body),
            Some("MorphMany<Comment>".to_string())
        );
    }

    #[test]
    fn infer_morph_to_from_body() {
        // morphTo never has a related model class argument.
        let body = "{ return $this->morphTo(); }";
        assert_eq!(
            infer_relationship_from_body(body),
            Some("MorphTo".to_string())
        );
    }

    #[test]
    fn infer_morph_to_many_from_body() {
        let body = "{ return $this->morphToMany(Tag::class, 'taggable'); }";
        assert_eq!(
            infer_relationship_from_body(body),
            Some("MorphToMany<Tag>".to_string())
        );
    }

    #[test]
    fn infer_has_many_through_from_body() {
        let body = "{ return $this->hasManyThrough(Post::class, Country::class); }";
        assert_eq!(
            infer_relationship_from_body(body),
            Some("HasManyThrough<Post>".to_string())
        );
    }

    #[test]
    fn infer_has_one_through_from_body() {
        let body = "{ return $this->hasOneThrough(Owner::class, Car::class); }";
        assert_eq!(
            infer_relationship_from_body(body),
            Some("HasOneThrough<Owner>".to_string())
        );
    }

    #[test]
    fn infer_relationship_fqn_class_argument() {
        let body = r"{ return $this->hasMany(\App\Models\Post::class); }";
        assert_eq!(
            infer_relationship_from_body(body),
            Some("HasMany<Post>".to_string())
        );
    }

    #[test]
    fn infer_relationship_with_extra_arguments() {
        let body = "{ return $this->hasMany(Post::class, 'user_id', 'id'); }";
        assert_eq!(
            infer_relationship_from_body(body),
            Some("HasMany<Post>".to_string())
        );
    }

    #[test]
    fn infer_relationship_with_whitespace() {
        let body = "{
            return $this->hasMany(  Post::class  );
        }";
        assert_eq!(
            infer_relationship_from_body(body),
            Some("HasMany<Post>".to_string())
        );
    }

    #[test]
    fn infer_no_relationship_in_empty_body() {
        let body = "{ }";
        assert_eq!(infer_relationship_from_body(body), None);
    }

    #[test]
    fn infer_no_relationship_for_non_relationship_call() {
        let body = "{ return $this->query(); }";
        assert_eq!(infer_relationship_from_body(body), None);
    }

    #[test]
    fn infer_relationship_without_class_argument() {
        // Some projects use string-based relationship definitions.
        let body = "{ return $this->hasMany('App\\Models\\Post'); }";
        assert_eq!(
            infer_relationship_from_body(body),
            Some("HasMany".to_string()),
            "Without ::class argument, returns bare relationship name"
        );
    }

    #[test]
    fn infer_morph_to_with_arguments() {
        // morphTo can optionally take a name and type column.
        let body =
            "{ return $this->morphTo('commentable', 'commentable_type', 'commentable_id'); }";
        assert_eq!(
            infer_relationship_from_body(body),
            Some("MorphTo".to_string())
        );
    }

    #[test]
    fn infer_relationship_multiline_body() {
        let body = "{
            return $this
                ->hasMany(Post::class, 'author_id');
        }";
        // The needle `$this->hasMany(` won't match across a line break,
        // so this returns None.  This is an acceptable limitation
        // documented in the todo.
        assert_eq!(infer_relationship_from_body(body), None);
    }

    #[test]
    fn infer_relationship_same_line_chain() {
        let body = "{ return $this->hasMany(Post::class)->latest(); }";
        assert_eq!(
            infer_relationship_from_body(body),
            Some("HasMany<Post>".to_string())
        );
    }

    // ── extract_class_argument ──────────────────────────────────────────

    #[test]
    fn extract_simple_class_arg() {
        assert_eq!(
            extract_class_argument("Post::class)"),
            Some("Post".to_string())
        );
    }

    #[test]
    fn extract_fqn_class_arg() {
        assert_eq!(
            extract_class_argument("\\App\\Models\\Post::class)"),
            Some("Post".to_string())
        );
    }

    #[test]
    fn extract_class_arg_with_extra_args() {
        assert_eq!(
            extract_class_argument("Post::class, 'user_id', 'id')"),
            Some("Post".to_string())
        );
    }

    #[test]
    fn extract_class_arg_with_whitespace() {
        assert_eq!(
            extract_class_argument("  Post::class  )"),
            Some("Post".to_string())
        );
    }

    #[test]
    fn extract_class_arg_no_class_token() {
        assert_eq!(extract_class_argument("'App\\Models\\Post')"), None);
    }

    #[test]
    fn extract_class_arg_no_closing_paren() {
        assert_eq!(extract_class_argument("Post::class"), None);
    }

    #[test]
    fn extract_class_arg_empty() {
        assert_eq!(extract_class_argument(")"), None);
    }

    #[test]
    fn extract_class_arg_class_in_second_arg_only() {
        // `::class` appears only after the first comma — should return None.
        assert_eq!(extract_class_argument("'taggable', Tag::class)"), None);
    }

    // ── provider integration with body-inferred relationships ───────────

    #[test]
    fn synthesizes_property_from_body_inferred_has_many() {
        let provider = LaravelModelProvider;
        let mut user = make_class("App\\Models\\User");
        user.parent_class = Some("Illuminate\\Database\\Eloquent\\Model".to_string());

        // Method with no @return annotation — return_type is set by
        // the parser from body inference.
        user.methods.push(MethodInfo {
            name: "posts".to_string(),
            parameters: Vec::new(),
            return_type: Some("HasMany<Post>".to_string()),
            is_static: false,
            visibility: Visibility::Public,
            conditional_return: None,
            is_deprecated: false,
            template_params: Vec::new(),
            template_bindings: Vec::new(),
        });

        let model = make_class("Illuminate\\Database\\Eloquent\\Model");
        let loader = |name: &str| -> Option<ClassInfo> {
            if name == "Illuminate\\Database\\Eloquent\\Model" {
                Some(model.clone())
            } else {
                None
            }
        };

        let result = provider.provide(&user, &loader);
        let prop = result.properties.iter().find(|p| p.name == "posts");
        assert!(
            prop.is_some(),
            "Body-inferred HasMany<Post> should produce a 'posts' property"
        );
    }

    #[test]
    fn synthesizes_property_from_body_inferred_morph_to() {
        let provider = LaravelModelProvider;
        let mut comment = make_class("App\\Models\\Comment");
        comment.parent_class = Some("Illuminate\\Database\\Eloquent\\Model".to_string());

        // morphTo inferred from body — bare name, no generics.
        comment.methods.push(MethodInfo {
            name: "commentable".to_string(),
            parameters: Vec::new(),
            return_type: Some("MorphTo".to_string()),
            is_static: false,
            visibility: Visibility::Public,
            conditional_return: None,
            is_deprecated: false,
            template_params: Vec::new(),
            template_bindings: Vec::new(),
        });

        let model = make_class("Illuminate\\Database\\Eloquent\\Model");
        let loader = |name: &str| -> Option<ClassInfo> {
            if name == "Illuminate\\Database\\Eloquent\\Model" {
                Some(model.clone())
            } else {
                None
            }
        };

        let result = provider.provide(&comment, &loader);
        let prop = result.properties.iter().find(|p| p.name == "commentable");
        assert!(
            prop.is_some(),
            "Body-inferred MorphTo should produce a 'commentable' property"
        );
    }

    // ── Cast type mapping tests ─────────────────────────────────────────

    #[test]
    fn cast_datetime_maps_to_carbon() {
        assert_eq!(
            cast_type_to_php_type("datetime", &no_loader),
            "\\Carbon\\Carbon"
        );
    }

    #[test]
    fn cast_date_maps_to_carbon() {
        assert_eq!(
            cast_type_to_php_type("date", &no_loader),
            "\\Carbon\\Carbon"
        );
    }

    #[test]
    fn cast_timestamp_maps_to_int() {
        assert_eq!(cast_type_to_php_type("timestamp", &no_loader), "int");
    }

    #[test]
    fn cast_immutable_datetime_maps_to_carbon_immutable() {
        assert_eq!(
            cast_type_to_php_type("immutable_datetime", &no_loader),
            "\\Carbon\\CarbonImmutable"
        );
    }

    #[test]
    fn cast_immutable_date_maps_to_carbon_immutable() {
        assert_eq!(
            cast_type_to_php_type("immutable_date", &no_loader),
            "\\Carbon\\CarbonImmutable"
        );
    }

    #[test]
    fn cast_boolean_maps_to_bool() {
        assert_eq!(cast_type_to_php_type("boolean", &no_loader), "bool");
    }

    #[test]
    fn cast_bool_maps_to_bool() {
        assert_eq!(cast_type_to_php_type("bool", &no_loader), "bool");
    }

    #[test]
    fn cast_integer_maps_to_int() {
        assert_eq!(cast_type_to_php_type("integer", &no_loader), "int");
    }

    #[test]
    fn cast_int_maps_to_int() {
        assert_eq!(cast_type_to_php_type("int", &no_loader), "int");
    }

    #[test]
    fn cast_float_maps_to_float() {
        assert_eq!(cast_type_to_php_type("float", &no_loader), "float");
    }

    #[test]
    fn cast_double_maps_to_float() {
        assert_eq!(cast_type_to_php_type("double", &no_loader), "float");
    }

    #[test]
    fn cast_real_maps_to_float() {
        assert_eq!(cast_type_to_php_type("real", &no_loader), "float");
    }

    #[test]
    fn cast_string_maps_to_string() {
        assert_eq!(cast_type_to_php_type("string", &no_loader), "string");
    }

    #[test]
    fn cast_array_maps_to_array() {
        assert_eq!(cast_type_to_php_type("array", &no_loader), "array");
    }

    #[test]
    fn cast_json_maps_to_array() {
        assert_eq!(cast_type_to_php_type("json", &no_loader), "array");
    }

    #[test]
    fn cast_object_maps_to_object() {
        assert_eq!(cast_type_to_php_type("object", &no_loader), "object");
    }

    #[test]
    fn cast_collection_maps_to_illuminate_collection() {
        assert_eq!(
            cast_type_to_php_type("collection", &no_loader),
            "\\Illuminate\\Support\\Collection"
        );
    }

    #[test]
    fn cast_encrypted_maps_to_string() {
        assert_eq!(cast_type_to_php_type("encrypted", &no_loader), "string");
    }

    #[test]
    fn cast_encrypted_array_maps_to_array() {
        assert_eq!(
            cast_type_to_php_type("encrypted:array", &no_loader),
            "array"
        );
    }

    #[test]
    fn cast_encrypted_collection_maps_to_collection() {
        assert_eq!(
            cast_type_to_php_type("encrypted:collection", &no_loader),
            "\\Illuminate\\Support\\Collection"
        );
    }

    #[test]
    fn cast_encrypted_object_maps_to_object() {
        assert_eq!(
            cast_type_to_php_type("encrypted:object", &no_loader),
            "object"
        );
    }

    #[test]
    fn cast_hashed_maps_to_string() {
        assert_eq!(cast_type_to_php_type("hashed", &no_loader), "string");
    }

    #[test]
    fn cast_decimal_with_precision_maps_to_float() {
        assert_eq!(cast_type_to_php_type("decimal:2", &no_loader), "float");
    }

    #[test]
    fn cast_decimal_bare_maps_to_float() {
        assert_eq!(cast_type_to_php_type("decimal", &no_loader), "float");
    }

    #[test]
    fn cast_datetime_with_format_maps_to_carbon() {
        assert_eq!(
            cast_type_to_php_type("datetime:Y-m-d", &no_loader),
            "\\Carbon\\Carbon"
        );
    }

    #[test]
    fn cast_date_with_format_maps_to_carbon() {
        assert_eq!(
            cast_type_to_php_type("date:Y-m-d", &no_loader),
            "\\Carbon\\Carbon"
        );
    }

    #[test]
    fn cast_immutable_datetime_with_format() {
        assert_eq!(
            cast_type_to_php_type("immutable_datetime:Y-m-d H:i:s", &no_loader),
            "\\Carbon\\CarbonImmutable"
        );
    }

    #[test]
    fn cast_immutable_date_with_format() {
        assert_eq!(
            cast_type_to_php_type("immutable_date:Y-m-d", &no_loader),
            "\\Carbon\\CarbonImmutable"
        );
    }

    #[test]
    fn cast_case_insensitive() {
        assert_eq!(cast_type_to_php_type("Boolean", &no_loader), "bool");
        assert_eq!(
            cast_type_to_php_type("DATETIME", &no_loader),
            "\\Carbon\\Carbon"
        );
        assert_eq!(cast_type_to_php_type("Integer", &no_loader), "int");
    }

    #[test]
    fn cast_unknown_type_falls_back_to_mixed() {
        assert_eq!(cast_type_to_php_type("unknown_cast", &no_loader), "mixed");
    }

    #[test]
    fn cast_custom_class_with_get_method() {
        let loader = |name: &str| -> Option<ClassInfo> {
            if name == "App\\Casts\\MoneyCast" {
                let mut cast_class = make_class("MoneyCast");
                cast_class
                    .methods
                    .push(make_method("get", Some("\\App\\Money")));
                Some(cast_class)
            } else {
                None
            }
        };
        assert_eq!(
            cast_type_to_php_type("App\\Casts\\MoneyCast", &loader),
            "\\App\\Money"
        );
    }

    #[test]
    fn cast_custom_class_with_leading_backslash() {
        let loader = |name: &str| -> Option<ClassInfo> {
            if name == "App\\Casts\\MoneyCast" {
                let mut cast_class = make_class("MoneyCast");
                cast_class
                    .methods
                    .push(make_method("get", Some("\\App\\Money")));
                Some(cast_class)
            } else {
                None
            }
        };
        assert_eq!(
            cast_type_to_php_type("\\App\\Casts\\MoneyCast", &loader),
            "\\App\\Money"
        );
    }

    #[test]
    fn cast_custom_class_without_get_returns_mixed() {
        let loader = |name: &str| -> Option<ClassInfo> {
            if name == "App\\Casts\\WeirdCast" {
                Some(make_class("WeirdCast"))
            } else {
                None
            }
        };
        assert_eq!(
            cast_type_to_php_type("App\\Casts\\WeirdCast", &loader),
            "mixed"
        );
    }

    #[test]
    fn cast_enum_resolves_to_enum_class() {
        let loader = |name: &str| -> Option<ClassInfo> {
            if name == "App\\Enums\\Status" {
                let mut e = make_class("Status");
                e.kind = ClassLikeKind::Enum;
                Some(e)
            } else {
                None
            }
        };
        assert_eq!(
            cast_type_to_php_type("App\\Enums\\Status", &loader),
            "\\App\\Enums\\Status"
        );
    }

    #[test]
    fn cast_enum_with_leading_backslash() {
        let loader = |name: &str| -> Option<ClassInfo> {
            if name == "App\\Enums\\Status" {
                let mut e = make_class("Status");
                e.kind = ClassLikeKind::Enum;
                Some(e)
            } else {
                None
            }
        };
        assert_eq!(
            cast_type_to_php_type("\\App\\Enums\\Status", &loader),
            "\\App\\Enums\\Status"
        );
    }

    #[test]
    fn cast_castable_resolves_to_class_itself() {
        let loader = |name: &str| -> Option<ClassInfo> {
            if name == "App\\Casts\\Address" {
                let mut c = make_class("Address");
                c.interfaces = vec![CASTABLE_FQN.to_string()];
                Some(c)
            } else {
                None
            }
        };
        assert_eq!(
            cast_type_to_php_type("App\\Casts\\Address", &loader),
            "\\App\\Casts\\Address"
        );
    }

    #[test]
    fn cast_castable_with_leading_backslash_interface() {
        let loader = |name: &str| -> Option<ClassInfo> {
            if name == "App\\Casts\\Address" {
                let mut c = make_class("Address");
                c.interfaces = vec![format!("\\{CASTABLE_FQN}")];
                Some(c)
            } else {
                None
            }
        };
        assert_eq!(
            cast_type_to_php_type("App\\Casts\\Address", &loader),
            "\\App\\Casts\\Address"
        );
    }

    #[test]
    fn cast_castable_short_interface_name() {
        let loader = |name: &str| -> Option<ClassInfo> {
            if name == "App\\Casts\\Address" {
                let mut c = make_class("Address");
                c.interfaces = vec!["Castable".to_string()];
                Some(c)
            } else {
                None
            }
        };
        assert_eq!(
            cast_type_to_php_type("App\\Casts\\Address", &loader),
            "\\App\\Casts\\Address"
        );
    }

    #[test]
    fn cast_class_with_colon_argument_suffix() {
        let loader = |name: &str| -> Option<ClassInfo> {
            if name == "App\\Casts\\Address" {
                let mut c = make_class("Address");
                c.interfaces = vec![CASTABLE_FQN.to_string()];
                Some(c)
            } else {
                None
            }
        };
        assert_eq!(
            cast_type_to_php_type("App\\Casts\\Address:nullable", &loader),
            "\\App\\Casts\\Address"
        );
    }

    #[test]
    fn cast_enum_with_colon_argument_suffix() {
        let loader = |name: &str| -> Option<ClassInfo> {
            if name == "App\\Enums\\Status" {
                let mut e = make_class("Status");
                e.kind = ClassLikeKind::Enum;
                Some(e)
            } else {
                None
            }
        };
        assert_eq!(
            cast_type_to_php_type("App\\Enums\\Status:force", &loader),
            "\\App\\Enums\\Status"
        );
    }

    #[test]
    fn cast_custom_class_with_colon_argument_and_get() {
        let loader = |name: &str| -> Option<ClassInfo> {
            if name == "App\\Casts\\MoneyCast" {
                let mut cast_class = make_class("MoneyCast");
                cast_class
                    .methods
                    .push(make_method("get", Some("\\App\\Money")));
                Some(cast_class)
            } else {
                None
            }
        };
        assert_eq!(
            cast_type_to_php_type("App\\Casts\\MoneyCast:precision,2", &loader),
            "\\App\\Money"
        );
    }

    #[test]
    fn is_castable_with_fqn() {
        let mut c = make_class("Address");
        c.interfaces = vec![CASTABLE_FQN.to_string()];
        assert!(is_castable(&c));
    }

    #[test]
    fn is_castable_with_leading_backslash() {
        let mut c = make_class("Address");
        c.interfaces = vec![format!("\\{CASTABLE_FQN}")];
        assert!(is_castable(&c));
    }

    #[test]
    fn is_castable_with_short_name() {
        let mut c = make_class("Address");
        c.interfaces = vec!["Castable".to_string()];
        assert!(is_castable(&c));
    }

    #[test]
    fn is_not_castable() {
        let c = make_class("SomePlainClass");
        assert!(!is_castable(&c));
    }

    // ── extract_tget_from_implements_generics tests ─────────────────────

    #[test]
    fn tget_from_casts_attributes_short_name() {
        let mut c = make_class("App\\Casts\\HtmlCast");
        c.implements_generics = vec![(
            "CastsAttributes".to_string(),
            vec!["HtmlString".to_string(), "HtmlString".to_string()],
        )];
        assert_eq!(
            extract_tget_from_implements_generics(&c),
            Some("HtmlString".to_string())
        );
    }

    #[test]
    fn tget_from_casts_attributes_fqn() {
        let mut c = make_class("App\\Casts\\HtmlCast");
        c.implements_generics = vec![(
            CASTS_ATTRIBUTES_FQN.to_string(),
            vec![
                "\\Illuminate\\Support\\HtmlString".to_string(),
                "string".to_string(),
            ],
        )];
        assert_eq!(
            extract_tget_from_implements_generics(&c),
            Some("\\Illuminate\\Support\\HtmlString".to_string())
        );
    }

    #[test]
    fn tget_from_casts_attributes_with_leading_backslash() {
        let mut c = make_class("App\\Casts\\HtmlCast");
        c.implements_generics = vec![(
            format!("\\{CASTS_ATTRIBUTES_FQN}"),
            vec!["HtmlString".to_string(), "HtmlString".to_string()],
        )];
        assert_eq!(
            extract_tget_from_implements_generics(&c),
            Some("HtmlString".to_string())
        );
    }

    #[test]
    fn tget_returns_none_when_no_implements_generics() {
        let c = make_class("App\\Casts\\HtmlCast");
        assert_eq!(extract_tget_from_implements_generics(&c), None);
    }

    #[test]
    fn tget_returns_none_for_unrelated_interface() {
        let mut c = make_class("App\\Casts\\HtmlCast");
        c.implements_generics = vec![("SomeOtherInterface".to_string(), vec!["Foo".to_string()])];
        assert_eq!(extract_tget_from_implements_generics(&c), None);
    }

    #[test]
    fn tget_returns_none_for_empty_args() {
        let mut c = make_class("App\\Casts\\HtmlCast");
        c.implements_generics = vec![("CastsAttributes".to_string(), vec![])];
        assert_eq!(extract_tget_from_implements_generics(&c), None);
    }

    #[test]
    fn tget_skips_empty_string_arg() {
        let mut c = make_class("App\\Casts\\HtmlCast");
        c.implements_generics = vec![(
            "CastsAttributes".to_string(),
            vec!["".to_string(), "HtmlString".to_string()],
        )];
        assert_eq!(extract_tget_from_implements_generics(&c), None);
    }

    #[test]
    fn cast_custom_class_falls_back_to_implements_generics() {
        let loader = |name: &str| -> Option<ClassInfo> {
            if name == "App\\Casts\\HtmlCast" {
                let mut cast_class = make_class("HtmlCast");
                // get() has no return type — mimics the real scenario.
                cast_class.methods.push(make_method("get", None));
                cast_class.implements_generics = vec![(
                    "CastsAttributes".to_string(),
                    vec!["HtmlString".to_string(), "HtmlString".to_string()],
                )];
                Some(cast_class)
            } else {
                None
            }
        };
        assert_eq!(
            cast_type_to_php_type("App\\Casts\\HtmlCast", &loader),
            "HtmlString"
        );
    }

    #[test]
    fn cast_custom_class_get_return_type_takes_priority_over_implements() {
        let loader = |name: &str| -> Option<ClassInfo> {
            if name == "App\\Casts\\HtmlCast" {
                let mut cast_class = make_class("HtmlCast");
                cast_class
                    .methods
                    .push(make_method("get", Some("?HtmlString")));
                cast_class.implements_generics = vec![(
                    "CastsAttributes".to_string(),
                    vec!["DifferentType".to_string(), "DifferentType".to_string()],
                )];
                Some(cast_class)
            } else {
                None
            }
        };
        assert_eq!(
            cast_type_to_php_type("App\\Casts\\HtmlCast", &loader),
            "?HtmlString"
        );
    }

    // ── Cast property synthesis tests ───────────────────────────────────

    #[test]
    fn synthesizes_cast_properties() {
        let provider = LaravelModelProvider;
        let mut user = make_class(ELOQUENT_MODEL_FQN);
        user.name = "User".to_string();
        user.parent_class = Some(ELOQUENT_MODEL_FQN.to_string());
        user.casts_definitions = vec![
            ("is_admin".to_string(), "boolean".to_string()),
            ("created_at".to_string(), "datetime".to_string()),
            ("options".to_string(), "array".to_string()),
        ];

        let result = provider.provide(&user, &no_loader);

        let is_admin = result.properties.iter().find(|p| p.name == "is_admin");
        assert!(is_admin.is_some(), "should produce is_admin property");
        assert_eq!(is_admin.unwrap().type_hint.as_deref(), Some("bool"));

        let created_at = result.properties.iter().find(|p| p.name == "created_at");
        assert!(created_at.is_some(), "should produce created_at property");
        assert_eq!(
            created_at.unwrap().type_hint.as_deref(),
            Some("\\Carbon\\Carbon")
        );

        let options = result.properties.iter().find(|p| p.name == "options");
        assert!(options.is_some(), "should produce options property");
        assert_eq!(options.unwrap().type_hint.as_deref(), Some("array"));
    }

    #[test]
    fn cast_properties_are_public_and_not_static() {
        let provider = LaravelModelProvider;
        let mut user = make_class(ELOQUENT_MODEL_FQN);
        user.name = "User".to_string();
        user.parent_class = Some(ELOQUENT_MODEL_FQN.to_string());
        user.casts_definitions = vec![("is_admin".to_string(), "boolean".to_string())];

        let result = provider.provide(&user, &no_loader);
        let prop = result
            .properties
            .iter()
            .find(|p| p.name == "is_admin")
            .unwrap();
        assert_eq!(prop.visibility, Visibility::Public);
        assert!(!prop.is_static);
        assert!(!prop.is_deprecated);
    }

    #[test]
    fn cast_properties_coexist_with_relationships_and_scopes() {
        let provider = LaravelModelProvider;
        let mut user = make_class(ELOQUENT_MODEL_FQN);
        user.name = "User".to_string();
        user.parent_class = Some(ELOQUENT_MODEL_FQN.to_string());
        user.casts_definitions = vec![("is_admin".to_string(), "boolean".to_string())];
        user.methods
            .push(make_method("posts", Some("HasMany<Post, $this>")));
        user.methods.push(make_method_with_params(
            "scopeActive",
            Some("void"),
            vec![make_param("$query", Some("Builder"), true)],
        ));

        let result = provider.provide(&user, &no_loader);

        // Cast property
        assert!(result.properties.iter().any(|p| p.name == "is_admin"));
        // Relationship property
        assert!(result.properties.iter().any(|p| p.name == "posts"));
        // Scope methods
        assert!(
            result
                .methods
                .iter()
                .any(|m| m.name == "active" && !m.is_static)
        );
        assert!(
            result
                .methods
                .iter()
                .any(|m| m.name == "active" && m.is_static)
        );
    }

    #[test]
    fn cast_properties_coexist_with_accessors() {
        let provider = LaravelModelProvider;
        let mut user = make_class(ELOQUENT_MODEL_FQN);
        user.name = "User".to_string();
        user.parent_class = Some(ELOQUENT_MODEL_FQN.to_string());
        user.casts_definitions = vec![("is_admin".to_string(), "boolean".to_string())];
        user.methods
            .push(make_method("getFullNameAttribute", Some("string")));
        user.methods.push(make_method(
            "avatarUrl",
            Some("Illuminate\\Database\\Eloquent\\Casts\\Attribute"),
        ));

        let result = provider.provide(&user, &no_loader);

        // Cast property
        assert!(result.properties.iter().any(|p| p.name == "is_admin"));
        // Legacy accessor
        assert!(result.properties.iter().any(|p| p.name == "full_name"));
        // Modern accessor
        assert!(result.properties.iter().any(|p| p.name == "avatar_url"));
    }

    #[test]
    fn empty_casts_produces_no_properties() {
        let provider = LaravelModelProvider;
        let mut user = make_class(ELOQUENT_MODEL_FQN);
        user.name = "User".to_string();
        user.parent_class = Some(ELOQUENT_MODEL_FQN.to_string());
        user.casts_definitions = Vec::new();

        let result = provider.provide(&user, &no_loader);
        assert!(result.properties.is_empty());
    }

    #[test]
    fn cast_decimal_with_precision_synthesizes_float() {
        let provider = LaravelModelProvider;
        let mut user = make_class(ELOQUENT_MODEL_FQN);
        user.name = "User".to_string();
        user.parent_class = Some(ELOQUENT_MODEL_FQN.to_string());
        user.casts_definitions = vec![("price".to_string(), "decimal:2".to_string())];

        let result = provider.provide(&user, &no_loader);
        let prop = result
            .properties
            .iter()
            .find(|p| p.name == "price")
            .unwrap();
        assert_eq!(prop.type_hint.as_deref(), Some("float"));
    }

    // ── Attribute default property synthesis tests ───────────────────────

    #[test]
    fn synthesizes_attribute_default_properties() {
        let provider = LaravelModelProvider;
        let mut user = make_class(ELOQUENT_MODEL_FQN);
        user.name = "User".to_string();
        user.parent_class = Some(ELOQUENT_MODEL_FQN.to_string());
        user.attributes_definitions = vec![
            ("role".to_string(), "string".to_string()),
            ("is_active".to_string(), "bool".to_string()),
            ("login_count".to_string(), "int".to_string()),
        ];

        let result = provider.provide(&user, &no_loader);

        let role = result.properties.iter().find(|p| p.name == "role");
        assert!(role.is_some(), "should produce role property");
        assert_eq!(role.unwrap().type_hint.as_deref(), Some("string"));

        let is_active = result.properties.iter().find(|p| p.name == "is_active");
        assert!(is_active.is_some(), "should produce is_active property");
        assert_eq!(is_active.unwrap().type_hint.as_deref(), Some("bool"));

        let login_count = result.properties.iter().find(|p| p.name == "login_count");
        assert!(login_count.is_some(), "should produce login_count property");
        assert_eq!(login_count.unwrap().type_hint.as_deref(), Some("int"));
    }

    #[test]
    fn attribute_defaults_are_public_and_not_static() {
        let provider = LaravelModelProvider;
        let mut user = make_class(ELOQUENT_MODEL_FQN);
        user.name = "User".to_string();
        user.parent_class = Some(ELOQUENT_MODEL_FQN.to_string());
        user.attributes_definitions = vec![("role".to_string(), "string".to_string())];

        let result = provider.provide(&user, &no_loader);
        let prop = result.properties.iter().find(|p| p.name == "role").unwrap();
        assert_eq!(prop.visibility, Visibility::Public);
        assert!(!prop.is_static);
        assert!(!prop.is_deprecated);
    }

    #[test]
    fn casts_take_priority_over_attribute_defaults() {
        let provider = LaravelModelProvider;
        let mut user = make_class(ELOQUENT_MODEL_FQN);
        user.name = "User".to_string();
        user.parent_class = Some(ELOQUENT_MODEL_FQN.to_string());
        // Both $casts and $attributes define is_active
        user.casts_definitions = vec![("is_active".to_string(), "boolean".to_string())];
        user.attributes_definitions = vec![("is_active".to_string(), "int".to_string())];

        let result = provider.provide(&user, &no_loader);

        // Should only have one is_active property (from casts)
        let matching: Vec<_> = result
            .properties
            .iter()
            .filter(|p| p.name == "is_active")
            .collect();
        assert_eq!(
            matching.len(),
            1,
            "should have exactly one is_active property"
        );
        assert_eq!(
            matching[0].type_hint.as_deref(),
            Some("bool"),
            "casts type should win over attributes type"
        );
    }

    #[test]
    fn attribute_defaults_coexist_with_casts_for_different_columns() {
        let provider = LaravelModelProvider;
        let mut user = make_class(ELOQUENT_MODEL_FQN);
        user.name = "User".to_string();
        user.parent_class = Some(ELOQUENT_MODEL_FQN.to_string());
        user.casts_definitions = vec![("is_admin".to_string(), "boolean".to_string())];
        user.attributes_definitions = vec![("role".to_string(), "string".to_string())];

        let result = provider.provide(&user, &no_loader);

        assert!(
            result.properties.iter().any(|p| p.name == "is_admin"),
            "cast property should be present"
        );
        assert!(
            result.properties.iter().any(|p| p.name == "role"),
            "attribute default property should be present"
        );
    }

    #[test]
    fn attribute_defaults_coexist_with_relationships_and_scopes() {
        let provider = LaravelModelProvider;
        let mut user = make_class(ELOQUENT_MODEL_FQN);
        user.name = "User".to_string();
        user.parent_class = Some(ELOQUENT_MODEL_FQN.to_string());
        user.attributes_definitions = vec![("role".to_string(), "string".to_string())];
        user.methods
            .push(make_method("posts", Some("HasMany<Post, $this>")));
        user.methods.push(make_method_with_params(
            "scopeActive",
            Some("void"),
            vec![make_param("$query", Some("Builder"), true)],
        ));

        let result = provider.provide(&user, &no_loader);

        assert!(
            result.properties.iter().any(|p| p.name == "role"),
            "attribute default property"
        );
        assert!(
            result.properties.iter().any(|p| p.name == "posts"),
            "relationship property"
        );
        assert!(
            result
                .methods
                .iter()
                .any(|m| m.name == "active" && !m.is_static),
            "scope instance method"
        );
    }

    #[test]
    fn empty_attributes_produces_no_properties() {
        let provider = LaravelModelProvider;
        let mut user = make_class(ELOQUENT_MODEL_FQN);
        user.name = "User".to_string();
        user.parent_class = Some(ELOQUENT_MODEL_FQN.to_string());
        user.attributes_definitions = Vec::new();

        let result = provider.provide(&user, &no_loader);
        assert!(result.properties.is_empty());
    }

    #[test]
    fn attribute_default_float_type() {
        let provider = LaravelModelProvider;
        let mut user = make_class(ELOQUENT_MODEL_FQN);
        user.name = "User".to_string();
        user.parent_class = Some(ELOQUENT_MODEL_FQN.to_string());
        user.attributes_definitions = vec![("rating".to_string(), "float".to_string())];

        let result = provider.provide(&user, &no_loader);
        let prop = result
            .properties
            .iter()
            .find(|p| p.name == "rating")
            .unwrap();
        assert_eq!(prop.type_hint.as_deref(), Some("float"));
    }

    #[test]
    fn attribute_default_null_type() {
        let provider = LaravelModelProvider;
        let mut user = make_class(ELOQUENT_MODEL_FQN);
        user.name = "User".to_string();
        user.parent_class = Some(ELOQUENT_MODEL_FQN.to_string());
        user.attributes_definitions = vec![("bio".to_string(), "null".to_string())];

        let result = provider.provide(&user, &no_loader);
        let prop = result.properties.iter().find(|p| p.name == "bio").unwrap();
        assert_eq!(prop.type_hint.as_deref(), Some("null"));
    }

    #[test]
    fn attribute_default_array_type() {
        let provider = LaravelModelProvider;
        let mut user = make_class(ELOQUENT_MODEL_FQN);
        user.name = "User".to_string();
        user.parent_class = Some(ELOQUENT_MODEL_FQN.to_string());
        user.attributes_definitions = vec![("settings".to_string(), "array".to_string())];

        let result = provider.provide(&user, &no_loader);
        let prop = result
            .properties
            .iter()
            .find(|p| p.name == "settings")
            .unwrap();
        assert_eq!(prop.type_hint.as_deref(), Some("array"));
    }

    // ── Column name property synthesis tests ($fillable/$guarded/$hidden) ──

    #[test]
    fn synthesizes_column_name_properties_as_mixed() {
        let provider = LaravelModelProvider;
        let mut user = make_class(ELOQUENT_MODEL_FQN);
        user.name = "User".to_string();
        user.parent_class = Some(ELOQUENT_MODEL_FQN.to_string());
        user.column_names = vec![
            "name".to_string(),
            "email".to_string(),
            "password".to_string(),
        ];

        let result = provider.provide(&user, &no_loader);

        let name = result.properties.iter().find(|p| p.name == "name");
        assert!(name.is_some(), "should produce name property");
        assert_eq!(name.unwrap().type_hint.as_deref(), Some("mixed"));

        let email = result.properties.iter().find(|p| p.name == "email");
        assert!(email.is_some(), "should produce email property");
        assert_eq!(email.unwrap().type_hint.as_deref(), Some("mixed"));

        let password = result.properties.iter().find(|p| p.name == "password");
        assert!(password.is_some(), "should produce password property");
        assert_eq!(password.unwrap().type_hint.as_deref(), Some("mixed"));
    }

    #[test]
    fn column_name_properties_are_public_and_not_static() {
        let provider = LaravelModelProvider;
        let mut user = make_class(ELOQUENT_MODEL_FQN);
        user.name = "User".to_string();
        user.parent_class = Some(ELOQUENT_MODEL_FQN.to_string());
        user.column_names = vec!["name".to_string()];

        let result = provider.provide(&user, &no_loader);
        let prop = result.properties.iter().find(|p| p.name == "name").unwrap();
        assert_eq!(prop.visibility, Visibility::Public);
        assert!(!prop.is_static);
        assert!(!prop.is_deprecated);
    }

    #[test]
    fn casts_take_priority_over_column_names() {
        let provider = LaravelModelProvider;
        let mut user = make_class(ELOQUENT_MODEL_FQN);
        user.name = "User".to_string();
        user.parent_class = Some(ELOQUENT_MODEL_FQN.to_string());
        user.casts_definitions = vec![("is_admin".to_string(), "boolean".to_string())];
        user.column_names = vec!["is_admin".to_string(), "name".to_string()];

        let result = provider.provide(&user, &no_loader);

        let matching: Vec<_> = result
            .properties
            .iter()
            .filter(|p| p.name == "is_admin")
            .collect();
        assert_eq!(matching.len(), 1, "should have exactly one is_admin");
        assert_eq!(
            matching[0].type_hint.as_deref(),
            Some("bool"),
            "casts type should win over column name mixed"
        );

        let name = result.properties.iter().find(|p| p.name == "name");
        assert!(name.is_some(), "column-only name should still appear");
        assert_eq!(name.unwrap().type_hint.as_deref(), Some("mixed"));
    }

    #[test]
    fn attributes_take_priority_over_column_names() {
        let provider = LaravelModelProvider;
        let mut user = make_class(ELOQUENT_MODEL_FQN);
        user.name = "User".to_string();
        user.parent_class = Some(ELOQUENT_MODEL_FQN.to_string());
        user.attributes_definitions = vec![("role".to_string(), "string".to_string())];
        user.column_names = vec!["role".to_string(), "email".to_string()];

        let result = provider.provide(&user, &no_loader);

        let matching: Vec<_> = result
            .properties
            .iter()
            .filter(|p| p.name == "role")
            .collect();
        assert_eq!(matching.len(), 1, "should have exactly one role");
        assert_eq!(
            matching[0].type_hint.as_deref(),
            Some("string"),
            "attributes type should win over column name mixed"
        );

        let email = result.properties.iter().find(|p| p.name == "email");
        assert!(email.is_some(), "column-only email should still appear");
        assert_eq!(email.unwrap().type_hint.as_deref(), Some("mixed"));
    }

    #[test]
    fn all_three_sources_coexist() {
        let provider = LaravelModelProvider;
        let mut user = make_class(ELOQUENT_MODEL_FQN);
        user.name = "User".to_string();
        user.parent_class = Some(ELOQUENT_MODEL_FQN.to_string());
        user.casts_definitions = vec![("is_admin".to_string(), "boolean".to_string())];
        user.attributes_definitions = vec![("role".to_string(), "string".to_string())];
        user.column_names = vec![
            "is_admin".to_string(),
            "role".to_string(),
            "email".to_string(),
        ];

        let result = provider.provide(&user, &no_loader);

        let is_admin = result
            .properties
            .iter()
            .find(|p| p.name == "is_admin")
            .unwrap();
        assert_eq!(is_admin.type_hint.as_deref(), Some("bool"), "from casts");

        let role = result.properties.iter().find(|p| p.name == "role").unwrap();
        assert_eq!(role.type_hint.as_deref(), Some("string"), "from attributes");

        let email = result
            .properties
            .iter()
            .find(|p| p.name == "email")
            .unwrap();
        assert_eq!(
            email.type_hint.as_deref(),
            Some("mixed"),
            "from column_names"
        );
    }

    #[test]
    fn column_names_coexist_with_relationships_and_scopes() {
        let provider = LaravelModelProvider;
        let mut user = make_class(ELOQUENT_MODEL_FQN);
        user.name = "User".to_string();
        user.parent_class = Some(ELOQUENT_MODEL_FQN.to_string());
        user.column_names = vec!["email".to_string()];
        user.methods
            .push(make_method("posts", Some("HasMany<Post, $this>")));
        user.methods.push(make_method_with_params(
            "scopeActive",
            Some("void"),
            vec![make_param("$query", Some("Builder"), true)],
        ));

        let result = provider.provide(&user, &no_loader);

        assert!(
            result.properties.iter().any(|p| p.name == "email"),
            "column name property"
        );
        assert!(
            result.properties.iter().any(|p| p.name == "posts"),
            "relationship property"
        );
        assert!(
            result
                .methods
                .iter()
                .any(|m| m.name == "active" && !m.is_static),
            "scope instance method"
        );
    }

    #[test]
    fn empty_column_names_produces_no_extra_properties() {
        let provider = LaravelModelProvider;
        let mut user = make_class(ELOQUENT_MODEL_FQN);
        user.name = "User".to_string();
        user.parent_class = Some(ELOQUENT_MODEL_FQN.to_string());
        user.column_names = Vec::new();

        let result = provider.provide(&user, &no_loader);
        assert!(result.properties.is_empty());
    }

    // ── Factory helper tests ────────────────────────────────────────────

    // ── model_to_factory_fqn tests ──────────────────────────────────────

    #[test]
    fn model_to_factory_standard() {
        assert_eq!(
            model_to_factory_fqn("App\\Models\\User"),
            "Database\\Factories\\UserFactory"
        );
    }

    #[test]
    fn model_to_factory_subdirectory() {
        assert_eq!(
            model_to_factory_fqn("App\\Models\\Admin\\SuperUser"),
            "Database\\Factories\\Admin\\SuperUserFactory"
        );
    }

    #[test]
    fn model_to_factory_no_models_segment() {
        assert_eq!(
            model_to_factory_fqn("App\\User"),
            "Database\\Factories\\UserFactory"
        );
    }

    #[test]
    fn model_to_factory_bare_name() {
        assert_eq!(
            model_to_factory_fqn("User"),
            "Database\\Factories\\UserFactory"
        );
    }

    #[test]
    fn model_to_factory_leading_backslash() {
        assert_eq!(
            model_to_factory_fqn("\\App\\Models\\User"),
            "Database\\Factories\\UserFactory"
        );
    }

    #[test]
    fn model_to_factory_models_only_namespace() {
        assert_eq!(
            model_to_factory_fqn("Models\\Post"),
            "Database\\Factories\\PostFactory"
        );
    }

    // ── factory_to_model_fqn tests ──────────────────────────────────────

    #[test]
    fn factory_to_model_standard() {
        assert_eq!(
            factory_to_model_fqn("Database\\Factories\\UserFactory"),
            Some("App\\Models\\User".to_string())
        );
    }

    #[test]
    fn factory_to_model_subdirectory() {
        assert_eq!(
            factory_to_model_fqn("Database\\Factories\\Admin\\SuperUserFactory"),
            Some("App\\Models\\Admin\\SuperUser".to_string())
        );
    }

    #[test]
    fn factory_to_model_leading_backslash() {
        assert_eq!(
            factory_to_model_fqn("\\Database\\Factories\\UserFactory"),
            Some("App\\Models\\User".to_string())
        );
    }

    #[test]
    fn factory_to_model_no_factory_suffix() {
        assert_eq!(
            factory_to_model_fqn("Database\\Factories\\UserBuilder"),
            None
        );
    }

    #[test]
    fn factory_to_model_bare_factory() {
        // "Factory" alone has an empty model short name — should return None.
        assert_eq!(factory_to_model_fqn("Factory"), None);
    }

    // ── is_eloquent_factory / extends_eloquent_factory tests ────────────

    #[test]
    fn is_eloquent_factory_fqn() {
        assert!(is_eloquent_factory(FACTORY_FQN));
    }

    #[test]
    fn is_eloquent_factory_with_backslash() {
        assert!(is_eloquent_factory(&format!("\\{FACTORY_FQN}")));
    }

    #[test]
    fn is_eloquent_factory_rejects_unrelated() {
        assert!(!is_eloquent_factory("App\\Factories\\UserFactory"));
    }

    #[test]
    fn extends_factory_direct() {
        let mut class = make_class("UserFactory");
        class.parent_class = Some(FACTORY_FQN.to_string());
        assert!(extends_eloquent_factory(&class, &no_loader));
    }

    #[test]
    fn extends_factory_indirect() {
        let mut class = make_class("UserFactory");
        class.parent_class = Some("BaseFactory".to_string());

        let mut base = make_class("BaseFactory");
        base.parent_class = Some(FACTORY_FQN.to_string());

        let loader = move |name: &str| -> Option<ClassInfo> {
            if name == "BaseFactory" {
                Some(base.clone())
            } else {
                None
            }
        };
        assert!(extends_eloquent_factory(&class, &loader));
    }

    #[test]
    fn does_not_extend_factory() {
        let class = make_class("SomeClass");
        assert!(!extends_eloquent_factory(&class, &no_loader));
    }

    // ── has_factory_extends_generic tests ────────────────────────────────

    #[test]
    fn has_factory_extends_generic_present() {
        let mut class = make_class("UserFactory");
        class.extends_generics = vec![("Factory".to_string(), vec!["User".to_string()])];
        assert!(has_factory_extends_generic(&class));
    }

    #[test]
    fn has_factory_extends_generic_fqn() {
        let mut class = make_class("UserFactory");
        class.extends_generics = vec![(FACTORY_FQN.to_string(), vec!["User".to_string()])];
        assert!(has_factory_extends_generic(&class));
    }

    #[test]
    fn has_factory_extends_generic_not_present() {
        let class = make_class("UserFactory");
        assert!(!has_factory_extends_generic(&class));
    }

    #[test]
    fn has_factory_extends_generic_empty_args() {
        let mut class = make_class("UserFactory");
        class.extends_generics = vec![("Factory".to_string(), vec![])];
        assert!(!has_factory_extends_generic(&class));
    }

    // ── build_factory_model_methods tests ───────────────────────────────

    #[test]
    fn build_factory_model_methods_synthesizes_create_and_make() {
        let mut factory = make_class("Database\\Factories\\UserFactory");
        factory.parent_class = Some(FACTORY_FQN.to_string());

        let model = make_class("App\\Models\\User");
        let loader = move |name: &str| -> Option<ClassInfo> {
            if name == "App\\Models\\User" {
                Some(model.clone())
            } else {
                None
            }
        };

        let methods = build_factory_model_methods(&factory, &loader);
        assert_eq!(methods.len(), 2);

        let create = methods.iter().find(|m| m.name == "create").unwrap();
        assert!(!create.is_static);
        assert_eq!(create.return_type.as_deref(), Some("\\App\\Models\\User"));

        let make = methods.iter().find(|m| m.name == "make").unwrap();
        assert!(!make.is_static);
        assert_eq!(make.return_type.as_deref(), Some("\\App\\Models\\User"));
    }

    #[test]
    fn build_factory_model_methods_returns_empty_when_model_missing() {
        let mut factory = make_class("Database\\Factories\\UserFactory");
        factory.parent_class = Some(FACTORY_FQN.to_string());

        let methods = build_factory_model_methods(&factory, &no_loader);
        assert!(methods.is_empty());
    }

    #[test]
    fn build_factory_model_methods_returns_empty_for_non_factory_name() {
        let mut class = make_class("App\\Builders\\UserBuilder");
        class.parent_class = Some(FACTORY_FQN.to_string());

        let methods = build_factory_model_methods(&class, &no_loader);
        assert!(methods.is_empty());
    }

    // ── LaravelFactoryProvider tests ────────────────────────────────────

    #[test]
    fn factory_provider_applies_to_factory_subclass() {
        let provider = LaravelFactoryProvider;
        let mut factory = make_class("Database\\Factories\\UserFactory");
        factory.parent_class = Some(FACTORY_FQN.to_string());

        let loader = |name: &str| -> Option<ClassInfo> {
            if name == FACTORY_FQN {
                Some(make_class(FACTORY_FQN))
            } else {
                None
            }
        };
        assert!(provider.applies_to(&factory, &loader));
    }

    #[test]
    fn factory_provider_does_not_apply_to_factory_base_class() {
        let provider = LaravelFactoryProvider;
        let class = make_class(FACTORY_FQN);
        assert!(!provider.applies_to(&class, &no_loader));
    }

    #[test]
    fn factory_provider_does_not_apply_when_extends_generic_present() {
        let provider = LaravelFactoryProvider;
        let mut factory = make_class("Database\\Factories\\UserFactory");
        factory.parent_class = Some(FACTORY_FQN.to_string());
        factory.extends_generics = vec![("Factory".to_string(), vec!["User".to_string()])];

        assert!(!provider.applies_to(&factory, &no_loader));
    }

    #[test]
    fn factory_provider_does_not_apply_to_non_factory() {
        let provider = LaravelFactoryProvider;
        let class = make_class("App\\Models\\User");
        assert!(!provider.applies_to(&class, &no_loader));
    }

    #[test]
    fn factory_provider_synthesizes_create_and_make() {
        let provider = LaravelFactoryProvider;
        let mut factory = make_class("Database\\Factories\\UserFactory");
        factory.parent_class = Some(FACTORY_FQN.to_string());

        let model = make_class("App\\Models\\User");
        let loader = move |name: &str| -> Option<ClassInfo> {
            if name == "App\\Models\\User" {
                Some(model.clone())
            } else {
                None
            }
        };

        let result = provider.provide(&factory, &loader);
        assert_eq!(result.methods.len(), 2);

        let create = result.methods.iter().find(|m| m.name == "create").unwrap();
        assert_eq!(create.return_type.as_deref(), Some("\\App\\Models\\User"));
        assert!(!create.is_static);

        let make = result.methods.iter().find(|m| m.name == "make").unwrap();
        assert_eq!(make.return_type.as_deref(), Some("\\App\\Models\\User"));
        assert!(!make.is_static);
    }

    #[test]
    fn factory_provider_empty_when_model_not_found() {
        let provider = LaravelFactoryProvider;
        let mut factory = make_class("Database\\Factories\\UserFactory");
        factory.parent_class = Some(FACTORY_FQN.to_string());

        let result = provider.provide(&factory, &no_loader);
        assert!(result.methods.is_empty());
    }

    #[test]
    fn factory_provider_subdirectory_convention() {
        let provider = LaravelFactoryProvider;
        let mut factory = make_class("Database\\Factories\\Admin\\SuperUserFactory");
        factory.parent_class = Some(FACTORY_FQN.to_string());

        let model = make_class("App\\Models\\Admin\\SuperUser");
        let loader = move |name: &str| -> Option<ClassInfo> {
            if name == "App\\Models\\Admin\\SuperUser" {
                Some(model.clone())
            } else {
                None
            }
        };

        let result = provider.provide(&factory, &loader);
        assert_eq!(result.methods.len(), 2);

        let create = result.methods.iter().find(|m| m.name == "create").unwrap();
        assert_eq!(
            create.return_type.as_deref(),
            Some("\\App\\Models\\Admin\\SuperUser")
        );
    }

    // ─── build_scope_methods_for_builder ─────────────────────────────

    #[test]
    fn builder_scope_returns_empty_when_model_not_found() {
        let methods = build_scope_methods_for_builder("App\\Models\\Missing", &no_loader);
        assert!(methods.is_empty());
    }

    #[test]
    fn builder_scope_returns_empty_for_non_model() {
        let loader = |name: &str| -> Option<ClassInfo> {
            if name == "App\\Models\\Plain" {
                Some(make_class("App\\Models\\Plain"))
            } else {
                None
            }
        };
        let methods = build_scope_methods_for_builder("App\\Models\\Plain", &loader);
        assert!(methods.is_empty());
    }

    #[test]
    fn builder_scope_extracts_scope_methods_as_instance() {
        let mut model = make_class("App\\Models\\User");
        model.parent_class = Some(ELOQUENT_MODEL_FQN.to_string());
        model.methods.push(make_method("scopeActive", Some("void")));
        model
            .methods
            .push(make_method("scopeVerified", Some("void")));
        model.methods.push(make_method("getName", Some("string")));

        let loader = |name: &str| -> Option<ClassInfo> {
            if name == "App\\Models\\User" {
                Some(model.clone())
            } else if name == ELOQUENT_MODEL_FQN {
                Some(make_class(ELOQUENT_MODEL_FQN))
            } else {
                None
            }
        };

        let methods = build_scope_methods_for_builder("App\\Models\\User", &loader);
        let names: Vec<&str> = methods.iter().map(|m| m.name.as_str()).collect();

        assert!(
            names.contains(&"active"),
            "should contain active, got: {names:?}"
        );
        assert!(
            names.contains(&"verified"),
            "should contain verified, got: {names:?}"
        );
        assert!(
            !names.contains(&"getName"),
            "should not contain non-scope getName, got: {names:?}"
        );
        // All should be instance methods
        assert!(methods.iter().all(|m| !m.is_static));
    }

    #[test]
    fn builder_scope_substitutes_static_in_return_type() {
        let mut model = make_class("App\\Models\\Brand");
        model.parent_class = Some(ELOQUENT_MODEL_FQN.to_string());
        // Default scope return type contains `static`
        model
            .methods
            .push(make_method("scopePopular", Some("void")));

        let loader = |name: &str| -> Option<ClassInfo> {
            if name == "App\\Models\\Brand" {
                Some(model.clone())
            } else if name == ELOQUENT_MODEL_FQN {
                Some(make_class(ELOQUENT_MODEL_FQN))
            } else {
                None
            }
        };

        let methods = build_scope_methods_for_builder("App\\Models\\Brand", &loader);
        assert_eq!(methods.len(), 1);
        let popular = &methods[0];
        assert_eq!(popular.name, "popular");
        // The default return type `\...\Builder<static>` should have
        // `static` substituted with the concrete model name.
        let ret = popular.return_type.as_deref().unwrap();
        assert!(
            ret.contains("App\\Models\\Brand"),
            "return type should contain model name, got: {ret}"
        );
        assert!(
            !ret.contains("static"),
            "return type should not contain 'static', got: {ret}"
        );
    }

    #[test]
    fn builder_scope_strips_query_parameter() {
        let mut model = make_class("App\\Models\\Task");
        model.parent_class = Some(ELOQUENT_MODEL_FQN.to_string());
        model.methods.push(make_method_with_params(
            "scopeOfType",
            Some("void"),
            vec![
                make_param("$query", Some("Builder"), true),
                make_param("$type", Some("string"), true),
            ],
        ));

        let loader = |name: &str| -> Option<ClassInfo> {
            if name == "App\\Models\\Task" {
                Some(model.clone())
            } else if name == ELOQUENT_MODEL_FQN {
                Some(make_class(ELOQUENT_MODEL_FQN))
            } else {
                None
            }
        };

        let methods = build_scope_methods_for_builder("App\\Models\\Task", &loader);
        assert_eq!(methods.len(), 1);
        let of_type = &methods[0];
        assert_eq!(of_type.name, "ofType");
        // $query should be stripped, only $type remains
        assert_eq!(of_type.parameters.len(), 1);
        assert_eq!(of_type.parameters[0].name, "$type");
    }

    #[test]
    fn builder_scope_with_custom_return_type() {
        let mut model = make_class("App\\Models\\Post");
        model.parent_class = Some(ELOQUENT_MODEL_FQN.to_string());
        model.methods.push(make_method(
            "scopeDraft",
            Some("\\Illuminate\\Database\\Eloquent\\Builder<static>"),
        ));

        let loader = |name: &str| -> Option<ClassInfo> {
            if name == "App\\Models\\Post" {
                Some(model.clone())
            } else if name == ELOQUENT_MODEL_FQN {
                Some(make_class(ELOQUENT_MODEL_FQN))
            } else {
                None
            }
        };

        let methods = build_scope_methods_for_builder("App\\Models\\Post", &loader);
        assert_eq!(methods.len(), 1);
        let draft = &methods[0];
        assert_eq!(draft.name, "draft");
        let ret = draft.return_type.as_deref().unwrap();
        assert_eq!(
            ret,
            "\\Illuminate\\Database\\Eloquent\\Builder<App\\Models\\Post>"
        );
    }

    #[test]
    fn builder_scope_preserves_deprecated() {
        let mut model = make_class("App\\Models\\Item");
        model.parent_class = Some(ELOQUENT_MODEL_FQN.to_string());
        let mut scope = make_method("scopeOld", Some("void"));
        scope.is_deprecated = true;
        model.methods.push(scope);

        let loader = |name: &str| -> Option<ClassInfo> {
            if name == "App\\Models\\Item" {
                Some(model.clone())
            } else if name == ELOQUENT_MODEL_FQN {
                Some(make_class(ELOQUENT_MODEL_FQN))
            } else {
                None
            }
        };

        let methods = build_scope_methods_for_builder("App\\Models\\Item", &loader);
        assert_eq!(methods.len(), 1);
        assert!(methods[0].is_deprecated);
    }
}
