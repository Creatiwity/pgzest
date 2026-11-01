use std::collections::{HashMap, HashSet};

use pgweave_ast::ddl::{DdlStatement, FunctionParam, ReturnType, TriggerEvent};
use pgweave_ast::expr::Ident;
use pgweave_ast::plpgsql::{
    Block, Declaration, IntoTarget, PlStatement, PlStatementKind, PlpgsqlBody, VarType,
};
use pgweave_ast::stmt::{InsertSource, Statement};
use pgweave_ast::types::DataType;

use pgweave_diagnostics::Diagnostic;
use pgweave_diagnostics::codes;
use pgweave_schema::provider::{
    ColumnInfo, FunctionSignature, SchemaProvider, TableSchema, object_name_to_string,
};

// ── Trigger context ───────────────────────────────────────────────────────────

/// Resolved trigger context for RETURNS TRIGGER functions.
///
/// The per-event flags below are populated by trigger resolution but not yet
/// consumed by the checker; they are the groundwork for narrowing NEW/OLD
/// availability per event (INSERT: NEW only, DELETE: OLD only, UPDATE: both).
#[allow(dead_code)]
pub struct TriggerCtx {
    /// Lowercase name of the table the trigger fires on.
    pub table: String,
    /// Combined events across all triggers pointing to this function.
    pub events: Vec<TriggerEvent>,
    /// Whether any trigger is statement-level (affects NEW/OLD semantics).
    pub has_statement_level: bool,
    /// Whether any trigger fires on INSERT (NEW defined, OLD undefined).
    pub has_insert: bool,
    /// Whether any trigger fires on DELETE (OLD defined, NEW undefined).
    pub has_delete: bool,
    /// Whether any trigger fires on UPDATE (both NEW and OLD defined).
    pub has_update: bool,
    /// Tables de transition déclarées par `REFERENCING { NEW | OLD } TABLE AS
    /// nom` (noms en minuscules) : dans le corps, ce sont des tables ayant les
    /// colonnes de la table du trigger.
    pub transition_tables: Vec<String>,
}

// ── Type context ──────────────────────────────────────────────────────────────

struct TypeContext<'a> {
    /// Variables declared in DECLARE
    vars: HashMap<String, DataType>,
    /// Function parameters
    params: HashMap<String, DataType>,
    /// True when the function has RETURNS TRIGGER (even if the table is unknown).
    is_trigger: bool,
    /// Trigger table schema (None when trigger table is not in the schema).
    trigger_table: Option<&'a TableSchema>,
    /// Resolved trigger context (events, row/statement level, …)
    #[allow(dead_code)]
    trigger_ctx: Option<TriggerCtx>,
    /// Temp tables created within the function body
    temp_tables: HashMap<String, TableSchema>,
    /// Schema access (global)
    schema: &'a dyn SchemaProvider,
    /// Declared return type (reserved for future type-compatibility checks)
    #[allow(dead_code)]
    return_type: &'a ReturnType,
}

// ── Body schema provider ──────────────────────────────────────────────────────

/// Overlays function-local temp tables on the global schema provider.
struct BodySchemaProvider<'a> {
    temp_tables: &'a HashMap<String, TableSchema>,
    global: &'a dyn SchemaProvider,
}

impl<'a> SchemaProvider for BodySchemaProvider<'a> {
    fn get_table(&self, name: &str) -> Option<&TableSchema> {
        let lower = name.to_lowercase();
        self.temp_tables
            .get(&lower)
            .or_else(|| self.global.get_table(name))
    }

    fn get_function(&self, name: &str) -> Option<&FunctionSignature> {
        self.global.get_function(name)
    }

