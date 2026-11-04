//! Scope of a query: the `FROM` sources and their columns, the enclosing
//! scope (correlated subqueries), the visible CTEs.

use pgzest_ast::Span;
use pgzest_diagnostics::codes;

use crate::types::Ty;

/// Nullability (PLAN.md D5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Nullability {
    NonNull,
    Nullable,
    /// Not established: neither asserted null nor asserted non-null.
    Unknown,
}

impl Nullability {
    /// Result of a strict operation: null as soon as an input is.
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

    /// Optional side of an outer join.
    pub fn outer(self) -> Self {
        Nullability::Nullable
    }
}

/// Column visible in a scope.
#[derive(Debug, Clone, PartialEq)]
pub struct Column {
    pub name: String,
    /// `None`: type not established (view column before P3, for example).
    pub ty: Option<Ty>,
    /// Type modifiers (`varchar(20)`: `["20"]`), empty if none.
    pub modifiers: Vec<String>,
    pub nullable: Nullability,
}

/// `FROM` source: table, view, CTE, subquery or function.
#[derive(Debug, Clone)]
pub struct Source {
    /// Name under which the source is visible (alias, otherwise name).
    pub name: Option<String>,
    pub columns: Vec<Column>,
    /// Columns visible by name but absent from `*`: system columns
    /// of a table (`ctid`, `xmin`, `xmax`…).
    pub hidden: Vec<Column>,
    /// Row type of the source: the relation's, `record` otherwise.
    pub rowtype: Option<Ty>,
    /// Relation designated without an alias: also visible as
    /// `schema.table.column`.
    pub relation: Option<crate::types::Ty>,
}

impl Source {
    pub fn new(name: Option<String>, columns: Vec<Column>) -> Self {
        Source {
            name,
            columns,
            hidden: Vec::new(),
            rowtype: None,
            relation: None,
        }
    }

    /// Adds the system columns of a table (`heap.c`, `SysAtt`).
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

/// Analysis error, with the code of the diagnostic it produces.
#[derive(Debug, Clone, PartialEq)]
pub struct SemaError {
    pub code: &'static str,
    pub message: String,
    pub span: Span,
    /// Fix suggestion, if there is one.
    pub hint: Option<String>,
}

impl SemaError {
    pub fn new(code: &'static str, message: impl Into<String>, span: Span) -> Self {
        SemaError {
            code,
            message: message.into(),
            span,
            hint: None,
        }
    }

    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    /// Construct the analysis does not cover: reported, never guessed.
    pub fn not_analyzed(what: impl std::fmt::Display, span: Span) -> Self {
        SemaError::new(codes::NOT_ANALYZED, format!("not analyzed: {what}"), span)
    }
}

/// Scope of one query level.
#[derive(Debug, Clone, Default)]
pub struct Scope<'p> {
    pub sources: Vec<Source>,
    /// Columns of `SELECT *`, in PostgreSQL order: (source,
    /// column). A `USING` join puts its common columns first and
    /// does not repeat them.
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

    /// Adds a source; its columns are appended to `*`.
    pub fn push(&mut self, source: Source) -> usize {
        let i = self.sources.len();
        self.star.extend((0..source.columns.len()).map(|c| (i, c)));
        self.sources.push(source);
        i
    }

    /// Column `table.column` or `column`, here then in the enclosing
    /// scopes.
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
                codes::AMBIGUOUS_COLUMN,
                format!("column reference \"{column}\" is ambiguous"),
                span,
            )),
            // Column merged by `USING`: the one from `*`.
            [first, ..] => Ok(self
                .star
                .iter()
                .map(|(s, c)| &self.sources[*s].columns[*c])
                .find(|c| c.name == column)
                .unwrap_or(first)),
            // Source found at this level, column absent: PostgreSQL does not
            // look further.
            [] if table_seen => Err(SemaError::new(
                codes::COLUMN_NOT_FOUND,
                format!(
                    "column {}.{column} does not exist",
                    table.unwrap_or_default()
                ),
                span,
            )),
            [] => match self.parent {
                Some(p) => p.lookup(table, column, span),
                None if table.is_some() && !table_seen => Err(SemaError::new(
                    codes::TABLE_NOT_FOUND,
                    format!(
                        "missing FROM-clause entry for table \"{}\"",
                        table.unwrap_or_default()
                    ),
                    span,
                )),
                None => Err(SemaError::new(
                    codes::COLUMN_NOT_FOUND,
                    match table {
                        Some(t) => format!("column {t}.{column} does not exist"),
                        None => format!("column \"{column}\" does not exist"),
                    },
                    span,
                )),
            },
        }
    }

    /// Column of a reference `column`, `table.column` or
    /// `schema.table.column`.
    pub fn lookup_ref(
        &self,
        c: &pgzest_ast::expr::ColumnRef,
        span: Span,
    ) -> Result<&Column, SemaError> {
        let (Some(schema), Some(table)) = (&c.schema, &c.table) else {
            return self.lookup(
                c.table.as_ref().map(|t| t.value.as_str()),
                &c.column.value,
                span,
            );
        };
        let mut scope = Some(self);
        while let Some(s) = scope {
            if let Some(src) = s.sources.iter().find(|src| {
                src.relation
                    .as_ref()
                    .is_some_and(|r| r.schema == schema.value && r.name == table.value)
            }) {
                return src
                    .columns
                    .iter()
                    .chain(&src.hidden)
                    .find(|col| col.name == c.column.value)
                    .ok_or_else(|| {
                        SemaError::new(
                            codes::COLUMN_NOT_FOUND,
                            format!(
                                "column {}.{}.{} does not exist",
                                schema.value, table.value, c.column.value
                            ),
                            span,
                        )
                    });
            }
            scope = s.parent;
        }
        Err(SemaError::new(
            codes::TABLE_NOT_FOUND,
            format!("missing FROM-clause entry for table \"{}\"", table.value),
            span,
        ))
    }

    /// No source, at any level: an expression outside a query.
    pub fn is_empty(&self) -> bool {
        self.sources.is_empty() && self.parent.is_none_or(|p| p.is_empty())
    }

    /// A source of this level (without the enclosing scopes) has a column
    /// with this name.
    pub fn has_local_column(&self, column: &str) -> bool {
        self.sources
            .iter()
            .any(|s| s.columns.iter().chain(&s.hidden).any(|c| c.name == column))
    }

    /// Column merged by `USING`: visible only once in `*`.
    fn using_merged(&self, column: &str) -> bool {
        self.star
            .iter()
            .filter(|(s, c)| self.sources[*s].columns[*c].name == column)
            .count()
            == 1
    }

    /// Type of the whole row of the source named `name`, here then in
    /// the enclosing scopes.
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

    /// Source visible under this name.
    pub fn source(&self, name: &str) -> Option<&Source> {
        self.sources
            .iter()
            .find(|s| s.name.as_deref() == Some(name))
    }
}
