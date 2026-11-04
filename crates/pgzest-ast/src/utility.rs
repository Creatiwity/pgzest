//! Utility statements: neither queries nor object definitions, but
//! present in migrations and scripts — `DO`, `CALL`, `COPY`, `SET`,
//! transactions, `COMMENT ON`, `GRANT`/`REVOKE`, `TRUNCATE`, `ANALYZE`,
//! `VACUUM`.

use crate::ddl::{FunctionBody, FunctionLanguage, SettingValue};
use crate::expr::{Expr, Ident, ObjectName};
use crate::node::Span;
use crate::stmt::SelectStmt;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Utility {
    pub span: Span,
    pub kind: UtilityKind,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum UtilityKind {
    /// `DO [LANGUAGE l] $$ … $$`: anonymous block, executed immediately.
    Do {
        language: FunctionLanguage,
        body: FunctionBody,
        /// Position of the body's first character in the document.
        body_start: u32,
    },
    /// `CALL procedure(arguments)`.
    Call {
        name: ObjectName,
        args: Vec<Expr>,
    },
    Copy(CopyStatement),
    /// `SET [LOCAL | SESSION] parameter { TO | = } value`. `local`: until
    /// the end of the transaction.
    Set {
        local: bool,
        name: String,
        value: SettingValue,
    },
    Transaction(TransactionCommand),
    /// `COMMENT ON object IS 'text'` (`None`: `IS NULL`, removal).
    Comment {
        target: CommentTarget,
        name: ObjectName,
        text: Option<String>,
    },
    /// `GRANT` / `REVOKE`. Modeled as written, pending the privileges
    /// design phase (PLAN.md, §6): the migration diff must merely
    /// not lose them.
    Privilege {
        grant: bool,
        privileges: String,
        objects: String,
        grantees: Vec<String>,
    },
    Truncate {
        tables: Vec<ObjectName>,
        restart_identity: bool,
        cascade: bool,
    },
    /// `ANALYZE` / `VACUUM [(options)] [table [(columns)], …]`.
    Maintenance {
        command: MaintenanceCommand,
        /// Options in parentheses, or keywords of the old syntax
        /// (`VACUUM FULL VERBOSE`), uppercase, in writing order.
        options: Vec<MaintenanceOption>,
        /// Empty: all tables of the database.
        tables: Vec<MaintenanceTarget>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MaintenanceCommand {
    Analyze,
    Vacuum,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MaintenanceOption {
    pub name: String,
    /// Written value (`ON`, `false`, `4`…), absent for a bare flag.
    pub value: Option<String>,
    /// Old syntax, without parentheses.
    pub legacy: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MaintenanceTarget {
    pub table: ObjectName,
    pub columns: Vec<Ident>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CopyStatement {
    pub source: CopySource,
    /// `TO` (export) or `FROM` (import).
    pub to: bool,
    pub target: CopyTarget,
    pub options: Vec<CopyOption>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum CopySource {
    /// `COPY table [(columns)]`: empty columns = all, in order.
    Table {
        name: ObjectName,
        columns: Vec<Ident>,
    },
    /// `COPY (query) TO …`.
    Query(Box<SelectStmt>),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum CopyTarget {
    Stdin,
    Stdout,
    File(String),
    Program(String),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum CopyOption {
    /// `text`, `csv` or `binary`.
    Format(String),
    Header(bool),
    Delimiter(char),
    Null(String),
    Quote(char),
    Escape(char),
    Encoding(String),
    Freeze(bool),
    ForceQuote(Vec<Ident>),
    ForceNotNull(Vec<Ident>),
    ForceNull(Vec<Ident>),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum TransactionCommand {
    Begin,
    Commit,
    Rollback,
    Savepoint(Ident),
    ReleaseSavepoint(Ident),
    RollbackToSavepoint(Ident),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CommentTarget {
    Table,
    Column,
    View,
    MaterializedView,
    Index,
    Sequence,
    Type,
    Domain,
    Function,
    Procedure,
    Schema,
    Extension,
}
