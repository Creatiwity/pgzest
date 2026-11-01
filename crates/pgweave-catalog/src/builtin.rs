//! Packs livrés avec pgweave, générés par `cargo xtask catalog-gen` et
//! versionnés dans `crates/pgweave-catalog/packs/`.

use crate::catalog::Catalog;
use crate::model::quote_ident;
use crate::pack::{EXTSCHEMA, Pack, PackError};

/// (nom, version, contenu).
const PACKS: &[(&str, &str, &str)] = &[
    ("postgres", "18", include_str!("../packs/postgres-18.json")),
    (
        "btree_gist",
        "1.8",
        include_str!("../packs/btree_gist-1.8.json"),
    ),
    ("citext", "1.8", include_str!("../packs/citext-1.8.json")),
    (
        "fuzzystrmatch",
        "1.2",
        include_str!("../packs/fuzzystrmatch-1.2.json"),
    ),
    ("pg_trgm", "1.6", include_str!("../packs/pg_trgm-1.6.json")),
    (
        "unaccent",
        "1.1",
        include_str!("../packs/unaccent-1.1.json"),
    ),
];

/// Versions disponibles d'un contributeur.
pub fn versions(name: &str) -> Vec<&'static str> {
    PACKS
        .iter()
        .filter(|(n, _, _)| *n == name)
        .map(|(_, v, _)| *v)
        .collect()
}

/// Pack `nom@version`. Une version absente est une erreur explicite : jamais
/// de repli sur une autre version (D9).
pub fn pack(name: &str, version: &str) -> Result<Pack, PackError> {
    let Some((_, _, text)) = PACKS.iter().find(|(n, v, _)| *n == name && *v == version) else {
        let available = versions(name);
        return Err(PackError(if available.is_empty() {
            format!("aucun pack pour « {name} »")
        } else {
            format!(
                "pack {name}@{version} indisponible (versions disponibles : {})",
                available.join(", ")
            )
        }));
    };
    Pack::from_json(text)
}

/// Pack d'extension dont les objets sont rangés dans `schema` (le schéma de
/// `CREATE EXTENSION … SCHEMA`).
pub fn extension_pack(name: &str, version: &str, schema: &str) -> Result<Pack, PackError> {
    let pack = pack(name, version)?;
    let mut value = serde_json::to_value(&pack).map_err(|e| PackError(e.to_string()))?;
    let placeholder = format!("{}.", quote_ident(EXTSCHEMA));
    let target = format!("{}.", quote_ident(schema));
    relocate(&mut value, &placeholder, &target);
    serde_json::from_value(value).map_err(|e| PackError(e.to_string()))
}

/// Remplace le préfixe de schéma de tous les noms qualifiés, sérialisés en
/// chaînes `schéma.nom` (références de type comprises).
fn relocate(v: &mut serde_json::Value, from: &str, to: &str) {
    match v {
        serde_json::Value::String(s) => {
            if let Some(rest) = s.strip_prefix(from) {
                *s = format!("{to}{rest}");
            }
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(|i| relocate(i, from, to)),
        serde_json::Value::Object(map) => map.values_mut().for_each(|i| relocate(i, from, to)),
        _ => {}
    }
}

/// Sorte d'objet cherché dans les packs d'extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectSort {
    Type,
    Routine,
}

/// Extensions (`nom@version`) dont le pack fournit un objet de ce nom :
/// la suggestion d'un `CREATE EXTENSION` manquant (D9).
pub fn extensions_providing(sort: ObjectSort, name: &str) -> Vec<String> {
    static PACKS_PARSED: std::sync::OnceLock<Vec<Pack>> = std::sync::OnceLock::new();
    let packs = PACKS_PARSED.get_or_init(|| {
        PACKS
            .iter()
            .filter(|(n, _, _)| *n != "postgres")
            .filter_map(|(_, _, text)| Pack::from_json(text).ok())
            .collect()
    });
    packs
        .iter()
        .filter(|p| match sort {
            ObjectSort::Type => p.types.iter().any(|t| t.name.name == name),
            ObjectSort::Routine => p.routines.iter().any(|r| r.name.name == name),
        })
        .map(|p| p.contributor.to_string())
        .collect()
}

impl Catalog {
    /// Catalogue de base d'une version majeure de PostgreSQL.
    ///
    /// Le pack n'est lu qu'une fois par processus ; chaque appel en rend une
    /// copie.
    pub fn for_postgres(major: u32) -> Result<Catalog, PackError> {
        static PG18: std::sync::OnceLock<Result<Catalog, String>> = std::sync::OnceLock::new();
        if major != 18 {
            // Lève l'erreur explicite de version indisponible.
            pack("postgres", &major.to_string())?;
        }
        PG18.get_or_init(|| {
            let mut c = Catalog::new();
            c.load_pack(&pack("postgres", "18").map_err(|e| e.0)?);
            Ok(c)
        })
        .clone()
        .map_err(PackError)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::SearchPath;
    use crate::model::QName;

    #[test]
    fn le_pack_postgres_se_charge() {
        let c = Catalog::for_postgres(18).unwrap();
        let path = SearchPath::default();
        assert!(c.resolve_type(None, "int4", &path).is_some());
        assert!(
            !c.resolve_routines(None, "pg_trigger_depth", &path)
                .is_empty()
        );
        assert!(!c.resolve_routines(None, "normalize", &path).is_empty());
        assert!(
            c.resolve_relation(Some("information_schema"), "columns", &path)
                .is_some()
        );
        assert!(c.schema_exists("public"));
    }

    #[test]
    fn extension_qui_fournit() {
        assert_eq!(
            extensions_providing(ObjectSort::Type, "citext"),
            ["citext@1.8"]
        );
        assert_eq!(
            extensions_providing(ObjectSort::Routine, "levenshtein"),
            ["fuzzystrmatch@1.2"]
        );
        assert!(extensions_providing(ObjectSort::Type, "int4").is_empty());
    }

    #[test]
    fn une_version_absente_est_une_erreur() {
        let e = Catalog::for_postgres(17).unwrap_err();
        assert!(e.0.contains("postgres@17"), "{e}");
    }

    #[test]
    fn une_extension_se_range_dans_son_schema() {
        let mut c = Catalog::for_postgres(18).unwrap();
        c.load_pack(&extension_pack("citext", "1.8", "extensions").unwrap());
        assert!(c.types.contains_key(&QName::new("extensions", "citext")));
        let path = SearchPath::new(["public", "extensions"]);
        assert!(c.resolve_type(None, "citext", &path).is_some());
        assert!(
            c.resolve_type(None, "citext", &SearchPath::default())
                .is_none()
        );
    }
}
