//! Structured representation of PHP type expressions.
//!
//! This module provides [`PhpType`], an owned enum that represents PHP type
//! expressions as a tree. It is converted from the borrowed
//! `mago_type_syntax::ast::Type<'input>` AST and can be displayed back into a
//! canonical string form.
//!
//! # Design
//!
//! `mago_type_syntax::ast::Type` is `#[non_exhaustive]` with 69 variants and
//! borrows from input. `PhpType` is simpler: keyword types are collapsed into
//! `Named`, generic-parameterised references become `Generic`, and rarely-used
//! variants fall back to `Raw`.
//!
//! `PhpType::parse()` never fails. If the input cannot be parsed or mapped,
//! it returns `PhpType::Raw(input)`.

use std::fmt;

use mago_database::file::FileId;
use mago_span::{Position, Span};
use mago_type_syntax::ast;

// ---------------------------------------------------------------------------
// Data types
// ---------------------------------------------------------------------------

/// A structured, owned representation of a PHP type expression.
#[derive(Debug, Clone, PartialEq)]
pub enum PhpType {
    /// A named type: keywords (`int`, `string`, `mixed`, `void`, …),
    /// class references (`Foo\Bar`), or special names (`self`, `static`,
    /// `parent`). Also used for PHPDoc variable references (`$this`).
    Named(String),

    /// Nullable type: `?T`.
    Nullable(Box<PhpType>),

    /// Union type: `T|U|V`. Always contains two or more members.
    Union(Vec<PhpType>),

    /// Intersection type: `T&U`. Always contains two or more members.
    Intersection(Vec<PhpType>),

    /// Generic (parameterised) type: `Collection<int, User>`, `array<string>`,
    /// `list<int>`, `non-empty-array<string>`, `iterable<K, V>`, etc.
    Generic(String, Vec<PhpType>),

    /// The `T[]` slice syntax (sugar for `array<int, T>`).
    Array(Box<PhpType>),

    /// Array shape: `array{key: string, age?: int}`.
    ArrayShape(Vec<ShapeEntry>),

    /// Object shape: `object{name: string}`.
    ObjectShape(Vec<ShapeEntry>),

    /// Callable or Closure type with optional specification.
    /// `callable(int, string): bool`, `Closure(int): void`,
    /// `pure-callable(T): U`, `pure-Closure(T): U`.
    Callable {
        /// One of `"callable"`, `"Closure"`, `"pure-callable"`, `"pure-Closure"`.
        kind: String,
        /// Parameter types.
        params: Vec<CallableParam>,
        /// Optional return type.
        return_type: Option<Box<PhpType>>,
    },

    /// Conditional return type: `$x is T ? U : V`.
    Conditional {
        /// The subject (typically a variable like `$this`).
        param: String,
        /// Whether the condition is negated (`is not`).
        negated: bool,
        /// The condition type.
        condition: Box<PhpType>,
        /// The type when the condition is true.
        then_type: Box<PhpType>,
        /// The type when the condition is false.
        else_type: Box<PhpType>,
    },

    /// `class-string<T>` or bare `class-string`.
    ClassString(Option<Box<PhpType>>),

    /// `interface-string<T>` or bare `interface-string`.
    InterfaceString(Option<Box<PhpType>>),

    /// `key-of<T>`.
    KeyOf(Box<PhpType>),

    /// `value-of<T>`.
    ValueOf(Box<PhpType>),

    /// `int<min, max>` range type.
    IntRange(String, String),

    /// Index access type: `T[K]`.
    IndexAccess(Box<PhpType>, Box<PhpType>),

    /// A literal type: integer (`42`), float (`3.14`), or string (`'foo'`).
    Literal(String),

    /// Fallback for anything we cannot parse or do not yet map.
    Raw(String),
}

/// A single field in an array or object shape.
#[derive(Debug, Clone, PartialEq)]
pub struct ShapeEntry {
    /// The key name or integer index. `None` for positional (unkeyed) entries.
    pub key: Option<String>,
    /// The value type of this field.
    pub value_type: PhpType,
    /// Whether this field is optional (`key?: type`).
    pub optional: bool,
}

/// A single parameter in a callable type specification.
#[derive(Debug, Clone, PartialEq)]
pub struct CallableParam {
    /// The type of this parameter.
    pub type_hint: PhpType,
    /// Whether the parameter is optional (has `=`).
    pub optional: bool,
    /// Whether the parameter is variadic (`...`).
    pub variadic: bool,
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

impl PhpType {
    /// Parse a PHP type string into a structured [`PhpType`].
    ///
    /// This never fails. If the input cannot be parsed by `mago_type_syntax`,
    /// returns `PhpType::Raw(input)`.
    pub fn parse(input: &str) -> PhpType {
        if input.is_empty() {
            return PhpType::Raw(String::new());
        }

        let span = Span::new(
            FileId::zero(),
            Position::new(0),
            Position::new(input.len() as u32),
        );

        match mago_type_syntax::parse_str(span, input) {
            Ok(ty) => convert(&ty),
            Err(_) => PhpType::Raw(input.to_owned()),
        }
    }

    /// Produce a new `PhpType` with all class names resolved through
    /// the provided callback.
    ///
    /// The callback receives each class-like name (from `Named`,
    /// `Generic`, `ClassString`, etc.) and returns the resolved
    /// fully-qualified name. Names that are keywords/scalars are
    /// never passed to the callback.
    ///
    /// This replaces the character-by-character `resolve_type_string`
    /// function in `ast_update.rs` with a clean tree traversal.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// let ty = PhpType::parse("Collection<int, User>|null");
    /// let resolved = ty.resolve_names(&|name| {
    ///     use_map.get(name).cloned()
    ///         .unwrap_or_else(|| format!("App\\{}", name))
    /// });
    /// // → Generic("App\\Collection", [Named("int"), Named("App\\User")]) | Named("null")
    /// ```
    pub fn resolve_names(&self, resolver: &dyn Fn(&str) -> String) -> PhpType {
        match self {
            PhpType::Named(s) => {
                if is_keyword_type(s) {
                    PhpType::Named(s.clone())
                } else {
                    PhpType::Named(resolver(s))
                }
            }

            PhpType::Nullable(inner) => PhpType::Nullable(Box::new(inner.resolve_names(resolver))),

            PhpType::Union(types) => {
                PhpType::Union(types.iter().map(|t| t.resolve_names(resolver)).collect())
            }

            PhpType::Intersection(types) => {
                PhpType::Intersection(types.iter().map(|t| t.resolve_names(resolver)).collect())
            }

            PhpType::Generic(name, args) => {
                let resolved_name = if is_keyword_type(name) {
                    name.clone()
                } else {
                    resolver(name)
                };
                PhpType::Generic(
                    resolved_name,
                    args.iter().map(|a| a.resolve_names(resolver)).collect(),
                )
            }

            PhpType::Array(inner) => PhpType::Array(Box::new(inner.resolve_names(resolver))),

            PhpType::ArrayShape(entries) => PhpType::ArrayShape(
                entries
                    .iter()
                    .map(|e| ShapeEntry {
                        key: e.key.clone(),
                        value_type: e.value_type.resolve_names(resolver),
                        optional: e.optional,
                    })
                    .collect(),
            ),

            PhpType::ObjectShape(entries) => PhpType::ObjectShape(
                entries
                    .iter()
                    .map(|e| ShapeEntry {
                        key: e.key.clone(),
                        value_type: e.value_type.resolve_names(resolver),
                        optional: e.optional,
                    })
                    .collect(),
            ),

            PhpType::Callable {
                kind,
                params,
                return_type,
            } => PhpType::Callable {
                kind: if is_keyword_type(kind) {
                    kind.clone()
                } else {
                    resolver(kind)
                },
                params: params
                    .iter()
                    .map(|p| CallableParam {
                        type_hint: p.type_hint.resolve_names(resolver),
                        optional: p.optional,
                        variadic: p.variadic,
                    })
                    .collect(),
                return_type: return_type
                    .as_ref()
                    .map(|rt| Box::new(rt.resolve_names(resolver))),
            },

            PhpType::Conditional {
                param,
                negated,
                condition,
                then_type,
                else_type,
            } => PhpType::Conditional {
                param: param.clone(),
                negated: *negated,
                condition: Box::new(condition.resolve_names(resolver)),
                then_type: Box::new(then_type.resolve_names(resolver)),
                else_type: Box::new(else_type.resolve_names(resolver)),
            },

            PhpType::ClassString(inner) => {
                PhpType::ClassString(inner.as_ref().map(|i| Box::new(i.resolve_names(resolver))))
            }

            PhpType::InterfaceString(inner) => PhpType::InterfaceString(
                inner.as_ref().map(|i| Box::new(i.resolve_names(resolver))),
            ),

            PhpType::KeyOf(inner) => PhpType::KeyOf(Box::new(inner.resolve_names(resolver))),

            PhpType::ValueOf(inner) => PhpType::ValueOf(Box::new(inner.resolve_names(resolver))),

            PhpType::IntRange(min, max) => PhpType::IntRange(min.clone(), max.clone()),

            PhpType::IndexAccess(target, index) => PhpType::IndexAccess(
                Box::new(target.resolve_names(resolver)),
                Box::new(index.resolve_names(resolver)),
            ),

            PhpType::Literal(s) => PhpType::Literal(s.clone()),

            // Raw types can't be structurally resolved — pass through.
            PhpType::Raw(s) => PhpType::Raw(s.clone()),
        }
    }

    /// Return the short (unqualified) name from a potentially
    /// namespace-qualified type name. Returns only the part after the
    /// last `\`. Non-class types pass through unchanged.
    fn short_name_of(name: &str) -> &str {
        let t = name.trim();
        t.rsplit('\\').next().unwrap_or(t)
    }

    /// Produce a new `PhpType` with all namespace-qualified names
    /// shortened to their unqualified form.
    ///
    /// For example, `App\Models\User|null` becomes `User|null`, and
    /// `array<int, App\Models\User>` becomes `array<int, User>`.
    pub fn shorten(&self) -> PhpType {
        match self {
            PhpType::Named(s) => PhpType::Named(Self::short_name_of(s).to_owned()),

            PhpType::Nullable(inner) => PhpType::Nullable(Box::new(inner.shorten())),

            PhpType::Union(types) => PhpType::Union(types.iter().map(|t| t.shorten()).collect()),

            PhpType::Intersection(types) => {
                PhpType::Intersection(types.iter().map(|t| t.shorten()).collect())
            }

            PhpType::Generic(name, args) => PhpType::Generic(
                Self::short_name_of(name).to_owned(),
                args.iter().map(|a| a.shorten()).collect(),
            ),

            PhpType::Array(inner) => PhpType::Array(Box::new(inner.shorten())),

            PhpType::ArrayShape(entries) => PhpType::ArrayShape(
                entries
                    .iter()
                    .map(|e| ShapeEntry {
                        key: e.key.clone(),
                        value_type: e.value_type.shorten(),
                        optional: e.optional,
                    })
                    .collect(),
            ),

            PhpType::ObjectShape(entries) => PhpType::ObjectShape(
                entries
                    .iter()
                    .map(|e| ShapeEntry {
                        key: e.key.clone(),
                        value_type: e.value_type.shorten(),
                        optional: e.optional,
                    })
                    .collect(),
            ),

            PhpType::Callable {
                kind,
                params,
                return_type,
            } => PhpType::Callable {
                kind: kind.clone(),
                params: params
                    .iter()
                    .map(|p| CallableParam {
                        type_hint: p.type_hint.shorten(),
                        optional: p.optional,
                        variadic: p.variadic,
                    })
                    .collect(),
                return_type: return_type.as_ref().map(|rt| Box::new(rt.shorten())),
            },

            PhpType::Conditional {
                param,
                negated,
                condition,
                then_type,
                else_type,
            } => PhpType::Conditional {
                param: param.clone(),
                negated: *negated,
                condition: Box::new(condition.shorten()),
                then_type: Box::new(then_type.shorten()),
                else_type: Box::new(else_type.shorten()),
            },

            PhpType::ClassString(inner) => {
                PhpType::ClassString(inner.as_ref().map(|i| Box::new(i.shorten())))
            }

            PhpType::InterfaceString(inner) => {
                PhpType::InterfaceString(inner.as_ref().map(|i| Box::new(i.shorten())))
            }

            PhpType::KeyOf(inner) => PhpType::KeyOf(Box::new(inner.shorten())),

            PhpType::ValueOf(inner) => PhpType::ValueOf(Box::new(inner.shorten())),

            PhpType::IntRange(min, max) => PhpType::IntRange(min.clone(), max.clone()),

            PhpType::IndexAccess(target, index) => {
                PhpType::IndexAccess(Box::new(target.shorten()), Box::new(index.shorten()))
            }

            PhpType::Literal(s) => PhpType::Literal(s.clone()),

            PhpType::Raw(s) => {
                // Best-effort: apply the old string-based shortening
                // for raw types that we couldn't parse structurally.
                PhpType::Raw(s.clone())
            }
        }
    }

