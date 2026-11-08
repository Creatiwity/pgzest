//! Ordered application of statements to the catalog: the replay.
//!
//! Each statement modifies the catalog as PostgreSQL would: creation,
//! `ALTER`, `DROP`, `CREATE EXTENSION` (which loads the extension's pack),
//! `SET search_path`. What PostgreSQL would reject (missing object, unknown
//! type, drop without `CASCADE` of a referenced object) is reported, without
//! interrupting the replay — on migrations applied in production, such a
//! diagnostic reveals a gap in pgzest.

use pgzest_ast::Statement;
use pgzest_ast::ddl::FunctionBody;
use pgzest_ast::ddl::SettingValue;
use pgzest_ast::ddl::{
    AlterColumnAction, AlterExtensionOp, AlterTableOp, AlterTypeOp, ColumnConstraint, ColumnDef,
    CreateFunction, CreateProcedure, CreateTable, DdlStatement, DropObject, EnumValuePosition,
    FunctionLanguage, FunctionParam, IndexElement, LikeFeature, ObjectKind, ParamMode,
    PolicyCommand, ReturnType, TableConstraint, TriggerEvent, TriggerForEach, TriggerTiming,
    TypeDefinition,
};
use pgzest_ast::emit::{emit_data_type, emit_expr, emit_statement};
use pgzest_ast::expr::{ExprKind, Ident, ObjectName};
use pgzest_ast::plpgsql::{Block, PlStatement, PlStatementKind};
use pgzest_ast::stmt::{SelectItem, SelectStmt, TableSource};
use pgzest_ast::types::DataType;
use pgzest_ast::utility::UtilityKind;
use pgzest_diagnostics::{Diagnostic, codes};

use crate::builtin;
use crate::catalog::{Catalog, SearchPath};
use crate::model::{
    Arg, ArgMode, Attribute, CheckConstraint, Column, Constraint, ConstraintKind, Extension,
    Generated, IdentityKind, Index, Persistence, Policy, QName, Relation, RelationKind, Routine,
    RoutineKind, Schema, Sequence, Trigger, TypeDef, TypeKind, TypeRef, Volatility,
};
use crate::names::{array_type_name, choose_name, columns_addition};

/// Replay state: the catalog and the current `search_path`.
/// Typing of a view's query.
///
/// Provided by semantic analysis (`pgzest-sema`), which depends on the
/// catalog and therefore cannot be called from here. Without a typer, the
/// columns of a view keep an unknown type — never assumed.
pub trait QueryTyper: Send + Sync {
    /// Columns produced by `query`: name and type, modifiers included.
    /// `Err` says why they are not established.
    fn output_columns(
        &self,
        catalog: &Catalog,
        path: &SearchPath,
        query: &SelectStmt,
    ) -> Result<Vec<(String, TypeRef)>, String>;
}

pub struct Replay {
    pub catalog: Catalog,
    pub search_path: SearchPath,
    /// Typer for view queries, if there is one.
    pub typer: Option<std::sync::Arc<dyn QueryTyper>>,
    /// Extension packs of the project, besides those shipped with pgzest.
    pub packs: std::sync::Arc<crate::ProjectPacks>,
}

impl Replay {
    pub fn new(catalog: Catalog) -> Self {
        Self {
            catalog,
            search_path: SearchPath::default(),
            typer: None,
            packs: Default::default(),
        }
    }

    /// Applies a statement; diagnostics are appended to `out`.
    pub fn apply(&mut self, stmt: &Statement, out: &mut Vec<Diagnostic>) {
        let mut a = Applier {
            cat: &mut self.catalog,
            path: &mut self.search_path,
            out,
            pending_indexes: Vec::new(),
            typer: self.typer.as_deref(),
            packs: &self.packs,
        };
        a.statement(stmt);
    }
}

struct Applier<'a> {
    cat: &'a mut Catalog,
    path: &'a mut SearchPath,
    out: &'a mut Vec<Diagnostic>,
    /// Indexes copied by `LIKE … INCLUDING INDEXES`, created once the table
    /// is registered.
    pending_indexes: Vec<pgzest_ast::ddl::CreateIndex>,
    typer: Option<&'a dyn QueryTyper>,
    packs: &'a crate::ProjectPacks,
}

/// Written name, split: `(explicit schema, name)`.
fn split_name(n: &ObjectName) -> (Option<&str>, &str) {
    match n.0.as_slice() {
        [name] => (None, name.value.as_str()),
        [.., schema, name] => (Some(schema.value.as_str()), name.value.as_str()),
        [] => (None, ""),
    }
}

fn written(n: &ObjectName) -> String {
    n.0.iter()
        .map(|i| i.value.as_str())
        .collect::<Vec<_>>()
        .join(".")
}

/// Built-in type designated by an AST type: name in `pg_catalog` and
/// modifiers.
fn builtin_type(dt: &DataType) -> Option<(&'static str, Vec<String>)> {
    Some(match dt {
        DataType::SmallInt => ("int2", vec![]),
        DataType::Integer => ("int4", vec![]),
        DataType::BigInt => ("int8", vec![]),
        DataType::Numeric { precision, scale } => (
            "numeric",
            match (precision, scale) {
                (Some(p), Some(s)) => vec![p.to_string(), s.to_string()],
                (Some(p), None) => vec![p.to_string()],
                _ => vec![],
            },
        ),
        DataType::Real => ("float4", vec![]),
        DataType::DoublePrecision => ("float8", vec![]),
        DataType::Text => ("text", vec![]),
        DataType::Varchar(n) => ("varchar", n.iter().map(u64::to_string).collect()),
        // `char` without a length is `character(1)`.
        DataType::Char(n) => ("bpchar", vec![n.unwrap_or(1).to_string()]),
        DataType::Date => ("date", vec![]),
        DataType::Time(p) => ("time", p.iter().map(u64::to_string).collect()),
        DataType::TimeTz(p) => ("timetz", p.iter().map(u64::to_string).collect()),
        DataType::Timestamp(p) => ("timestamp", p.iter().map(u64::to_string).collect()),
        DataType::TimestampTz(p) => ("timestamptz", p.iter().map(u64::to_string).collect()),
        DataType::Interval => ("interval", vec![]),
        DataType::Boolean => ("bool", vec![]),
        DataType::Bytea => ("bytea", vec![]),
        DataType::Json => ("json", vec![]),
        DataType::Jsonb => ("jsonb", vec![]),
        DataType::Uuid => ("uuid", vec![]),
        _ => return None,
    })
}

/// `serial` pseudo-types: actual type and name.
fn serial_kind(dt: &DataType) -> Option<&'static str> {
    match dt {
        DataType::Custom(n) => match n.as_str() {
            "serial" | "serial4" => Some("int4"),
            "bigserial" | "serial8" => Some("int8"),
            "smallserial" | "serial2" => Some("int2"),
            _ => None,
        },
        _ => None,
    }
}

