//! En-tête de migration (PLAN.md, D9) : le contexte global contre lequel une
//! migration a été générée et vérifiée.
//!
//! ```sql
//! -- pgweave: postgres=18 extensions=btree_gist@1.7,citext@1.6
//! ```
//!
//! Il se place parmi les lignes de tête du fichier (commentaires et lignes
//! vides), avant le premier statement. `postgres` est obligatoire,
//! `extensions` facultatif ; chaque extension porte sa version. Toute autre
//! forme est une erreur : un en-tête mal lu ferait analyser la migration
//! contre un autre contexte que celui annoncé.

use std::fmt;
use std::ops::Range;

/// Préfixe d'une ligne d'en-tête. Distinct des directives
/// (`-- pgweave-disable…`), qui n'ont pas de `:`.
const PREFIX: &str = "-- pgweave:";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationHeader {
    /// Version majeure de PostgreSQL.
    pub postgres: u32,
    /// Extensions actives, dans l'ordre écrit.
    pub extensions: Vec<ExtensionRef>,
    /// Position de la ligne dans le fichier (octets, sans le saut de ligne).
    pub span: Range<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtensionRef {
    pub name: String,
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeaderError {
    pub message: String,
    pub span: Range<usize>,
}

impl MigrationHeader {
    /// En-tête d'un fichier : `None` s'il n'en a pas, une erreur s'il est
    /// mal formé, dupliqué ou placé après le premier statement.
    pub fn find(text: &str) -> Option<Result<MigrationHeader, HeaderError>> {
        let mut found: Option<MigrationHeader> = None;
        let mut in_head = true;
        let mut offset = 0;
        for line in text.split_inclusive('\n') {
            let start = offset;
            offset += line.len();
            let content = line.trim_end_matches(['\n', '\r']);
            let span = start..start + content.len();
            let trimmed = content.trim_start();
            if !trimmed.starts_with(PREFIX) {
                if !(trimmed.is_empty() || trimmed.starts_with("--")) {
                    in_head = false;
                }
                continue;
            }
            if !in_head {
                return Some(Err(HeaderError {
                    message: "l'en-tête `-- pgweave:` doit précéder le premier statement".into(),
                    span,
                }));
            }
            if found.is_some() {
                return Some(Err(HeaderError {
                    message: "en-tête `-- pgweave:` en double".into(),
                    span,
                }));
            }
            match parse_fields(&trimmed[PREFIX.len()..]) {
                Ok((postgres, extensions)) => {
                    found = Some(MigrationHeader {
                        postgres,
                        extensions,
                        span,
                    })
                }
                Err(message) => return Some(Err(HeaderError { message, span })),
            }
        }
        found.map(Ok)
    }
}

fn parse_fields(fields: &str) -> Result<(u32, Vec<ExtensionRef>), String> {
    let mut postgres = None;
    let mut extensions: Option<Vec<ExtensionRef>> = None;
    for field in fields.split_whitespace() {
        let Some((key, value)) = field.split_once('=') else {
            return Err(format!("champ `{field}` : `clé=valeur` attendu"));
        };
        match key {
            "postgres" => {
                if postgres.is_some() {
                    return Err("`postgres` en double".into());
                }
                postgres = Some(value.parse::<u32>().map_err(|_| {
                    format!("`postgres={value}` : version majeure attendue (ex. `postgres=18`)")
                })?);
            }
            "extensions" => {
                if extensions.is_some() {
                    return Err("`extensions` en double".into());
                }
                extensions = Some(parse_extensions(value)?);
            }
            other => return Err(format!("champ inconnu `{other}`")),
        }
    }
    let postgres = postgres.ok_or("champ `postgres` manquant")?;
    Ok((postgres, extensions.unwrap_or_default()))
}

fn parse_extensions(value: &str) -> Result<Vec<ExtensionRef>, String> {
    let mut out: Vec<ExtensionRef> = Vec::new();
    for item in value.split(',') {
        let Some((name, version)) = item.split_once('@') else {
            return Err(format!("extension `{item}` : `nom@version` attendu"));
        };
        if name.is_empty() || version.is_empty() {
            return Err(format!("extension `{item}` : `nom@version` attendu"));
        }
        if out.iter().any(|e| e.name == name) {
            return Err(format!("extension `{name}` en double"));
        }
        out.push(ExtensionRef {
            name: name.to_string(),
            version: version.to_string(),
        });
    }
    Ok(out)
}

/// Ligne d'en-tête, extensions triées par nom : la même entrée donne
/// toujours le même texte.
impl fmt::Display for MigrationHeader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{PREFIX} postgres={}", self.postgres)?;
        if !self.extensions.is_empty() {
            let mut exts: Vec<&ExtensionRef> = self.extensions.iter().collect();
            exts.sort_by(|a, b| a.name.cmp(&b.name));
            let list: Vec<String> = exts
                .iter()
                .map(|e| format!("{}@{}", e.name, e.version))
                .collect();
            write!(f, " extensions={}", list.join(","))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(text: &str) -> MigrationHeader {
        MigrationHeader::find(text)
            .expect("en-tête")
            .expect("valide")
    }

    fn err(text: &str) -> String {
        MigrationHeader::find(text)
            .expect("en-tête")
            .expect_err("invalide")
            .message
    }

    #[test]
    fn en_tete_complet() {
        let text =
            "-- pgweave: postgres=18 extensions=citext@1.6,btree_gist@1.7\nCREATE TABLE t ();";
        let h = ok(text);
        assert_eq!(h.postgres, 18);
        assert_eq!(h.extensions.len(), 2);
        assert_eq!(h.extensions[0].name, "citext");
        assert_eq!(h.span, 0..text.find('\n').unwrap());
    }

    #[test]
    fn sans_extensions_et_apres_des_commentaires() {
        let h = ok("-- Migration 42\n\n-- pgweave: postgres=18\nSELECT 1;");
        assert_eq!(h.postgres, 18);
        assert!(h.extensions.is_empty());
    }

    #[test]
    fn absent() {
        assert!(MigrationHeader::find("-- pgweave-disable-file\nSELECT 1;").is_none());
    }

    #[test]
    fn rendu_deterministe() {
        let h = ok("-- pgweave: extensions=citext@1.6,btree_gist@1.7 postgres=18");
        assert_eq!(
            h.to_string(),
            "-- pgweave: postgres=18 extensions=btree_gist@1.7,citext@1.6"
        );
        assert_eq!(ok(&h.to_string()).extensions.len(), 2);
    }

    #[test]
    fn formes_refusees() {
        assert!(err("-- pgweave: extensions=citext@1.6").contains("postgres"));
        assert!(err("-- pgweave: postgres=dix-huit").contains("version majeure"));
        assert!(err("-- pgweave: postgres=18 extensions=citext").contains("nom@version"));
        assert!(err("-- pgweave: postgres=18 extensions=a@1,a@2").contains("double"));
        assert!(err("-- pgweave: postgres=18 pg=17").contains("inconnu"));
        assert!(err("-- pgweave: postgres=18\n-- pgweave: postgres=18").contains("double"));
        assert!(err("SELECT 1;\n-- pgweave: postgres=18").contains("précéder"));
    }
}
