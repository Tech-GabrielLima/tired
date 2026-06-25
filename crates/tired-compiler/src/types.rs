//! The TIRED type system as the checker understands it.
//!
//! Inference is deliberately *shallow*: where a type is known (a binding annotation, a
//! declared record, a literal) we track it and check against it; everywhere else we
//! fall back to [`Type::Unknown`], which suppresses checks. The guiding principle is
//! **no false positives** — a missing schema must never turn into a spurious error.

use std::collections::BTreeMap;
use tired_syntax::ast::{FieldDecl, TypeExpr};

#[derive(Clone, Debug, PartialEq)]
pub enum Type {
    /// No static information — all checks against this type are skipped.
    Unknown,
    Int,
    Float,
    Bool,
    String,
    Null,
    Duration,
    /// A semantic scalar like `Url`, `Email`, `UUID`, `DateTime`. Behaves like a
    /// string for field access but carries its name for diagnostics and validation.
    Semantic(String),
    /// A named record type declared with `type`/`contract`. Fields live in [`TypeTable`].
    Record(String),
    Array(Box<Type>),
    Optional(Box<Type>),
    Result(Box<Type>, ErrDomain),
}

/// The set of error variants a `Result`'s `Err` side can take.
#[derive(Clone, Debug, PartialEq)]
pub enum ErrDomain {
    /// Any error is possible (a single named error type, e.g. `ApiError`). A match must
    /// therefore include a catch-all `Err(e)` or `_` arm.
    Open,
    /// A closed union, e.g. `NotFound | Unauthorized` — every variant must be handled.
    Variants(Vec<String>),
}

impl Type {
    pub fn display(&self) -> String {
        match self {
            Type::Unknown => "?".into(),
            Type::Int => "Integer".into(),
            Type::Float => "Float".into(),
            Type::Bool => "Boolean".into(),
            Type::String => "String".into(),
            Type::Null => "Null".into(),
            Type::Duration => "Duration".into(),
            Type::Semantic(n) => n.clone(),
            Type::Record(n) => n.clone(),
            Type::Array(t) => format!("{}[]", t.display()),
            Type::Optional(t) => format!("{}?", t.display()),
            Type::Result(t, d) => match d {
                ErrDomain::Open => format!("Result<{}, ?>", t.display()),
                ErrDomain::Variants(v) => format!("Result<{}, {}>", t.display(), v.join(" | ")),
            },
        }
    }

    pub fn is_unknown(&self) -> bool {
        matches!(self, Type::Unknown)
    }

    pub fn is_result(&self) -> bool {
        matches!(self, Type::Result(..))
    }

    /// The element type if this is an array (peeling an outer optional).
    pub fn element(&self) -> Option<Type> {
        match self {
            Type::Array(t) => Some((**t).clone()),
            Type::Optional(t) => t.element(),
            _ => None,
        }
    }
}

/// Declared record types: name → ordered fields. Built from `type`/`contract` items.
#[derive(Default)]
pub struct TypeTable {
    records: BTreeMap<String, Vec<RecordField>>,
}

#[derive(Clone)]
pub struct RecordField {
    pub name: String,
    pub ty: Type,
    pub decl: FieldDecl,
}

impl TypeTable {
    pub fn new() -> Self {
        TypeTable::default()
    }

    pub fn declare(&mut self, name: String, fields: Vec<RecordField>) {
        self.records.insert(name, fields);
    }

    pub fn is_record(&self, name: &str) -> bool {
        self.records.contains_key(name)
    }

    pub fn fields(&self, name: &str) -> Option<&[RecordField]> {
        self.records.get(name).map(|v| v.as_slice())
    }

    pub fn field(&self, name: &str, field: &str) -> Option<&RecordField> {
        self.records.get(name)?.iter().find(|f| f.name == field)
    }

    pub fn field_names(&self, name: &str) -> Vec<&str> {
        self.records
            .get(name)
            .map(|v| v.iter().map(|f| f.name.as_str()).collect())
            .unwrap_or_default()
    }

    /// Resolve a syntactic [`TypeExpr`] into a semantic [`Type`]. Names that are neither
    /// a primitive, a semantic scalar, nor a declared record resolve to `Unknown`.
    pub fn resolve(&self, te: &TypeExpr) -> Type {
        match te {
            TypeExpr::Named(n) => self.resolve_named(n),
            TypeExpr::Optional(inner) => Type::Optional(Box::new(self.resolve(inner))),
            TypeExpr::Array(inner) => Type::Array(Box::new(self.resolve(inner))),
            TypeExpr::Generic(name, args) => match name.as_str() {
                "Result" if args.len() == 2 => {
                    Type::Result(Box::new(self.resolve(&args[0])), self.err_domain(&args[1]))
                }
                "Option" if args.len() == 1 => Type::Optional(Box::new(self.resolve(&args[0]))),
                _ => Type::Unknown,
            },
            TypeExpr::Union(_) => Type::Unknown,
        }
    }

    fn resolve_named(&self, n: &str) -> Type {
        match n {
            "Int" | "Integer" => Type::Int,
            "Float" | "Number" => Type::Float,
            "Bool" | "Boolean" => Type::Bool,
            "String" | "Str" => Type::String,
            "Null" => Type::Null,
            "Duration" => Type::Duration,
            "Url" | "Email" | "UUID" | "Uuid" | "DateTime" | "Date" | "Status" | "Time" => {
                Type::Semantic(n.to_string())
            }
            _ if self.is_record(n) => Type::Record(n.to_string()),
            _ => Type::Unknown,
        }
    }

    fn err_domain(&self, te: &TypeExpr) -> ErrDomain {
        match te {
            TypeExpr::Union(alts) => {
                let mut names = Vec::new();
                for a in alts {
                    if let TypeExpr::Named(n) = a {
                        names.push(n.clone());
                    }
                }
                ErrDomain::Variants(names)
            }
            // A single named error type is treated as an open domain: any error variant
            // may occur, so a match must provide a catch-all.
            _ => ErrDomain::Open,
        }
    }
}