impl Applier<'_> {
    fn error(&mut self, code: &'static str, msg: String) {
        self.out.push(Diagnostic::error(code, msg));
    }

    fn next_order(&mut self) -> u64 {
        self.cat.next_order += 1;
        self.cat.next_order
    }

    // ── Names ────────────────────────────────────────────────────────────────

    /// Qualified name of a created object: explicit schema, `pg_temp` for a
    /// temporary object, otherwise the creation schema of the `search_path`.
    fn creation_name(&mut self, n: &ObjectName, temporary: bool) -> Option<QName> {
        let (schema, name) = split_name(n);
        let schema = match schema {
            Some(s) => s.to_string(),
            None if temporary => "pg_temp".to_string(),
            None => match self.path.creation_schema() {
                Some(s) => s.to_string(),
                None => {
                    self.error(
                        codes::SCHEMA_NOT_FOUND,
                        format!("no schema has been selected to create \"{name}\" in (empty search_path)"),
                    );
                    return None;
                }
            },
        };
        if schema != "pg_temp" && !self.cat.schema_exists(&schema) {
            self.error(
                codes::SCHEMA_NOT_FOUND,
                format!("schema \"{schema}\" does not exist"),
            );
            return None;
        }
        Some(QName::new(schema, name))
    }

    fn relation_name(&mut self, n: &ObjectName) -> Option<QName> {
        let (schema, name) = split_name(n);
        match self.cat.resolve_relation(schema, name, self.path) {
            Some(r) => Some(r.name.clone()),
            None => {
                self.error(
                    codes::OBJECT_NOT_FOUND,
                    format!("relation \"{}\" does not exist", written(n)),
                );
                None
            }
        }
    }

    fn type_name(&self, schema: Option<&str>, name: &str) -> Option<QName> {
        if let Some(t) = self.cat.resolve_type(schema, name, self.path) {
            return Some(t.name.clone());
        }
        // A table's row type has its name.
        self.cat
            .resolve_relation(schema, name, self.path)
            .map(|r| r.name.clone())
    }

    /// Type reference of an AST type. A name that cannot be found is reported
    /// and kept as written.
    fn type_ref(&mut self, dt: &DataType) -> TypeRef {
        if let Some((name, modifiers)) = builtin_type(dt) {
            return TypeRef::Named {
                name: QName::pg_catalog(name),
                modifiers,
            };
        }
        let (written, modifiers) = match dt {
            DataType::Array(inner) => return TypeRef::Array(Box::new(self.type_ref(inner))),
            DataType::Custom(n) => (n.as_str(), Vec::new()),
            DataType::Modified { name, modifiers } => (name.as_str(), modifiers.clone()),
            _ => unreachable!("built-in types handled above"),
        };
        let (schema, name) = match written.rsplit_once('.') {
            Some((s, n)) => (Some(s), n),
            None => (None, written),
        };
        match self.type_name(schema, name) {
            Some(q) => TypeRef::Named { name: q, modifiers },
            None => {
                let mut d = Diagnostic::error(
                    codes::TYPE_NOT_FOUND,
                    format!("type \"{written}\" does not exist"),
                );
                let from =
                    builtin::extensions_providing(builtin::ObjectSort::Type, name, self.packs);
                if let Some(ext) = from.first() {
                    let ext_name = ext.split('@').next().unwrap_or(ext);
                    d = d.with_hint(format!(
                        "\"{name}\" is provided by extension {ext}, missing here: add `CREATE EXTENSION {ext_name}`"
                    ));
                }
                self.out.push(d);
                TypeRef::Unresolved(emit_data_type(dt))
            }
        }
    }

    // ── Dispatch ─────────────────────────────────────────────────────────────

    fn statement(&mut self, stmt: &Statement) {
        match stmt {
            Statement::Ddl(ddl) => self.ddl(ddl),
            Statement::Utility(u) => match &u.kind {
                UtilityKind::Set { name, value, .. }
                    if name.eq_ignore_ascii_case("search_path") =>
                {
                    self.set_search_path(value);
                }
                UtilityKind::Comment { target, name, text } => {
                    let key = format!("{target:?} {}", written(name));
                    match text {
                        Some(_) => {
                            self.cat.comments.insert(key, emit_statement(stmt));
                        }
                        None => {
                            self.cat.comments.remove(&key);
                        }
                    }
                }
                UtilityKind::Call { .. } => self.calls_with_effects(stmt),
                UtilityKind::Do { body, .. } => match body {
                    FunctionBody::Parsed(b) => {
                        let _ = self.exec_block(&b.block);
                    }
                    FunctionBody::Raw(_) => self.not_replayed("unanalyzed DO block"),
                    _ => {}
                },
                _ => {}
            },
            // Queries and data modifications do not change the catalog —
            // except through the routines they call.
            Statement::Select(_)
            | Statement::Insert(_)
            | Statement::Update(_)
            | Statement::Delete(_) => self.calls_with_effects(stmt),
        }
    }

    // ── DO blocks ────────────────────────────────────────────────────────────

    fn not_replayed(&mut self, why: &str) {
        self.out.push(
            Diagnostic::warning(
                codes::DDL_NOT_REPLAYED,
                format!("DDL not replayed ({why}): the catalog may differ from the database"),
            )
            .with_hint("only conditions on the system catalog (pg_type, pg_enum, information_schema…) are evaluated"),
        );
    }

    /// Runs a block; `false` if a `RETURN` interrupted it.
    fn exec_block(&mut self, block: &Block) -> bool {
        // Exception handlers are not walked: on an applied migration, the
        // block ran without error.
        self.exec_statements(&block.statements)
    }

    fn exec_statements(&mut self, stmts: &[PlStatement]) -> bool {
        for s in stmts {
            if !self.exec_statement(s) {
                return false;
            }
        }
        true
    }

    fn exec_statement(&mut self, stmt: &PlStatement) -> bool {
        match &stmt.kind {
            PlStatementKind::Sql { statement, .. } => {
                self.statement(statement);
                true
            }
            PlStatementKind::Return(_) => false,
            PlStatementKind::Block(b) => self.exec_block(b),
            PlStatementKind::If {
                condition,
                then_branch,
                elsif_branches,
                else_branch,
            } => {
                let branches = std::iter::once((condition, then_branch))
                    .chain(elsif_branches.iter().map(|(c, b)| (c, b)));
                for (cond, body) in branches {
                    match crate::query::eval_condition(self.cat, cond) {
                        Some(true) => return self.exec_statements(body),
                        Some(false) => continue,
                        None => {
                            if has_effect(std::slice::from_ref(stmt)) {
                                self.not_replayed("condition not evaluable on the catalog");
                            }
                            return true;
                        }
                    }
                }
                match else_branch {
                    Some(b) => self.exec_statements(b),
                    None => true,
                }
            }
            // `RAISE EXCEPTION` reached: the migration would have failed. It
            // was applied, so the rebuilt state differs from reality.
            PlStatementKind::Raise {
                level: pgzest_ast::plpgsql::RaiseLevel::Exception,
                ..
            } => {
                self.not_replayed("RAISE EXCEPTION reached: inconsistent rebuilt state");
                false
            }
            other => {
                let stmt_ref = std::slice::from_ref(stmt);
                if has_effect(stmt_ref) {
                    let _ = other;
                    self.not_replayed("dynamic SQL, loop or CASE in a DO block");
                }
                true
            }
        }
    }

    /// Calls to project routines whose body modifies the catalog: their
    /// effects are not replayed, and we say so.
    fn calls_with_effects(&mut self, stmt: &Statement) {
        let Ok(v) = serde_json::to_value(stmt) else {
            return;
        };
        let mut names = Vec::new();
        collect_calls(&v, &mut names);
        if let Statement::Utility(pgzest_ast::Utility {
            kind: UtilityKind::Call { name, .. },
            ..
        }) = stmt
        {
            names.push(name.0.iter().map(|i| i.value.clone()).collect());
        }
        for parts in names {
            let (schema, name) = match parts.as_slice() {
                [n] => (None, n.as_str()),
                [s, n] => (Some(s.as_str()), n.as_str()),
                _ => continue,
            };
            let effects = self
                .cat
                .resolve_routines(schema, name, self.path)
                .iter()
                .any(|r| r.catalog_effects);
            if effects {
                self.not_replayed(&format!(
                    "call to \"{name}\", whose body modifies the catalog"
                ));
            }
        }
    }

    fn set_search_path(&mut self, value: &SettingValue) {
        match value {
            SettingValue::Values(v) if v.is_empty() => *self.path = SearchPath::default(),
            SettingValue::Values(v) => {
                *self.path = SearchPath::new(
                    v.iter()
                        .map(|s| s.trim().to_string())
                        .filter(|s| s != "\"$user\"" && s != "$user"),
                );
            }
            SettingValue::FromCurrent => {}
        }
    }

    fn ddl(&mut self, ddl: &DdlStatement) {
        match ddl {
            DdlStatement::CreateSchema(cs) => {
                if self.cat.schema_exists(&cs.name.value) {
                    if !cs.if_not_exists {
                        self.error(
                            codes::OBJECT_ALREADY_EXISTS,
                            format!("schema \"{}\" already exists", cs.name.value),
                        );
                    }
                    return;
                }
                self.cat.schemas.insert(
                    cs.name.value.clone(),
                    Schema {
                        name: cs.name.value.clone(),
                        owner: cs.authorization.as_ref().map(|a| a.value.clone()),
                    },
                );
            }
            DdlStatement::CreateExtension(ce) => self.create_extension(ce),
            DdlStatement::AlterExtension(ae) => self.alter_extension(ae),
            DdlStatement::CreateTable(ct) => self.create_table(ct),
            DdlStatement::AlterTable(at) => {
                let Some(name) = self
                    .cat
                    .resolve_relation(split_name(&at.name).0, split_name(&at.name).1, self.path)
                    .map(|r| r.name.clone())
                else {
                    if !at.if_exists {
                        self.error(
                            codes::OBJECT_NOT_FOUND,
                            format!("relation \"{}\" does not exist", written(&at.name)),
                        );
                    }
                    return;
                };
                for op in &at.operations {
                    self.alter_table(&name, op);
                }
            }
            DdlStatement::CreateIndex(ci) => self.create_index(ci),
            DdlStatement::CreateType(ct) => self.create_type(ct),
            DdlStatement::AlterType(at) => self.alter_type(at),
            DdlStatement::CreateDomain(cd) => {
                let Some(name) = self.creation_name(&cd.name, false) else {
                    return;
                };
                let base = self.type_ref(&cd.data_type);
                let checks = cd
                    .checks
                    .iter()
                    .map(|c| CheckConstraint {
                        name: c.name.as_ref().map(|n| n.value.clone()),
                        expr: emit_expr(&c.expr),
                    })
                    .collect();
                self.add_type(
                    name,
                    TypeKind::Domain {
                        base,
                        not_null: cd.not_null,
                        default: cd.default.as_ref().map(emit_expr),
                        checks,
                    },
                    'U',
                );
            }
            DdlStatement::CreateFunction(cf) => self.create_function(cf),
            DdlStatement::CreateProcedure(cp) => self.create_procedure(cp),
            DdlStatement::CreateTrigger(ct) => {
                let Some(table) = self.relation_name(&ct.table) else {
                    return;
                };
                let (fs, fname) = split_name(&ct.function);
                let Some(function) = self
                    .cat
                    .resolve_routines(fs, fname, self.path)
                    .into_iter()
                    .find(|r| r.input_types().next().is_none())
                    .map(|r| r.name.clone())
                else {
                    self.error(
                        codes::FUNCTION_NOT_FOUND,
                        format!("function {}() does not exist", written(&ct.function)),
                    );
                    return;
                };
                let triggers = self.cat.triggers.entry(table.clone()).or_default();
                if triggers.contains_key(&ct.name.value) && !ct.or_replace {
                    self.error(
                        codes::OBJECT_ALREADY_EXISTS,
                        format!(
                            "trigger \"{}\" for relation {table} already exists",
                            ct.name.value
                        ),
                    );
                    return;
                }
                let old_table = ct
                    .referencing
                    .iter()
                    .find(|t| !t.new)
                    .map(|t| t.name.value.clone());
                let new_table = ct
                    .referencing
                    .iter()
                    .find(|t| t.new)
                    .map(|t| t.name.value.clone());
                triggers.insert(
                    ct.name.value.clone(),
                    Trigger {
                        name: ct.name.value.clone(),
                        table,
                        function,
                        timing: match ct.timing {
                            TriggerTiming::Before => "BEFORE",
                            TriggerTiming::After => "AFTER",
                            TriggerTiming::InsteadOf => "INSTEAD OF",
                        }
                        .into(),
                        events: ct
                            .events
                            .iter()
                            .map(|e| match e {
                                TriggerEvent::Insert => "INSERT".to_string(),
                                TriggerEvent::Update(cols) if cols.is_empty() => "UPDATE".into(),
                                TriggerEvent::Update(cols) => format!(
                                    "UPDATE OF {}",
                                    cols.iter()
                                        .map(|c| c.value.as_str())
                                        .collect::<Vec<_>>()
                                        .join(", ")
                                ),
                                TriggerEvent::Delete => "DELETE".into(),
                                TriggerEvent::Truncate => "TRUNCATE".into(),
                            })
                            .collect(),
                        for_each_row: matches!(ct.for_each, TriggerForEach::Row),
                        old_table,
                        new_table,
                        definition: Some(emit_statement(&Statement::Ddl(ddl.clone()))),
                    },
                );
            }
            DdlStatement::CreatePolicy(cp) => {
                let Some(table) = self.relation_name(&cp.table) else {
                    return;
                };
                let policies = self.cat.policies.entry(table.clone()).or_default();
                if policies.contains_key(&cp.name.value) {
                    self.error(
                        codes::OBJECT_ALREADY_EXISTS,
                        format!(
                            "policy \"{}\" for table {table} already exists",
                            cp.name.value
                        ),
                    );
                    return;
                }
                policies.insert(
                    cp.name.value.clone(),
                    Policy {
                        name: cp.name.value.clone(),
                        table,
                        command: match cp.command {
                            PolicyCommand::All => "ALL",
                            PolicyCommand::Select => "SELECT",
                            PolicyCommand::Insert => "INSERT",
                            PolicyCommand::Update => "UPDATE",
                            PolicyCommand::Delete => "DELETE",
                        }
                        .into(),
                        permissive: cp.permissive,
                        roles: cp.roles.iter().map(|r| r.value.clone()).collect(),
                        using: cp.using.as_ref().map(emit_expr),
                        with_check: cp.with_check.as_ref().map(emit_expr),
                        definition: Some(emit_statement(&Statement::Ddl(ddl.clone()))),
                    },
                );
            }
            DdlStatement::CreateView(cv) => self.create_view(cv),
            DdlStatement::CreateSequence(cs) => {
                let Some(name) = self.creation_name(&cs.name, cs.temporary) else {
                    return;
                };
                if self.cat.sequences.contains_key(&name) || self.cat.relations.contains_key(&name)
                {
                    if !cs.if_not_exists {
                        self.error(
                            codes::OBJECT_ALREADY_EXISTS,
                            format!("relation \"{name}\" already exists"),
                        );
                    }
                    return;
                }
                let owned_by = cs.owned_by.as_ref().and_then(|o| match o.0.as_slice() {
                    [.., table, column] => {
                        let t = ObjectName(o.0[..o.0.len() - 1].to_vec());
                        let _ = table;
                        self.relation_name(&t).map(|q| (q, column.value.clone()))
                    }
                    _ => None,
                });
                self.cat.sequences.insert(
                    name.clone(),
                    Sequence {
                        name,
                        owned_by,
                        definition: Some(emit_statement(&Statement::Ddl(ddl.clone()))),
                    },
                );
            }
            DdlStatement::Drop(d) => self.drop(d),
        }
    }

    // ── Extensions ───────────────────────────────────────────────────────────

    fn create_extension(&mut self, ce: &pgzest_ast::ddl::CreateExtension) {
        let name = ce.name.value.clone();
        if self.cat.extensions.contains_key(&name) {
            if !ce.if_not_exists {
                self.error(
                    codes::OBJECT_ALREADY_EXISTS,
                    format!("extension \"{name}\" already exists"),
                );
            }
            return;
        }
        let schema = match &ce.schema {
            Some(s) => s.value.clone(),
            None => match self.path.creation_schema() {
                Some(s) => s.to_string(),
                None => {
                    self.error(codes::SCHEMA_NOT_FOUND, "empty search_path".into());
                    return;
                }
            },
        };
        if !self.cat.schema_exists(&schema) {
            self.error(
                codes::SCHEMA_NOT_FOUND,
                format!("schema \"{schema}\" does not exist"),
            );
            return;
        }
        let Some(version) = self.extension_version(&name, ce.version.as_deref()) else {
            return;
        };
        self.load_extension(&name, &version, &schema);
    }

    /// Version to load: the requested one, otherwise the only available pack
    /// (the project's and those shipped with pgzest).
    fn extension_version(&mut self, name: &str, requested: Option<&str>) -> Option<String> {
        let mut available = self.packs.versions(name);
        for v in builtin::versions(name) {
            if !available.contains(&v) {
                available.push(v);
            }
        }
        let chosen = match requested {
            Some(v) => available.iter().find(|a| **a == v).map(|v| v.to_string()),
            None if available.len() == 1 => Some(available[0].to_string()),
            None => None,
        };
        if chosen.is_none() {
            let wanted = requested.map(|v| format!("@{v}")).unwrap_or_default();
            self.out.push(
                Diagnostic::error(
                    codes::EXTENSION_UNAVAILABLE,
                    format!("no catalog pack for extension \"{name}{wanted}\""),
                )
                .with_hint(if available.is_empty() {
                    format!(
                        "generate its pack with `pgzest db pack --extension {name}` on a server \
                         where it is installed, and list its directory in `[postgres] packs`"
                    )
                } else {
                    format!("available versions: {}", available.join(", "))
                }),
            );
        }
        chosen
    }

    fn load_extension(&mut self, name: &str, version: &str, schema: &str) {
        // The project's pack first: it may replace the shipped one.
        let pack = match self.packs.get(name, version) {
            Some(p) => builtin::relocated(p, schema),
            None => builtin::extension_pack(name, version, schema),
        };
        match pack {
            Ok(pack) => {
                self.cat.load_pack(&pack);
                self.cat.extension_packs.insert(name.to_string(), pack);
                self.cat.extensions.insert(
                    name.to_string(),
                    Extension {
                        name: name.to_string(),
                        version: Some(version.to_string()),
                        schema: schema.to_string(),
                    },
                );
            }
            Err(e) => self.error(codes::EXTENSION_UNAVAILABLE, e.0),
        }
    }

    fn alter_extension(&mut self, ae: &pgzest_ast::ddl::AlterExtension) {
        let name = ae.name.value.clone();
        let Some(ext) = self.cat.extensions.get(&name).cloned() else {
            self.error(
                codes::OBJECT_NOT_FOUND,
                format!("extension \"{name}\" does not exist"),
            );
            return;
        };
        let (version, schema) = match &ae.operation {
            AlterExtensionOp::Update(v) => {
                let Some(version) = self.extension_version(&name, v.as_deref()) else {
                    return;
                };
                (version, ext.schema.clone())
            }
            AlterExtensionOp::SetSchema(s) => {
                (ext.version.clone().unwrap_or_default(), s.value.clone())
            }
        };
        self.cat.unload_extension(&name);
        self.load_extension(&name, &version, &schema);
    }

    // ── Tables ───────────────────────────────────────────────────────────────

    fn create_table(&mut self, ct: &CreateTable) {
        let Some(name) = self.creation_name(&ct.name, ct.temporary) else {
            return;
        };
        if self.cat.relations.contains_key(&name) {
            if !ct.if_not_exists {
                self.error(
                    codes::OBJECT_ALREADY_EXISTS,
                    format!("relation \"{name}\" already exists"),
                );
            }
            return;
        }
        let mut rel = Relation {
            name: name.clone(),
            kind: RelationKind::Table,
            persistence: if ct.temporary {
                Persistence::Temporary
            } else if ct.unlogged {
                Persistence::Unlogged
            } else {
                Persistence::Permanent
            },
            columns: Vec::new(),
            constraints: Vec::new(),
            row_level_security: false,
            force_row_level_security: false,
            opaque: false,
            definition: None,
            order: self.next_order(),
        };
        // Inserted first: automatically named constraints must see the table,
        // and an FK may reference itself.
        self.cat.relations.insert(name.clone(), rel.clone());
        self.add_rowtype_array(&name);

        if let Some(q) = &ct.as_query {
            // `CREATE TABLE … AS`: the query's columns, named by the optional
            // list.
            let aliases: Vec<&Ident> = ct.columns.iter().map(|c| &c.name).collect();
            let (columns, opaque) = self.query_columns("table", &name, q, &aliases);
            rel.columns = columns;
            rel.opaque = opaque;
            self.cat.relations.insert(name.clone(), rel);
            return;
        }
        let mut pending: Vec<(TableConstraint, Option<String>)> = Vec::new();
        let mut ok = true;
        for (i, col) in ct.columns.iter().enumerate() {
            for like in ct.likes.iter().filter(|l| l.position == i) {
                ok &= self.copy_like(&mut rel, like);
            }
            self.add_column_def(&mut rel, col, &mut pending);
        }
        for like in ct.likes.iter().filter(|l| l.position >= ct.columns.len()) {
            ok &= self.copy_like(&mut rel, like);
        }
        if !ok {
            // Statement rejected as a whole: no table, row type or sequence.
            self.drop_relation(&name);
            self.cat
                .sequences
                .retain(|_, s| s.owned_by.as_ref().is_none_or(|(t, _)| *t != name));
            return;
        }
        for tc in &ct.constraints {
            pending.push((tc.clone(), None));
        }
        self.cat.relations.insert(name.clone(), rel);
        for (tc, column) in pending {
            self.add_constraint(&name, &tc, column.as_deref(), false);
        }
        for ci in std::mem::take(&mut self.pending_indexes) {
            self.create_index(&ci);
        }
    }

    /// Copies the columns of a `LIKE`; `false` if the source does not exist
    /// (PostgreSQL then rejects the whole `CREATE TABLE`).
    fn copy_like(&mut self, rel: &mut Relation, like: &pgzest_ast::ddl::TableLike) -> bool {
        let Some(src_name) = self.relation_name(&like.source) else {
            return false;
        };
        let src = self.cat.relations[&src_name].clone();
        let include = |f: LikeFeature| {
            like.options
                .iter()
                .rev()
                .find(|o| o.feature == f || o.feature == LikeFeature::All)
                .is_some_and(|o| o.including)
        };
        let (defaults, identity, generated, constraints, indexes) = (
            include(LikeFeature::Defaults),
            include(LikeFeature::Identity),
            include(LikeFeature::Generated),
            include(LikeFeature::Constraints),
            include(LikeFeature::Indexes),
        );
        for c in &src.columns {
            rel.columns.push(Column {
                name: c.name.clone(),
                ty: c.ty.clone(),
                not_null: c.not_null,
                default: if defaults { c.default.clone() } else { None },
                identity: if identity { c.identity } else { None },
                generated: if generated { c.generated.clone() } else { None },
            });
        }
        for con in &src.constraints {
            let keep = match &con.kind {
                ConstraintKind::Check { .. } => constraints,
                ConstraintKind::PrimaryKey { .. }
                | ConstraintKind::Unique { .. }
                | ConstraintKind::Exclude { .. } => indexes,
                ConstraintKind::ForeignKey { .. } => false,
            };
            if !keep {
                continue;
            }
            // Constraints copied by `LIKE` are renamed after the new table
            // (CHECKs keep their name).
            let name = match &con.kind {
                ConstraintKind::Check { .. } => con.name.clone(),
                ConstraintKind::PrimaryKey { .. } => self.free_constraint_name(rel, None, "pkey"),
                ConstraintKind::Unique { columns } => {
                    let add = columns_addition(columns.iter().map(String::as_str));
                    self.free_constraint_name(rel, Some(&add), "key")
                }
                ConstraintKind::Exclude { .. } => self.free_constraint_name(rel, None, "excl"),
                ConstraintKind::ForeignKey { .. } => unreachable!(),
            };
            rel.constraints.push(Constraint {
                name,
                kind: con.kind.clone(),
                definition: con.definition.clone(),
                not_valid: con.not_valid,
            });
        }
        // Plain indexes of the source, recreated on the new table with a name
        // chosen as PostgreSQL does (`<table>_<columns>_idx`).
        if indexes {
            let sources: Vec<Index> = self
                .cat
                .indexes
                .values()
                .filter(|i| i.table == src_name)
                .cloned()
                .collect();
            for idx in sources {
                let Ok(stmts) = pgzest_parser::parse(&idx.definition) else {
                    continue;
                };
                let Some(Statement::Ddl(DdlStatement::CreateIndex(mut ci))) =
                    stmts.into_iter().next()
                else {
                    continue;
                };
                ci.name = None;
                ci.table = ObjectName(vec![
                    Ident {
                        value: rel.name.schema.clone(),
                        quoted: true,
                    },
                    Ident {
                        value: rel.name.name.clone(),
                        quoted: true,
                    },
                ]);
                self.pending_indexes.push(ci);
            }
        }
        true
    }

    /// Column of a `CREATE TABLE` / `ADD COLUMN`, with its column constraints
    /// (turned into table constraints, applied by the caller).
    fn add_column_def(
        &mut self,
        rel: &mut Relation,
        col: &ColumnDef,
        pending: &mut Vec<(TableConstraint, Option<String>)>,
    ) {
        let mut column = Column {
            name: col.name.value.clone(),
            ty: TypeRef::Unknown,
            not_null: !col.nullable,
            default: None,
            identity: col.identity.map(|i| match i {
                pgzest_ast::ddl::Identity::Always => IdentityKind::Always,
                pgzest_ast::ddl::Identity::ByDefault => IdentityKind::ByDefault,
            }),
            generated: col.generated.as_ref().map(|g| Generated {
                expr: emit_expr(&g.expr),
                stored: g.stored,
            }),
        };
        if let Some(real) = serial_kind(&col.data_type) {
            // `serial`: integer, NOT NULL, default taken from a sequence owned
            // by the column.
            column.ty = TypeRef::named(QName::pg_catalog(real));
            column.not_null = true;
            let seq = self.owned_sequence(&rel.name, &column.name);
            column.default = Some(format!(
                "nextval('{}'::regclass)",
                if seq.schema == "public"
                    || Some(seq.schema.as_str()) == self.path.creation_schema()
                {
                    crate::model::quote_ident(&seq.name)
                } else {
                    seq.to_string()
                }
                .replace('\'', "''")
            ));
        } else {
            column.ty = self.type_ref(&col.data_type);
            if let Some(e) = &col.default {
                column.default = self.stored_default(e, &column.ty);
            }
        }
        if column.identity.is_some() {
            column.not_null = true;
            self.owned_sequence(&rel.name, &column.name);
        }
        for c in &col.constraints {
            let name = col.name.clone();
            let tc = match c {
                ColumnConstraint::NotNull => {
                    column.not_null = true;
                    continue;
                }
                ColumnConstraint::Default(e) => {
                    column.default = self.stored_default(e, &column.ty);
                    continue;
                }
                ColumnConstraint::PrimaryKey => {
                    column.not_null = true;
                    TableConstraint::PrimaryKey {
                        name: None,
                        columns: vec![name],
                    }
                }
                ColumnConstraint::Unique => TableConstraint::Unique {
                    name: None,
                    columns: vec![name],
                },
                ColumnConstraint::Check(e) => TableConstraint::Check {
                    name: None,
                    expr: e.clone(),
                },
                ColumnConstraint::References {
                    table,
                    column: ref_col,
                    on_delete,
                    on_update,
                } => TableConstraint::ForeignKey {
                    name: None,
                    columns: vec![name],
                    ref_table: table.clone(),
                    ref_columns: ref_col.iter().cloned().collect(),
                    on_delete: on_delete.clone(),
                    on_update: on_update.clone(),
                },
            };
            pending.push((tc, Some(col.name.value.clone())));
        }
        rel.columns.push(column);
    }

    /// `table_column_seq` sequence of a `serial` or an identity.
    fn owned_sequence(&mut self, table: &QName, column: &str) -> QName {
        let name = choose_name(&table.name, Some(column), "seq", |n| {
            let q = QName::new(table.schema.clone(), n);
            self.cat.relations.contains_key(&q) || self.cat.sequences.contains_key(&q)
        });
        let q = QName::new(table.schema.clone(), name);
        self.cat.sequences.insert(
            q.clone(),
            Sequence {
                name: q.clone(),
                owned_by: Some((table.clone(), column.to_string())),
                definition: None,
            },
        );
        q
    }

    /// Free name for a constraint of `rel`: neither a constraint of the
    /// table, nor a relation or index of the schema (a PK or UNIQUE creates an
    /// index with the same name).
    fn free_constraint_name(&self, rel: &Relation, name2: Option<&str>, label: &str) -> String {
        choose_name(&rel.name.name, name2, label, |n| {
            let q = QName::new(rel.name.schema.clone(), n);
            rel.constraints.iter().any(|c| c.name == n)
                || self.cat.relations.contains_key(&q)
                || self.cat.indexes.contains_key(&q)
                || self.cat.sequences.contains_key(&q)
        })
    }

    fn add_constraint(
        &mut self,
        table: &QName,
        tc: &TableConstraint,
        column: Option<&str>,
        not_valid: bool,
    ) {
        let Some(rel) = self.cat.relations.get(table).cloned() else {
            return;
        };
        let cols = |c: &[Ident]| c.iter().map(|i| i.value.clone()).collect::<Vec<String>>();
        let (explicit, kind, label, addition) = match tc {
            TableConstraint::PrimaryKey { name, columns } => (
                name,
                ConstraintKind::PrimaryKey {
                    columns: cols(columns),
                },
                "pkey",
                None,
            ),
            TableConstraint::Unique { name, columns } => (
                name,
                ConstraintKind::Unique {
                    columns: cols(columns),
                },
                "key",
                Some(columns_addition(columns.iter().map(|c| c.value.as_str()))),
            ),
            TableConstraint::ForeignKey {
                name,
                columns,
                ref_table,
                ref_columns,
                ..
            } => {
                let Some(references) = self.relation_name(ref_table) else {
                    return;
                };
                // Without referenced columns: the target's primary key.
                let ref_columns = if ref_columns.is_empty() {
                    self.cat
                        .relations
                        .get(&references)
                        .and_then(|r| {
                            r.constraints.iter().find_map(|c| match &c.kind {
                                ConstraintKind::PrimaryKey { columns } => Some(columns.clone()),
                                _ => None,
                            })
                        })
                        .unwrap_or_default()
                } else {
                    cols(ref_columns)
                };
                if ref_columns.is_empty() {
                    self.error(
                        codes::OBJECT_NOT_FOUND,
                        format!(
                            "there is no primary key for referenced table \"{}\"",
                            references.name
                        ),
                    );
                    return;
                }
                (
                    name,
                    ConstraintKind::ForeignKey {
                        columns: cols(columns),
                        references,
                        ref_columns,
                    },
                    "fkey",
                    Some(columns_addition(columns.iter().map(|c| c.value.as_str()))),
                )
            }
            TableConstraint::Check { name, expr } => {
                // Named after the column of the column constraint, otherwise
                // the first referenced column.
                let first = column.map(str::to_string).or_else(|| first_column(expr));
                (
                    name,
                    ConstraintKind::Check {
                        expr: emit_expr(expr),
                        columns: columns_of(expr),
                    },
                    "check",
                    first,
                )
            }
            TableConstraint::Exclude {
                name,
                elements,
                where_clause,
                ..
            } => {
                let names: Vec<String> = elements
                    .iter()
                    .map(|e| match &e.element {
                        IndexElement::Column(c) => c.value.clone(),
                        IndexElement::Expr(x) => index_column_name(x),
                    })
                    .collect();
                (
                    name,
                    ConstraintKind::Exclude {
                        definition: pgzest_ast::emit::emit_table_constraint(tc),
                        columns: {
                            let mut cols = Vec::new();
                            for e in elements {
                                match &e.element {
                                    IndexElement::Column(c) => push_unique(&mut cols, &c.value),
                                    IndexElement::Expr(x) => {
                                        for c in columns_of(x) {
                                            push_unique(&mut cols, &c);
                                        }
                                    }
                                }
                            }
                            for c in where_clause.iter().flat_map(columns_of) {
                                push_unique(&mut cols, &c);
                            }
                            cols
                        },
                    },
                    "excl",
                    Some(columns_addition(names.iter().map(String::as_str))),
                )
            }
        };
        let name = match explicit {
            Some(n) => n.value.clone(),
            None => self.free_constraint_name(&rel, addition.as_deref(), label),
        };
        if rel.constraints.iter().any(|c| c.name == name) {
            self.error(
                codes::OBJECT_ALREADY_EXISTS,
                format!("constraint \"{name}\" for relation {table} already exists"),
            );
            return;
        }
        let rel = self.cat.relations.get_mut(table).expect("relation present");
        if let ConstraintKind::PrimaryKey { columns } = &kind {
            for c in &mut rel.columns {
                if columns.contains(&c.name) {
                    c.not_null = true;
                }
            }
        }
        // As PostgreSQL records it: a foreign key names the columns it
        // references, the primary key when it was written without them.
        let mut stored = unnamed(tc);
        if let (
            TableConstraint::ForeignKey { ref_columns, .. },
            ConstraintKind::ForeignKey {
                ref_columns: resolved,
                ..
            },
        ) = (&mut stored, &kind)
            && ref_columns.is_empty()
        {
            *ref_columns = resolved.iter().map(Ident::new).collect();
        }
        rel.constraints.push(Constraint {
            name,
            kind,
            definition: Some(pgzest_ast::emit::emit_table_constraint(&stored)),
            not_valid,
        });
    }

    fn alter_table(&mut self, table: &QName, op: &AlterTableOp) {
        match op {
            AlterTableOp::EnableRowLevelSecurity => {
                if let Some(r) = self.cat.relations.get_mut(table) {
                    r.row_level_security = true;
                }
            }
            // Independent of ENABLE/DISABLE, as in PostgreSQL.
            AlterTableOp::ForceRowLevelSecurity | AlterTableOp::NoForceRowLevelSecurity => {
                if let Some(r) = self.cat.relations.get_mut(table) {
                    r.force_row_level_security = matches!(op, AlterTableOp::ForceRowLevelSecurity);
                }
            }
            AlterTableOp::DisableRowLevelSecurity => {
                if let Some(r) = self.cat.relations.get_mut(table) {
                    r.row_level_security = false;
                }
            }
            AlterTableOp::AddColumn {
                column,
                if_not_exists,
            } => {
                let Some(mut rel) = self.cat.relations.get(table).cloned() else {
                    return;
                };
                if rel.column(&column.name.value).is_some() {
                    if !if_not_exists {
                        self.error(
                            codes::OBJECT_ALREADY_EXISTS,
                            format!(
                                "column \"{}\" of relation {table} already exists",
                                column.name.value
                            ),
                        );
                    }
                    return;
                }
                let mut pending = Vec::new();
                self.add_column_def(&mut rel, column, &mut pending);
                self.cat.relations.insert(table.clone(), rel);
                for (tc, col) in pending {
                    self.add_constraint(table, &tc, col.as_deref(), false);
                }
            }
            AlterTableOp::DropColumn {
                name,
                if_exists,
                cascade,
            } => {
                let col = name.value.clone();
                let Some(rel) = self.cat.relations.get(table) else {
                    return;
                };
                if rel.column(&col).is_none() {
                    if !if_exists {
                        self.error(
                            codes::OBJECT_NOT_FOUND,
                            format!("column \"{col}\" of relation {table} does not exist"),
                        );
                    }
                    return;
                }
                if !cascade && self.referenced_by_foreign_key(table, Some(&col)) {
                    self.error(
                        codes::DEPENDENT_OBJECTS,
                        format!(
                            "cannot drop column \"{col}\" of {table}: it is referenced by a foreign key"
                        ),
                    );
                }
                self.drop_column(table, &col);
            }
            AlterTableOp::AlterColumn { name, action } => {
                let col = name.value.clone();
                let ty = match action {
                    AlterColumnAction::SetDataType { data_type, .. } => {
                        Some(self.type_ref(data_type))
                    }
                    _ => None,
                };
                let default = match action {
                    AlterColumnAction::SetDefault(e) => {
                        let current = self
                            .cat
                            .relations
                            .get(table)
                            .and_then(|r| r.columns.iter().find(|c| c.name == col))
                            .map(|c| c.ty.clone());
                        current.and_then(|t| self.stored_default(e, &t))
                    }
                    _ => None,
                };
                let Some(rel) = self.cat.relations.get_mut(table) else {
                    return;
                };
                let Some(c) = rel.columns.iter_mut().find(|c| c.name == col) else {
                    self.error(
                        codes::OBJECT_NOT_FOUND,
                        format!("column \"{col}\" of relation {table} does not exist"),
                    );
                    return;
                };
                match action {
                    AlterColumnAction::SetDefault(_) => c.default = default,
                    AlterColumnAction::DropDefault => c.default = None,
                    AlterColumnAction::SetNotNull => c.not_null = true,
                    AlterColumnAction::DropNotNull => c.not_null = false,
                    AlterColumnAction::SetDataType { .. } => {
                        // A literal default took the column's type at its
                        // definition: PostgreSQL keeps that constant and
                        // converts it to the new type on each insert.
                        if let Some(d) = &c.default
                            && is_string_literal(d)
                        {
                            c.default = Some(typed_literal(d, &c.ty));
                        }
                        c.ty = ty.expect("computed above");
                    }
                }
            }
            AlterTableOp::AddConstraint {
                constraint,
                not_valid,
            } => self.add_constraint(table, constraint, None, *not_valid),
            AlterTableOp::DropConstraint {
                name, if_exists, ..
            } => {
                let Some(rel) = self.cat.relations.get_mut(table) else {
                    return;
                };
                let before = rel.constraints.len();
                rel.constraints.retain(|c| c.name != name.value);
                if rel.constraints.len() == before && !if_exists {
                    self.error(
                        codes::OBJECT_NOT_FOUND,
                        format!(
                            "constraint \"{}\" of relation {table} does not exist",
                            name.value
                        ),
                    );
                }
            }
            AlterTableOp::ValidateConstraint(name) => {
                let found = self
                    .cat
                    .relations
                    .get_mut(table)
                    .and_then(|r| r.constraints.iter_mut().find(|c| c.name == name.value))
                    .map(|c| c.not_valid = false)
                    .is_some();
                if !found {
                    self.error(
                        codes::OBJECT_NOT_FOUND,
                        format!(
                            "constraint \"{}\" of relation {table} does not exist",
                            name.value
                        ),
                    );
                }
            }
            AlterTableOp::RenameColumn { from, to } => {
                self.rename_column(table, &from.value, &to.value)
            }
            AlterTableOp::RenameTo(new) => {
                let target = QName::new(table.schema.clone(), new.value.clone());
                self.move_relation(table, target);
            }
            AlterTableOp::SetSchema(schema) => {
                if !self.cat.schema_exists(&schema.value) {
                    self.error(
                        codes::SCHEMA_NOT_FOUND,
                        format!("schema \"{}\" does not exist", schema.value),
                    );
                    return;
                }
                let target = QName::new(schema.value.clone(), table.name.clone());
                self.move_relation(table, target);
            }
        }
    }

    fn referenced_by_foreign_key(&self, table: &QName, column: Option<&str>) -> bool {
        self.cat.relations.values().any(|r| {
            r.name != *table
                && r.constraints.iter().any(|c| match &c.kind {
                    ConstraintKind::ForeignKey {
                        references,
                        ref_columns,
                        ..
                    } => {
                        references == table
                            && column.is_none_or(|col| ref_columns.iter().any(|x| x == col))
                    }
                    _ => false,
                })
        })
    }

    /// Drops a column and what depends on it: constraints referencing it,
    /// indexes on it, owned sequence.
    fn drop_column(&mut self, table: &QName, col: &str) {
        if let Some(rel) = self.cat.relations.get_mut(table) {
            rel.columns.retain(|c| c.name != col);
            rel.constraints.retain(|c| !constraint_uses(c, col));
        }
        self.cat
            .indexes
            .retain(|_, i| !(i.table == *table && i.columns.iter().any(|c| c == col)));
        for r in self.cat.relations.values_mut() {
            r.constraints.retain(|c| match &c.kind {
                ConstraintKind::ForeignKey {
                    references,
                    ref_columns,
                    ..
                } => !(references == table && ref_columns.iter().any(|x| x == col)),
                _ => true,
            });
        }
        self.cat
            .sequences
            .retain(|_, s| s.owned_by.as_ref() != Some(&(table.clone(), col.to_string())));
    }

    fn rename_column(&mut self, table: &QName, from: &str, to: &str) {
        let Some(rel) = self.cat.relations.get_mut(table) else {
            return;
        };
        let Some(c) = rel.columns.iter_mut().find(|c| c.name == from) else {
            self.error(
                codes::OBJECT_NOT_FOUND,
                format!("column \"{from}\" of relation {table} does not exist"),
            );
            return;
        };
        c.name = to.to_string();
        let rename = |v: &mut Vec<String>| {
            for x in v.iter_mut() {
                if x == from {
                    *x = to.to_string();
                }
            }
        };
        for con in &mut rel.constraints {
            match &mut con.kind {
                ConstraintKind::PrimaryKey { columns }
                | ConstraintKind::Unique { columns }
                | ConstraintKind::ForeignKey { columns, .. }
                | ConstraintKind::Check { columns, .. }
                | ConstraintKind::Exclude { columns, .. } => rename(columns),
            }
        }
        for i in self.cat.indexes.values_mut() {
            if i.table == *table {
                rename(&mut i.columns);
            }
        }
        for r in self.cat.relations.values_mut() {
            for con in &mut r.constraints {
                if let ConstraintKind::ForeignKey {
                    references,
                    ref_columns,
                    ..
                } = &mut con.kind
                    && references == table
                {
                    rename(ref_columns);
                }
            }
        }
        for s in self.cat.sequences.values_mut() {
            if let Some((t, c)) = &mut s.owned_by
                && t == table
                && c == from
            {
                *c = to.to_string();
            }
        }
    }

    /// Renames or moves a relation, and everything that designates it.
    fn move_relation(&mut self, from: &QName, to: QName) {
        if self.cat.relations.contains_key(&to) {
            self.error(
                codes::OBJECT_ALREADY_EXISTS,
                format!("relation \"{to}\" already exists"),
            );
            return;
        }
        let array = self.rowtype_array(from);
        let Some(mut rel) = self.cat.relations.remove(from) else {
            return;
        };
        rel.name = to.clone();
        if let Some(arr) = array {
            self.cat.types.remove(&arr);
        }
        self.cat.relations.insert(to.clone(), rel);
        self.add_rowtype_array(&to);
        for r in self.cat.relations.values_mut() {
            for c in &mut r.constraints {
                if let ConstraintKind::ForeignKey { references, .. } = &mut c.kind
                    && references == from
                {
                    *references = to.clone();
                }
            }
        }
        for i in self.cat.indexes.values_mut() {
            if i.table == *from {
                i.table = to.clone();
            }
        }
        for s in self.cat.sequences.values_mut() {
            if let Some((t, _)) = &mut s.owned_by
                && t == from
            {
                *t = to.clone();
            }
        }
        if let Some(mut t) = self.cat.triggers.remove(from) {
            for tr in t.values_mut() {
                tr.table = to.clone();
            }
            self.cat.triggers.insert(to.clone(), t);
        }
        if let Some(mut p) = self.cat.policies.remove(from) {
            for po in p.values_mut() {
                po.table = to.clone();
            }
            self.cat.policies.insert(to, p);
        }
    }

    /// Array type of a relation's row type (`_table`), which PostgreSQL
    /// creates with every table or view.
    fn add_rowtype_array(&mut self, rel: &QName) {
        let name = array_type_name(&rel.name, |n| {
            self.cat
                .types
                .contains_key(&QName::new(rel.schema.clone(), n))
        });
        let q = QName::new(rel.schema.clone(), name);
        self.cat.types.insert(
            q.clone(),
            TypeDef {
                name: q,
                oid: None,
                kind: TypeKind::Array {
                    element: rel.clone(),
                },
                category: 'A',
                preferred: false,
                array: None,
            },
        );
    }

    fn rowtype_array(&self, rel: &QName) -> Option<QName> {
        self.cat
            .types
            .values()
            .find(|t| matches!(&t.kind, TypeKind::Array { element } if element == rel))
            .map(|t| t.name.clone())
    }

    // ── Indexes ──────────────────────────────────────────────────────────────

    fn create_index(&mut self, ci: &pgzest_ast::ddl::CreateIndex) {
        let Some(table) = self.relation_name(&ci.table) else {
            return;
        };
        let name = match &ci.name {
            Some(n) => QName::new(table.schema.clone(), n.value.clone()),
            None => {
                let cols: Vec<String> = ci
                    .elements
                    .iter()
                    .map(|e| match &e.element {
                        IndexElement::Column(c) => c.value.clone(),
                        IndexElement::Expr(x) => index_column_name(x),
                    })
                    // The name PostgreSQL chooses also includes the INCLUDE
                    // columns.
                    .chain(ci.include.iter().map(|c| c.value.clone()))
                    .collect();
                let add = columns_addition(cols.iter().map(String::as_str));
                let n = choose_name(&table.name, Some(&add), "idx", |n| {
                    let q = QName::new(table.schema.clone(), n);
                    self.cat.relations.contains_key(&q) || self.cat.indexes.contains_key(&q)
                });
                QName::new(table.schema.clone(), n)
            }
        };
        if self.cat.indexes.contains_key(&name) || self.cat.relations.contains_key(&name) {
            if !ci.if_not_exists {
                self.error(
                    codes::OBJECT_ALREADY_EXISTS,
                    format!("relation \"{name}\" already exists"),
                );
            }
            return;
        }
        let mut columns = Vec::new();
        for e in &ci.elements {
            match &e.element {
                IndexElement::Column(c) => push_unique(&mut columns, &c.value),
                IndexElement::Expr(x) => {
                    for c in columns_of(x) {
                        push_unique(&mut columns, &c);
                    }
                }
            }
        }
        for c in &ci.include {
            push_unique(&mut columns, &c.value);
        }
        for c in ci.where_clause.iter().flat_map(columns_of) {
            push_unique(&mut columns, &c);
        }
        self.cat.indexes.insert(
            name.clone(),
            Index {
                name: name.clone(),
                table,
                unique: ci.unique,
                // With the effective name: an unnamed index is named by
                // PostgreSQL, and export must recreate it identically.
                definition: {
                    let mut named = ci.clone();
                    named.name = Some(Ident {
                        value: name.name.clone(),
                        quoted: false,
                    });
                    emit_statement(&Statement::Ddl(DdlStatement::CreateIndex(named)))
                },
                columns,
            },
        );
    }

    // ── Types ────────────────────────────────────────────────────────────────

    /// Adds a type and its array type.
    fn add_type(&mut self, name: QName, kind: TypeKind, category: char) {
        if self.cat.types.contains_key(&name) {
            self.error(
                codes::OBJECT_ALREADY_EXISTS,
                format!("type \"{name}\" already exists"),
            );
            return;
        }
        let array_name = array_type_name(&name.name, |n| {
            self.cat
                .types
                .contains_key(&QName::new(name.schema.clone(), n))
        });
        let array = QName::new(name.schema.clone(), array_name);
        self.cat.types.insert(
            array.clone(),
            TypeDef {
                name: array.clone(),
                oid: None,
                kind: TypeKind::Array {
                    element: name.clone(),
                },
                category: 'A',
                preferred: false,
                array: None,
            },
        );
        self.cat.types.insert(
            name.clone(),
            TypeDef {
                name,
                oid: None,
                kind,
                category,
                preferred: false,
                array: Some(array),
            },
        );
    }

    fn create_type(&mut self, ct: &pgzest_ast::ddl::CreateType) {
        let Some(name) = self.creation_name(&ct.name, false) else {
            return;
        };
        match &ct.kind {
            TypeDefinition::Enum(labels) => self.add_type(
                name,
                TypeKind::Enum {
                    labels: labels.clone(),
                },
                'E',
            ),
            TypeDefinition::Composite(fields) => {
                let attributes = fields
                    .iter()
                    .map(|(n, t)| Attribute {
                        name: n.value.clone(),
                        ty: self.type_ref(t),
                    })
                    .collect();
                self.add_type(name, TypeKind::Composite { attributes }, 'C')
            }
        }
    }

    fn alter_type(&mut self, at: &pgzest_ast::ddl::AlterType) {
        let (schema, n) = split_name(&at.name);
        let Some(name) = self
            .cat
            .resolve_type(schema, n, self.path)
            .map(|t| t.name.clone())
        else {
            self.error(
                codes::TYPE_NOT_FOUND,
                format!("type \"{}\" does not exist", written(&at.name)),
            );
            return;
        };
        match &at.operation {
            AlterTypeOp::AddValue {
                value,
                if_not_exists,
                position,
            } => {
                let Some(TypeDef {
                    kind: TypeKind::Enum { labels },
                    ..
                }) = self.cat.types.get_mut(&name)
                else {
                    self.error(
                        codes::OBJECT_NOT_FOUND,
                        format!("\"{name}\" is not an enum"),
                    );
                    return;
                };
                if labels.contains(value) {
                    if !if_not_exists {
                        self.error(
                            codes::OBJECT_ALREADY_EXISTS,
                            format!("enum label \"{value}\" already exists in {name}"),
                        );
                    }
                    return;
                }
                let at = match position {
                    None => Some(labels.len()),
                    Some(EnumValuePosition::Before(v)) => labels.iter().position(|l| l == v),
                    Some(EnumValuePosition::After(v)) => {
                        labels.iter().position(|l| l == v).map(|i| i + 1)
                    }
                };
                match at {
                    Some(i) => labels.insert(i, value.clone()),
                    None => self.error(
                        codes::OBJECT_NOT_FOUND,
                        format!("neighbor enum label not found in {name}"),
                    ),
                }
            }
            AlterTypeOp::RenameValue { from, to } => {
                if let Some(TypeDef {
                    kind: TypeKind::Enum { labels },
                    ..
                }) = self.cat.types.get_mut(&name)
                    && let Some(l) = labels.iter_mut().find(|l| *l == from)
                {
                    *l = to.clone();
                } else {
                    self.error(
                        codes::OBJECT_NOT_FOUND,
                        format!("enum label \"{from}\" does not exist in {name}"),
                    );
                }
            }
            AlterTypeOp::RenameTo(new) => {
                let target = QName::new(name.schema.clone(), new.value.clone());
                self.move_type(&name, target);
            }
            AlterTypeOp::SetSchema(s) => {
                let target = QName::new(s.value.clone(), name.name.clone());
                self.move_type(&name, target);
            }
        }
    }

    /// Renames or moves a type; the columns and attributes using it follow
    /// (the type is designated by its OID in PostgreSQL).
    fn move_type(&mut self, from: &QName, to: QName) {
        if self.cat.types.contains_key(&to) {
            self.error(
                codes::OBJECT_ALREADY_EXISTS,
                format!("type \"{to}\" already exists"),
            );
            return;
        }
        let Some(mut t) = self.cat.types.remove(from) else {
            return;
        };
        t.name = to.clone();
        // The array type follows the schema move, and is renamed.
        if let Some(arr) = t.array.take() {
            let mut a = self.cat.types.remove(&arr);
            let arr_name = array_type_name(&to.name, |n| {
                self.cat
                    .types
                    .contains_key(&QName::new(to.schema.clone(), n))
            });
            let new_arr = QName::new(to.schema.clone(), arr_name);
            if let Some(a) = &mut a {
                a.name = new_arr.clone();
                a.kind = TypeKind::Array {
                    element: to.clone(),
                };
                self.cat.types.insert(new_arr.clone(), a.clone());
            }
            t.array = Some(new_arr);
        }
        self.cat.types.insert(to.clone(), t);
        let rename = |r: &mut TypeRef| retarget(r, from, &to);
        for rel in self.cat.relations.values_mut() {
            for c in &mut rel.columns {
                rename(&mut c.ty);
            }
        }
        for ty in self.cat.types.values_mut() {
            match &mut ty.kind {
                TypeKind::Composite { attributes } => {
                    for a in attributes {
                        rename(&mut a.ty);
                    }
                }
                TypeKind::Domain { base, .. } => rename(base),
                _ => {}
            }
        }
        for overloads in self.cat.routines.values_mut() {
            for r in overloads {
                for a in &mut r.args {
                    rename(&mut a.ty);
                }
                if let Some(ret) = &mut r.returns {
                    rename(ret);
                }
            }
        }
    }

    // ── Routines ─────────────────────────────────────────────────────────────

    fn args(&mut self, params: &[FunctionParam]) -> Vec<Arg> {
        params
            .iter()
            .map(|p| Arg {
                name: p.name.as_ref().map(|n| n.value.clone()),
                ty: self.type_ref(&p.data_type),
                mode: match p.mode {
                    ParamMode::In => ArgMode::In,
                    ParamMode::Out => ArgMode::Out,
                    ParamMode::InOut => ArgMode::InOut,
                    ParamMode::Variadic => ArgMode::Variadic,
                },
                has_default: p.default.is_some(),
            })
            .collect()
    }

    fn add_routine(&mut self, routine: Routine, or_replace: bool) {
        let sig: Vec<String> = routine.input_types().map(|t| t.to_string()).collect();
        let overloads = self.cat.routines.entry(routine.name.clone()).or_default();
        if let Some(i) = overloads
            .iter()
            .position(|o| o.input_types().map(|t| t.to_string()).collect::<Vec<_>>() == sig)
        {
            if !or_replace {
                self.error(
                    codes::OBJECT_ALREADY_EXISTS,
                    format!(
                        "function {}({}) already exists with same argument types",
                        routine.name,
                        sig.join(", ")
                    ),
                );
                return;
            }
            overloads[i] = routine;
        } else {
            overloads.push(routine);
        }
    }

    fn create_function(&mut self, cf: &CreateFunction) {
        let Some(name) = self.creation_name(&cf.name, false) else {
            return;
        };
        let mut args = self.args(&cf.params);
        let (returns, returns_set) = match &cf.return_type {
            // Without RETURNS: the only OUT parameter, or `record`.
            ReturnType::OutParams => {
                let outs: Vec<&Arg> = args
                    .iter()
                    .filter(|a| matches!(a.mode, ArgMode::Out | ArgMode::InOut))
                    .collect();
                match outs.as_slice() {
                    [one] => (one.ty.clone(), false),
                    _ => (TypeRef::named(QName::pg_catalog("record")), false),
                }
            }
            ReturnType::Scalar(dt) => (self.type_ref(dt), false),
            ReturnType::SetOf(dt) => (self.type_ref(dt), true),
            ReturnType::Table(cols) => {
                for (n, dt) in cols {
                    args.push(Arg {
                        name: Some(n.value.clone()),
                        ty: self.type_ref(dt),
                        mode: ArgMode::Table,
                        has_default: false,
                    });
                }
                (TypeRef::named(QName::pg_catalog("record")), true)
            }
            ReturnType::Trigger => (TypeRef::named(QName::pg_catalog("trigger")), false),
            ReturnType::Void => (TypeRef::named(QName::pg_catalog("void")), false),
        };
        let order = self.next_order();
        self.add_routine(
            Routine {
                name,
                oid: None,
                kind: RoutineKind::Function,
                args,
                returns: Some(returns),
                returns_set,
                volatility: match cf.volatility {
                    pgzest_ast::ddl::Volatility::Immutable => Volatility::Immutable,
                    pgzest_ast::ddl::Volatility::Stable => Volatility::Stable,
                    pgzest_ast::ddl::Volatility::Volatile => Volatility::Volatile,
                },
                strict: cf.strict,
                security_definer: cf.security_definer,
                language: language(&cf.language),
                definition: Some(emit_statement(&Statement::Ddl(
                    DdlStatement::CreateFunction(cf.clone()),
                ))),
                catalog_effects: body_has_effects(&cf.body),
                order,
                source: None,
            },
            cf.or_replace,
        );
    }

    fn create_procedure(&mut self, cp: &CreateProcedure) {
        let Some(name) = self.creation_name(&cp.name, false) else {
            return;
        };
        let args = self.args(&cp.params);
        let order = self.next_order();
        self.add_routine(
            Routine {
                name,
                oid: None,
                kind: RoutineKind::Procedure,
                args,
                returns: None,
                returns_set: false,
                volatility: Volatility::Volatile,
                strict: false,
                security_definer: cp.security_definer,
                language: language(&cp.language),
                definition: Some(emit_statement(&Statement::Ddl(
                    DdlStatement::CreateProcedure(cp.clone()),
                ))),
                catalog_effects: body_has_effects(&cp.body),
                order,
                source: None,
            },
            cp.or_replace,
        );
    }

    // ── Views ────────────────────────────────────────────────────────────────

    fn create_view(&mut self, cv: &pgzest_ast::ddl::CreateView) {
        let Some(name) = self.creation_name(&cv.name, cv.temporary) else {
            return;
        };
        if self.cat.relations.contains_key(&name) && !cv.or_replace {
            self.error(
                codes::OBJECT_ALREADY_EXISTS,
                format!("relation \"{name}\" already exists"),
            );
            return;
        }
        let aliases: Vec<&Ident> = cv.columns.iter().collect();
        let (columns, opaque) = self.query_columns("view", &name, &cv.query, &aliases);
        let existed = self.cat.relations.contains_key(&name);
        if !existed {
            self.add_rowtype_array(&name);
        }
        let order = self.next_order();
        self.cat.relations.insert(
            name.clone(),
            Relation {
                name,
                kind: if cv.materialized {
                    RelationKind::MaterializedView
                } else {
                    RelationKind::View
                },
                persistence: if cv.temporary {
                    Persistence::Temporary
                } else {
                    Persistence::Permanent
                },
                columns,
                constraints: Vec::new(),
                row_level_security: false,
                force_row_level_security: false,
                opaque,
                definition: Some(emit_statement(&Statement::Ddl(DdlStatement::CreateView(
                    cv.clone(),
                )))),
                order,
            },
        );
    }

    /// Columns produced by the query of a view or of a `CREATE TABLE … AS`
    /// (renamed by `aliases`), and true if their names are not established.
    /// Types are those of the query analysis, if a typer is provided and
    /// establishes them; unknown otherwise, never assumed.
    fn query_columns(
        &mut self,
        what: &str,
        name: &QName,
        query: &SelectStmt,
        aliases: &[&Ident],
    ) -> (Vec<Column>, bool) {
        let (mut names, opaque) = match self.output_names(query) {
            Some(n) => (n, false),
            None => (Vec::new(), true),
        };
        for (n, alias) in names.iter_mut().zip(aliases) {
            *n = alias.value.clone();
        }
        let types: Vec<TypeRef> = match self.typer {
            Some(t) => match t.output_columns(self.cat, self.path, query) {
                Ok(cols) if cols.len() == names.len() => {
                    cols.into_iter().map(|(_, ty)| ty).collect()
                }
                Ok(cols) => {
                    self.out.push(Diagnostic::info(
                        codes::VIEW_NOT_TYPED,
                        format!(
                            "{what} \"{name}\": {} columns analyzed for {} named; types not established",
                            cols.len(),
                            names.len()
                        ),
                    ));
                    Vec::new()
                }
                Err(why) => {
                    self.out.push(Diagnostic::info(
                        codes::VIEW_NOT_TYPED,
                        format!("{what} \"{name}\": column types not established ({why})"),
                    ));
                    Vec::new()
                }
            },
            None => Vec::new(),
        };
        let columns = names
            .into_iter()
            .enumerate()
            .map(|(i, n)| Column {
                name: n,
                ty: types.get(i).cloned().unwrap_or(TypeRef::Unknown),
                not_null: false,
                default: None,
                identity: None,
                generated: None,
            })
            .collect();
        (columns, opaque)
    }

    /// Names of the columns produced by a query, as PostgreSQL names them.
    /// `None` if a `*` covers a source that cannot be expanded here.
    fn output_names(&self, sel: &SelectStmt) -> Option<Vec<String>> {
        let ctes: Vec<&str> = sel.with.iter().map(|c| c.name.value.as_str()).collect();
        let source_columns = |alias: Option<&str>| -> Option<Vec<String>> {
            let mut out = Vec::new();
            for t in sel.from.iter().chain(sel.joins.iter().map(|j| &j.table)) {
                if alias.is_some_and(|a| t.visible_name() != Some(a)) {
                    continue;
                }
                let TableSource::Table(n) = &t.source else {
                    return None;
                };
                let (schema, name) = split_name(n);
                if schema.is_none() && ctes.contains(&name) {
                    return None;
                }
                let rel = self.cat.resolve_relation(schema, name, self.path)?;
                if rel.opaque {
                    return None;
                }
                let mut cols: Vec<String> = rel.columns.iter().map(|c| c.name.clone()).collect();
                for (c, a) in cols.iter_mut().zip(&t.column_aliases) {
                    *c = a.value.clone();
                }
                out.extend(cols);
            }
            Some(out)
        };
        let mut out = Vec::new();
        for item in &sel.columns {
            match item {
                SelectItem::Wildcard => out.extend(source_columns(None)?),
                SelectItem::QualifiedWildcard(q) => out.extend(source_columns(Some(&q.value))?),
                SelectItem::Expr { alias: Some(a), .. } => out.push(a.value.clone()),
                SelectItem::Expr { expr, alias: None } => out.push(output_name(expr)),
            }
        }
        Some(out)
    }

    // ── DROP ─────────────────────────────────────────────────────────────────

    fn drop(&mut self, d: &DropObject) {
        for (i, n) in d.names.iter().enumerate() {
            let (schema, name) = split_name(n);
            let found = match d.kind {
                ObjectKind::Table | ObjectKind::View | ObjectKind::MaterializedView => {
                    match self
                        .cat
                        .resolve_relation(schema, name, self.path)
                        .map(|r| r.name.clone())
                    {
                        Some(q) => {
                            if !d.cascade && self.referenced_by_foreign_key(&q, None) {
                                self.error(
                                    codes::DEPENDENT_OBJECTS,
                                    format!(
                                        "\"{q}\" is referenced by a foreign key (CASCADE required)"
                                    ),
                                );
                            }
                            self.drop_relation(&q);
                            true
                        }
                        None => false,
                    }
                }
                ObjectKind::Index => {
                    let q = match schema {
                        Some(s) => Some(QName::new(s, name)),
                        None => self
                            .path
                            .lookup_order()
                            .into_iter()
                            .map(|s| QName::new(s, name))
                            .find(|q| self.cat.indexes.contains_key(q)),
                    };
                    q.and_then(|q| self.cat.indexes.remove(&q)).is_some()
                }
                ObjectKind::Sequence => {
                    let q = match schema {
                        Some(s) => Some(QName::new(s, name)),
                        None => self
                            .path
                            .lookup_order()
                            .into_iter()
                            .map(|s| QName::new(s, name))
                            .find(|q| self.cat.sequences.contains_key(q)),
                    };
                    q.and_then(|q| self.cat.sequences.remove(&q)).is_some()
                }
                ObjectKind::Type | ObjectKind::Domain => {
                    match self.cat.resolve_type(schema, name, self.path).cloned() {
                        Some(t) => {
                            self.cat.types.remove(&t.name);
                            if let Some(a) = &t.array {
                                self.cat.types.remove(a);
                            }
                            true
                        }
                        None => false,
                    }
                }
                ObjectKind::Function | ObjectKind::Procedure => {
                    let sig = d.signatures.get(i).cloned().flatten();
                    self.drop_routine(schema, name, sig.as_deref())
                }
                ObjectKind::Trigger | ObjectKind::Policy => {
                    let Some(table) = d.on_table.as_ref().and_then(|t| {
                        let (s, n) = split_name(t);
                        self.cat
                            .resolve_relation(s, n, self.path)
                            .map(|r| r.name.clone())
                    }) else {
                        if !d.if_exists {
                            self.error(
                                codes::OBJECT_NOT_FOUND,
                                format!("table of \"{name}\" not found"),
                            );
                        }
                        continue;
                    };
                    let map = if d.kind == ObjectKind::Trigger {
                        self.cat
                            .triggers
                            .get_mut(&table)
                            .map(|m| m.remove(name).is_some())
                    } else {
                        self.cat
                            .policies
                            .get_mut(&table)
                            .map(|m| m.remove(name).is_some())
                    };
                    map.unwrap_or(false)
                }
                ObjectKind::Schema => {
                    if self.cat.schemas.remove(name).is_some() {
                        self.cat.drop_schema_objects(name);
                        true
                    } else {
                        false
                    }
                }
                ObjectKind::Extension => {
                    if self.cat.extensions.contains_key(name) {
                        self.cat.unload_extension(name);
                        true
                    } else {
                        false
                    }
                }
            };
            if !found && !d.if_exists {
                self.error(
                    codes::OBJECT_NOT_FOUND,
                    format!("\"{}\" does not exist", written(n)),
                );
            }
        }
    }

    fn drop_relation(&mut self, q: &QName) {
        if let Some(arr) = self.rowtype_array(q) {
            self.cat.types.remove(&arr);
        }
        self.cat.relations.remove(q);
        self.cat.indexes.retain(|_, i| i.table != *q);
        self.cat.triggers.remove(q);
        self.cat.policies.remove(q);
        self.cat
            .sequences
            .retain(|_, s| s.owned_by.as_ref().is_none_or(|(t, _)| t != q));
        for r in self.cat.relations.values_mut() {
            r.constraints.retain(|c| {
                !matches!(&c.kind, ConstraintKind::ForeignKey { references, .. } if references == q)
            });
        }
    }

    fn drop_routine(&mut self, schema: Option<&str>, name: &str, sig: Option<&[DataType]>) -> bool {
        let wanted: Option<Vec<String>> =
            sig.map(|types| types.iter().map(|t| self.type_ref(t).to_string()).collect());
        let candidates: Vec<(QName, Vec<String>)> = self
            .cat
            .resolve_routines(schema, name, self.path)
            .into_iter()
            .map(|r| {
                (
                    r.name.clone(),
                    r.input_types().map(|t| t.to_string()).collect(),
                )
            })
            .filter(|(_, s)| wanted.as_ref().is_none_or(|w| w == s))
            .collect();
        match candidates.as_slice() {
            [] => false,
            [(q, s)] => {
                if let Some(list) = self.cat.routines.get_mut(q) {
                    list.retain(|r| {
                        r.input_types().map(|t| t.to_string()).collect::<Vec<_>>() != *s
                    });
                    if list.is_empty() {
                        self.cat.routines.remove(q);
                    }
                }
                true
            }
            _ => {
                self.error(
                    codes::AMBIGUOUS_ROUTINE,
                    format!("\"{name}\" designates several overloads: specify the argument types"),
                );
                true
            }
        }
    }
}

