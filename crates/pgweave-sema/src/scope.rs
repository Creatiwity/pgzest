//! Portée d'une requête : les sources de `FROM` et leurs colonnes, la portée
//! englobante (sous-requêtes corrélées), les CTE visibles.

use pgweave_ast::Span;

use crate::types::Ty;

/// Nullabilité (PLAN.md D5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Nullability {
    NonNull,
    Nullable,
    /// Non établie : ni affirmée nulle, ni affirmée non nulle.
    Unknown,
}

impl Nullability {
    /// Résultat d'une opération stricte : nulle dès qu'une entrée l'est.
    pub fn strict<I: IntoIterator<Item = Nullability>>(inputs: I) -> Self {
        let mut out = Nullability::NonNull;
        for n in inputs {
            match n {
                Nullability::Nullable => return Nullability::Nullable,
                Nullability::Unknown => out = Nullability::Unknown,
                Nullability::NonNull => {}
            }
        }
        out
    }

    /// Côté optionnel d'une jointure externe.
    pub fn outer(self) -> Self {
        Nullability::Nullable
    }
}

/// Colonne visible dans une portée.
#[derive(Debug, Clone, PartialEq)]
pub struct Column {
    pub name: String,
    /// `None` : type non établi (colonne de vue avant P3, par exemple).
    pub ty: Option<Ty>,
    /// Modificateurs du type (`varchar(20)` : `["20"]`), vides si aucun.
    pub modifiers: Vec<String>,
    pub nullable: Nullability,
}

/// Source de `FROM` : table, vue, CTE, sous-requête ou fonction.
#[derive(Debug, Clone)]
pub struct Source {
    /// Nom sous lequel la source est visible (alias, sinon nom).
    pub name: Option<String>,
    pub columns: Vec<Column>,
    /// Colonnes visibles par leur nom mais absentes de `*` : colonnes
    /// système d'une table (`ctid`, `xmin`, `xmax`…).
    pub hidden: Vec<Column>,
    /// Type ligne de la source : celui de la relation, `record` sinon.
    pub rowtype: Option<Ty>,
}

impl Source {
    pub fn new(name: Option<String>, columns: Vec<Column>) -> Self {
        Source {
            name,
            columns,
            hidden: Vec::new(),
            rowtype: None,
        }
    }

    /// Ajoute les colonnes système d'une table (`heap.c`, `SysAtt`).
    pub fn with_system_columns(mut self) -> Self {
        let sys = [
            ("tableoid", "oid"),
            ("cmax", "cid"),
            ("xmax", "xid"),
            ("cmin", "cid"),
            ("xmin", "xid"),
            ("ctid", "tid"),
        ];
        self.hidden = sys
            .iter()
            .map(|(n, t)| Column {
                name: (*n).to_string(),
                ty: Some(crate::types::pg(t)),
                modifiers: Vec::new(),
                nullable: Nullability::NonNull,
            })
            .collect();
        self
    }
}

/// Erreur d'analyse, avec le code du diagnostic qu'elle produit.
#[derive(Debug, Clone, PartialEq)]
pub struct SemaError {
    pub code: &'static str,
    pub message: String,
    pub span: Span,
}

impl SemaError {
    pub fn new(code: &'static str, message: impl Into<String>, span: Span) -> Self {
        SemaError {
            code,
            message: message.into(),
            span,
        }
    }

    /// Construction que l'analyse ne couvre pas : signalée, jamais devinée.
    pub fn not_analyzed(what: impl std::fmt::Display, span: Span) -> Self {
        SemaError::new("not-analyzed", format!("non analysé : {what}"), span)
    }
}

/// Portée d'un niveau de requête.
#[derive(Debug, Clone, Default)]
pub struct Scope<'p> {
    pub sources: Vec<Source>,
    /// Colonnes de `SELECT *`, dans l'ordre de PostgreSQL : (source,
    /// colonne). Une jointure `USING` place ses colonnes communes en tête et
    /// ne les répète pas.
    pub star: Vec<(usize, usize)>,
    pub parent: Option<&'p Scope<'p>>,
}

impl<'p> Scope<'p> {
    pub fn child(parent: &'p Scope<'p>) -> Self {
        Scope {
            parent: Some(parent),
            ..Scope::default()
        }
    }

    /// Ajoute une source ; ses colonnes rejoignent `*` à la suite.
    pub fn push(&mut self, source: Source) -> usize {
        let i = self.sources.len();
        self.star.extend((0..source.columns.len()).map(|c| (i, c)));
        self.sources.push(source);
        i
    }

    /// Colonne `table.column` ou `column`, ici puis dans les portées
    /// englobantes.
    pub fn lookup(
        &self,
        table: Option<&str>,
        column: &str,
        span: Span,
    ) -> Result<&Column, SemaError> {
        let mut found: Vec<&Column> = Vec::new();
        let mut table_seen = false;
        for s in &self.sources {
            if let Some(t) = table {
                if s.name.as_deref() != Some(t) {
                    continue;
                }
                table_seen = true;
            }
            found.extend(s.columns.iter().filter(|c| c.name == column));
        }
        if found.is_empty() {
            for s in &self.sources {
                if table.is_none_or(|t| s.name.as_deref() == Some(t)) {
                    found.extend(s.hidden.iter().filter(|c| c.name == column));
                }
            }
        }
        match found.as_slice() {
            [one] => Ok(one),
            [_, _, ..] if table.is_some() || !self.using_merged(column) => Err(SemaError::new(
                "ambiguous-column",
                format!("column reference \"{column}\" is ambiguous"),
                span,
            )),
            [first, ..] => Ok(first),
            [] => match self.parent {
                Some(p) => p.lookup(table, column, span),
                None if table.is_some() && !table_seen => Err(SemaError::new(
                    "table-not-found",
                    format!(
                        "missing FROM-clause entry for table \"{}\"",
                        table.unwrap_or_default()
                    ),
                    span,
                )),
                None => Err(SemaError::new(
                    "column-not-found",
                    match table {
                        Some(t) => format!("column {t}.{column} does not exist"),
                        None => format!("column \"{column}\" does not exist"),
                    },
                    span,
                )),
            },
        }
    }

    /// Colonne fusionnée par `USING` : visible une seule fois dans `*`.
    fn using_merged(&self, column: &str) -> bool {
        self.star
            .iter()
            .filter(|(s, c)| self.sources[*s].columns[*c].name == column)
            .count()
            == 1
    }

    /// Type de la ligne entière de la source nommée `name`, ici puis dans
    /// les portées englobantes.
    pub fn whole_row(&self, name: &str) -> Option<Ty> {
        match self.source(name) {
            Some(s) => Some(
                s.rowtype
                    .clone()
                    .unwrap_or_else(|| crate::types::pg("record")),
            ),
            None => self.parent.and_then(|p| p.whole_row(name)),
        }
    }

    /// Source visible sous ce nom.
    pub fn source(&self, name: &str) -> Option<&Source> {
        self.sources
            .iter()
            .find(|s| s.name.as_deref() == Some(name))
    }
}
