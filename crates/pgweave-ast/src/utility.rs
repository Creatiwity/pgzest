//! Statements utilitaires : ni requêtes ni définitions d'objets, mais
//! présents dans les migrations et les scripts — `DO`, `CALL`, `COPY`, `SET`,
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
    /// `DO [LANGUAGE l] $$ … $$` : bloc anonyme, exécuté immédiatement.
    Do {
        language: FunctionLanguage,
        body: FunctionBody,
        /// Position du premier caractère du corps dans le document.
        body_start: u32,
    },
    /// `CALL procédure(arguments)`.
    Call {
        name: ObjectName,
        args: Vec<Expr>,
    },
    Copy(CopyStatement),
    /// `SET [LOCAL | SESSION] paramètre { TO | = } valeur`. `local` : jusqu'à
    /// la fin de la transaction.
    Set {
        local: bool,
        name: String,
        value: SettingValue,
    },
    Transaction(TransactionCommand),
    /// `COMMENT ON objet IS 'texte'` (`None` : `IS NULL`, suppression).
    Comment {
        target: CommentTarget,
        name: ObjectName,
        text: Option<String>,
    },
    /// `GRANT` / `REVOKE`. Modélisé tel qu'écrit, en attendant la phase de
    /// conception des privilèges (PLAN.md, §6) : le diff de migration doit
    /// seulement ne pas les perdre.
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
    /// `ANALYZE` / `VACUUM [(options)] [table [(colonnes)], …]`.
    Maintenance {
        command: MaintenanceCommand,
        /// Options entre parenthèses, ou mots-clés de l'ancienne syntaxe
        /// (`VACUUM FULL VERBOSE`), en majuscules, dans l'ordre d'écriture.
        options: Vec<MaintenanceOption>,
        /// Vide : toutes les tables de la base.
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
    /// Valeur écrite (`ON`, `false`, `4`…), absente pour un drapeau nu.
    pub value: Option<String>,
    /// Ancienne syntaxe, sans parenthèses.
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
    /// `TO` (export) ou `FROM` (import).
    pub to: bool,
    pub target: CopyTarget,
    pub options: Vec<CopyOption>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum CopySource {
    /// `COPY table [(colonnes)]` : colonnes vides = toutes, dans l'ordre.
    Table {
        name: ObjectName,
        columns: Vec<Ident>,
    },
    /// `COPY (requête) TO …`.
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
    /// `text`, `csv` ou `binary`.
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