fn language(l: &FunctionLanguage) -> String {
    match l {
        FunctionLanguage::Sql => "sql".into(),
        FunctionLanguage::PlPgSql => "plpgsql".into(),
        FunctionLanguage::C => "c".into(),
        FunctionLanguage::Internal => "internal".into(),
        FunctionLanguage::Other(s) => s.to_lowercase(),
    }
}

/// Column name PostgreSQL gives to an unaliased expression.
fn output_name(expr: &pgzest_ast::Expr) -> String {
    match &expr.kind {
        ExprKind::Column(c) => c.column.value.clone(),
        ExprKind::FunctionCall { name, .. } => name
            .0
            .last()
            .map(|i| i.value.clone())
            .unwrap_or_else(|| "?column?".into()),
        ExprKind::Cast { expr, data_type } => match &expr.kind {
            ExprKind::Column(c) => c.column.value.clone(),
            _ => cast_output_name(data_type),
        },
        ExprKind::FieldAccess { path, .. } => {
            path.last().cloned().unwrap_or_else(|| "?column?".into())
        }
        ExprKind::Case { .. } => "case".into(),
        ExprKind::Exists { .. } => "exists".into(),
        _ => "?column?".into(),
    }
}

/// `'x'::int` is named after the type (`int4`).
pub(crate) fn cast_output_name(dt: &DataType) -> String {
    match builtin_type(dt) {
        Some((n, _)) => n.to_string(),
        None => match dt {
            DataType::Custom(n) | DataType::Modified { name: n, .. } => {
                n.rsplit('.').next().unwrap_or(n).to_string()
            }
            _ => "?column?".into(),
        },
    }
}

