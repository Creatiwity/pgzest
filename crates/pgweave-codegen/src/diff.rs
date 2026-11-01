use std::collections::HashMap;

use pgweave_ast::*;

use crate::emit::{
    emit_column_def, emit_create_function, emit_create_index, emit_create_policy,
    emit_create_procedure, emit_create_table, emit_create_trigger, emit_data_type, emit_ident,
    emit_object_name,
};

// ── Public types ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub enum DiffOp {
    CreateTable(CreateTable),
    DropTable {
        name: String,
    },
    AddColumn {
        table: String,
        column: ColumnDef,
    },
    DropColumn {
        table: String,
        column: String,
    },
    AlterColumnType {
        table: String,
        column: String,
        new_type: DataType,
    },
    CreateIndex(CreateIndex),
    DropIndex {
        name: String,
    },
    CreatePolicy(CreatePolicy),
    DropPolicy {
        name: String,
        table: String,
    },
    CreateFunction(CreateFunction),
    CreateProcedure(CreateProcedure),
    CreateTrigger(CreateTrigger),
    DropTrigger {
        name: String,
        table: String,
    },
}

#[derive(Debug, Clone, Default)]
pub struct MigrationDiff {
    pub operations: Vec<DiffOp>,
}

impl MigrationDiff {
    pub fn to_sql(&self) -> String {
        self.operations
            .iter()
            .map(emit_diff_op)
            .collect::<Vec<_>>()
            .join("\n")
    }
}

// ── Diff engine ───────────────────────────────────────────────────────────────

/// Compare two sets of DDL statements (old schema vs new schema) and produce
/// the minimal set of operations required to migrate from old to new.
///
/// Handles:
/// - Tables: create, drop, add/drop/alter columns
/// - Indexes: create/drop by name
/// - Policies: create/drop by (name, table)
/// - Functions & procedures: replace when changed
/// - Triggers: create/drop by (name, table)
pub fn diff_schemas(old: &[Statement], new: &[Statement]) -> MigrationDiff {
    let old_ddl = collect_ddl(old);
    let new_ddl = collect_ddl(new);

    let mut ops: Vec<DiffOp> = Vec::new();

    // ── Tables ────────────────────────────────────────────────────────────────
    diff_tables(&old_ddl.tables, &new_ddl.tables, &mut ops);

    // ── Indexes ───────────────────────────────────────────────────────────────
    diff_indexes(&old_ddl.indexes, &new_ddl.indexes, &mut ops);

    // ── Policies ──────────────────────────────────────────────────────────────
    diff_policies(&old_ddl.policies, &new_ddl.policies, &mut ops);

    // ── Functions ─────────────────────────────────────────────────────────────
    diff_functions(&old_ddl.functions, &new_ddl.functions, &mut ops);

    // ── Procedures ────────────────────────────────────────────────────────────
    diff_procedures(&old_ddl.procedures, &new_ddl.procedures, &mut ops);

    // ── Triggers ──────────────────────────────────────────────────────────────
    diff_triggers(&old_ddl.triggers, &new_ddl.triggers, &mut ops);

    MigrationDiff { operations: ops }
}

// ── Internal schema snapshot ──────────────────────────────────────────────────

#[derive(Default)]
struct SchemaSnapshot {
    /// table FQN → CreateTable
    tables: HashMap<String, CreateTable>,
    /// index name → CreateIndex
    indexes: HashMap<String, CreateIndex>,
    /// (policy_name, table_fqn) → CreatePolicy
    policies: HashMap<(String, String), CreatePolicy>,
    /// function FQN → CreateFunction
    functions: HashMap<String, CreateFunction>,
    /// procedure FQN → CreateProcedure
    procedures: HashMap<String, CreateProcedure>,
    /// (trigger_name, table_fqn) → CreateTrigger
    triggers: HashMap<(String, String), CreateTrigger>,
}

