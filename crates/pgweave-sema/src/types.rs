//! Types as PostgreSQL identifies them: a type is a `pg_type` entry,
//! designated by its qualified name. An array is its own type
//! (`pg_catalog._int4`), as in PostgreSQL.

use std::collections::HashMap;

use pgweave_catalog::model::{Cast, TypeDef, TypeKind, TypeRef};
use pgweave_catalog::{Catalog, QName};

/// Identity of a type (`pg_type`).
pub type Ty = QName;

/// Type from `pg_catalog`.
pub fn pg(name: &str) -> Ty {
    QName::pg_catalog(name)
}

/// `typcategory` category of strings and of unknown types.
pub(crate) const CATEGORY_STRING: char = 'S';
pub(crate) const CATEGORY_UNKNOWN: char = 'X';

/// Catalog object an analysis depends on: what it resolved.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Dependency {
    Relation(QName),
    Type(QName),
    /// Routine and the types of its input arguments (the overload).
    Routine(QName, Vec<String>),
    /// Operator and the types of its operands.
    Operator(QName, Option<String>, String),
}

impl Dependency {
    /// Object built into PostgreSQL: fixed by the version, irrelevant to
    /// invalidating a project.
    pub fn is_builtin(&self) -> bool {
        let q = match self {
            Dependency::Relation(q)
            | Dependency::Type(q)
            | Dependency::Routine(q, _)
            | Dependency::Operator(q, _, _) => q,
        };
        matches!(q.schema.as_str(), "pg_catalog" | "information_schema")
    }
}

impl std::fmt::Display for Dependency {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Dependency::Relation(q) | Dependency::Type(q) => write!(f, "{q}"),
            Dependency::Routine(q, args) => write!(f, "{q}({})", args.join(", ")),
            Dependency::Operator(q, l, r) => {
                write!(f, "{q}({}, {r})", l.as_deref().unwrap_or("NONE"))
            }
        }
    }
}

/// "Type system" view of a catalog.
pub struct TypeSystem<'c> {
    pub catalog: &'c Catalog,
    /// `pg_cast` indexed by (source, target).
    casts: HashMap<(Ty, Ty), &'c Cast>,
    /// Objects resolved by the analyses done with this system.
    pub depends: std::cell::RefCell<std::collections::BTreeSet<Dependency>>,
}

impl<'c> TypeSystem<'c> {
    pub fn new(catalog: &'c Catalog) -> Self {
        let mut casts = HashMap::new();
        for c in &catalog.casts {
            if let (TypeRef::Named { name: s, .. }, TypeRef::Named { name: t, .. }) =
                (&c.source, &c.target)
            {
                casts.insert((s.clone(), t.clone()), c);
            }
        }
        TypeSystem {
            catalog,
            casts,
            depends: Default::default(),
        }
    }

    /// Records a dependency.
    pub fn depend(&self, d: Dependency) {
        self.depends.borrow_mut().insert(d);
    }