/// First column referenced by an expression (name of a CHECK constraint).
fn first_column(expr: &pgzest_ast::Expr) -> Option<String> {
    if let ExprKind::Column(c) = &expr.kind {
        return Some(c.column.value.clone());
    }
    expr.children().into_iter().find_map(first_column)
}

/// A table constraint referencing the column goes away with it.
fn constraint_uses(c: &Constraint, col: &str) -> bool {
    match &c.kind {
        ConstraintKind::PrimaryKey { columns }
        | ConstraintKind::Unique { columns }
        | ConstraintKind::ForeignKey { columns, .. }
        | ConstraintKind::Check { columns, .. }
        | ConstraintKind::Exclude { columns, .. } => columns.iter().any(|x| x == col),
    }
}

/// Columns referenced by a constraint or index expression, in order of first
/// appearance (excluding subqueries, forbidden there).
fn columns_of(expr: &pgzest_ast::Expr) -> Vec<String> {
    fn walk(e: &pgzest_ast::Expr, out: &mut Vec<String>) {
        if let ExprKind::Column(c) = &e.kind {
            push_unique(out, &c.column.value);
        }
        for child in e.children() {
            walk(child, out);
        }
    }
    let mut out = Vec::new();
    walk(expr, &mut out);
    out
}

fn push_unique(v: &mut Vec<String>, s: &str) {
    if !v.iter().any(|x| x == s) {
        v.push(s.to_string());
    }
}

