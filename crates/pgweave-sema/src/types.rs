//! Types tels que PostgreSQL les identifie : un type est une entrée de
//! `pg_type`, désignée par son nom qualifié. Un tableau est son propre type
//! (`pg_catalog._int4`), comme dans PostgreSQL.

use std::collections::HashMap;

use pgweave_catalog::model::{Cast, TypeDef, TypeKind, TypeRef};
use pgweave_catalog::{Catalog, QName};

/// Identité d'un type (`pg_type`).
pub type Ty = QName;

/// Type de `pg_catalog`.
pub fn pg(name: &str) -> Ty {
    QName::pg_catalog(name)
}

/// Catégorie `typcategory` des chaînes et des types inconnus.
pub(crate) const CATEGORY_STRING: char = 'S';
pub(crate) const CATEGORY_UNKNOWN: char = 'X';

/// Objet du catalogue dont dépend une analyse : ce qu'elle a résolu.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Dependency {
    Relation(QName),
    Type(QName),
    /// Routine et types de ses arguments d'entrée (la surcharge).
    Routine(QName, Vec<String>),
    /// Opérateur et types de ses opérandes.
    Operator(QName, Option<String>, String),
}

impl Dependency {
    /// Objet intégré à PostgreSQL : fixé par la version, sans intérêt pour
    /// l'invalidation d'un projet.
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

/// Vue « système de types » d'un catalogue.
pub struct TypeSystem<'c> {
    pub catalog: &'c Catalog,
    /// `pg_cast` indexé par (source, cible).
    casts: HashMap<(Ty, Ty), &'c Cast>,
    /// Objets résolus par les analyses faites avec ce système.
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

    /// Note une dépendance.
    pub fn depend(&self, d: Dependency) {
        self.depends.borrow_mut().insert(d);
    }

    /// Relation désignée par un nom écrit, notée comme dépendance.
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

    /// Type désigné par une référence du catalogue (arguments de routine,
    /// colonnes). `None` pour une référence non résolue ou inconnue.
    pub fn of_ref(&self, r: &TypeRef) -> Option<Ty> {
        match r {
            TypeRef::Named { name, .. } => Some(name.clone()),
            TypeRef::Array(inner) => self.array_of(&self.of_ref(inner)?),
            TypeRef::Unresolved(_) | TypeRef::Unknown => None,
        }
    }

    /// `typcategory` ; `X` pour un type absent du catalogue.
    pub fn category(&self, ty: &Ty) -> char {
        self.def(ty).map_or(CATEGORY_UNKNOWN, |d| d.category)
    }

    /// `IsPreferredType` : préféré dans la catégorie donnée.
    pub fn is_preferred(&self, category: char, ty: &Ty) -> bool {
        self.def(ty)
            .is_some_and(|d| d.preferred && d.category == category)
    }

    /// `getBaseType` : un domaine est remplacé par son type de base, en
    /// remontant les domaines de domaines.
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

    /// `get_element_type` : élément d'un type tableau (pas d'un domaine).
    pub fn element(&self, ty: &Ty) -> Option<Ty> {
        match self.def(ty).map(|d| &d.kind) {
            Some(TypeKind::Array { element }) => Some(element.clone()),
            _ => None,
        }
    }

    /// `get_array_type` : type tableau d'un type.
    pub fn array_of(&self, ty: &Ty) -> Option<Ty> {
        self.def(ty).and_then(|d| d.array.clone())
    }

    /// `type_is_array_domain` : tableau, ou domaine sur un tableau.
    pub fn is_array_or_array_domain(&self, ty: &Ty) -> bool {
        self.element(&self.base_type(ty)).is_some()
    }

    pub fn is_enum(&self, ty: &Ty) -> bool {
        matches!(self.def(ty).map(|d| &d.kind), Some(TypeKind::Enum { .. }))
    }

    /// Sous-type d'un type intervalle (`get_range_subtype`).
    pub fn range_subtype(&self, ty: &Ty) -> Option<Ty> {
        match self.def(ty).map(|d| &d.kind) {
            Some(TypeKind::Range { subtype }) => Some(subtype.clone()),
            _ => None,
        }
    }

    /// Multi-intervalle d'un intervalle (`get_range_multirange`).
    pub fn range_multirange(&self, range: &Ty) -> Option<Ty> {
        self.catalog.types.values().find_map(|d| match &d.kind {
            TypeKind::Multirange { range: r } if r == range => Some(d.name.clone()),
            _ => None,
        })
    }

    /// Intervalle d'un multi-intervalle (`get_multirange_range`).
    pub fn multirange_range(&self, ty: &Ty) -> Option<Ty> {
        match self.def(ty).map(|d| &d.kind) {
            Some(TypeKind::Multirange { range }) => Some(range.clone()),
            _ => None,
        }
    }

    /// `ISCOMPLEX` : type composite (y compris le type ligne d'une table,
    /// ou un domaine sur l'un d'eux), ou `record`.
    pub fn is_complex(&self, ty: &Ty) -> bool {
        let base = self.base_type(ty);
        *ty == pg("record")
            || self.catalog.relations.contains_key(&base)
            || matches!(
                self.def(&self.base_type(ty)).map(|d| &d.kind),
                Some(TypeKind::Composite { .. })
            )
    }

    /// Entrée `pg_cast` de `source` vers `target`.
    pub(crate) fn cast(&self, source: &Ty, target: &Ty) -> Option<&'c Cast> {
        self.casts.get(&(source.clone(), target.clone())).copied()
    }
}

/// Types pseudo du polymorphisme (`IsPolymorphicType`).
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