fn collect_ddl(stmts: &[Statement]) -> SchemaSnapshot {
    let mut snap = SchemaSnapshot::default();
    for stmt in stmts {
        if let Statement::Ddl(ddl) = stmt {
            match ddl {
                DdlStatement::CreateTable(ct) => {
                    snap.tables.insert(emit_object_name(&ct.name), ct.clone());
                }
                DdlStatement::CreateIndex(ci) => {
                    if let Some(name) = &ci.name {
                        snap.indexes.insert(emit_ident(name), ci.clone());
                    }
                }
                DdlStatement::CreatePolicy(cp) => {
                    snap.policies.insert(
                        (emit_ident(&cp.name), emit_object_name(&cp.table)),
                        cp.clone(),
                    );
                }
                DdlStatement::CreateFunction(cf) => {
                    snap.functions
                        .insert(emit_object_name(&cf.name), cf.clone());
                }
                DdlStatement::CreateProcedure(cp) => {
                    snap.procedures
                        .insert(emit_object_name(&cp.name), cp.clone());
                }
                DdlStatement::CreateTrigger(ct) => {
                    snap.triggers.insert(
                        (emit_ident(&ct.name), emit_object_name(&ct.table)),
                        ct.clone(),
                    );
                }
                DdlStatement::AlterTable(_) => {
                    // ALTER TABLE statements are not diffed — they are
                    // the output of the diff, not an input schema element.
                }
                // Types, domaines, extensions, schémas, vues et séquences ne
                // sont pas encore comparés : le diff sur le modèle complet du
                // catalogue arrive en P7 (PLAN.md). Les DROP, comme les ALTER,
                // sont des sorties de diff, pas des éléments de schéma.
                DdlStatement::CreateType(_)
                | DdlStatement::AlterType(_)
                | DdlStatement::CreateDomain(_)
                | DdlStatement::CreateExtension(_)
                | DdlStatement::AlterExtension(_)
                | DdlStatement::CreateSchema(_)
                | DdlStatement::CreateView(_)
                | DdlStatement::CreateSequence(_)
                | DdlStatement::Drop(_) => {}
            }
        }
    }
    snap
}

// ── Table diffing ─────────────────────────────────────────────────────────────

fn diff_tables(
    old: &HashMap<String, CreateTable>,
    new: &HashMap<String, CreateTable>,
    ops: &mut Vec<DiffOp>,
) {
    // Tables in new but not in old → CREATE
    for (name, new_table) in new {
        if !old.contains_key(name) {
            ops.push(DiffOp::CreateTable(new_table.clone()));
        }
    }

    // Tables in old but not in new → DROP
    for name in old.keys() {
        if !new.contains_key(name) {
            ops.push(DiffOp::DropTable { name: name.clone() });
        }
    }

    // Tables in both → diff columns
    for (name, new_table) in new {
        if let Some(old_table) = old.get(name) {
            diff_columns(name, old_table, new_table, ops);
        }
    }
}

fn diff_columns(table_name: &str, old: &CreateTable, new: &CreateTable, ops: &mut Vec<DiffOp>) {
    let old_cols: HashMap<&str, &ColumnDef> = old
        .columns
        .iter()
        .map(|c| (c.name.value.as_str(), c))
        .collect();
    let new_cols: HashMap<&str, &ColumnDef> = new
        .columns
        .iter()
        .map(|c| (c.name.value.as_str(), c))
        .collect();

    // Columns in new but not in old → ADD COLUMN
    for (col_name, col) in &new_cols {
        if !old_cols.contains_key(col_name) {
            ops.push(DiffOp::AddColumn {
                table: table_name.to_string(),
                column: (*col).clone(),
            });
        }
    }

    // Columns in old but not in new → DROP COLUMN
    for col_name in old_cols.keys() {
        if !new_cols.contains_key(col_name) {
            ops.push(DiffOp::DropColumn {
                table: table_name.to_string(),
                column: col_name.to_string(),
            });
        }
    }

    // Columns in both — check type changes
    for (col_name, new_col) in &new_cols {
        if let Some(old_col) = old_cols.get(col_name)
            && old_col.data_type != new_col.data_type
        {
            ops.push(DiffOp::AlterColumnType {
                table: table_name.to_string(),
                column: col_name.to_string(),
                new_type: new_col.data_type.clone(),
            });
        }
    }
}

// ── Index diffing ─────────────────────────────────────────────────────────────

fn diff_indexes(
    old: &HashMap<String, CreateIndex>,
    new: &HashMap<String, CreateIndex>,
    ops: &mut Vec<DiffOp>,
) {
    for (name, idx) in new {
        if !old.contains_key(name) {
            ops.push(DiffOp::CreateIndex(idx.clone()));
        }
    }
    for name in old.keys() {
        if !new.contains_key(name) {
            ops.push(DiffOp::DropIndex { name: name.clone() });
        }
    }
}

