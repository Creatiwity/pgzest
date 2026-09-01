use std::collections::{HashMap, HashSet};

use pgsql_ast::ddl::{DdlStatement, FunctionParam, ReturnType, TriggerEvent};
use pgsql_ast::plpgsql::{
    AssignTarget, Block, ElsIf, PlExpr, PlStatement, PlpgsqlBody, RaiseLevel,
};
use pgsql_ast::stmt::{InsertSource, Statement};
use pgsql_ast::types::DataType;

use pgsql_diagnostics::Diagnostic;
use pgsql_diagnostics::codes;
use pgsql_schema::provider::{
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

    // Build vars from DECLARE.
    let mut vars: HashMap<String, DataType> = HashMap::new();
    for decl in &body.declare {
        vars.insert(decl.name.value.to_lowercase(), decl.data_type.clone());
    }

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

/// Text-based checker (fallback when the body is not parsed).
///
/// Diagnostic codes:
/// - W002  empty body
/// - E001  column referenced via OLD./NEW. does not exist on the trigger table
pub fn check_plpgsql_body(
    body: &str,
    function_name: &str,
    trigger_table: Option<&str>,
    schema: &dyn SchemaProvider,
    diags: &mut Vec<Diagnostic>,
) {
    // empty-function-body — empty body.
    if body.trim().is_empty() {
        diags.push(
            Diagnostic::warning(
                codes::EMPTY_FUNCTION_BODY,
                format!("function '{}' has an empty body", function_name),
            )
            .with_hint("add a BEGIN … END block or a RETURN statement"),
        );
        return;
    }

    let trigger_table_schema: Option<&TableSchema> =
        trigger_table.and_then(|t| schema.get_table(t));

    for line in body.lines() {
        let trimmed = line.trim();
        let upper = trimmed.to_uppercase();

        // --- RAISE EXCEPTION / WARNING / NOTICE — always valid, skip.
        if upper.starts_with("RAISE") {
            continue;
        }

        // --- Embedded SQL queries: attempt to parse and check them.
        // E002 (table/alias not in FROM) is suppressed here — without knowing
        // the declared variables, qualified refs like `v_rec.col` always fire
        // false positives. E001 and E003 are kept as best-effort signals.
        if contains_dml_keyword(&upper)
            && let Some(sql_fragment) = extract_sql_fragment(trimmed)
        {
            // Fragment parse failures are expected for multi-line or
            // PL/pgSQL-variable-containing queries — ignore silently.
            if let Ok(stmts) = pgsql_parser::parse(sql_fragment) {
                let inner = super::check_statements(&stmts, schema);
                for mut d in inner {
                    if d.code == codes::TABLE_NOT_IN_SCOPE {
                        continue;
                    }
                    d.message = format!("[in {}] {}", function_name, d.message);
                    // Clear fragment-relative spans so the outer checker can
                    // backfill them with the function body offset (body_start).
                    d.span = None;
                    diags.push(d);
                }
            }
        }

        // --- OLD.column / NEW.column references in trigger bodies.
        if let Some(ts) = trigger_table_schema {
            check_old_new_refs(trimmed, ts, function_name, diags);
        }
    }
}

// ── AST checker internals ─────────────────────────────────────────────────────

/// Build the set of all known PL/pgSQL identifiers (vars + params, lower-cased).
/// Used to suppress false-positive E001 when those names appear in embedded SQL.
fn plvars(ctx: &TypeContext<'_>) -> HashSet<String> {
    ctx.vars.keys().chain(ctx.params.keys()).cloned().collect()
}

fn check_block(block: &Block, ctx: &TypeContext<'_>, diags: &mut Vec<Diagnostic>) {
    for stmt in &block.statements {
        check_statement(stmt, ctx, diags);
    }

    // Also check exception handlers.
    if let Some(exc) = &block.exception {
        for handler in &exc.handlers {
            check_block(&handler.block, ctx, diags);
        }
    }
}

fn check_statement(stmt: &PlStatement, ctx: &TypeContext<'_>, diags: &mut Vec<Diagnostic>) {
    match stmt {
        // NEW.col as assignment target — verify the column exists on the trigger table.
        PlStatement::Assign {
            target: AssignTarget::NewField(col),
            value,
        } => {
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
                    .with_hint(format!(
                        "available columns: {}",
                        ts.columns
                            .iter()
                            .map(|c| c.name.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )),
                );
            }
            check_plexpr(value, ctx, diags);
        }

        // OLD.col as assignment target — always an error.
        PlStatement::Assign {
            target: AssignTarget::OldField(col),
            value,
        } => {
            diags.push(Diagnostic::error(
                codes::ASSIGN_TO_OLD,
                format!("cannot assign to OLD.{}", col.value),
            ));
            check_plexpr(value, ctx, diags);
        }

        // Simple variable or field assignment.
        PlStatement::Assign { value, .. } => {
            check_plexpr(value, ctx, diags);
        }

        // SELECT … INTO: check the query (passing PL/pgSQL vars so they aren't flagged as columns).
        PlStatement::SelectInto { query, .. } => {
            diags.extend(check_select_in_ctx(query, ctx));
        }

        // IF statement.
        PlStatement::If {
            condition,
            then_block,
            elsif_clauses,
            else_block,
        } => {
            check_plexpr(condition, ctx, diags);
            check_block(then_block, ctx, diags);
            for ElsIf { condition, block } in elsif_clauses {
                check_plexpr(condition, ctx, diags);
                check_block(block, ctx, diags);
            }
            if let Some(eb) = else_block {
                check_block(eb, ctx, diags);
            }
        }

        // RAISE EXCEPTION with no message — validate USING options.
        PlStatement::Raise {
            level: RaiseLevel::Exception,
            message: None,
            options,
            args,
        } => {
            for opt in options {
                check_plexpr(&opt.value, ctx, diags);
            }
            for arg in args {
                check_plexpr(arg, ctx, diags);
            }
        }

        // Any other RAISE level.
        PlStatement::Raise { args, options, .. } => {
            for arg in args {
                check_plexpr(arg, ctx, diags);
            }
            for opt in options {
                check_plexpr(&opt.value, ctx, diags);
            }
        }

        // RETURN expr.
        PlStatement::Return(Some(expr)) => {
            check_plexpr(expr, ctx, diags);
        }
        PlStatement::Return(None) => {}

        // RETURN NEXT expr.
        PlStatement::ReturnNext(expr) => {
            check_plexpr(expr, ctx, diags);
        }

        // RETURN QUERY select.
        PlStatement::ReturnQuery(query) => {
            diags.extend(check_select_in_ctx(query, ctx));
        }

        // Direct SQL statement (INSERT, UPDATE, DELETE, CREATE TEMP TABLE, etc.)
        PlStatement::Sql(sql_stmt) => {
            check_sql_in_body(sql_stmt, ctx, diags);
        }

        // Nested block.
        PlStatement::SubBlock(block) => {
            check_block(block, ctx, diags);
        }

        // WHILE loop.
        PlStatement::While { condition, body } => {
            check_plexpr(condition, ctx, diags);
            check_block(body, ctx, diags);
        }

        // LOOP (unconditional).
        PlStatement::Loop(body) => {
            check_block(body, ctx, diags);
        }

        // FOR … IN range.
        PlStatement::ForRange {
            from, to, by, body, ..
        } => {
            check_plexpr(from, ctx, diags);
            check_plexpr(to, ctx, diags);
            if let Some(step) = by {
                check_plexpr(step, ctx, diags);
            }
            check_block(body, ctx, diags);
        }

        // FOR … IN query.
        PlStatement::ForSelect { query, body, .. } => {
            diags.extend(check_select_in_ctx(query, ctx));
            check_block(body, ctx, diags);
        }

        // FOR … IN EXECUTE.
        PlStatement::ForExecute {
            sql, using, body, ..
        } => {
            check_plexpr(sql, ctx, diags);
            for expr in using {
                check_plexpr(expr, ctx, diags);
            }
            check_block(body, ctx, diags);
        }

        // PERFORM.
        PlStatement::Perform(expr) => {
            check_plexpr(expr, ctx, diags);
        }

        // EXECUTE.
        PlStatement::Execute { sql, using, .. } => {
            check_plexpr(sql, ctx, diags);
            for expr in using {
                check_plexpr(expr, ctx, diags);
            }
        }

        // EXIT / CONTINUE with optional WHEN.
        PlStatement::Exit { when, .. } | PlStatement::Continue { when, .. } => {
            if let Some(expr) = when {
                check_plexpr(expr, ctx, diags);
            }
        }

        // CALL.
        PlStatement::Call { args, .. } => {
            for arg in args {
                check_plexpr(arg, ctx, diags);
            }
        }

        // NULL no-op and GET DIAGNOSTICS — nothing to check.
        PlStatement::Null | PlStatement::GetDiagnostics(_) => {}
    }
}