fn retarget(r: &mut TypeRef, from: &QName, to: &QName) {
    match r {
        TypeRef::Named { name, .. } if name == from => *name = to.clone(),
        TypeRef::Array(inner) => retarget(inner, from, to),
        _ => {}
    }
}

/// Can the statement (or what it contains) modify the catalog? Dynamic SQL
/// (`EXECUTE`) counts: its content is unknown.
fn has_effect(stmts: &[PlStatement]) -> bool {
    stmts.iter().any(|s| match &s.kind {
        PlStatementKind::Sql { statement, .. } => matches!(
            statement,
            Statement::Ddl(_)
                | Statement::Utility(pgzest_ast::Utility {
                    kind: UtilityKind::Set { .. } | UtilityKind::Do { .. },
                    ..
                })
        ),
        PlStatementKind::Execute { .. }
        | PlStatementKind::ForExecute { .. }
        | PlStatementKind::ReturnQueryExecute { .. } => true,
        PlStatementKind::If {
            then_branch,
            elsif_branches,
            else_branch,
            ..
        } => {
            has_effect(then_branch)
                || elsif_branches.iter().any(|(_, b)| has_effect(b))
                || else_branch.as_deref().is_some_and(has_effect)
        }
        PlStatementKind::Case {
            whens, else_branch, ..
        } => {
            whens.iter().any(|(_, b)| has_effect(b))
                || else_branch.as_deref().is_some_and(has_effect)
        }
        PlStatementKind::Loop { body, .. }
        | PlStatementKind::While { body, .. }
        | PlStatementKind::ForRange { body, .. }
        | PlStatementKind::ForQuery { body, .. }
        | PlStatementKind::ForCursor { body, .. }
        | PlStatementKind::Foreach { body, .. } => has_effect(body),
        PlStatementKind::Block(b) => has_effect(&b.statements),
        _ => false,
    })
}