// ── Policy diffing ────────────────────────────────────────────────────────────

fn diff_policies(
    old: &HashMap<(String, String), CreatePolicy>,
    new: &HashMap<(String, String), CreatePolicy>,
    ops: &mut Vec<DiffOp>,
) {
    for (key, policy) in new {
        if !old.contains_key(key) {
            ops.push(DiffOp::CreatePolicy(policy.clone()));
        }
    }
    for (key, policy) in old {
        if !new.contains_key(key) {
            ops.push(DiffOp::DropPolicy {
                name: emit_ident(&policy.name),
                table: emit_object_name(&policy.table),
            });
        }
    }
}

// ── Function diffing ──────────────────────────────────────────────────────────

fn diff_functions(
    old: &HashMap<String, CreateFunction>,
    new: &HashMap<String, CreateFunction>,
    ops: &mut Vec<DiffOp>,
) {
    for (name, func) in new {
        match old.get(name) {
            None => ops.push(DiffOp::CreateFunction(func.clone())),
            Some(old_func) if old_func != func => {
                // Emit as CREATE OR REPLACE
                let mut replacement = func.clone();
                replacement.or_replace = true;
                ops.push(DiffOp::CreateFunction(replacement));
            }
            _ => {}
        }
    }
}

// ── Procedure diffing ─────────────────────────────────────────────────────────

fn diff_procedures(
    old: &HashMap<String, CreateProcedure>,
    new: &HashMap<String, CreateProcedure>,
    ops: &mut Vec<DiffOp>,
) {
    for (name, proc) in new {
        match old.get(name) {
            None => ops.push(DiffOp::CreateProcedure(proc.clone())),
            Some(old_proc) if old_proc != proc => {
                let mut replacement = proc.clone();
                replacement.or_replace = true;
                ops.push(DiffOp::CreateProcedure(replacement));
            }
            _ => {}
        }
    }
}

// ── Trigger diffing ───────────────────────────────────────────────────────────

fn diff_triggers(
    old: &HashMap<(String, String), CreateTrigger>,
    new: &HashMap<(String, String), CreateTrigger>,
    ops: &mut Vec<DiffOp>,
) {
    for (key, trigger) in new {
        if !old.contains_key(key) {
            ops.push(DiffOp::CreateTrigger(trigger.clone()));
        }
    }
    for (key, trigger) in old {
        if !new.contains_key(key) {
            ops.push(DiffOp::DropTrigger {
                name: emit_ident(&trigger.name),
                table: emit_object_name(&trigger.table),
            });
        }
    }
}

// ── SQL emission for diff ops ─────────────────────────────────────────────────

fn emit_diff_op(op: &DiffOp) -> String {
    match op {
        DiffOp::CreateTable(ct) => emit_create_table(ct),
        DiffOp::DropTable { name } => format!("DROP TABLE {};", name),
        DiffOp::AddColumn { table, column } => {
            format!(
                "ALTER TABLE {} ADD COLUMN {};",
                table,
                emit_column_def(column)
            )
        }
        DiffOp::DropColumn { table, column } => {
            format!("ALTER TABLE {} DROP COLUMN {};", table, column)
        }
        DiffOp::AlterColumnType {
            table,
            column,
            new_type,
        } => {
            format!(
                "ALTER TABLE {} ALTER COLUMN {} SET DATA TYPE {};",
                table,
                column,
                emit_data_type(new_type)
            )
        }
        DiffOp::CreateIndex(ci) => emit_create_index(ci),
        DiffOp::DropIndex { name } => format!("DROP INDEX {};", name),
        DiffOp::CreatePolicy(cp) => emit_create_policy(cp),
        DiffOp::DropPolicy { name, table } => {
            format!("DROP POLICY {} ON {};", name, table)
        }
        DiffOp::CreateFunction(cf) => emit_create_function(cf),
        DiffOp::CreateProcedure(cp) => emit_create_procedure(cp),
        DiffOp::CreateTrigger(ct) => emit_create_trigger(ct),
        DiffOp::DropTrigger { name, table } => {
            format!("DROP TRIGGER {} ON {};", name, table)
        }
    }
}