fn check_plexpr(expr: &PlExpr, ctx: &TypeContext<'_>, diags: &mut Vec<Diagnostic>) {
    match expr {
        PlExpr::NewField(col) => {
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
                    .with_hint(format!(
                        "available columns: {}",
                        ts.columns
                            .iter()
                            .map(|c| c.name.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )),
                );
            }
        }

        PlExpr::OldField(col) => {
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
                    .with_hint(format!(
                        "available columns: {}",
                        ts.columns
                            .iter()
                            .map(|c| c.name.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )),
                );
            }
        }

        PlExpr::Var(ident) => {
            let name = &ident.value;
            let lower = name.to_lowercase();
            let upper = name.to_uppercase();
            if !ctx.vars.contains_key(&lower)
                && !ctx.params.contains_key(&lower)
                && upper != "FOUND"
            {
                // undeclared-variable: variable may not be declared (warning, not error —
                // could be an untracked parameter).
                diags.push(
                    Diagnostic::warning(
                        codes::UNDECLARED_VARIABLE,
                        format!("variable '{}' may not be declared", name),
                    )
                    .with_hint(
                        "declare it in the DECLARE section or verify it's a function parameter",
                    ),
                );
            }
        }

        PlExpr::Found | PlExpr::Sqlstate => {}

        // Expression SQL d'un RETURN, d'un IF, d'un PERFORM… Les identifiants
        // non qualifiés y désignent des variables ou des paramètres : en
        // PL/pgSQL, hors requête, aucune colonne de table n'est en portée.
        // PostgreSQL lui-même répond `column "x" does not exist` sur un
        // identifiant inconnu.
        PlExpr::Sql(expr) => check_sql_expr_idents(expr, ctx, diags),

        // `rec.field` : le type du record n'est pas suivi, donc rien à vérifier
        // sans produire de faux positifs.
        PlExpr::RecordField { .. } => {}

        PlExpr::Not(inner) => check_plexpr(inner, ctx, diags),

        PlExpr::And(l, r) | PlExpr::Or(l, r) | PlExpr::BinOp(l, _, r) => {
            check_plexpr(l, ctx, diags);
            check_plexpr(r, ctx, diags);
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
    expr: &pgsql_ast::Expr,
    ctx: &TypeContext<'_>,
    diags: &mut Vec<Diagnostic>,
) {
    use pgsql_ast::expr::ExprKind;

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
                    diags.push(
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
                    );
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

            diags.push(
                Diagnostic::warning(
                    codes::UNDECLARED_VARIABLE,
                    format!("variable '{}' may not be declared", name),
                )
                .with_hint(hint),
            );
        }

        ExprKind::BinaryOp { left, right, .. } => {
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
fn collect_temp_tables(block: &Block) -> HashMap<String, TableSchema> {
    let mut out: HashMap<String, TableSchema> = HashMap::new();
    collect_temp_tables_in_block(block, &mut out);
    out
}

fn collect_temp_tables_in_block(block: &Block, out: &mut HashMap<String, TableSchema>) {
    for stmt in &block.statements {
        if let PlStatement::Sql(Statement::Ddl(DdlStatement::CreateTable(ct))) = stmt
            && ct.temporary
        {
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
            out.insert(name.to_lowercase(), TableSchema { name, columns });
        }
        // Recurse into nested blocks.
        match stmt {
            PlStatement::If {
                then_block,
                elsif_clauses,
                else_block,
                ..
            } => {
                collect_temp_tables_in_block(then_block, out);
                for e in elsif_clauses {
                    collect_temp_tables_in_block(&e.block, out);
                }
                if let Some(b) = else_block {
                    collect_temp_tables_in_block(b, out);
                }
            }
            PlStatement::While { body, .. }
            | PlStatement::Loop(body)
            | PlStatement::SubBlock(body) => {
                collect_temp_tables_in_block(body, out);
            }
            PlStatement::ForSelect { body, .. }
            | PlStatement::ForRange { body, .. }
            | PlStatement::ForExecute { body, .. } => {
                collect_temp_tables_in_block(body, out);
            }
            _ => {}
        }
    }
    // Also recurse into exception handlers.
    if let Some(exc) = &block.exception {
        for handler in &exc.handlers {
            collect_temp_tables_in_block(&handler.block, out);
        }
    }
}

/// Check a SELECT statement with the current body context — body schema, plvars,
/// and phantom qualifiers (NEW/OLD when trigger table is unknown).
fn check_select_in_ctx(
    sel: &pgsql_ast::stmt::SelectStmt,
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
    let check_sel = |sel: &pgsql_ast::stmt::SelectStmt,
                     provider: &dyn pgsql_schema::provider::SchemaProvider,
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
                check_sel(sel, &body, diags);
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

// ── Text-based checker helpers ────────────────────────────────────────────────

/// Returns true if the (already-uppercased) line contains a DML keyword at a
/// word boundary (to avoid false-positive matches inside identifiers or strings).
fn contains_dml_keyword(upper: &str) -> bool {
    for kw in &["SELECT", "INSERT", "UPDATE", "DELETE"] {
        if let Some(pos) = upper.find(kw) {
            let before_ok = pos == 0
                || !upper
                    .as_bytes()
                    .get(pos - 1)
                    .map(|b| b.is_ascii_alphanumeric() || *b == b'_')
                    .unwrap_or(false);
            let after_pos = pos + kw.len();
            let after_ok = after_pos >= upper.len()
                || !upper
                    .as_bytes()
                    .get(after_pos)
                    .map(|b| b.is_ascii_alphanumeric() || *b == b'_')
                    .unwrap_or(false);
            if before_ok && after_ok {
                return true;
            }
        }
    }
    false
}

/// Try to extract a plausible SQL fragment from a PL/pgSQL body line.
fn extract_sql_fragment(line: &str) -> Option<&str> {
    let t = line.trim();
    // Skip comment lines.
    if t.starts_with("--") {
        return None;
    }
    // Strip trailing semicolon for the parser.
    let t = t.strip_suffix(';').unwrap_or(t).trim();
    // If the line starts with an assignment (e.g. `v_count := SELECT ...`),
    // strip the left side.
    let t = if let Some(idx) = t.find(":=") {
        t[idx + 2..].trim()
    } else {
        t
    };
    if t.is_empty() { None } else { Some(t) }
}

/// Scan a single body line for `OLD.<col>` and `NEW.<col>` references.
fn check_old_new_refs(
    line: &str,
    trigger_table_schema: &TableSchema,
    function_name: &str,
    diags: &mut Vec<Diagnostic>,
) {
    let upper_line = line.to_uppercase();

    for prefix in &["OLD.", "NEW."] {
        let mut search_start = 0;
        while let Some(rel_pos) = upper_line[search_start..].find(prefix) {
            let abs_pos = search_start + rel_pos;
            let rest = &line[abs_pos + prefix.len()..];
            let col_name: String = rest
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();

            if !col_name.is_empty() && trigger_table_schema.get_column(&col_name).is_none() {
                diags.push(
                    Diagnostic::error(
                        codes::COLUMN_NOT_FOUND,
                        format!(
                            "[in {}] column '{}' does not exist on trigger table '{}'",
                            function_name, col_name, trigger_table_schema.name
                        ),
                    )
                    .with_hint(format!(
                        "available columns: {}",
                        trigger_table_schema
                            .columns
                            .iter()
                            .map(|c| c.name.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )),
                );
            }

            search_start = abs_pos + prefix.len();
        }
    }
}