    /// Whether this type is a primitive scalar / built-in type that
    /// cannot have members accessed on it at runtime.
    ///
    /// Matches the narrow set of primitive PHP types:
    /// `int`, `float`, `string`, `bool`, `void`, `never`, `null`,
    /// `false`, `true`, `array`, `callable`, `iterable`, `resource`
    /// (and their aliases `integer`, `double`, `boolean`).
    ///
    /// Unlike [`is_scalar`], this does **not** include `mixed`, `object`,
    /// `class-string`, `self`, `static`, `parent`, or other PHPDoc
    /// pseudo-types on which member access may be valid.
    pub fn is_primitive_scalar(&self) -> bool {
        match self {
            PhpType::Named(s) => is_primitive_scalar_name(s),
            PhpType::Nullable(inner) => inner.is_primitive_scalar(),
            PhpType::Generic(name, _) => is_primitive_scalar_name(name),
            PhpType::Array(_) => true,
            PhpType::ArrayShape(_) => true,
            PhpType::Callable { .. } => true,
            PhpType::IntRange(_, _) => true,
            PhpType::Literal(_) => true,
            PhpType::Raw(_) => false,
            _ => false,
        }
    }

    /// Whether this type is a scalar/built-in type that does not refer
    /// to a user-defined class.
    ///
    /// Matches built-in PHP types and common PHPDoc pseudo-types like
    /// `mixed`, `class-string`, etc.
    pub fn is_scalar(&self) -> bool {
        match self {
            PhpType::Named(s) => is_scalar_name(s),
            PhpType::Nullable(inner) => inner.is_scalar(),
            PhpType::Generic(name, _) => is_scalar_name(name),
            PhpType::Array(_) => true,
            PhpType::ArrayShape(_) => true,
            PhpType::ObjectShape(_) => true,
            PhpType::Callable { .. } => true,
            PhpType::ClassString(_) => true,
            PhpType::InterfaceString(_) => true,
            PhpType::KeyOf(_) => true,
            PhpType::ValueOf(_) => true,
            PhpType::IntRange(_, _) => true,
            PhpType::Literal(_) => true,
            PhpType::Raw(_) => false,
            // Union, Intersection, Conditional, IndexAccess are
            // composite — not scalar by themselves.
            _ => false,
        }
    }

    /// Extract the base class name from a type, if it refers to a single
    /// named class (possibly with generic parameters).
    ///
    /// Returns `Some("User")` for `User`, `Collection<int, User>`,
    /// `?User`, etc. Returns `None` for unions, intersections, scalars,
    /// callables, shapes, and other non-class types.
    pub fn base_name(&self) -> Option<&str> {
        match self {
            PhpType::Named(s) if !is_scalar_name(s) => Some(s.as_str()),
            PhpType::Generic(name, _) if !is_scalar_name(name) => Some(name.as_str()),
            PhpType::Nullable(inner) => inner.base_name(),
            _ => None,
        }
    }

    /// Return the top-level union members if this is a union type,
    /// or a single-element slice containing `self` otherwise.
    ///
    /// This replaces `split_top_level_union` for structured types.
    pub fn union_members(&self) -> Vec<&PhpType> {
        match self {
            PhpType::Union(members) => members.iter().collect(),
            other => vec![other],
        }
    }

    /// Return the top-level intersection members if this is an intersection
    /// type, or a single-element slice containing `self` otherwise.
    pub fn intersection_members(&self) -> Vec<&PhpType> {
        match self {
            PhpType::Intersection(members) => members.iter().collect(),
            other => vec![other],
        }
    }

    /// Extract the "value" type from a generic iterable type.
    ///
    /// Returns the element type that iteration would yield as a value:
    ///   - `User[]`                        → `Some(Named("User"))`
    ///   - `list<User>`                    → `Some(Named("User"))`
    ///   - `array<int, User>`              → `Some(Named("User"))`
    ///   - `Collection<int, User>`         → `Some(Named("User"))`
    ///   - `Generator<int, User, …>`       → `Some(Named("User"))` (2nd param)
    ///   - `?list<User>`                   → `Some(Named("User"))`
    ///   - `int`                           → `None`
    ///
    /// When `skip_scalar` is true, returns `None` if the extracted type
    /// is a scalar (matching `extract_generic_value_type` behaviour for
    /// class-based completion). When false, returns any element type
    /// (matching `extract_iterable_element_type` behaviour).
    pub fn extract_value_type(&self, skip_scalar: bool) -> Option<&PhpType> {
        match self {
            PhpType::Array(inner) => {
                if skip_scalar && inner.is_scalar() {
                    None
                } else {
                    Some(inner.as_ref())
                }
            }
            PhpType::Generic(name, args) if !args.is_empty() => {
                let value = if Self::short_name_of(name) == "Generator" {
                    // Generator<TKey, TValue, TSend, TReturn>: value is
                    // the 2nd param (index 1). When only one param is
                    // given, treat it as the value type.
                    args.get(1).or(args.last())
                } else {
                    // Default: last generic parameter (works for array,
                    // list, iterable, Collection, etc.).
                    args.last()
                };
                match value {
                    Some(v) if skip_scalar && v.is_scalar() => None,
                    Some(v) => Some(v),
                    None => None,
                }
            }
            PhpType::Nullable(inner) => inner.extract_value_type(skip_scalar),
            _ => None,
        }
    }

    /// Extract the "key" type from a generic iterable type.
    ///
    /// Returns the key type only when the generic has 2+ parameters:
    ///   - `array<string, User>`  → `Some(Named("string"))`
    ///   - `array<int, User>`     → `Some(Named("int"))`
    ///   - `list<User>`           → `None` (single param → implicit int key)
    ///   - `User[]`               → `None` (shorthand → implicit int key)
    ///
    /// When `skip_scalar` is true, returns `None` if the key type is
    /// scalar (matching `extract_generic_key_type` behaviour).
    pub fn extract_key_type(&self, skip_scalar: bool) -> Option<&PhpType> {
        match self {
            PhpType::Generic(_, args) if args.len() >= 2 => {
                let key = &args[0];
                if skip_scalar && key.is_scalar() {
                    None
                } else {
                    Some(key)
                }
            }
            PhpType::Nullable(inner) => inner.extract_key_type(skip_scalar),
            _ => None,
        }
    }

    /// Extract the element (value) type from an iterable, including
    /// scalar element types.
    ///
    /// This is the `PhpType` equivalent of `extract_iterable_element_type`.
    /// Unlike `extract_value_type(true)`, this never skips scalars.
    pub fn extract_element_type(&self) -> Option<&PhpType> {
        self.extract_value_type(false)
    }

    /// Return the non-null part of a type.
    ///
    /// For a union like `User|null`, returns `Some(Named("User"))`.
    /// For `User|Admin|null`, returns `Some(Union([Named("User"), Named("Admin")]))`.
    /// For a type that doesn't contain `null`, returns `None`.
    /// For bare `null`, returns `None`.
    ///
    /// This replaces the `clean_type` pattern of extracting the
    /// non-null part from a union type.
    pub fn non_null_type(&self) -> Option<PhpType> {
        match self {
            PhpType::Nullable(inner) => Some(inner.as_ref().clone()),
            PhpType::Union(members) => {
                let non_null: Vec<&PhpType> = members
                    .iter()
                    .filter(|m| !matches!(m, PhpType::Named(s) if s == "null"))
                    .collect();
                match non_null.len() {
                    0 => None,
                    1 => Some(non_null[0].clone()),
                    _ => Some(PhpType::Union(non_null.into_iter().cloned().collect())),
                }
            }
            // Not a union or nullable — no null to strip.
            _ => None,
        }
    }

    /// Whether all non-null members of this type are scalar.
    ///
    /// For unions like `string|null`, returns `true`.
    /// For `User|null`, returns `false` (User is a class).
    /// For bare scalars like `int`, returns `true`.
    /// For bare classes like `User`, returns `false`.
    ///
    /// This replaces the `clean_type` + `is_scalar` pattern used in
    /// diagnostics to check whether a type is purely scalar.
    pub fn all_members_scalar(&self) -> bool {
        match self {
            PhpType::Union(members) => members
                .iter()
                .filter(|m| !matches!(m, PhpType::Named(s) if s == "null"))
                .all(|m| m.is_scalar()),
            PhpType::Nullable(inner) => inner.is_scalar(),
            other => other.is_scalar(),
        }
    }

    /// Like [`all_members_scalar`] but uses the narrow
    /// [`is_primitive_scalar`] check.
    ///
    /// Returns `true` only when every non-null member of the type is a
    /// primitive scalar (int, string, bool, float, array, void, never,
    /// etc.).  Returns `false` for `mixed`, `object`, `class-string`,
    /// and other pseudo-types on which member access may be valid.
    ///
    /// This is the correct replacement for the old
    /// `clean_type` + `is_scalar` pattern in `resolve_scalar_subject_type`.
    pub fn all_members_primitive_scalar(&self) -> bool {
        match self {
            PhpType::Union(members) => members
                .iter()
                .filter(|m| !matches!(m, PhpType::Named(s) if s == "null"))
                .all(|m| m.is_primitive_scalar()),
            PhpType::Nullable(inner) => inner.is_primitive_scalar(),
            other => other.is_primitive_scalar(),
        }
    }