/// The constraint without its name: the body of its definition, the name
/// (written or chosen) being carried separately.
fn unnamed(tc: &TableConstraint) -> TableConstraint {
    let mut tc = tc.clone();
    match &mut tc {
        TableConstraint::PrimaryKey { name, .. }
        | TableConstraint::Unique { name, .. }
        | TableConstraint::ForeignKey { name, .. }
        | TableConstraint::Check { name, .. }
        | TableConstraint::Exclude { name, .. } => *name = None,
    }
    tc
}

/// Default as PostgreSQL keeps it: `DEFAULT NULL` (possibly cast,
/// `NULL::text`) is not recorded.
impl Applier<'_> {
    /// Default stored in `pg_attrdef` for a column of type `ty`.
    ///
    /// PostgreSQL (`AddRelationNewConstraints`) does not write a default whose
    /// expression, once converted to the column type, is a bare NULL
    /// constant. It writes it as soon as the conversion leaves a node:
    /// coercion to a domain, length coercion (`varchar(1)`, `numeric(5,2)`),
    /// or explicit cast to another type.
    fn stored_default(&mut self, e: &pgzest_ast::Expr, ty: &TypeRef) -> Option<String> {
        let mut x = e;
        let mut outer_cast = None;
        while let ExprKind::Cast { expr, data_type } = &x.kind {
            outer_cast.get_or_insert(data_type);
            x = expr;
        }
        if !matches!(x.kind, ExprKind::Literal(pgzest_ast::expr::Literal::Null)) {
            return Some(emit_expr(e));
        }
        let kept = match outer_cast {
            // Literal `NULL`: converted directly to the column type, modifier
            // included.
            None => self.coercion_leaves_node(ty),
            // Explicit cast: it leaves its own node if it applies a modifier
            // (`NULL::varchar(1)` is stored), and the conversion to the column
            // leaves one if the types differ.
            Some(dt) => {
                let written = self.type_ref(dt);
                strip_modifiers(&written) != strip_modifiers(ty)
                    || self.coercion_leaves_node(&written)
                    || self.coercion_leaves_node(ty)
            }
        };
        kept.then(|| emit_expr(e))
    }

    /// Does converting a constant to `ty` leave an expression node (domain,
    /// or length function applied to the modifier)?
    fn coercion_leaves_node(&self, ty: &TypeRef) -> bool {
        match ty {
            TypeRef::Array(inner) => self.coercion_leaves_node(inner),
            TypeRef::Named { name, modifiers } => {
                if self
                    .cat
                    .types
                    .get(name)
                    .is_some_and(|t| matches!(t.kind, TypeKind::Domain { .. }))
                {
                    return true;
                }
                !modifiers.is_empty()
                    && self.cat.casts.iter().any(|c| {
                        c.source == c.target
                            && matches!(&c.source, TypeRef::Named { name: n, .. } if *n == *name)
                    })
            }
            TypeRef::Unresolved(_) | TypeRef::Unknown => false,
        }
    }
}

