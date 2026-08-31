use pgsql_ast::expr::ExprKind;
use pgsql_ast::*;

/// Configuration for build-time field injection.
#[derive(Debug, Clone, Default)]
pub struct InjectionConfig {
    /// Inject `created_at` and `updated_at` columns if not present.
    pub audit_fields: bool,
    /// Inject `deleted_at` column for soft-delete pattern.
    pub soft_delete: bool,
    /// Inject a NOTIFY trigger after INSERT/UPDATE/DELETE mutations.
    pub notify_on_change: bool,
}

/// Takes a `CreateTable` AST node and injects configured columns if not
/// already present in the table definition.
pub fn inject_audit_fields(table: &mut CreateTable, config: &InjectionConfig) {
    if config.audit_fields {
        if !has_column(table, "created_at") {
            table
                .columns
                .push(make_timestamptz_col("created_at", Some("NOW()")));
        }
        if !has_column(table, "updated_at") {
            table
                .columns
                .push(make_timestamptz_col("updated_at", Some("NOW()")));
        }
    }
    if config.soft_delete && !has_column(table, "deleted_at") {
        table
            .columns
            .push(make_nullable_timestamptz_col("deleted_at"));
    }
}

/// Generates a `set_updated_at` trigger function and a corresponding trigger
/// for the given table.
///
/// Produces two `Statement`s:
/// 1. `CREATE OR REPLACE FUNCTION set_updated_at_<table>()` — PL/pgSQL body
/// 2. `CREATE TRIGGER trg_set_updated_at_<table>` BEFORE UPDATE ON <table>
pub fn generate_updated_at_trigger(table_name: &str) -> Vec<Statement> {
    let fn_name = format!("set_updated_at_{}", table_name);
    let trigger_name = format!("trg_set_updated_at_{}", table_name);

    let function = Statement::Ddl(DdlStatement::CreateFunction(CreateFunction {
        id: NodeId::DUMMY,
        span: Span::DUMMY,
        body_start: 0,
        name: ObjectName::unquoted(&[&fn_name]),
        or_replace: true,
        params: vec![],
        return_type: ReturnType::Trigger,
        language: FunctionLanguage::PlPgSql,
        body: FunctionBody::Raw(
            "\nBEGIN\n  NEW.updated_at = NOW();\n  RETURN NEW;\nEND;\n".to_string(),
        ),
        security_definer: false,
        volatility: Volatility::Volatile,
        cost: None,
        rows: None,
    }));

    let trigger = Statement::Ddl(DdlStatement::CreateTrigger(CreateTrigger {
        name: Ident::new(trigger_name),
        table: ObjectName::unquoted(&[table_name]),
        timing: TriggerTiming::Before,
        events: vec![TriggerEvent::Update(vec![])],
        for_each: TriggerForEach::Row,
        when: None,
        function: ObjectName::unquoted(&[&fn_name]),
    }));

    vec![function, trigger]
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn has_column(table: &CreateTable, name: &str) -> bool {
    table.columns.iter().any(|c| c.name.value == name)
}

fn make_timestamptz_col(name: &str, default_expr: Option<&str>) -> ColumnDef {
    // Colonne injectée par le codegen : elle n'existe pas dans la source,
    // donc pas de span.
    let default = default_expr.map(|expr| {
        Expr::bare(ExprKind::FunctionCall {
            name: ObjectName::unquoted(&[expr.trim_end_matches("()")]),
            args: vec![],
            filter: None,
            distinct: false,
        })
    });
    let constraints = match &default {
        Some(d) => vec![ColumnConstraint::Default(d.clone())],
        None => vec![],
    };
    ColumnDef {
        name: Ident::new(name),
        data_type: DataType::TimestampTz,
        nullable: false,
        default,
        constraints,
    }
}

fn make_nullable_timestamptz_col(name: &str) -> ColumnDef {
    ColumnDef {
        name: Ident::new(name),
        data_type: DataType::TimestampTz,
        nullable: true,
        default: None,
        constraints: vec![],
    }
}