    /// Produce a new `PhpType` with `self`, `static`, and `$this`
    /// replaced by the given class name.
    ///
    /// Walks the entire type tree and replaces any `Named("self")`,
    /// `Named("static")`, or `Named("$this")` with
    /// `Named(class_name)`.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// let ty = PhpType::parse("self|null");
    /// let replaced = ty.replace_self("App\\User");
    /// assert_eq!(replaced.to_string(), "App\\User | null");
    /// ```
    pub fn replace_self(&self, class_name: &str) -> PhpType {
        match self {
            PhpType::Named(s) if s == "self" || s == "static" || s == "$this" => {
                PhpType::Named(class_name.to_string())
            }

            PhpType::Named(_) | PhpType::Literal(_) | PhpType::Raw(_) => self.clone(),

            PhpType::Nullable(inner) => PhpType::Nullable(Box::new(inner.replace_self(class_name))),

            PhpType::Union(types) => {
                PhpType::Union(types.iter().map(|t| t.replace_self(class_name)).collect())
            }

            PhpType::Intersection(types) => {
                PhpType::Intersection(types.iter().map(|t| t.replace_self(class_name)).collect())
            }

            PhpType::Generic(name, args) => {
                let resolved_name = match name.as_str() {
                    "self" | "static" | "$this" => class_name.to_string(),
                    _ => name.clone(),
                };
                PhpType::Generic(
                    resolved_name,
                    args.iter().map(|a| a.replace_self(class_name)).collect(),
                )
            }

            PhpType::Array(inner) => PhpType::Array(Box::new(inner.replace_self(class_name))),

            PhpType::ArrayShape(entries) => PhpType::ArrayShape(
                entries
                    .iter()
                    .map(|e| ShapeEntry {
                        key: e.key.clone(),
                        value_type: e.value_type.replace_self(class_name),
                        optional: e.optional,
                    })
                    .collect(),
            ),

            PhpType::ObjectShape(entries) => PhpType::ObjectShape(
                entries
                    .iter()
                    .map(|e| ShapeEntry {
                        key: e.key.clone(),
                        value_type: e.value_type.replace_self(class_name),
                        optional: e.optional,
                    })
                    .collect(),
            ),

            PhpType::Callable {
                kind,
                params,
                return_type,
            } => PhpType::Callable {
                kind: kind.clone(),
                params: params
                    .iter()
                    .map(|p| CallableParam {
                        type_hint: p.type_hint.replace_self(class_name),
                        optional: p.optional,
                        variadic: p.variadic,
                    })
                    .collect(),
                return_type: return_type
                    .as_ref()
                    .map(|r| Box::new(r.replace_self(class_name))),
            },

            PhpType::Conditional {
                param,
                negated,
                condition,
                then_type,
                else_type,
            } => PhpType::Conditional {
                param: param.clone(),
                negated: *negated,
                condition: Box::new(condition.replace_self(class_name)),
                then_type: Box::new(then_type.replace_self(class_name)),
                else_type: Box::new(else_type.replace_self(class_name)),
            },

            PhpType::ClassString(inner) => {
                PhpType::ClassString(inner.as_ref().map(|t| Box::new(t.replace_self(class_name))))
            }

            PhpType::InterfaceString(inner) => PhpType::InterfaceString(
                inner.as_ref().map(|t| Box::new(t.replace_self(class_name))),
            ),

            PhpType::KeyOf(inner) => PhpType::KeyOf(Box::new(inner.replace_self(class_name))),

            PhpType::ValueOf(inner) => PhpType::ValueOf(Box::new(inner.replace_self(class_name))),

            PhpType::IntRange(lo, hi) => PhpType::IntRange(lo.clone(), hi.clone()),

            PhpType::IndexAccess(base, index) => PhpType::IndexAccess(
                Box::new(base.replace_self(class_name)),
                Box::new(index.replace_self(class_name)),
            ),
        }
    }

    /// Substitute template parameter names throughout this type tree.
    ///
    /// Walks the entire type tree and replaces any `Named(s)` node whose
    /// name appears as a key in `subs` with `PhpType::parse(replacement)`.
    /// All other nodes are recursively rebuilt with their children
    /// substituted.
    ///
    /// This is the structured-type equivalent of the string-surgery
    /// `apply_substitution` function in `inheritance.rs`.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// use std::collections::HashMap;
    /// let ty = PhpType::parse("Collection<TKey, TValue>");
    /// let subs: HashMap<String, String> =
    ///     [("TKey".into(), "int".into()), ("TValue".into(), "User".into())]
    ///         .into_iter().collect();
    /// let result = ty.substitute(&subs);
    /// assert_eq!(result.to_string(), "Collection<int, User>");
    /// ```
    pub fn substitute(&self, subs: &std::collections::HashMap<String, String>) -> PhpType {
        if subs.is_empty() {
            return self.clone();
        }
        match self {
            PhpType::Named(s) => {
                if let Some(replacement) = subs.get(s.as_str()) {
                    PhpType::parse(replacement)
                } else {
                    self.clone()
                }
            }

            PhpType::Literal(_) | PhpType::Raw(_) | PhpType::IntRange(_, _) => self.clone(),

            PhpType::Nullable(inner) => {
                let resolved = inner.substitute(subs);
                // If the substitution produced a union or nullable,
                // don't double-wrap.
                match &resolved {
                    PhpType::Nullable(_) => resolved,
                    PhpType::Union(members) => {
                        // Already nullable if it contains null
                        if members.iter().any(
                            |m| matches!(m, PhpType::Named(n) if n.eq_ignore_ascii_case("null")),
                        ) {
                            resolved
                        } else {
                            PhpType::Nullable(Box::new(resolved))
                        }
                    }
                    _ => PhpType::Nullable(Box::new(resolved)),
                }
            }

            PhpType::Union(types) => {
                let resolved: Vec<PhpType> = types.iter().map(|t| t.substitute(subs)).collect();
                // Flatten any nested unions produced by substitution.
                let mut flat = Vec::with_capacity(resolved.len());
                for t in resolved {
                    match t {
                        PhpType::Union(inner) => flat.extend(inner),
                        other => flat.push(other),
                    }
                }
                if flat.len() == 1 {
                    flat.into_iter().next().unwrap()
                } else {
                    PhpType::Union(flat)
                }
            }

            PhpType::Intersection(types) => {
                let resolved: Vec<PhpType> = types.iter().map(|t| t.substitute(subs)).collect();
                let mut flat = Vec::with_capacity(resolved.len());
                for t in resolved {
                    match t {
                        PhpType::Intersection(inner) => flat.extend(inner),
                        other => flat.push(other),
                    }
                }
                if flat.len() == 1 {
                    flat.into_iter().next().unwrap()
                } else {
                    PhpType::Intersection(flat)
                }
            }

            PhpType::Generic(name, args) => {
                // The base name might itself be a template parameter.
                let resolved_name = if let Some(replacement) = subs.get(name.as_str()) {
                    // If the replacement is a simple name, use it as the
                    // generic base. Otherwise, fall back to string form.
                    let parsed = PhpType::parse(replacement);
                    match parsed {
                        PhpType::Named(n) => n,
                        _ => replacement.clone(),
                    }
                } else {
                    name.clone()
                };
                PhpType::Generic(
                    resolved_name,
                    args.iter().map(|a| a.substitute(subs)).collect(),
                )
            }

            PhpType::Array(inner) => PhpType::Array(Box::new(inner.substitute(subs))),

            PhpType::ArrayShape(entries) => PhpType::ArrayShape(
                entries
                    .iter()
                    .map(|e| ShapeEntry {
                        key: e.key.clone(),
                        value_type: e.value_type.substitute(subs),
                        optional: e.optional,
                    })
                    .collect(),
            ),

            PhpType::ObjectShape(entries) => PhpType::ObjectShape(
                entries
                    .iter()
                    .map(|e| ShapeEntry {
                        key: e.key.clone(),
                        value_type: e.value_type.substitute(subs),
                        optional: e.optional,
                    })
                    .collect(),
            ),

            PhpType::Callable {
                kind,
                params,
                return_type,
            } => PhpType::Callable {
                kind: kind.clone(),
                params: params
                    .iter()
                    .map(|p| CallableParam {
                        type_hint: p.type_hint.substitute(subs),
                        optional: p.optional,
                        variadic: p.variadic,
                    })
                    .collect(),
                return_type: return_type.as_ref().map(|r| Box::new(r.substitute(subs))),
            },

            PhpType::Conditional {
                param,
                negated,
                condition,
                then_type,
                else_type,
            } => PhpType::Conditional {
                param: param.clone(),
                negated: *negated,
                condition: Box::new(condition.substitute(subs)),
                then_type: Box::new(then_type.substitute(subs)),
                else_type: Box::new(else_type.substitute(subs)),
            },

            PhpType::ClassString(inner) => {
                PhpType::ClassString(inner.as_ref().map(|t| Box::new(t.substitute(subs))))
            }

            PhpType::InterfaceString(inner) => {
                PhpType::InterfaceString(inner.as_ref().map(|t| Box::new(t.substitute(subs))))
            }

            PhpType::KeyOf(inner) => PhpType::KeyOf(Box::new(inner.substitute(subs))),

            PhpType::ValueOf(inner) => PhpType::ValueOf(Box::new(inner.substitute(subs))),

            PhpType::IndexAccess(base, index) => PhpType::IndexAccess(
                Box::new(base.substitute(subs)),
                Box::new(index.substitute(subs)),
            ),
        }
    }

    /// Extract all class-like names from this type, recursively.
    ///
    /// Walks the entire type tree and collects the base names of all
    /// class-like types (including those nested inside generics,
    /// callables, shapes, etc.). Scalar types, keywords, `null`,
    /// and literals are skipped.
    ///
    /// For `Collection<int, User>|null`, returns `["Collection", "User"]`.
    /// For `?User`, returns `["User"]`.
    /// For `int|string`, returns `[]`.
    pub fn extract_class_names(&self) -> Vec<String> {
        let mut names = Vec::new();
        self.collect_class_names(&mut names);
        names
    }

    /// Extract only top-level class names from this type.
    ///
    /// Unlike [`extract_class_names`], this does **not** recurse into
    /// generic type arguments, callable parameters, shape entries, or
    /// other nested positions. It returns only the outermost class
    /// names that are directly part of the type expression.
    ///
    /// For `Collection<int, User>|null`, returns `["Collection"]`.
    /// For `User|Admin`, returns `["User", "Admin"]`.
    /// For `?User`, returns `["User"]`.
    /// For `User[]`, returns `["User"]`.
    /// For `int|string`, returns `[]`.
    ///
    /// This is the correct replacement for the string-based
    /// `extract_class_names_from_type_string` in
    /// `definition/type_definition.rs`, where go-to-type-definition
    /// should jump to the container class, not its type arguments.
    pub fn top_level_class_names(&self) -> Vec<String> {
        let mut names = Vec::new();
        self.collect_top_level_class_names(&mut names);
        names
    }

    /// Recursive helper for [`extract_class_names`].
    fn collect_class_names(&self, names: &mut Vec<String>) {
        match self {
            PhpType::Named(s) => {
                if !is_keyword_type(s) && !s.is_empty() && !names.contains(s) {
                    names.push(s.clone());
                }
            }

            PhpType::Nullable(inner) => inner.collect_class_names(names),

            PhpType::Union(types) | PhpType::Intersection(types) => {
                for t in types {
                    t.collect_class_names(names);
                }
            }

            PhpType::Generic(name, args) => {
                if !is_keyword_type(name) && !name.is_empty() && !names.contains(name) {
                    names.push(name.clone());
                }
                for a in args {
                    a.collect_class_names(names);
                }
            }

            PhpType::Array(inner) => inner.collect_class_names(names),

            PhpType::ArrayShape(entries) | PhpType::ObjectShape(entries) => {
                for e in entries {
                    e.value_type.collect_class_names(names);
                }
            }

            PhpType::Callable {
                params,
                return_type,
                ..
            } => {
                for p in params {
                    p.type_hint.collect_class_names(names);
                }
                if let Some(ret) = return_type {
                    ret.collect_class_names(names);
                }
            }

            PhpType::ClassString(inner) => {
                if let Some(t) = inner {
                    t.collect_class_names(names);
                }
            }

            PhpType::InterfaceString(inner) => {
                if let Some(t) = inner {
                    t.collect_class_names(names);
                }
            }

            PhpType::KeyOf(inner) | PhpType::ValueOf(inner) => {
                inner.collect_class_names(names);
            }

            PhpType::IndexAccess(base, index) => {
                base.collect_class_names(names);
                index.collect_class_names(names);
            }

            PhpType::Conditional {
                condition,
                then_type,
                else_type,
                ..
            } => {
                condition.collect_class_names(names);
                then_type.collect_class_names(names);
                else_type.collect_class_names(names);
            }

            PhpType::Literal(_) | PhpType::Raw(_) | PhpType::IntRange(_, _) => {}
        }
    }