    fn all_table_names(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self.temp_tables.keys().map(String::as_str).collect();
        names.extend(self.global.all_table_names());
        names
    }
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Structural checker: operates on the parsed AST.
pub fn check_plpgsql_ast(
    body: &PlpgsqlBody,
    function_name: &str,
    return_type: &ReturnType,
    trigger_ctx: Option<TriggerCtx>,
    fn_params: &[FunctionParam],
    schema: &dyn SchemaProvider,
    diags: &mut Vec<Diagnostic>,
) {
    // empty-function-body — empty block (no statements and no exception handlers).
    if body.block.statements.is_empty() && body.block.exception.is_none() {
        diags.push(
            Diagnostic::warning(
                codes::EMPTY_FUNCTION_BODY,
                format!("function '{}' has an empty body", function_name),
            )
            .with_hint("add a BEGIN … END block or a RETURN statement"),
        );
        return;
    }

    // Resolve the trigger table schema from the trigger context.
    let trigger_table_schema: Option<&TableSchema> = trigger_ctx
        .as_ref()
        .and_then(|tc| schema.get_table(&tc.table));

    // Noms déclarés : variables, alias et curseurs de tous les blocs, et
    // variables de boucle (déclarées implicitement par FOR / FOREACH). La
    // visibilité par bloc n'est pas suivie ici — un nom déclaré dans un bloc
    // imbriqué est accepté partout : cela ne peut produire de faux positif,
    // seulement manquer une erreur de portée (analyse fine : P3).
    let mut vars: HashMap<String, DataType> = HashMap::new();
    collect_declared(&body.block, &mut vars);

    // Build params from function/procedure parameters.
    let mut params: HashMap<String, DataType> = HashMap::new();
    for p in fn_params {
        if let Some(name) = &p.name {
            params.insert(name.value.to_lowercase(), p.data_type.clone());
        }
    }

    // First pass: collect any temp tables created in the body so embedded SQL
    // can reference them without false-positive E003 errors.
    let mut temp_tables = collect_temp_tables(&body.block);

    // Register NEW and OLD as table aliases so embedded SQL in trigger bodies
    // (e.g. `WHERE NEW.id = p_id`) resolves without spurious E002 errors.
    // Statement-level triggers use transition tables, not row variables —
    // the semantics differ but registration is the same for column checking purposes.
    if let Some(ts) = trigger_table_schema {
        temp_tables.insert("new".into(), ts.clone());
        temp_tables.insert("old".into(), ts.clone());
        if let Some(tc) = &trigger_ctx {
            for name in &tc.transition_tables {
                temp_tables.insert(name.clone(), ts.clone());
            }
        }
    }

    let ctx = TypeContext {
        vars,
        params,
        is_trigger: *return_type == ReturnType::Trigger,
        trigger_table: trigger_table_schema,
        trigger_ctx,
        temp_tables,
        schema,
        return_type,
    };

    check_block(&body.block, &ctx, diags);
}

/// Corps `LANGUAGE sql` (plusieurs statements, ou `BEGIN ATOMIC`) : chaque
/// statement est vérifié avec les paramètres de la routine en portée, et les
/// tables temporaires créées dans le corps.
pub fn check_sql_function_body(
    stmts: &[Statement],
    return_type: &ReturnType,
    fn_params: &[FunctionParam],
    schema: &dyn SchemaProvider,
    diags: &mut Vec<Diagnostic>,
) {
    let params: HashMap<String, DataType> = fn_params
        .iter()
        .filter_map(|p| Some((p.name.as_ref()?.value.to_lowercase(), p.data_type.clone())))
        .collect();
    let temp_tables: HashMap<String, TableSchema> = stmts
        .iter()
        .filter_map(|s| match s {
            Statement::Ddl(DdlStatement::CreateTable(ct)) if ct.temporary => {
                Some(temp_table_schema(ct))
            }
            _ => None,
        })
        .collect();
    let ctx = TypeContext {
        vars: HashMap::new(),
        params,
        is_trigger: false,
        trigger_table: None,
        trigger_ctx: None,
        temp_tables,
        schema,
        return_type,
    };
    for stmt in stmts {
        check_sql_in_body(stmt, &ctx, diags);
    }
}

// ── AST checker internals ─────────────────────────────────────────────────────

/// Build the set of all known PL/pgSQL identifiers (vars + params, lower-cased).
/// Used to suppress false-positive E001 when those names appear in embedded SQL.
fn plvars(ctx: &TypeContext<'_>) -> HashSet<String> {
    ctx.vars.keys().chain(ctx.params.keys()).cloned().collect()
}

fn check_block(block: &Block, ctx: &TypeContext<'_>, diags: &mut Vec<Diagnostic>) {
    for decl in &block.declarations {
        if let Declaration::Variable(v) = decl
            && let Some(default) = &v.default
        {
            check_body_expr(default, ctx, diags);
        }
    }
    check_statements(&block.statements, ctx, diags);
    if let Some(exc) = &block.exception {
        for handler in &exc.handlers {
            check_statements(&handler.statements, ctx, diags);
        }
    }
}

fn check_statements(stmts: &[PlStatement], ctx: &TypeContext<'_>, diags: &mut Vec<Diagnostic>) {
    for stmt in stmts {
        let first_new = diags.len();
        check_statement(stmt, ctx, diags);
        // Un diagnostic sans position est rattaché à son statement plutôt
        // qu'à la fonction entière.
        if stmt.span != pgweave_ast::Span::DUMMY {
            let span = pgweave_diagnostics::Span {
                start: stmt.span.start as usize,
                end: stmt.span.end as usize,
            };
            for d in &mut diags[first_new..] {
                if d.span.is_none() {
                    d.span = Some(span.clone());
                }
            }
        }
    }
}

fn check_statement(stmt: &PlStatement, ctx: &TypeContext<'_>, diags: &mut Vec<Diagnostic>) {
    match &stmt.kind {
        PlStatementKind::Assign { target, value } => {
            check_assign_target(target, ctx, diags);
            check_body_expr(value, ctx, diags);
        }

        PlStatementKind::Sql { statement, into } => {
            check_sql_in_body(statement, ctx, diags);
            if let Some(into) = into {
                check_into(into, ctx, diags);
            }
        }

        PlStatementKind::Perform(sel) | PlStatementKind::ReturnQuery(sel) => {
            diags.extend(check_select_in_ctx(sel, ctx));
        }

        PlStatementKind::Execute { sql, into, using } => {
            check_body_expr(sql, ctx, diags);
            for e in using {
                check_body_expr(e, ctx, diags);
            }
            if let Some(into) = into {
                check_into(into, ctx, diags);
            }
        }

        PlStatementKind::Return(e) | PlStatementKind::ReturnNext(e) => {
            if let Some(e) = e {
                check_body_expr(e, ctx, diags);
            }
        }

        PlStatementKind::ReturnQueryExecute { sql, using } => {
            check_body_expr(sql, ctx, diags);
            for e in using {
                check_body_expr(e, ctx, diags);
            }
        }

        PlStatementKind::Raise { args, options, .. } => {
            for arg in args {
                check_body_expr(arg, ctx, diags);
            }
            for opt in options {
                check_body_expr(&opt.value, ctx, diags);
            }
        }

        PlStatementKind::Assert { condition, message } => {
            check_body_expr(condition, ctx, diags);
            if let Some(m) = message {
                check_body_expr(m, ctx, diags);
            }
        }

        PlStatementKind::If {
            condition,
            then_branch,
            elsif_branches,
            else_branch,
        } => {
            check_body_expr(condition, ctx, diags);
            check_statements(then_branch, ctx, diags);
            for (c, branch) in elsif_branches {
                check_body_expr(c, ctx, diags);
                check_statements(branch, ctx, diags);
            }
            if let Some(b) = else_branch {
                check_statements(b, ctx, diags);
            }
        }

        PlStatementKind::Case {
            operand,
            whens,
            else_branch,
        } => {
            if let Some(o) = operand {
                check_body_expr(o, ctx, diags);
            }
            for (values, branch) in whens {
                for v in values {
                    check_body_expr(v, ctx, diags);
                }
                check_statements(branch, ctx, diags);
            }
            if let Some(b) = else_branch {
                check_statements(b, ctx, diags);
            }
        }

        PlStatementKind::Loop { body, .. } => check_statements(body, ctx, diags),
        PlStatementKind::While {
            condition, body, ..
        } => {
            check_body_expr(condition, ctx, diags);
            check_statements(body, ctx, diags);
        }
        PlStatementKind::ForRange {
            from, to, by, body, ..
        } => {
            check_body_expr(from, ctx, diags);
            check_body_expr(to, ctx, diags);
            if let Some(step) = by {
                check_body_expr(step, ctx, diags);
            }
            check_statements(body, ctx, diags);
        }
        PlStatementKind::ForQuery { query, body, .. } => {
            diags.extend(check_select_in_ctx(query, ctx));
            check_statements(body, ctx, diags);
        }
        PlStatementKind::ForExecute {
            sql, using, body, ..
        } => {
            check_body_expr(sql, ctx, diags);
            for e in using {
                check_body_expr(e, ctx, diags);
            }
            check_statements(body, ctx, diags);
        }
        PlStatementKind::ForCursor { args, body, .. } => {
            for a in args {
                check_body_expr(a, ctx, diags);
            }
            check_statements(body, ctx, diags);
        }
        PlStatementKind::Foreach { array, body, .. } => {
            check_body_expr(array, ctx, diags);
            check_statements(body, ctx, diags);
        }

        PlStatementKind::Exit { when, .. } | PlStatementKind::Continue { when, .. } => {
            if let Some(e) = when {
                check_body_expr(e, ctx, diags);
            }
        }

        PlStatementKind::Open { args, query, .. } => {
            for a in args {
                check_body_expr(a, ctx, diags);
            }
            if let Some(q) = query {
                diags.extend(check_select_in_ctx(q, ctx));
            }
        }
        PlStatementKind::Fetch { into, .. } => check_into(into, ctx, diags),

        PlStatementKind::Block(b) => check_block(b, ctx, diags),

        PlStatementKind::GetDiagnostics { .. }
        | PlStatementKind::Close(_)
        | PlStatementKind::Null => {}
    }
}

/// Cible d'affectation : `NEW.col` doit exister sur la table du trigger,
/// `OLD.col` n'est jamais affectable, une variable doit être déclarée.
fn check_assign_target(target: &[Ident], ctx: &TypeContext<'_>, diags: &mut Vec<Diagnostic>) {
    match target {
        [record, col] if record.value.eq_ignore_ascii_case("old") && ctx.is_trigger => {
            diags.push(Diagnostic::error(
                codes::ASSIGN_TO_OLD,
                format!("cannot assign to OLD.{}", col.value),
            ));
        }
        [record, col] if record.value.eq_ignore_ascii_case("new") => {
            if let Some(ts) = ctx.trigger_table
                && ts.get_column(&col.value).is_none()
            {
                diags.push(
                    Diagnostic::error(
                        codes::COLUMN_NOT_FOUND,
                        format!(
                            "column '{}' does not exist on trigger table '{}'",
                            col.value, ts.name
                        ),
                    )
                    .with_hint(available_columns(ts)),
                );
            }
        }
        [var, ..] => check_declared(var, ctx, diags),
        [] => {}
    }
}

/// Cibles d'un `INTO` : leur racine doit être une variable déclarée.
fn check_into(into: &IntoTarget, ctx: &TypeContext<'_>, diags: &mut Vec<Diagnostic>) {
    for target in &into.targets {
        if let Some(var) = target.first()
            && !(ctx.is_trigger
                && (var.value.eq_ignore_ascii_case("new") || var.value.eq_ignore_ascii_case("old")))
        {
            check_declared(var, ctx, diags);
        }
    }
}

fn check_declared(var: &Ident, ctx: &TypeContext<'_>, diags: &mut Vec<Diagnostic>) {
    let lower = var.value.to_lowercase();
    let upper = var.value.to_uppercase();
    if ctx.vars.contains_key(&lower)
        || ctx.params.contains_key(&lower)
        || SPECIAL_VARS.contains(&upper.as_str())
    {
        return;
    }
    diags.push(
        Diagnostic::warning(
            codes::UNDECLARED_VARIABLE,
            format!("variable '{}' may not be declared", var.value),
        )
        .with_hint("declare it in the DECLARE section or verify it's a function parameter"),
    );
}

fn available_columns(ts: &TableSchema) -> String {
    format!(
        "available columns: {}",
        ts.columns
            .iter()
            .map(|c| c.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    )
}

/// Expression d'un statement PL/pgSQL (hors requête) : ses identifiants non
/// qualifiés sont des variables ou des paramètres.
fn check_body_expr(expr: &pgweave_ast::Expr, ctx: &TypeContext<'_>, diags: &mut Vec<Diagnostic>) {
    check_sql_expr_idents(expr, ctx, diags);
}

/// Noms déclarés dans un bloc et ses blocs imbriqués, variables de boucle
/// comprises.
fn collect_declared(block: &Block, out: &mut HashMap<String, DataType>) {
    for decl in &block.declarations {
        let (name, ty) = match decl {
            Declaration::Variable(v) => (
                &v.name,
                match &v.var_type {
                    VarType::Data(dt) => dt.clone(),
                    VarType::RowTypeOf(t) => {
                        DataType::Custom(format!("{}%ROWTYPE", object_name_to_string(t)))
                    }
                    VarType::TypeOf(t) => {
                        DataType::Custom(format!("{}%TYPE", object_name_to_string(t)))
                    }
                    VarType::Record => DataType::Custom("record".into()),
                },
            ),
            Declaration::Alias { name, .. } => (name, DataType::Custom("alias".into())),
            Declaration::Cursor { name, .. } => (name, DataType::Custom("refcursor".into())),
        };
        out.insert(name.value.to_lowercase(), ty);
    }
    for_each_statement(&block.statements, &mut |s| {
        let loop_vars: Vec<&Ident> = match &s.kind {
            PlStatementKind::ForRange { var, .. } | PlStatementKind::ForCursor { var, .. } => {
                vec![var]
            }
            PlStatementKind::ForQuery { targets, .. }
            | PlStatementKind::ForExecute { targets, .. }
            | PlStatementKind::Foreach { targets, .. } => targets.iter().collect(),
            PlStatementKind::Block(b) => {
                collect_declared(b, out);
                vec![]
            }
            _ => vec![],
        };
        for v in loop_vars {
            out.entry(v.value.to_lowercase())
                .or_insert_with(|| DataType::Custom("loop variable".into()));
        }
    });
    if let Some(exc) = &block.exception {
        for h in &exc.handlers {
            for_each_statement(&h.statements, &mut |s| {
                if let PlStatementKind::Block(b) = &s.kind {
                    collect_declared(b, out);
                }
            });
        }
    }
}

/// Parcourt les statements et ceux de leurs branches et boucles (pas ceux des
/// blocs imbriqués, que l'appelant traite comme il l'entend).
fn for_each_statement(stmts: &[PlStatement], f: &mut dyn FnMut(&PlStatement)) {
    for s in stmts {
        f(s);
        match &s.kind {
            PlStatementKind::If {
                then_branch,
                elsif_branches,
                else_branch,
                ..
            } => {
                for_each_statement(then_branch, f);
                for (_, b) in elsif_branches {
                    for_each_statement(b, f);
                }
                if let Some(b) = else_branch {
                    for_each_statement(b, f);
                }
            }
            PlStatementKind::Case {
                whens, else_branch, ..
            } => {
                for (_, b) in whens {
                    for_each_statement(b, f);
                }
                if let Some(b) = else_branch {
                    for_each_statement(b, f);
                }
            }
            PlStatementKind::Loop { body, .. }
            | PlStatementKind::While { body, .. }
            | PlStatementKind::ForRange { body, .. }
            | PlStatementKind::ForQuery { body, .. }
            | PlStatementKind::ForExecute { body, .. }
            | PlStatementKind::ForCursor { body, .. }
            | PlStatementKind::Foreach { body, .. } => for_each_statement(body, f),
            _ => {}
        }
    }
}

/// Variables spéciales de PL/pgSQL, toujours en portée.
///
/// Sans cette liste, `RETURN TG_OP` — parfaitement valide — serait signalé sur
/// chaque trigger : le parser en fait un identifiant non qualifié comme un
/// autre.
const SPECIAL_VARS: &[&str] = &[
    "FOUND",
    "SQLSTATE",
    "SQLERRM",
    "TG_NAME",
    "TG_WHEN",
    "TG_LEVEL",
    "TG_OP",
    "TG_RELID",
    "TG_RELNAME",
    "TG_TABLE_NAME",
    "TG_TABLE_SCHEMA",
    "TG_NARGS",
    "TG_ARGV",
];

/// Rattache un diagnostic à la position de l'expression qui le cause, quand
/// elle est connue.
fn at_expr(d: Diagnostic, expr: &pgweave_ast::Expr) -> Diagnostic {
    if expr.span == pgweave_ast::Span::DUMMY {
        return d;
    }
    d.with_span(pgweave_diagnostics::Span {
        start: expr.span.start as usize,
        end: expr.span.end as usize,
    })
}

/// Vérifie les identifiants non qualifiés d'une expression SQL de corps.
///
/// # Position
///
/// Le diagnostic est émis **sans span**, donc rapporté au niveau du statement
/// englobant (la fonction). Les spans des expressions existent bien, mais ils
/// sont relatifs à un `SELECT <expr>` synthétique : les fragments PL/pgSQL sont
/// reconstruits par jonction de tokens (`parts.join(" ")`), ce qui détruit la
/// correspondance octet à octet avec la source. Mieux vaut pointer la bonne
/// fonction qu'une position fausse. Rendre ces spans exacts demande de trancher
/// la source au lieu de rejoindre des tokens — un chantier à part.
fn check_sql_expr_idents(
    expr: &pgweave_ast::Expr,
    ctx: &TypeContext<'_>,
    diags: &mut Vec<Diagnostic>,
) {
    use pgweave_ast::expr::ExprKind;

    match &expr.kind {
        ExprKind::Column(col) => {
            if let Some(qualifier) = &col.table {
                let q = qualifier.value.to_lowercase();

                // `NEW.col` / `OLD.col` au sein d'une expression SQL. Ce chemin
                // n'existait pas : le parser tronquait toute expression
                // commençant par `NEW.`, donc seul le cas `NEW.col` isolé
                // arrivait ici, par la branche `PlExpr::NewField`. Maintenant
                // qu'une condition composite est conservée entière, la colonne
                // doit être vérifiée ici aussi.
                if (q == "new" || q == "old")
                    && let Some(ts) = ctx.trigger_table
                    && ts.get_column(&col.column.value).is_none()
                {
                    diags.push(at_expr(
                        Diagnostic::error(
                            codes::COLUMN_NOT_FOUND,
                            format!(
                                "column '{}' does not exist on trigger table '{}'",
                                col.column.value, ts.name
                            ),
                        )
                        .with_hint(format!(
                            "available columns: {}",
                            ts.columns
                                .iter()
                                .map(|c| c.name.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        )),
                        expr,
                    ));
                }

                // Tout autre qualificateur (`rec.champ`, alias de requête
                // englobante…) reste trop ambigu pour trancher sans faux
                // positifs.
                return;
            }
            let name = &col.column.value;
            let lower = name.to_lowercase();
            let upper = name.to_uppercase();

            if ctx.vars.contains_key(&lower)
                || ctx.params.contains_key(&lower)
                || SPECIAL_VARS.contains(&upper.as_str())
            {
                return;
            }

            // `NEW` et `OLD` nus — comme dans `RETURN NEW` — sont les variables
            // ligne d'un trigger. Le parser n'en fait un `NewField` que
            // lorsqu'un `.colonne` suit, donc il faut les reconnaître ici.
            //
            // Conditionné à `is_trigger` plutôt que blanchi : `RETURN NEW` dans
            // une fonction ordinaire est une vraie erreur, autant la signaler.
            if ctx.is_trigger && (upper == "NEW" || upper == "OLD") {
                return;
            }

            // Même code et même sévérité que la branche `PlExpr::Var` : un
            // identifiant inconnu dans une expression de corps, c'est la même
            // chose, et rester cohérent évite deux vocabulaires pour un fait.
            let mut known: Vec<&str> = ctx
                .vars
                .keys()
                .chain(ctx.params.keys())
                .map(String::as_str)
                .collect();
            known.sort_unstable();

            let hint = if known.is_empty() {
                "declare it in the DECLARE section or verify it's a function parameter".to_string()
            } else {
                format!("declared variables and parameters: {}", known.join(", "))
            };

            diags.push(at_expr(
                Diagnostic::warning(
                    codes::UNDECLARED_VARIABLE,
                    format!("variable '{}' may not be declared", name),
                )
                .with_hint(hint),
                expr,
            ));
        }

        ExprKind::BinaryOp { left, right, .. } | ExprKind::Quantified { left, right, .. } => {
            check_sql_expr_idents(left, ctx, diags);
            check_sql_expr_idents(right, ctx, diags);
        }
        ExprKind::UnaryOp { expr, .. } | ExprKind::Cast { expr, .. } => {
            check_sql_expr_idents(expr, ctx, diags);
        }
        ExprKind::FunctionCall { args, filter, .. } => {
            for a in args {
                check_sql_expr_idents(a, ctx, diags);
            }
            if let Some(f) = filter {
                check_sql_expr_idents(f, ctx, diags);
            }
        }
        ExprKind::Case {
            operand,
            when_clauses,
            else_clause,
        } => {
            if let Some(op) = operand {
                check_sql_expr_idents(op, ctx, diags);
            }
            for (w, t) in when_clauses {
                check_sql_expr_idents(w, ctx, diags);
                check_sql_expr_idents(t, ctx, diags);
            }
            if let Some(e) = else_clause {
                check_sql_expr_idents(e, ctx, diags);
            }
        }
        ExprKind::InList { expr, list, .. } => {
            check_sql_expr_idents(expr, ctx, diags);
            for e in list {
                check_sql_expr_idents(e, ctx, diags);
            }
        }
        ExprKind::Between {
            expr, low, high, ..
        } => {
            check_sql_expr_idents(expr, ctx, diags);
            check_sql_expr_idents(low, ctx, diags);
            check_sql_expr_idents(high, ctx, diags);
        }

        // Les sous-requêtes ont leur propre portée : les colonnes y sont
        // légitimes, ce n'est pas à cette vérification de s'en occuper.
        ExprKind::Subquery(_) | ExprKind::InSubquery { .. } => {}

        // Descendre dans les constructions ajoutées.
        ExprKind::AtTimeZone {
            timestamp,
            time_zone,
        } => {
            check_sql_expr_idents(timestamp, ctx, diags);
            check_sql_expr_idents(time_zone, ctx, diags);
        }
        ExprKind::Collate { expr, .. } => check_sql_expr_idents(expr, ctx, diags),
        ExprKind::Tuple(items) => {
            for e in items {
                check_sql_expr_idents(e, ctx, diags);
            }
        }
        ExprKind::FieldAccess { root, .. } => check_sql_expr_idents(root, ctx, diags),

        // `EXISTS` ouvre une portée de requête : ses colonnes n'ont pas à être
        // des variables du corps.
        ExprKind::Exists { .. } => {}

        ExprKind::Literal(_)
        | ExprKind::Wildcard
        | ExprKind::QualifiedWildcard(_)
        | ExprKind::Interval(_)
        | ExprKind::Parameter(_)
        | ExprKind::Unsupported(_) => {}
    }
}

// ── AST checker helpers ───────────────────────────────────────────────────────

/// First pass over a block: collect all `CREATE TEMP TABLE` statements so the
/// rest of the body can reference them without false-positive E003 errors.
/// Schéma d'une table temporaire créée dans un corps, avec sa clé de
/// recherche (nom en minuscules).
fn temp_table_schema(ct: &pgweave_ast::ddl::CreateTable) -> (String, TableSchema) {
    let name = object_name_to_string(&ct.name);
    let columns = ct
        .columns
        .iter()
        .map(|col| ColumnInfo {
            name: col.name.value.clone(),
            data_type: col.data_type.clone(),
            nullable: col.nullable,
        })
        .collect();
    (name.to_lowercase(), TableSchema { name, columns })
}

fn collect_temp_tables(block: &Block) -> HashMap<String, TableSchema> {
    let mut out: HashMap<String, TableSchema> = HashMap::new();
    collect_temp_tables_in_block(block, &mut out);
    out
}

fn collect_temp_tables_in_block(block: &Block, out: &mut HashMap<String, TableSchema>) {
    let mut visit = |s: &PlStatement| match &s.kind {
        PlStatementKind::Sql {
            statement: Statement::Ddl(DdlStatement::CreateTable(ct)),
            ..
        } if ct.temporary => {
            let (key, table) = temp_table_schema(ct);
            out.insert(key, table);
        }
        PlStatementKind::Block(b) => collect_temp_tables_in_block(b, out),
        _ => {}
    };
    for_each_statement(&block.statements, &mut visit);
    if let Some(exc) = &block.exception {
        for handler in &exc.handlers {
            for_each_statement(&handler.statements, &mut visit);
        }
    }
}

/// Check a SELECT statement with the current body context — body schema, plvars,
/// and phantom qualifiers (NEW/OLD when trigger table is unknown).
fn check_select_in_ctx(
    sel: &pgweave_ast::stmt::SelectStmt,
    ctx: &TypeContext<'_>,
) -> Vec<Diagnostic> {
    use super::select::{check_select_with_plvars, check_select_with_plvars_and_phantoms};
    let body = BodySchemaProvider {
        temp_tables: &ctx.temp_tables,
        global: ctx.schema,
    };
    let pv = plvars(ctx);
    if ctx.is_trigger && ctx.trigger_table.is_none() {
        let ph: HashSet<String> = ["new".to_string(), "old".to_string()].into_iter().collect();
        check_select_with_plvars_and_phantoms(sel, &body, &pv, &ph)
    } else {
        check_select_with_plvars(sel, &body, &pv)
    }
}

/// La requête source d'un `WITH … INSERT … SELECT`, avec les CTE du
/// statement : elles y sont visibles comme dans un `WITH … SELECT`.
fn with_statement_ctes<'a>(
    sel: &'a pgweave_ast::stmt::SelectStmt,
    with: &[pgweave_ast::stmt::Cte],
    recursive: bool,
) -> std::borrow::Cow<'a, pgweave_ast::stmt::SelectStmt> {
    if with.is_empty() {
        return std::borrow::Cow::Borrowed(sel);
    }
    let mut owned = sel.clone();
    owned.with = with.iter().cloned().chain(owned.with).collect();
    owned.with_recursive |= recursive;
    std::borrow::Cow::Owned(owned)
}

/// Check a SQL statement embedded in a PL/pgSQL body.
///
/// Uses the body schema (global + temp tables) and suppresses plvar names so
/// variable references don't fire false-positive E001/E002 errors.
fn check_sql_in_body(stmt: &Statement, ctx: &TypeContext<'_>, diags: &mut Vec<Diagnostic>) {
    let body = BodySchemaProvider {
        temp_tables: &ctx.temp_tables,
        global: ctx.schema,
    };
    let pv = plvars(ctx);

    // When the function is a trigger but we don't know the trigger table,
    // treat NEW and OLD as phantom qualifiers (suppress E002, skip column checks)
    // rather than firing false-positive errors.
    let phantoms: HashSet<String> = if ctx.is_trigger && ctx.trigger_table.is_none() {
        ["new".to_string(), "old".to_string()].into_iter().collect()
    } else {
        HashSet::new()
    };
    let check_sel = |sel: &pgweave_ast::stmt::SelectStmt,
                     provider: &dyn pgweave_schema::provider::SchemaProvider,
                     out: &mut Vec<Diagnostic>| {
        use super::select::{check_select_with_plvars, check_select_with_plvars_and_phantoms};
        if phantoms.is_empty() {
            out.extend(check_select_with_plvars(sel, provider, &pv));
        } else {
            out.extend(check_select_with_plvars_and_phantoms(
                sel, provider, &pv, &phantoms,
            ));
        }
    };

    match stmt {
        Statement::Select(sel) => {
            check_sel(sel, &body, diags);
        }

        Statement::Insert(ins) => {
            let name = object_name_to_string(&ins.table);
            if body.get_table(&name).is_none() {
                diags.push(
                    Diagnostic::error(
                        codes::TABLE_NOT_FOUND,
                        format!("table '{}' does not exist", name),
                    )
                    .with_hint("check the table name or provide a schema with that table"),
                );
            }
            if let InsertSource::Select(sel) = &ins.source {
                check_sel(
                    &with_statement_ctes(sel, &ins.with, ins.with_recursive),
                    &body,
                    diags,
                );
            } else {
                for cte in &ins.with {
                    check_sel(&cte.query, &body, diags);
                }
            }
        }

        Statement::Update(upd) => {
            let name = object_name_to_string(&upd.table.name);
            if body.get_table(&name).is_none() {
                diags.push(Diagnostic::error(
                    codes::TABLE_NOT_FOUND,
                    format!("table '{}' does not exist", name),
                ));
            }
        }

        Statement::Delete(del) => {
            let name = object_name_to_string(&del.table.name);
            if body.get_table(&name).is_none() {
                diags.push(Diagnostic::error(
                    codes::TABLE_NOT_FOUND,
                    format!("table '{}' does not exist", name),
                ));
            }
        }

        // CREATE TEMP TABLE is already tracked by collect_temp_tables — skip.
        Statement::Ddl(DdlStatement::CreateTable(ct)) if ct.temporary => {}

        // Other DDL (CREATE INDEX, etc.) — fall back to full checker with body schema.
        _ => {
            diags.extend(super::check_statements(std::slice::from_ref(stmt), &body));
        }
    }
}