/// Type without its modifiers (`varchar(1)` → `varchar`).
fn strip_modifiers(ty: &TypeRef) -> TypeRef {
    match ty {
        TypeRef::Named { name, .. } => TypeRef::named(name.clone()),
        TypeRef::Array(inner) => TypeRef::Array(Box::new(strip_modifiers(inner))),
        other => other.clone(),
    }
}

/// Name of an index column carrying an expression (`FigureIndexColname`):
/// name of the function or column, otherwise `expr`.
fn index_column_name(e: &pgzest_ast::Expr) -> String {
    match output_name(e).as_str() {
        "?column?" => "expr".to_string(),
        n => n.to_string(),
    }
}

/// Does a routine body contain DDL or dynamic SQL?
fn body_has_effects(body: &FunctionBody) -> bool {
    match body {
        FunctionBody::Parsed(b) => has_effect(&b.block.statements),
        FunctionBody::Sql(stmts) => stmts.iter().any(|s| matches!(s, Statement::Ddl(_))),
        _ => false,
    }
}

/// Names of the functions called in a serialized AST.
fn collect_calls(v: &serde_json::Value, out: &mut Vec<Vec<String>>) {
    match v {
        serde_json::Value::Object(map) => {
            if let Some(f) = map.get("FunctionCall")
                && let Some(parts) = f.get("name").and_then(|n| n.as_array())
            {
                let names: Option<Vec<String>> = parts
                    .iter()
                    .map(|i| i.get("value").and_then(|x| x.as_str()).map(str::to_string))
                    .collect();
                out.extend(names);
            }
            map.values().for_each(|c| collect_calls(c, out));
        }
        serde_json::Value::Array(items) => items.iter().for_each(|c| collect_calls(c, out)),
        _ => {}
    }
}

/// Emitted `'…'`: a lone string constant, with no written type.
fn is_string_literal(sql: &str) -> bool {
    sql.len() >= 2
        && sql.starts_with('\'')
        && sql.ends_with('\'')
        && !sql[1..sql.len() - 1].replace("''", "").contains('\'')
}

/// `'…'::type`, in the emitted form of a default (read back then re-emitted:
/// the declarative schema writing it that way replays to this same text).
fn typed_literal(literal: &str, ty: &TypeRef) -> String {
    let written = format!("{literal}::{}", crate::dump::type_sql(ty));
    let parsed = pgzest_parser::parse(&format!("SELECT {written}"));
    match parsed.as_deref() {
        Ok([Statement::Select(s)]) => match s.columns.as_slice() {
            [pgzest_ast::SelectItem::Expr { expr, .. }] => emit_expr(expr),
            _ => written,
        },
        _ => written,
    }
}