    /// Recursive helper for [`top_level_class_names`].
    ///
    /// Only descends through union, intersection, and nullable
    /// wrappers. Does not recurse into generic args, callable
    /// params/return, shapes, class-string inner types, etc.
    fn collect_top_level_class_names(&self, names: &mut Vec<String>) {
        match self {
            PhpType::Named(s) => {
                if !is_keyword_type(s) && !s.is_empty() && !names.contains(s) {
                    names.push(s.clone());
                }
            }

            PhpType::Nullable(inner) => inner.collect_top_level_class_names(names),

            PhpType::Union(types) | PhpType::Intersection(types) => {
                for t in types {
                    t.collect_top_level_class_names(names);
                }
            }

            // For generics, only the base name is top-level.
            // `Collection<int, User>` → `["Collection"]`.
            PhpType::Generic(name, _) => {
                if !is_keyword_type(name) && !name.is_empty() && !names.contains(name) {
                    names.push(name.clone());
                }
            }

            // `User[]` — the inner type is the top-level class.
            PhpType::Array(inner) => inner.collect_top_level_class_names(names),

            // Shapes, callables, class-string, key-of, value-of,
            // conditionals, literals, int-ranges — no navigable
            // top-level class name.
            _ => {}
        }
    }

    /// Check whether two `PhpType` values refer to the same type,
    /// ignoring namespace qualification differences.
    ///
    /// Returns `true` when the only difference is that one uses a
    /// fully-qualified class name (e.g. `App\Models\User`) while the
    /// other uses the short name (`User`). Handles unions, intersections,
    /// nullable types, and generic parameters.
    pub fn equivalent(&self, other: &PhpType) -> bool {
        if self == other {
            return true;
        }
        match (self, other) {
            (PhpType::Named(a), PhpType::Named(b)) => {
                Self::short_name_of(a) == Self::short_name_of(b)
            }
            (PhpType::Nullable(a), PhpType::Nullable(b)) => a.equivalent(b),
            // `?X` is equivalent to `X|null` — normalise Nullable to a
            // two-element Union before comparing so that both notations
            // are treated as identical.
            (PhpType::Nullable(inner), PhpType::Union(members))
            | (PhpType::Union(members), PhpType::Nullable(inner)) => {
                let as_union =
                    PhpType::Union(vec![inner.as_ref().clone(), PhpType::Named("null".into())]);
                as_union.equivalent(&PhpType::Union(members.clone()))
            }
            (PhpType::Union(a), PhpType::Union(b))
            | (PhpType::Intersection(a), PhpType::Intersection(b)) => {
                if a.len() != b.len() {
                    return false;
                }
                // Sort both sides by their shortened display form so
                // that `Foo|null` matches `null|Foo`.
                let mut sa: Vec<String> = a.iter().map(|t| t.shorten().to_string()).collect();
                let mut sb: Vec<String> = b.iter().map(|t| t.shorten().to_string()).collect();
                sa.sort_unstable();
                sb.sort_unstable();
                sa == sb
            }
            (PhpType::Generic(na, aa), PhpType::Generic(nb, ab)) => {
                Self::short_name_of(na) == Self::short_name_of(nb)
                    && aa.len() == ab.len()
                    && aa.iter().zip(ab.iter()).all(|(x, y)| x.equivalent(y))
            }
            (PhpType::Array(a), PhpType::Array(b)) => a.equivalent(b),
            _ => false,
        }
    }
}

/// Whether a type name is a keyword that should never be resolved as a
/// class name.
///
/// This is a superset of [`is_scalar_name`] that also includes PHPDoc-only
/// pseudo-types and special names that `resolve_type_string` skips.
fn is_keyword_type(name: &str) -> bool {
    if is_scalar_name(name) {
        return true;
    }
    matches!(
        name.to_ascii_lowercase().as_str(),
        // ── Integer refinements ─────────────────────────────────
        "non-zero-int"
            | "int-mask"
            | "int-mask-of"
            // ── String refinements ──────────────────────────────────
            | "literal-string"
            | "callable-string"
            | "uppercase-string"
            | "non-empty-uppercase-string"
            | "non-empty-literal-string"
            // ── Class-string variants ───────────────────────────────
            | "trait-string"
            | "enum-string"
            // ── Array / list refinements ────────────────────────────
            | "associative-array"
            // ── Scalar / mixed variants ─────────────────────────────
            | "empty-scalar"
            | "non-empty-scalar"
            | "non-empty-mixed"
            | "number"
            | "empty"
            // ── Object / callable variants ──────────────────────────
            | "callable-object"
            | "callable-array"
            // ── Resource variants ───────────────────────────────────
            | "closed-resource"
            | "open-resource"
            // ── Never aliases ───────────────────────────────────────
            | "no-return"
            | "noreturn"
            | "never-return"
            | "never-returns"
            // ── Key / value projection ──────────────────────────────
            | "key-of"
            | "value-of"
            // ── Special keywords ────────────────────────────────────
            | "class"
    )
}

/// Whether a type name refers to a scalar / built-in type.
/// Narrow primitive scalar check matching built-in PHP types.
fn is_primitive_scalar_name(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "int"
            | "integer"
            | "float"
            | "double"
            | "string"
            | "bool"
            | "boolean"
            | "void"
            | "never"
            | "null"
            | "false"
            | "true"
            | "array"
            | "callable"
            | "iterable"
            | "resource"
    )
}

fn is_scalar_name(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "int"
            | "integer"
            | "float"
            | "double"
            | "string"
            | "bool"
            | "boolean"
            | "void"
            | "never"
            | "null"
            | "false"
            | "true"
            | "array"
            | "callable"
            | "iterable"
            | "resource"
            | "mixed"
            | "object"
            | "self"
            | "static"
            | "parent"
            | "$this"
            | "class-string"
            | "interface-string"
            | "numeric-string"
            | "non-empty-string"
            | "non-empty-lowercase-string"
            | "lowercase-string"
            | "truthy-string"
            | "non-falsy-string"
            | "array-key"
            | "scalar"
            | "numeric"
            | "positive-int"
            | "negative-int"
            | "non-positive-int"
            | "non-negative-int"
            | "non-empty-array"
            | "non-empty-list"
            | "list"
    )
}

/// Convert a borrowed mago AST `Type` into an owned `PhpType`.
fn convert(ty: &ast::Type<'_>) -> PhpType {
    match ty {
        // -- Composite types --------------------------------------------------
        ast::Type::Union(_) => {
            let members = flatten_union(ty);
            PhpType::Union(members)
        }
        ast::Type::Intersection(_) => {
            let members = flatten_intersection(ty);
            PhpType::Intersection(members)
        }
        ast::Type::Nullable(n) => PhpType::Nullable(Box::new(convert(&n.inner))),
        ast::Type::Parenthesized(p) => convert(&p.inner),

        // -- Named / Reference types ------------------------------------------
        ast::Type::Reference(r) => {
            let name = r.identifier.value.to_string();
            match &r.parameters {
                Some(params) => {
                    let args: Vec<PhpType> =
                        params.entries.iter().map(|e| convert(&e.inner)).collect();
                    PhpType::Generic(name, args)
                }
                None => PhpType::Named(name),
            }
        }

        // -- Array-like types with optional generic parameters ----------------
        ast::Type::Array(a) => {
            convert_keyword_with_optional_generics(a.keyword.value, &a.parameters)
        }
        ast::Type::NonEmptyArray(a) => {
            convert_keyword_with_optional_generics(a.keyword.value, &a.parameters)
        }
        ast::Type::AssociativeArray(a) => {
            convert_keyword_with_optional_generics(a.keyword.value, &a.parameters)
        }
        ast::Type::List(l) => {
            convert_keyword_with_optional_generics(l.keyword.value, &l.parameters)
        }
        ast::Type::NonEmptyList(l) => {
            convert_keyword_with_optional_generics(l.keyword.value, &l.parameters)
        }
        ast::Type::Iterable(i) => {
            convert_keyword_with_optional_generics(i.keyword.value, &i.parameters)
        }

        // -- Slice: T[] -------------------------------------------------------
        ast::Type::Slice(s) => PhpType::Array(Box::new(convert(&s.inner))),

        // -- Shape types ------------------------------------------------------
        ast::Type::Shape(s) => {
            let entries: Vec<ShapeEntry> = s
                .fields
                .iter()
                .map(|field| {
                    let key = field.key.as_ref().map(|k| k.key.to_string());
                    let optional = field.is_optional();
                    let value_type = convert(&field.value);
                    ShapeEntry {
                        key,
                        value_type,
                        optional,
                    }
                })
                .collect();

            match s.kind {
                ast::ShapeTypeKind::Array
                | ast::ShapeTypeKind::NonEmptyArray
                | ast::ShapeTypeKind::AssociativeArray
                | ast::ShapeTypeKind::List
                | ast::ShapeTypeKind::NonEmptyList => PhpType::ArrayShape(entries),
            }
        }

        // -- Object type (with optional shape) --------------------------------
        ast::Type::Object(o) => match &o.properties {
            Some(props) => {
                let entries: Vec<ShapeEntry> = props
                    .fields
                    .iter()
                    .map(|field| {
                        let key = field.key.as_ref().map(|k| k.key.to_string());
                        let optional = field.is_optional();
                        let value_type = convert(&field.value);
                        ShapeEntry {
                            key,
                            value_type,
                            optional,
                        }
                    })
                    .collect();
                PhpType::ObjectShape(entries)
            }
            None => PhpType::Named("object".to_owned()),
        },

        // -- Callable types ---------------------------------------------------
        ast::Type::Callable(c) => {
            let kind = c.keyword.value.to_string();
            match &c.specification {
                Some(spec) => {
                    let params: Vec<CallableParam> = spec
                        .parameters
                        .entries
                        .iter()
                        .map(|p| {
                            let type_hint = match &p.parameter_type {
                                Some(t) => convert(t),
                                None => PhpType::Named("mixed".to_owned()),
                            };
                            CallableParam {
                                type_hint,
                                optional: p.is_optional(),
                                variadic: p.is_variadic(),
                            }
                        })
                        .collect();
                    let return_type = spec
                        .return_type
                        .as_ref()
                        .map(|rt| Box::new(convert(&rt.return_type)));
                    PhpType::Callable {
                        kind,
                        params,
                        return_type,
                    }
                }
                None => PhpType::Named(kind),
            }
        }

        // -- Conditional types ------------------------------------------------
        ast::Type::Conditional(c) => PhpType::Conditional {
            param: c.subject.to_string(),
            negated: c.is_negated(),
            condition: Box::new(convert(&c.target)),
            then_type: Box::new(convert(&c.then)),
            else_type: Box::new(convert(&c.otherwise)),
        },

        // -- class-string / interface-string ----------------------------------
        ast::Type::ClassString(c) => {
            let inner = c
                .parameter
                .as_ref()
                .map(|p| Box::new(convert(&p.entry.inner)));
            PhpType::ClassString(inner)
        }
        ast::Type::InterfaceString(i) => {
            let inner = i
                .parameter
                .as_ref()
                .map(|p| Box::new(convert(&p.entry.inner)));
            PhpType::InterfaceString(inner)
        }

        // -- key-of / value-of ------------------------------------------------
        ast::Type::KeyOf(k) => PhpType::KeyOf(Box::new(convert(&k.parameter.entry.inner))),
        ast::Type::ValueOf(v) => PhpType::ValueOf(Box::new(convert(&v.parameter.entry.inner))),

        // -- int range --------------------------------------------------------
        ast::Type::IntRange(r) => PhpType::IntRange(r.min.to_string(), r.max.to_string()),

        // -- Index access: T[K] -----------------------------------------------
        ast::Type::IndexAccess(i) => {
            PhpType::IndexAccess(Box::new(convert(&i.target)), Box::new(convert(&i.index)))
        }

        // -- Variable (e.g. $this in conditional types) -----------------------
        ast::Type::Variable(v) => PhpType::Named(v.value.to_string()),

        // -- Literal types ----------------------------------------------------
        ast::Type::LiteralInt(l) => PhpType::Literal(l.raw.to_string()),
        ast::Type::LiteralFloat(l) => PhpType::Literal(l.raw.to_string()),
        ast::Type::LiteralString(l) => PhpType::Literal(l.raw.to_string()),

        // -- Negated / Posited literals (e.g. -42, +42) -----------------------
        ast::Type::Negated(n) => PhpType::Literal(format!("-{}", n.number)),
        ast::Type::Posited(p) => PhpType::Literal(format!("+{}", p.number)),

        // -- Keyword types → Named -------------------------------------------
        ast::Type::Mixed(k)
        | ast::Type::NonEmptyMixed(k)
        | ast::Type::Null(k)
        | ast::Type::Void(k)
        | ast::Type::Never(k)
        | ast::Type::Resource(k)
        | ast::Type::ClosedResource(k)
        | ast::Type::OpenResource(k)
        | ast::Type::True(k)
        | ast::Type::False(k)
        | ast::Type::Bool(k)
        | ast::Type::Float(k)
        | ast::Type::Int(k)
        | ast::Type::PositiveInt(k)
        | ast::Type::NegativeInt(k)
        | ast::Type::NonPositiveInt(k)
        | ast::Type::NonNegativeInt(k)
        | ast::Type::String(k)
        | ast::Type::StringableObject(k)
        | ast::Type::ArrayKey(k)
        | ast::Type::Numeric(k)
        | ast::Type::Scalar(k)
        | ast::Type::NumericString(k)
        | ast::Type::NonEmptyString(k)
        | ast::Type::NonEmptyLowercaseString(k)
        | ast::Type::LowercaseString(k)
        | ast::Type::NonEmptyUppercaseString(k)
        | ast::Type::UppercaseString(k)
        | ast::Type::TruthyString(k)
        | ast::Type::NonFalsyString(k)
        | ast::Type::UnspecifiedLiteralInt(k)
        | ast::Type::UnspecifiedLiteralString(k)
        | ast::Type::UnspecifiedLiteralFloat(k)
        | ast::Type::NonEmptyUnspecifiedLiteralString(k) => PhpType::Named(k.value.to_string()),

        // -- Catch-all for anything else (non_exhaustive) ---------------------
        other => PhpType::Raw(other.to_string()),
    }
}