    /// Relation designated by a written name, recorded as a dependency.
    pub fn relation(
        &self,
        schema: Option<&str>,
        name: &str,
        path: &pgweave_catalog::SearchPath,
    ) -> Option<&'c pgweave_catalog::model::Relation> {
        let rel = self.catalog.resolve_relation(schema, name, path)?;
        self.depend(Dependency::Relation(rel.name.clone()));
        Some(rel)
    }

    pub fn def(&self, ty: &Ty) -> Option<&'c TypeDef> {
        self.catalog.types.get(ty)
    }

    /// Type designated by a catalog reference (routine arguments,
    /// columns). `None` for an unresolved or unknown reference.
    pub fn of_ref(&self, r: &TypeRef) -> Option<Ty> {
        match r {
            TypeRef::Named { name, .. } => Some(name.clone()),
            TypeRef::Array(inner) => self.array_of(&self.of_ref(inner)?),
            TypeRef::Unresolved(_) | TypeRef::Unknown => None,
        }
    }

    /// `typcategory`; `X` for a type absent from the catalog.
    pub fn category(&self, ty: &Ty) -> char {
        self.def(ty).map_or(CATEGORY_UNKNOWN, |d| d.category)
    }

    /// `IsPreferredType`: preferred in the given category.
    pub fn is_preferred(&self, category: char, ty: &Ty) -> bool {
        self.def(ty)
            .is_some_and(|d| d.preferred && d.category == category)
    }

    /// `getBaseType`: a domain is replaced by its base type, walking up
    /// domains over domains.
    pub fn base_type(&self, ty: &Ty) -> Ty {
        let mut t = ty.clone();
        for _ in 0..32 {
            match self.def(&t).map(|d| &d.kind) {
                Some(TypeKind::Domain { base, .. }) => match self.of_ref(base) {
                    Some(b) => t = b,
                    None => return t,
                },
                _ => return t,
            }
        }
        t
    }

    pub fn is_domain(&self, ty: &Ty) -> bool {
        matches!(self.def(ty).map(|d| &d.kind), Some(TypeKind::Domain { .. }))
    }

    /// `get_element_type`: element of an array type (not of a domain).
    pub fn element(&self, ty: &Ty) -> Option<Ty> {
        match self.def(ty).map(|d| &d.kind) {
            Some(TypeKind::Array { element }) => Some(element.clone()),
            _ => None,
        }
    }

    /// `get_array_type`: array type of a type.
    pub fn array_of(&self, ty: &Ty) -> Option<Ty> {
        self.def(ty).and_then(|d| d.array.clone())
    }

    /// `type_is_array_domain`: array, or domain over an array.
    pub fn is_array_or_array_domain(&self, ty: &Ty) -> bool {
        self.element(&self.base_type(ty)).is_some()
    }

    pub fn is_enum(&self, ty: &Ty) -> bool {
        matches!(self.def(ty).map(|d| &d.kind), Some(TypeKind::Enum { .. }))
    }

    /// Subtype of a range type (`get_range_subtype`).
    pub fn range_subtype(&self, ty: &Ty) -> Option<Ty> {
        match self.def(ty).map(|d| &d.kind) {
            Some(TypeKind::Range { subtype }) => Some(subtype.clone()),
            _ => None,
        }
    }

    /// Multirange of a range (`get_range_multirange`).
    pub fn range_multirange(&self, range: &Ty) -> Option<Ty> {
        self.catalog.types.values().find_map(|d| match &d.kind {
            TypeKind::Multirange { range: r } if r == range => Some(d.name.clone()),
            _ => None,
        })
    }

    /// Range of a multirange (`get_multirange_range`).
    pub fn multirange_range(&self, ty: &Ty) -> Option<Ty> {
        match self.def(ty).map(|d| &d.kind) {
            Some(TypeKind::Multirange { range }) => Some(range.clone()),
            _ => None,
        }
    }

    /// `ISCOMPLEX`: composite type (including the row type of a table,
    /// or a domain over one of them), or `record`.
    pub fn is_complex(&self, ty: &Ty) -> bool {
        let base = self.base_type(ty);
        *ty == pg("record")
            || self.catalog.relations.contains_key(&base)
            || matches!(
                self.def(&self.base_type(ty)).map(|d| &d.kind),
                Some(TypeKind::Composite { .. })
            )
    }

    /// `pg_cast` entry from `source` to `target`.
    pub(crate) fn cast(&self, source: &Ty, target: &Ty) -> Option<&'c Cast> {
        self.casts.get(&(source.clone(), target.clone())).copied()
    }
}

/// Polymorphic pseudo-types (`IsPolymorphicType`).
pub(crate) fn is_polymorphic(ty: &Ty) -> bool {
    ty.schema == "pg_catalog"
        && matches!(
            ty.name.as_str(),
            "anyelement"
                | "anyarray"
                | "anynonarray"
                | "anyenum"
                | "anyrange"
                | "anymultirange"
                | "anycompatible"
                | "anycompatiblearray"
                | "anycompatiblenonarray"
                | "anycompatiblerange"
                | "anycompatiblemultirange"
        )
}