/// Convert a keyword type that has optional generic parameters (like
/// `array`, `array<int>`, `list<string>`, `non-empty-array<int, string>`,
/// `iterable<K, V>`).
fn convert_keyword_with_optional_generics(
    keyword: &str,
    parameters: &Option<ast::GenericParameters<'_>>,
) -> PhpType {
    match parameters {
        Some(params) => {
            let args: Vec<PhpType> = params.entries.iter().map(|e| convert(&e.inner)).collect();
            PhpType::Generic(keyword.to_string(), args)
        }
        None => PhpType::Named(keyword.to_string()),
    }
}

/// Recursively flatten a left-leaning binary union tree into a flat `Vec`.
fn flatten_union(ty: &ast::Type<'_>) -> Vec<PhpType> {
    match ty {
        ast::Type::Union(u) => {
            let mut types = flatten_union(&u.left);
            types.extend(flatten_union(&u.right));
            types
        }
        other => vec![convert(other)],
    }
}

/// Recursively flatten a left-leaning binary intersection tree into a flat `Vec`.
fn flatten_intersection(ty: &ast::Type<'_>) -> Vec<PhpType> {
    match ty {
        ast::Type::Intersection(i) => {
            let mut types = flatten_intersection(&i.left);
            types.extend(flatten_intersection(&i.right));
            types
        }
        other => vec![convert(other)],
    }
}

// ---------------------------------------------------------------------------
// Display
// ---------------------------------------------------------------------------

impl fmt::Display for PhpType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PhpType::Named(s) => write!(f, "{s}"),

            PhpType::Nullable(inner) => write!(f, "?{inner}"),

            PhpType::Union(types) => {
                for (i, ty) in types.iter().enumerate() {
                    if i > 0 {
                        write!(f, "|")?;
                    }
                    write!(f, "{ty}")?;
                }
                Ok(())
            }

            PhpType::Intersection(types) => {
                for (i, ty) in types.iter().enumerate() {
                    if i > 0 {
                        write!(f, "&")?;
                    }
                    write!(f, "{ty}")?;
                }
                Ok(())
            }

            PhpType::Generic(name, args) => {
                write!(f, "{name}<")?;
                for (i, arg) in args.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{arg}")?;
                }
                write!(f, ">")
            }

            PhpType::Array(inner) => write!(f, "{inner}[]"),

            PhpType::ArrayShape(entries) => {
                write!(f, "array{{")?;
                for (i, entry) in entries.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{entry}")?;
                }
                write!(f, "}}")
            }

            PhpType::ObjectShape(entries) => {
                write!(f, "object{{")?;
                for (i, entry) in entries.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{entry}")?;
                }
                write!(f, "}}")
            }

            PhpType::Callable {
                kind,
                params,
                return_type,
            } => {
                write!(f, "{kind}(")?;
                for (i, param) in params.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{param}")?;
                }
                write!(f, ")")?;
                if let Some(ret) = return_type {
                    write!(f, ": {ret}")?;
                }
                Ok(())
            }

            PhpType::Conditional {
                param,
                negated,
                condition,
                then_type,
                else_type,
            } => {
                if *negated {
                    write!(f, "{param} is not {condition} ? {then_type} : {else_type}")
                } else {
                    write!(f, "{param} is {condition} ? {then_type} : {else_type}")
                }
            }

            PhpType::ClassString(inner) => match inner {
                Some(ty) => write!(f, "class-string<{ty}>"),
                None => write!(f, "class-string"),
            },

            PhpType::InterfaceString(inner) => match inner {
                Some(ty) => write!(f, "interface-string<{ty}>"),
                None => write!(f, "interface-string"),
            },

            PhpType::KeyOf(inner) => write!(f, "key-of<{inner}>"),

            PhpType::ValueOf(inner) => write!(f, "value-of<{inner}>"),

            PhpType::IntRange(min, max) => write!(f, "int<{min}..{max}>"),

            PhpType::IndexAccess(target, index) => write!(f, "{target}[{index}]"),

            PhpType::Literal(s) => write!(f, "{s}"),

            PhpType::Raw(s) => write!(f, "{s}"),
        }
    }
}

impl fmt::Display for ShapeEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.key {
            Some(key) => {
                let opt = if self.optional { "?" } else { "" };
                write!(f, "{key}{opt}: {}", self.value_type)
            }
            None => write!(f, "{}", self.value_type),
        }
    }
}

impl fmt::Display for CallableParam {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.type_hint)?;
        if self.optional {
            write!(f, "=")?;
        } else if self.variadic {
            write!(f, "...")?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse a type string with mago to get the canonical display form,
    /// then parse with `PhpType::parse()` and verify our Display matches.
    ///
    /// For types where mago's `Display` has a known bug (double angle
    /// brackets on `class-string`, `interface-string`, `key-of`,
    /// `value-of`), use [`assert_round_trip_expected`] instead.
    fn assert_round_trip(input: &str) {
        // First, get mago's canonical output.
        let span = Span::new(
            FileId::zero(),
            Position::new(0),
            Position::new(input.len() as u32),
        );
        let mago_canonical = match mago_type_syntax::parse_str(span, input) {
            Ok(ty) => ty.to_string(),
            Err(_) => {
                // If mago can't parse it, PhpType should produce Raw.
                let php_type = PhpType::parse(input);
                assert_eq!(
                    php_type,
                    PhpType::Raw(input.to_owned()),
                    "Unparseable input should become Raw"
                );
                return;
            }
        };

        let php_type = PhpType::parse(input);
        let our_output = php_type.to_string();
        assert_eq!(
            our_output, mago_canonical,
            "Round-trip mismatch for input: {input:?}\n  PhpType:  {php_type:?}\n  ours:     {our_output:?}\n  mago:     {mago_canonical:?}"
        );
    }

    /// Like [`assert_round_trip`] but compares against an explicit expected
    /// string instead of mago's `Display` output.  Used to work around a
    /// mago Display bug where `SingleGenericParameter` wraps the entry in
    /// `<>` and then the parent type adds another pair, producing double
    /// angle brackets (e.g. `class-string<<Foo>>`).
    fn assert_round_trip_expected(input: &str, expected: &str) {
        let php_type = PhpType::parse(input);
        let our_output = php_type.to_string();
        assert_eq!(
            our_output, expected,
            "Round-trip mismatch for input: {input:?}\n  PhpType:  {php_type:?}\n  ours:     {our_output:?}\n  expected: {expected:?}"
        );
    }

    #[test]
    fn round_trip_keywords() {
        let keywords = [
            "int",
            "string",
            "bool",
            "float",
            "mixed",
            "void",
            "null",
            "never",
            "true",
            "false",
            "object",
            "array",
            "callable",
            "iterable",
            "self",
            "static",
            "parent",
            "resource",
            "positive-int",
            "negative-int",
            "non-empty-string",
            "numeric-string",
            "array-key",
        ];
        for kw in keywords {
            assert_round_trip(kw);
        }
    }

    #[test]
    fn round_trip_nullable() {
        assert_round_trip("?int");
        assert_round_trip("?string");
        assert_round_trip("?Foo");
    }

    #[test]
    fn round_trip_union() {
        // mago Display uses spaced unions (`int | string`); we prefer
        // the PHP convention (`int|string`).
        assert_round_trip_expected("int|string", "int|string");
        assert_round_trip_expected("int|string|null", "int|string|null");
        assert_round_trip_expected("Foo|Bar|null", "Foo|Bar|null");
        assert_round_trip_expected("int|null", "int|null");
    }

    #[test]
    fn round_trip_intersection() {
        // mago Display uses spaced intersections (`Countable & Traversable`);
        // we prefer the PHP convention (`Countable&Traversable`).
        assert_round_trip_expected("Countable&Traversable", "Countable&Traversable");
    }

    #[test]
    fn round_trip_generics() {
        assert_round_trip("array<int, string>");
        assert_round_trip("array<string>");
        assert_round_trip("Collection<int, User>");
        assert_round_trip("list<int>");
        assert_round_trip("non-empty-list<string>");
        assert_round_trip("non-empty-array<string>");
    }

    #[test]
    fn round_trip_class_references() {
        assert_round_trip("Foo\\Bar");
        assert_round_trip("\\Foo\\Bar");
    }

    #[test]
    fn round_trip_shapes() {
        assert_round_trip("array{name: string, age: int}");
        assert_round_trip("array{0: string, 1: int}");
        assert_round_trip("array{name?: string}");
        assert_round_trip("object{name: string}");
    }

    #[test]
    fn round_trip_callables() {
        assert_round_trip("callable(int, string): bool");
        assert_round_trip("Closure(int): void");
        assert_round_trip("Closure(int, string): void");
        assert_round_trip("callable(): void");
    }

    #[test]
    fn round_trip_class_string() {
        // mago Display bug: class-string<Foo> → class-string<<Foo>>
        assert_round_trip_expected("class-string<Foo>", "class-string<Foo>");
        assert_round_trip("class-string");
    }

    #[test]
    fn round_trip_interface_string() {
        // mago Display bug: interface-string<Foo> → interface-string<<Foo>>
        assert_round_trip_expected("interface-string<Foo>", "interface-string<Foo>");
    }

    #[test]
    fn round_trip_key_of_value_of() {
        // mago Display bug: key-of<T> → key-of<<T>>, value-of<T> → value-of<<T>>
        assert_round_trip_expected("key-of<T>", "key-of<T>");
        assert_round_trip_expected("value-of<T>", "value-of<T>");
    }

    #[test]
    fn round_trip_int_range() {
        assert_round_trip("int<0, 100>");
        assert_round_trip("int<min, max>");
        assert_round_trip("int<0, max>");
    }

    #[test]
    fn round_trip_slice() {
        assert_round_trip("Foo[]");
    }

    #[test]
    fn round_trip_literals() {
        assert_round_trip("42");
        assert_round_trip("'foo'");
    }

    #[test]
    fn round_trip_conditional() {
        assert_round_trip("$this is string ? int : float");
    }

    #[test]
    fn round_trip_member_reference() {
        assert_round_trip("Foo::BAR");
        assert_round_trip("Foo::*");
    }

    #[test]
    fn parse_empty_returns_raw() {
        assert_eq!(PhpType::parse(""), PhpType::Raw(String::new()));
    }

    #[test]
    fn parse_garbage_returns_raw() {
        let php_type = PhpType::parse("|||");
        assert!(matches!(php_type, PhpType::Raw(_)));
    }

    #[test]
    fn union_is_flattened() {
        let ty = PhpType::parse("int|string|null");
        match ty {
            PhpType::Union(members) => {
                assert_eq!(members.len(), 3);
                assert_eq!(members[0], PhpType::Named("int".to_owned()));
                assert_eq!(members[1], PhpType::Named("string".to_owned()));
                assert_eq!(members[2], PhpType::Named("null".to_owned()));
            }
            other => panic!("Expected Union, got {other:?}"),
        }
    }

    #[test]
    fn intersection_is_flattened() {
        let ty = PhpType::parse("A&B&C");
        match ty {
            PhpType::Intersection(members) => {
                assert_eq!(members.len(), 3);
                assert_eq!(members[0], PhpType::Named("A".to_owned()));
                assert_eq!(members[1], PhpType::Named("B".to_owned()));
                assert_eq!(members[2], PhpType::Named("C".to_owned()));
            }
            other => panic!("Expected Intersection, got {other:?}"),
        }
    }

    #[test]
    fn generic_with_params() {
        let ty = PhpType::parse("array<int, string>");
        match ty {
            PhpType::Generic(name, args) => {
                assert_eq!(name, "array");
                assert_eq!(args.len(), 2);
                assert_eq!(args[0], PhpType::Named("int".to_owned()));
                assert_eq!(args[1], PhpType::Named("string".to_owned()));
            }
            other => panic!("Expected Generic, got {other:?}"),
        }
    }

    #[test]
    fn class_string_with_param() {
        let ty = PhpType::parse("class-string<Foo>");
        match ty {
            PhpType::ClassString(Some(inner)) => {
                assert_eq!(*inner, PhpType::Named("Foo".to_owned()));
            }
            other => panic!("Expected ClassString(Some), got {other:?}"),
        }
    }

    #[test]
    fn nullable_structure() {
        let ty = PhpType::parse("?int");
        match ty {
            PhpType::Nullable(inner) => {
                assert_eq!(*inner, PhpType::Named("int".to_owned()));
            }
            other => panic!("Expected Nullable, got {other:?}"),
        }
    }

    #[test]
    fn callable_structure() {
        let ty = PhpType::parse("callable(int, string): bool");
        match ty {
            PhpType::Callable {
                kind,
                params,
                return_type,
            } => {
                assert_eq!(kind, "callable");
                assert_eq!(params.len(), 2);
                assert_eq!(params[0].type_hint, PhpType::Named("int".to_owned()));
                assert_eq!(params[1].type_hint, PhpType::Named("string".to_owned()));
                assert_eq!(
                    return_type,
                    Some(Box::new(PhpType::Named("bool".to_owned())))
                );
            }
            other => panic!("Expected Callable, got {other:?}"),
        }
    }

    #[test]
    fn shape_structure() {
        let ty = PhpType::parse("array{name: string, age?: int}");
        match ty {
            PhpType::ArrayShape(entries) => {
                assert_eq!(entries.len(), 2);
                assert_eq!(entries[0].key, Some("name".to_owned()));
                assert_eq!(entries[0].value_type, PhpType::Named("string".to_owned()));
                assert!(!entries[0].optional);
                assert_eq!(entries[1].key, Some("age".to_owned()));
                assert_eq!(entries[1].value_type, PhpType::Named("int".to_owned()));
                assert!(entries[1].optional);
            }
            other => panic!("Expected ArrayShape, got {other:?}"),
        }
    }

    #[test]
    fn object_shape_structure() {
        let ty = PhpType::parse("object{name: string}");
        match ty {
            PhpType::ObjectShape(entries) => {
                assert_eq!(entries.len(), 1);
                assert_eq!(entries[0].key, Some("name".to_owned()));
                assert_eq!(entries[0].value_type, PhpType::Named("string".to_owned()));
            }
            other => panic!("Expected ObjectShape, got {other:?}"),
        }
    }

    #[test]
    fn slice_structure() {
        let ty = PhpType::parse("Foo[]");
        match ty {
            PhpType::Array(inner) => {
                assert_eq!(*inner, PhpType::Named("Foo".to_owned()));
            }
            other => panic!("Expected Array (slice), got {other:?}"),
        }
    }

    #[test]
    fn conditional_structure() {
        let ty = PhpType::parse("$this is string ? int : float");
        match ty {
            PhpType::Conditional {
                param,
                negated,
                condition,
                then_type,
                else_type,
            } => {
                assert_eq!(param, "$this");
                assert!(!negated);
                assert_eq!(*condition, PhpType::Named("string".to_owned()));
                assert_eq!(*then_type, PhpType::Named("int".to_owned()));
                assert_eq!(*else_type, PhpType::Named("float".to_owned()));
            }
            other => panic!("Expected Conditional, got {other:?}"),
        }
    }

    #[test]
    fn int_range_structure() {
        let ty = PhpType::parse("int<0, 100>");
        match ty {
            PhpType::IntRange(min, max) => {
                assert_eq!(min, "0");
                assert_eq!(max, "100");
            }
            other => panic!("Expected IntRange, got {other:?}"),
        }
    }

    #[test]
    fn key_of_structure() {
        let ty = PhpType::parse("key-of<T>");
        match ty {
            PhpType::KeyOf(inner) => {
                assert_eq!(*inner, PhpType::Named("T".to_owned()));
            }
            other => panic!("Expected KeyOf, got {other:?}"),
        }
    }

    #[test]
    fn value_of_structure() {
        let ty = PhpType::parse("value-of<T>");
        match ty {
            PhpType::ValueOf(inner) => {
                assert_eq!(*inner, PhpType::Named("T".to_owned()));
            }
            other => panic!("Expected ValueOf, got {other:?}"),
        }
    }

    #[test]
    fn literal_int() {
        let ty = PhpType::parse("42");
        assert_eq!(ty, PhpType::Literal("42".to_owned()));
    }

    #[test]
    fn literal_string() {
        let ty = PhpType::parse("'foo'");
        assert_eq!(ty, PhpType::Literal("'foo'".to_owned()));
    }

    // ─── extract_value_type tests ───────────────────────────────────────────

    #[test]
    fn extract_value_type_array_slice() {
        let ty = PhpType::parse("User[]");
        let val = ty.extract_value_type(true).unwrap();
        assert_eq!(*val, PhpType::Named("User".to_owned()));
    }

    #[test]
    fn extract_value_type_array_slice_scalar_skipped() {
        let ty = PhpType::parse("int[]");
        assert!(ty.extract_value_type(true).is_none());
    }

    #[test]
    fn extract_value_type_array_slice_scalar_not_skipped() {
        let ty = PhpType::parse("int[]");
        let val = ty.extract_value_type(false).unwrap();
        assert_eq!(*val, PhpType::Named("int".to_owned()));
    }

    #[test]
    fn extract_value_type_list() {
        let ty = PhpType::parse("list<User>");
        let val = ty.extract_value_type(true).unwrap();
        assert_eq!(*val, PhpType::Named("User".to_owned()));
    }

    #[test]
    fn extract_value_type_array_two_params() {
        let ty = PhpType::parse("array<int, User>");
        let val = ty.extract_value_type(true).unwrap();
        assert_eq!(*val, PhpType::Named("User".to_owned()));
    }

    #[test]
    fn extract_value_type_collection() {
        let ty = PhpType::parse("Collection<int, User>");
        let val = ty.extract_value_type(true).unwrap();
        assert_eq!(*val, PhpType::Named("User".to_owned()));
    }

    #[test]
    fn extract_value_type_generator() {
        // Generator<TKey, TValue, TSend, TReturn> — value is 2nd param
        let ty = PhpType::parse("Generator<int, User, mixed, void>");
        let val = ty.extract_value_type(true).unwrap();
        assert_eq!(*val, PhpType::Named("User".to_owned()));
    }

    #[test]
    fn extract_value_type_generator_single_param() {
        // Single-param Generator<User> — treated as value type
        let ty = PhpType::parse("Generator<User>");
        let val = ty.extract_value_type(true).unwrap();
        assert_eq!(*val, PhpType::Named("User".to_owned()));
    }

    #[test]
    fn extract_value_type_nullable() {
        let ty = PhpType::parse("?list<User>");
        let val = ty.extract_value_type(true).unwrap();
        assert_eq!(*val, PhpType::Named("User".to_owned()));
    }

    #[test]
    fn extract_value_type_scalar_returns_none() {
        let ty = PhpType::Named("int".to_owned());
        assert!(ty.extract_value_type(true).is_none());
    }

    #[test]
    fn extract_value_type_plain_class_returns_none() {
        let ty = PhpType::Named("User".to_owned());
        assert!(ty.extract_value_type(true).is_none());
    }

    // ─── extract_key_type tests ─────────────────────────────────────────────

    #[test]
    fn extract_key_type_two_params() {
        let ty = PhpType::parse("array<string, User>");
        let key = ty.extract_key_type(false).unwrap();
        assert_eq!(*key, PhpType::Named("string".to_owned()));
    }

    #[test]
    fn extract_key_type_scalar_skipped() {
        let ty = PhpType::parse("array<int, User>");
        assert!(ty.extract_key_type(true).is_none());
    }

    #[test]
    fn extract_key_type_single_param_returns_none() {
        let ty = PhpType::parse("list<User>");
        assert!(ty.extract_key_type(false).is_none());
    }

    #[test]
    fn extract_key_type_slice_returns_none() {
        let ty = PhpType::parse("User[]");
        assert!(ty.extract_key_type(false).is_none());
    }

    #[test]
    fn extract_key_type_class_key() {
        let ty = PhpType::parse("array<Request, Response>");
        let key = ty.extract_key_type(true).unwrap();
        assert_eq!(*key, PhpType::Named("Request".to_owned()));
    }

    // ─── non_null_type tests ────────────────────────────────────────────────

    #[test]
    fn non_null_type_nullable() {
        let ty = PhpType::parse("?User");
        let non_null = ty.non_null_type().unwrap();
        assert_eq!(non_null, PhpType::Named("User".to_owned()));
    }

    #[test]
    fn non_null_type_union_with_null() {
        let ty = PhpType::parse("User|null");
        let non_null = ty.non_null_type().unwrap();
        assert_eq!(non_null, PhpType::Named("User".to_owned()));
    }

    #[test]
    fn non_null_type_union_multiple_non_null() {
        let ty = PhpType::parse("User|Admin|null");
        let non_null = ty.non_null_type().unwrap();
        match non_null {
            PhpType::Union(members) => {
                assert_eq!(members.len(), 2);
                assert_eq!(members[0], PhpType::Named("User".to_owned()));
                assert_eq!(members[1], PhpType::Named("Admin".to_owned()));
            }
            other => panic!("Expected Union, got {other:?}"),
        }
    }

    #[test]
    fn non_null_type_no_null() {
        let ty = PhpType::Named("User".to_owned());
        assert!(ty.non_null_type().is_none());
    }

    #[test]
    fn non_null_type_bare_null() {
        let ty = PhpType::Named("null".to_owned());
        assert!(ty.non_null_type().is_none());
    }

    // ─── all_members_scalar tests ───────────────────────────────────────────

    #[test]
    fn all_members_scalar_int() {
        assert!(PhpType::Named("int".to_owned()).all_members_scalar());
    }

    #[test]
    fn all_members_scalar_string_or_null() {
        assert!(PhpType::parse("string|null").all_members_scalar());
    }

    #[test]
    fn all_members_scalar_nullable_int() {
        assert!(PhpType::parse("?int").all_members_scalar());
    }

    #[test]
    fn all_members_scalar_class() {
        assert!(!PhpType::Named("User".to_owned()).all_members_scalar());
    }

    #[test]
    fn all_members_scalar_class_or_null() {
        assert!(!PhpType::parse("User|null").all_members_scalar());
    }

    #[test]
    fn all_members_scalar_mixed_union() {
        assert!(!PhpType::parse("int|User").all_members_scalar());
    }

    // ─── intersection_members tests ─────────────────────────────────────────

    #[test]
    fn intersection_members_of_intersection() {
        let ty = PhpType::parse("Countable&Traversable");
        let members = ty.intersection_members();
        assert_eq!(members.len(), 2);
    }

    #[test]
    fn intersection_members_of_non_intersection() {
        let ty = PhpType::Named("User".to_owned());
        let members = ty.intersection_members();
        assert_eq!(members.len(), 1);
        assert_eq!(*members[0], PhpType::Named("User".to_owned()));
    }

    // ─── resolve_names tests ────────────────────────────────────────────────

    #[test]
    fn resolve_names_simple_class() {
        let ty = PhpType::Named("User".to_owned());
        let resolved = ty.resolve_names(&|name| format!("App\\Models\\{}", name));
        assert_eq!(resolved.to_string(), "App\\Models\\User");
    }

    #[test]
    fn resolve_names_scalar_untouched() {
        let ty = PhpType::Named("int".to_owned());
        let resolved = ty.resolve_names(&|name| format!("App\\{}", name));
        assert_eq!(resolved.to_string(), "int");
    }

    #[test]
    fn resolve_names_union() {
        let ty = PhpType::parse("User|null");
        let resolved = ty.resolve_names(&|name| format!("App\\{}", name));
        assert_eq!(resolved.to_string(), "App\\User|null");
    }

    #[test]
    fn resolve_names_generic() {
        let ty = PhpType::parse("Collection<int, User>");
        let resolved = ty.resolve_names(&|name| format!("App\\{}", name));
        assert_eq!(resolved.to_string(), "App\\Collection<int, App\\User>");
    }

    #[test]
    fn resolve_names_nullable() {
        let ty = PhpType::parse("?User");
        let resolved = ty.resolve_names(&|name| format!("App\\{}", name));
        assert_eq!(resolved.to_string(), "?App\\User");
    }

    #[test]
    fn resolve_names_array_shape() {
        let ty = PhpType::parse("array{name: string, user: User}");
        let resolved = ty.resolve_names(&|name| format!("App\\{}", name));
        assert_eq!(resolved.to_string(), "array{name: string, user: App\\User}");
    }

    #[test]
    fn resolve_names_callable() {
        let ty = PhpType::parse("callable(User): Response");
        let resolved = ty.resolve_names(&|name| format!("App\\{}", name));
        assert_eq!(resolved.to_string(), "callable(App\\User): App\\Response");
    }

    #[test]
    fn resolve_names_keyword_types_untouched() {
        // All of these should pass through without calling the resolver.
        for kw in &[
            "self",
            "static",
            "parent",
            "$this",
            "mixed",
            "void",
            "never",
            "class-string",
            "key-of",
            "value-of",
            "callable",
            "iterable",
            "positive-int",
            "non-empty-string",
            "array-key",
        ] {
            let ty = PhpType::Named(kw.to_string());
            let resolved = ty.resolve_names(&|name| panic!("should not resolve {}", name));
            assert_eq!(resolved.to_string(), *kw);
        }
    }

    #[test]
    fn resolve_names_class_string_inner() {
        let ty = PhpType::parse("class-string<User>");
        let resolved = ty.resolve_names(&|name| format!("App\\{}", name));
        assert_eq!(resolved.to_string(), "class-string<App\\User>");
    }

    #[test]
    fn resolve_names_intersection() {
        let ty = PhpType::parse("Countable&Traversable");
        let resolved = ty.resolve_names(&|name| format!("App\\{}", name));
        assert_eq!(resolved.to_string(), "App\\Countable&App\\Traversable");
    }

    // ─── shorten tests ──────────────────────────────────────────────────────

    #[test]
    fn shorten_plain_class() {
        let ty = PhpType::Named("App\\Models\\User".to_owned());
        assert_eq!(ty.shorten().to_string(), "User");
    }

    #[test]
    fn shorten_already_short() {
        let ty = PhpType::Named("User".to_owned());
        assert_eq!(ty.shorten().to_string(), "User");
    }

    #[test]
    fn shorten_scalar() {
        let ty = PhpType::Named("string".to_owned());
        assert_eq!(ty.shorten().to_string(), "string");
    }

    #[test]
    fn shorten_union() {
        let ty = PhpType::parse("App\\Models\\User|null");
        assert_eq!(ty.shorten().to_string(), "User|null");
    }

    #[test]
    fn shorten_generic() {
        let ty = PhpType::parse("array<int, App\\Models\\User>");
        assert_eq!(ty.shorten().to_string(), "array<int, User>");
    }

    #[test]
    fn shorten_nullable() {
        let ty = PhpType::parse("?App\\Models\\User");
        assert_eq!(ty.shorten().to_string(), "?User");
    }

    #[test]
    fn shorten_callable() {
        let ty = PhpType::parse("callable(App\\Models\\User): App\\Http\\Response");
        assert_eq!(ty.shorten().to_string(), "callable(User): Response");
    }

    #[test]
    fn shorten_class_string() {
        let ty = PhpType::parse("class-string<App\\Models\\User>");
        assert_eq!(ty.shorten().to_string(), "class-string<User>");
    }

    #[test]
    fn shorten_intersection() {
        let ty = PhpType::parse("App\\Contracts\\Countable&App\\Contracts\\Traversable");
        assert_eq!(ty.shorten().to_string(), "Countable&Traversable");
    }

    #[test]
    fn shorten_array_shape() {
        let ty = PhpType::parse("array{name: string, user: App\\Models\\User}");
        assert_eq!(ty.shorten().to_string(), "array{name: string, user: User}");
    }

    // ─── is_scalar tests ────────────────────────────────────────────────────

    #[test]
    fn is_scalar_keywords() {
        assert!(PhpType::Named("int".to_owned()).is_scalar());
        assert!(PhpType::Named("string".to_owned()).is_scalar());
        assert!(PhpType::Named("bool".to_owned()).is_scalar());
        assert!(PhpType::Named("float".to_owned()).is_scalar());
        assert!(PhpType::Named("mixed".to_owned()).is_scalar());
        assert!(PhpType::Named("void".to_owned()).is_scalar());
        assert!(PhpType::Named("null".to_owned()).is_scalar());
        assert!(PhpType::Named("array".to_owned()).is_scalar());
        assert!(PhpType::Named("callable".to_owned()).is_scalar());
        assert!(PhpType::Named("iterable".to_owned()).is_scalar());
    }

    #[test]
    fn is_scalar_class_is_not() {
        assert!(!PhpType::Named("User".to_owned()).is_scalar());
        assert!(!PhpType::Named("App\\Models\\User".to_owned()).is_scalar());
    }

    #[test]
    fn is_scalar_generic_array() {
        assert!(PhpType::parse("array<int, string>").is_scalar());
    }

    #[test]
    fn is_scalar_generic_class() {
        assert!(!PhpType::parse("Collection<int, User>").is_scalar());
    }

    #[test]
    fn is_scalar_nullable_scalar() {
        assert!(PhpType::parse("?int").is_scalar());
    }

    #[test]
    fn is_scalar_nullable_class() {
        assert!(!PhpType::parse("?User").is_scalar());
    }

    // ─── base_name tests ────────────────────────────────────────────────────

    #[test]
    fn base_name_simple_class() {
        assert_eq!(
            PhpType::Named("App\\Models\\User".to_owned()).base_name(),
            Some("App\\Models\\User")
        );
    }

    #[test]
    fn base_name_generic_class() {
        assert_eq!(
            PhpType::parse("Collection<int, User>").base_name(),
            Some("Collection")
        );
    }

    #[test]
    fn base_name_scalar_returns_none() {
        assert_eq!(PhpType::Named("int".to_owned()).base_name(), None);
    }

    #[test]
    fn base_name_nullable_class() {
        assert_eq!(PhpType::parse("?User").base_name(), Some("User"));
    }

    #[test]
    fn base_name_union_returns_none() {
        assert_eq!(PhpType::parse("User|null").base_name(), None);
    }

    // ─── union_members tests ────────────────────────────────────────────────

    #[test]
    fn union_members_of_union() {
        let ty = PhpType::parse("int|string|null");
        let members = ty.union_members();
        assert_eq!(members.len(), 3);
    }

    #[test]
    fn union_members_of_non_union() {
        let ty = PhpType::Named("User".to_owned());
        let members = ty.union_members();
        assert_eq!(members.len(), 1);
        assert_eq!(*members[0], PhpType::Named("User".to_owned()));
    }

    // ─── equivalent tests ───────────────────────────────────────────────────

    #[test]
    fn equivalent_identical() {
        let a = PhpType::Named("User".to_owned());
        let b = PhpType::Named("User".to_owned());
        assert!(a.equivalent(&b));
    }

    #[test]
    fn equivalent_fqn_vs_short() {
        let a = PhpType::Named("App\\Models\\User".to_owned());
        let b = PhpType::Named("User".to_owned());
        assert!(a.equivalent(&b));
    }

    #[test]
    fn equivalent_nullable() {
        let a = PhpType::parse("?App\\Models\\User");
        let b = PhpType::parse("?User");
        assert!(a.equivalent(&b));
    }

    #[test]
    fn equivalent_union_reordered() {
        let a = PhpType::parse("App\\Models\\User|null");
        let b = PhpType::parse("null|User");
        assert!(a.equivalent(&b));
    }

    #[test]
    fn equivalent_generic() {
        let a = PhpType::parse("array<int, App\\Models\\User>");
        let b = PhpType::parse("array<int, User>");
        assert!(a.equivalent(&b));
    }

    #[test]
    fn equivalent_nullable_vs_union_with_null() {
        // `?string` is semantically identical to `string|null`
        let a = PhpType::parse("?string");
        let b = PhpType::parse("string|null");
        assert!(a.equivalent(&b));
        assert!(b.equivalent(&a));
    }

    #[test]
    fn equivalent_nullable_vs_null_first_union() {
        // `?callable` is semantically identical to `null|callable`
        let a = PhpType::parse("?callable");
        let b = PhpType::parse("null|callable");
        assert!(a.equivalent(&b));
    }

    #[test]
    fn equivalent_nullable_vs_three_member_union_not_equal() {
        // `?Foo` is NOT equivalent to `Foo|Bar|null` (different arity)
        let a = PhpType::parse("?Foo");
        let b = PhpType::parse("Foo|Bar|null");
        assert!(!a.equivalent(&b));
    }

    #[test]
    fn equivalent_different_types() {
        let a = PhpType::Named("User".to_owned());
        let b = PhpType::Named("Post".to_owned());
        assert!(!a.equivalent(&b));
    }

    // ── replace_self ────────────────────────────────────────────

    #[test]
    fn replace_self_named() {
        let ty = PhpType::parse("self");
        assert_eq!(ty.replace_self("App\\User").to_string(), "App\\User");
    }

    #[test]
    fn replace_self_static() {
        let ty = PhpType::parse("static");
        assert_eq!(ty.replace_self("App\\User").to_string(), "App\\User");
    }

    #[test]
    fn replace_self_this() {
        let ty = PhpType::parse("$this");
        assert_eq!(ty.replace_self("App\\User").to_string(), "App\\User");
    }

    #[test]
    fn replace_self_in_union() {
        let ty = PhpType::parse("self|null");
        let replaced = ty.replace_self("App\\User");
        assert_eq!(replaced.to_string(), "App\\User|null");
    }

    #[test]
    fn replace_self_in_generic() {
        let ty = PhpType::parse("Collection<int, static>");
        let replaced = ty.replace_self("App\\User");
        assert_eq!(replaced.to_string(), "Collection<int, App\\User>");
    }

    #[test]
    fn replace_self_no_keywords_unchanged() {
        let ty = PhpType::parse("string");
        assert_eq!(ty.replace_self("App\\User").to_string(), "string");
    }

    #[test]
    fn replace_self_nullable() {
        let ty = PhpType::parse("?self");
        assert_eq!(ty.replace_self("App\\User").to_string(), "?App\\User");
    }

    #[test]
    fn replace_self_class_name_unchanged() {
        let ty = PhpType::parse("Collection<int, User>");
        assert_eq!(
            ty.replace_self("App\\Post").to_string(),
            "Collection<int, User>"
        );
    }

    #[test]
    fn replace_self_intersection() {
        let ty = PhpType::parse("self&JsonSerializable");
        let replaced = ty.replace_self("App\\User");
        assert_eq!(replaced.to_string(), "App\\User&JsonSerializable");
    }

    // ── extract_class_names (recursive) ─────────────────────────

    #[test]
    fn extract_class_names_simple() {
        let names = PhpType::parse("User").extract_class_names();
        assert_eq!(names, vec!["User"]);
    }

    #[test]
    fn extract_class_names_scalar() {
        let names = PhpType::parse("int").extract_class_names();
        assert!(names.is_empty());
    }

    #[test]
    fn extract_class_names_union() {
        let names = PhpType::parse("User|Admin|null").extract_class_names();
        assert_eq!(names, vec!["User", "Admin"]);
    }

    #[test]
    fn extract_class_names_generic_recurses() {
        let names = PhpType::parse("Collection<int, User>").extract_class_names();
        assert_eq!(names, vec!["Collection", "User"]);
    }

    #[test]
    fn extract_class_names_callable() {
        let names = PhpType::parse("Closure(User): Admin").extract_class_names();
        assert_eq!(names, vec!["User", "Admin"]);
    }

    #[test]
    fn extract_class_names_nullable() {
        let names = PhpType::parse("?User").extract_class_names();
        assert_eq!(names, vec!["User"]);
    }

    #[test]
    fn extract_class_names_no_duplicates() {
        let names = PhpType::parse("User|User").extract_class_names();
        assert_eq!(names, vec!["User"]);
    }

    // ── top_level_class_names ───────────────────────────────────

    #[test]
    fn top_level_class_names_simple() {
        let names = PhpType::parse("User").top_level_class_names();
        assert_eq!(names, vec!["User"]);
    }

    #[test]
    fn top_level_class_names_generic_base_only() {
        let names = PhpType::parse("Collection<int, User>").top_level_class_names();
        assert_eq!(names, vec!["Collection"]);
    }

    #[test]
    fn top_level_class_names_union() {
        let names = PhpType::parse("User|Admin").top_level_class_names();
        assert_eq!(names, vec!["User", "Admin"]);
    }

    #[test]
    fn top_level_class_names_nullable() {
        let names = PhpType::parse("?User").top_level_class_names();
        assert_eq!(names, vec!["User"]);
    }

    #[test]
    fn top_level_class_names_union_with_null() {
        let names = PhpType::parse("User|null").top_level_class_names();
        assert_eq!(names, vec!["User"]);
    }

    #[test]
    fn top_level_class_names_scalar_excluded() {
        let names = PhpType::parse("string|int").top_level_class_names();
        assert!(names.is_empty());
    }

    #[test]
    fn top_level_class_names_mixed_union() {
        let names = PhpType::parse("string|User|int|Admin|null").top_level_class_names();
        assert_eq!(names, vec!["User", "Admin"]);
    }

    #[test]
    fn top_level_class_names_array_of_class() {
        let names = PhpType::parse("User[]").top_level_class_names();
        assert_eq!(names, vec!["User"]);
    }

    #[test]
    fn top_level_class_names_array_shape_excluded() {
        let names = PhpType::parse("array{name: string}").top_level_class_names();
        assert!(names.is_empty());
    }

    #[test]
    fn top_level_class_names_intersection() {
        let names = PhpType::parse("User&JsonSerializable").top_level_class_names();
        assert_eq!(names, vec!["User", "JsonSerializable"]);
    }

    #[test]
    fn top_level_class_names_fqn() {
        let names = PhpType::parse("\\App\\Models\\User").top_level_class_names();
        assert_eq!(names, vec!["\\App\\Models\\User"]);
    }

    // ── substitute ──────────────────────────────────────────────────

    fn make_subs(pairs: &[(&str, &str)]) -> std::collections::HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn substitute_named_match() {
        let ty = PhpType::parse("TValue");
        let result = ty.substitute(&make_subs(&[("TValue", "User")]));
        assert_eq!(result.to_string(), "User");
    }

    #[test]
    fn substitute_named_no_match() {
        let ty = PhpType::parse("SomeClass");
        let result = ty.substitute(&make_subs(&[("TValue", "User")]));
        assert_eq!(result.to_string(), "SomeClass");
    }

    #[test]
    fn substitute_generic() {
        let ty = PhpType::parse("Collection<TKey, TValue>");
        let subs = make_subs(&[("TKey", "int"), ("TValue", "User")]);
        let result = ty.substitute(&subs);
        assert_eq!(result.to_string(), "Collection<int, User>");
    }

    #[test]
    fn substitute_generic_base_is_template() {
        let ty = PhpType::parse("TContainer<int>");
        let subs = make_subs(&[("TContainer", "Collection")]);
        let result = ty.substitute(&subs);
        assert_eq!(result.to_string(), "Collection<int>");
    }

    #[test]
    fn substitute_union() {
        let ty = PhpType::parse("TValue|null");
        let subs = make_subs(&[("TValue", "User")]);
        let result = ty.substitute(&subs);
        assert_eq!(result.to_string(), "User|null");
    }

    #[test]
    fn substitute_intersection() {
        let ty = PhpType::parse("TFirst&TSecond");
        let subs = make_subs(&[("TFirst", "Countable"), ("TSecond", "Iterator")]);
        let result = ty.substitute(&subs);
        assert_eq!(result.to_string(), "Countable&Iterator");
    }

    #[test]
    fn substitute_nullable() {
        let ty = PhpType::parse("?TValue");
        let subs = make_subs(&[("TValue", "User")]);
        let result = ty.substitute(&subs);
        assert_eq!(result.to_string(), "?User");
    }

    #[test]
    fn substitute_array_shorthand() {
        let ty = PhpType::parse("TValue[]");
        let subs = make_subs(&[("TValue", "User")]);
        let result = ty.substitute(&subs);
        assert_eq!(result.to_string(), "User[]");
    }

    #[test]
    fn substitute_array_shape() {
        let ty = PhpType::parse("array{name: TValue, age: int}");
        let subs = make_subs(&[("TValue", "string")]);
        let result = ty.substitute(&subs);
        assert_eq!(result.to_string(), "array{name: string, age: int}");
    }

    #[test]
    fn substitute_object_shape() {
        let ty = PhpType::parse("object{item: TValue}");
        let subs = make_subs(&[("TValue", "User")]);
        let result = ty.substitute(&subs);
        assert_eq!(result.to_string(), "object{item: User}");
    }

    #[test]
    fn substitute_callable() {
        let ty = PhpType::parse("Closure(TParam): TReturn");
        let subs = make_subs(&[("TParam", "int"), ("TReturn", "string")]);
        let result = ty.substitute(&subs);
        assert_eq!(result.to_string(), "Closure(int): string");
    }

    #[test]
    fn substitute_callable_no_return() {
        let ty = PhpType::parse("callable(TParam)");
        let subs = make_subs(&[("TParam", "User")]);
        let result = ty.substitute(&subs);
        assert_eq!(result.to_string(), "callable(User)");
    }

    #[test]
    fn substitute_class_string() {
        let ty = PhpType::parse("class-string<T>");
        let subs = make_subs(&[("T", "User")]);
        let result = ty.substitute(&subs);
        assert_eq!(result.to_string(), "class-string<User>");
    }

    #[test]
    fn substitute_key_of() {
        let ty = PhpType::parse("key-of<T>");
        let subs = make_subs(&[("T", "array<string, int>")]);
        let result = ty.substitute(&subs);
        assert_eq!(result.to_string(), "key-of<array<string, int>>");
    }

    #[test]
    fn substitute_value_of() {
        let ty = PhpType::parse("value-of<T>");
        let subs = make_subs(&[("T", "array<string, User>")]);
        let result = ty.substitute(&subs);
        assert_eq!(result.to_string(), "value-of<array<string, User>>");
    }

    #[test]
    fn substitute_nested_generic() {
        let ty = PhpType::parse("Collection<int, Promise<TValue>>");
        let subs = make_subs(&[("TValue", "User")]);
        let result = ty.substitute(&subs);
        assert_eq!(result.to_string(), "Collection<int, Promise<User>>");
    }

    #[test]
    fn substitute_empty_subs_unchanged() {
        let ty = PhpType::parse("Collection<int, User>");
        let subs = std::collections::HashMap::new();
        let result = ty.substitute(&subs);
        assert_eq!(result.to_string(), "Collection<int, User>");
    }

    #[test]
    fn substitute_scalar_unchanged() {
        let ty = PhpType::parse("int");
        let subs = make_subs(&[("TValue", "User")]);
        let result = ty.substitute(&subs);
        assert_eq!(result.to_string(), "int");
    }

    #[test]
    fn substitute_literal_unchanged() {
        let ty = PhpType::parse("42");
        let subs = make_subs(&[("42", "User")]);
        // Literal nodes are not substituted (only Named nodes are).
        let result = ty.substitute(&subs);
        assert_eq!(result.to_string(), "42");
    }

    #[test]
    fn substitute_conditional() {
        let ty = PhpType::parse("($x is T ? TTrue : TFalse)");
        let subs = make_subs(&[("T", "string"), ("TTrue", "User"), ("TFalse", "null")]);
        let result = ty.substitute(&subs);
        assert_eq!(result.to_string(), "$x is string ? User : null");
    }

    #[test]
    fn substitute_complex_real_world() {
        // Simulates resolving `Generator<TKey, TValue, TSend, TReturn>`
        // with concrete types.
        let ty = PhpType::parse("Generator<TKey, TValue, TSend, TReturn>");
        let subs = make_subs(&[
            ("TKey", "int"),
            ("TValue", "User"),
            ("TSend", "mixed"),
            ("TReturn", "void"),
        ]);
        let result = ty.substitute(&subs);
        assert_eq!(result.to_string(), "Generator<int, User, mixed, void>");
    }

    #[test]
    fn substitute_replacement_is_complex_type() {
        // When a template param is replaced with a union type.
        let ty = PhpType::parse("array<int, TValue>");
        let subs = make_subs(&[("TValue", "string|int")]);
        let result = ty.substitute(&subs);
        assert_eq!(result.to_string(), "array<int, string|int>");
    }

    #[test]
    fn substitute_union_flattens_nested() {
        // When a union member is replaced with another union.
        let ty = PhpType::parse("TValue|null");
        let subs = make_subs(&[("TValue", "string|int")]);
        let result = ty.substitute(&subs);
        // Should flatten to a single union, not `(string|int)|null`.
        match &result {
            PhpType::Union(members) => assert_eq!(members.len(), 3),
            other => panic!("expected Union, got: {other}"),
        }
    }

    #[test]
    fn substitute_index_access() {
        let ty = PhpType::parse("T[K]");
        let subs = make_subs(&[("T", "array<string, int>"), ("K", "string")]);
        let result = ty.substitute(&subs);
        assert_eq!(result.to_string(), "array<string, int>[string]");
    }

    #[test]
    fn substitute_interface_string() {
        let ty = PhpType::parse("interface-string<T>");
        let subs = make_subs(&[("T", "Countable")]);
        let result = ty.substitute(&subs);
        assert_eq!(result.to_string(), "interface-string<Countable>");
    }
}
