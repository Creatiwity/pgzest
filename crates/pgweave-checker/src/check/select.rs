use std::collections::{HashMap, HashSet};

use std::borrow::Cow;

use pgweave_ast::types::DataType;
use pgweave_ast::{Expr, SelectItem, SelectStmt, TableRef, TableSource};

use pgweave_ast::expr::ExprKind;
use pgweave_diagnostics::Diagnostic;
use pgweave_diagnostics::codes;
use pgweave_schema::provider::{ColumnInfo, SchemaProvider, TableSchema};

/// Check a single SELECT statement against the schema.
pub fn check_select(stmt: &SelectStmt, schema: &dyn SchemaProvider) -> Vec<Diagnostic> {
    check_select_inner(stmt, schema, &HashSet::new(), &HashSet::new())
}

/// Like `check_select`, but treats any identifier in `plvars` (lower-cased) as a
/// bound PL/pgSQL variable or parameter rather than a column reference — suppressing
/// false-positive E001 errors for names like `p_task_id` or `v_org_id`.
pub fn check_select_with_plvars(
    stmt: &SelectStmt,
    schema: &dyn SchemaProvider,
    plvars: &HashSet<String>,
) -> Vec<Diagnostic> {
    check_select_inner(stmt, schema, plvars, &HashSet::new())
}

/// Like `check_select_with_plvars`, but also accepts `phantom_qualifiers`: a set
/// of lowercase table qualifiers (e.g. `"new"`, `"old"`) that are known to be valid
/// row-variable aliases in the current context but whose schema is unavailable.
/// References to these qualifiers suppress E002 and skip column validation.
pub fn check_select_with_plvars_and_phantoms(
    stmt: &SelectStmt,
    schema: &dyn SchemaProvider,
    plvars: &HashSet<String>,
    phantom_qualifiers: &HashSet<String>,
) -> Vec<Diagnostic> {
    check_select_inner(stmt, schema, plvars, phantom_qualifiers)
}

/// Sources visibles d'un niveau de requête, chaînées aux niveaux englobants
/// (sous-requêtes corrélées, `LATERAL`). PostgreSQL résout un nom au niveau
/// le plus interne qui le connaît, puis remonte.
///
/// Valeur `None` : la source existe mais ses colonnes sont inconnues (table
/// déjà signalée comme inexistante, fonction sans colonnes connues) — les
/// références à ses colonnes ne sont alors pas vérifiées, pour ne pas
/// produire d'erreurs en cascade ni de faux positifs.
struct Scope<'s> {
    relations: HashMap<String, Option<Cow<'s, TableSchema>>>,
    outer: Option<&'s Scope<'s>>,
}

impl<'s> Scope<'s> {
    fn get(&self, key: &str) -> Option<&Option<Cow<'s, TableSchema>>> {
        self.relations
            .get(key)
            .or_else(|| self.outer.and_then(|o| o.get(key)))
    }

    /// Sources de tous les niveaux, du plus interne au plus externe.
    fn levels(&self) -> impl Iterator<Item = &HashMap<String, Option<Cow<'s, TableSchema>>>> {
        std::iter::successors(Some(self), |s| s.outer).map(|s| &s.relations)
    }
}

fn check_select_inner(
    stmt: &SelectStmt,
    schema: &dyn SchemaProvider,
    plvars: &HashSet<String>,
    phantom_qualifiers: &HashSet<String>,
) -> Vec<Diagnostic> {
    check_select_scoped(
        stmt,
        schema,
        plvars,
        phantom_qualifiers,
        None,
        &HashMap::new(),
    )
}

/// `ctes` : CTE visibles depuis ce niveau (nom en minuscules → schéma de ses
/// colonnes, `None` si elles ne sont pas déterminables).
fn check_select_scoped<'s>(
    stmt: &SelectStmt,
    schema: &'s dyn SchemaProvider,
    plvars: &HashSet<String>,
    phantom_qualifiers: &HashSet<String>,
    outer: Option<&'s Scope<'s>>,
    ctes: &HashMap<String, Option<TableSchema>>,
) -> Vec<Diagnostic> {
    let mut diags = Vec::new();

    // CTE : vérifiées dans l'ordre, chacune voyant les précédentes.
    let mut ctes = ctes.clone();
    for cte in &stmt.with {
        // WITH RECURSIVE : la CTE se cite dans sa propre définition. Ses
        // colonnes sont celles de sa liste explicite, sinon inconnues pendant
        // la vérification du corps.
        if stmt.with_recursive {
            let declared = (!cte.columns.is_empty()).then(|| {
                synthetic_schema(
                    &cte.name.value,
                    cte.columns.iter().map(|c| c.value.clone()).collect(),
                )
            });
            ctes.insert(cte.name.value.to_string(), declared);
        }
        let q = match &cte.query {
            pgweave_ast::stmt::CteQuery::Select(q) => q,
            // Corps modifiant : vérifié comme un statement ; ses colonnes
            // (`RETURNING`) ne sont pas établies ici (voir `pgweave-sema`).
            pgweave_ast::stmt::CteQuery::Modify(s) => {
                diags.extend(crate::check::check_statements(
                    std::slice::from_ref(&**s),
                    schema,
                ));
                let columns = (!cte.columns.is_empty())
                    .then(|| cte.columns.iter().map(|c| c.value.clone()).collect());
                ctes.insert(
                    cte.name.value.to_string(),
                    columns.map(|c| synthetic_schema(&cte.name.value, c)),
                );
                continue;
            }
        };
        diags.extend(check_select_scoped(
            q,
            schema,
            plvars,
            phantom_qualifiers,
            outer,
            &ctes,
        ));
        let columns = if cte.columns.is_empty() {
            output_names(q, schema, &ctes)
        } else {
            Some(cte.columns.iter().map(|c| c.value.clone()).collect())
        };
        ctes.insert(
            cte.name.value.to_string(),
            columns.map(|c| synthetic_schema(&cte.name.value, c)),
        );
    }

    let mut scope = Scope {
        relations: HashMap::new(),
        outer,
    };
    for table_ref in stmt.from.iter().chain(stmt.joins.iter().map(|j| &j.table)) {
        resolve_table_ref(
            table_ref,
            schema,
            plvars,
            phantom_qualifiers,
            &ctes,
            &mut scope,
            &mut diags,
        );
    }
    let ctx = &scope;

    for e in &stmt.distinct_on {
        check_expr(e, ctx, schema, plvars, phantom_qualifiers, &mut diags);
    }

    // Check SELECT columns.
    for item in &stmt.columns {
        match item {
            SelectItem::Wildcard => {
                // SELECT * is always fine.
            }
            SelectItem::QualifiedWildcard(ident) => {
                // table.* — only fire E002 if the alias is genuinely absent from
                // the FROM clause; if it's present but None (E003 already fired), skip.
                let key = ident.value.to_string();
                if ctx.get(&key).is_none() && !phantom_qualifiers.contains(&key) {
                    diags.push(Diagnostic::error(
                        codes::TABLE_NOT_IN_SCOPE,
                        format!("table or alias '{}' not found in FROM clause", ident.value),
                    ));
                }
            }
            SelectItem::Expr { expr, .. } => {
                check_expr(expr, ctx, schema, plvars, phantom_qualifiers, &mut diags);
            }
        }
    }

    // Conditions de jointure.
    for join in &stmt.joins {
        if let pgweave_ast::JoinConstraint::On(on) = &join.constraint {
            check_expr(on, ctx, schema, plvars, phantom_qualifiers, &mut diags);
        }
    }

    // Check WHERE clause.
    if let Some(where_expr) = &stmt.where_clause {
        check_expr(
            where_expr,
            ctx,
            schema,
            plvars,
            phantom_qualifiers,
            &mut diags,
        );
    }

    // Check GROUP BY.
    for expr in &stmt.group_by {
        check_expr(expr, ctx, schema, plvars, phantom_qualifiers, &mut diags);
    }

    // Check HAVING.
    if let Some(having) = &stmt.having {
        check_expr(having, ctx, schema, plvars, phantom_qualifiers, &mut diags);
    }

    // Fenêtres nommées (`WINDOW w AS (…)`).
    for w in &stmt.windows {
        for e in w.spec.exprs() {
            check_expr(e, ctx, schema, plvars, phantom_qualifiers, &mut diags);
        }
    }

    // ORDER BY peut citer un alias de sortie (`ORDER BY total`) : ces noms
    // sont des variables de plus, pour ce seul contrôle.
    let mut order_vars = plvars.clone();
    order_vars.extend(stmt.columns.iter().filter_map(|i| match i {
        SelectItem::Expr { alias: Some(a), .. } => Some(a.value.to_string()),
        _ => None,
    }));
    for order in &stmt.order_by {
        check_expr(
            &order.expr,
            ctx,
            schema,
            &order_vars,
            phantom_qualifiers,
            &mut diags,
        );
    }

    // Opérandes des opérations ensemblistes : même niveau englobant.
    for op in &stmt.set_ops {
        diags.extend(check_select_scoped(
            &op.select,
            schema,
            plvars,
            phantom_qualifiers,
            outer,
            &ctes,
        ));
    }

    diags
}

/// Schéma minimal d'une source dérivée (CTE, sous-requête, fonction à alias
/// de colonnes) : seuls les noms servent ici, au contrôle d'existence. Les
/// types ne sont pas connus à ce niveau (ils le seront avec l'analyse
/// sémantique, P3) et ne sont lus par personne.
fn synthetic_schema(name: &str, columns: Vec<String>) -> TableSchema {
    TableSchema {
        name: name.to_string(),
        columns: columns
            .into_iter()
            .map(|c| ColumnInfo {
                name: c,
                data_type: DataType::Custom("unknown".into()),
                nullable: true,
            })
            .collect(),
    }
}

/// Noms des colonnes produites par un SELECT, comme PostgreSQL les nomme :
/// alias, sinon nom de colonne ou de fonction, sinon `?column?`. `None` si
/// un `*` porte sur une source dont les colonnes sont inconnues.
fn output_names(
    sel: &SelectStmt,
    schema: &dyn SchemaProvider,
    ctes: &HashMap<String, Option<TableSchema>>,
) -> Option<Vec<String>> {
    let source_columns = |t: &TableRef| -> Option<Vec<String>> {
        let name = t.table_name()?;
        let key = pgweave_schema::provider::object_name_to_string(name).to_string();
        if name.0.len() == 1
            && let Some(cte) = ctes.get(&key)
        {
            return cte
                .as_ref()
                .map(|s| s.columns.iter().map(|c| c.name.clone()).collect());
        }
        schema
            .get_table(&key)
            .map(|s| s.columns.iter().map(|c| c.name.clone()).collect())
    };
    let sources: Vec<&TableRef> = sel
        .from
        .iter()
        .chain(sel.joins.iter().map(|j| &j.table))
        .collect();
    let mut out = Vec::new();
    for item in &sel.columns {
        match item {
            SelectItem::Wildcard => {
                for t in &sources {
                    out.extend(source_columns(t)?);
                }
            }
            SelectItem::QualifiedWildcard(q) => {
                let t = sources.iter().find(|t| {
                    t.visible_name()
                        .is_some_and(|n| n.eq_ignore_ascii_case(&q.value))
                })?;
                out.extend(source_columns(t)?);
            }
            SelectItem::Expr { alias: Some(a), .. } => out.push(a.value.clone()),
            SelectItem::Expr { expr, alias: None } => out.push(match &expr.kind {
                ExprKind::Column(c) => c.column.value.clone(),
                ExprKind::FunctionCall { name, .. } => name
                    .0
                    .last()
                    .map(|i| i.value.clone())
                    .unwrap_or_else(|| "?column?".into()),
                ExprKind::Cast { expr, .. } => match &expr.kind {
                    ExprKind::Column(c) => c.column.value.clone(),
                    _ => "?column?".into(),
                },
                _ => "?column?".into(),
            }),
        }
    }
    Some(out)
}

/// Résout une source de `FROM` / jointure et l'ajoute à la portée.
fn resolve_table_ref<'s>(
    table_ref: &TableRef,
    schema: &'s dyn SchemaProvider,
    plvars: &HashSet<String>,
    phantom_qualifiers: &HashSet<String>,
    ctes: &HashMap<String, Option<TableSchema>>,
    scope: &mut Scope<'s>,
    diags: &mut Vec<Diagnostic>,
) {
    let alias = table_ref.alias.as_ref().map(|a| a.value.to_string());
    let renamed = |base: Option<Vec<String>>| -> Option<Vec<String>> {
        if table_ref.column_aliases.is_empty() {
            return base;
        }
        let mut names = base.unwrap_or_default();
        for (i, a) in table_ref.column_aliases.iter().enumerate() {
            match names.get_mut(i) {
                Some(n) => *n = a.value.clone(),
                None => names.push(a.value.clone()),
            }
        }
        Some(names)
    };

    match &table_ref.source {
        TableSource::Table(name) => {
            let table_name = pgweave_schema::provider::object_name_to_string(name);
            let key = table_name.to_string();
            let visible = alias.clone().unwrap_or_else(|| key.clone());

            // Une CTE masque une table du même nom.
            if name.0.len() == 1
                && let Some(cte) = ctes.get(&key)
            {
                let cols = renamed(
                    cte.as_ref()
                        .map(|s| s.columns.iter().map(|c| c.name.clone()).collect()),
                );
                scope.relations.insert(
                    visible,
                    cols.map(|c| Cow::Owned(synthetic_schema(&table_name, c))),
                );
                return;
            }

            match schema.get_table(&key) {
                Some(ts) => {
                    let entry = if table_ref.column_aliases.is_empty() {
                        Cow::Borrowed(ts)
                    } else {
                        let mut renamed_ts = ts.clone();
                        for (col, a) in renamed_ts.columns.iter_mut().zip(&table_ref.column_aliases)
                        {
                            col.name = a.value.clone();
                        }
                        Cow::Owned(renamed_ts)
                    };
                    scope.relations.insert(visible, Some(entry));
                    // Also register under unaliased table name for qualified references.
                    if alias.is_some() {
                        scope.relations.insert(key, Some(Cow::Borrowed(ts)));
                    }
                }
                None => {
                    diags.push(Diagnostic::error(
                        codes::TABLE_NOT_FOUND,
                        format!("table '{}' does not exist", table_name),
                    ));
                    // Register alias (or table name) as None so qualified refs don't
                    // cascade into table-not-in-scope on top of the table-not-found already emitted.
                    scope.relations.insert(visible, None);
                }
            }
        }
        TableSource::Subquery { lateral, query } => {
            // Une sous-requête LATERAL voit les sources précédentes : on les
            // lui passe comme niveau englobant. Sinon, seulement les niveaux
            // au-dessus de la requête courante.
            let sub_diags = if *lateral {
                let outer = Scope {
                    relations: scope.relations.clone(),
                    outer: scope.outer,
                };
                check_select_scoped(
                    query,
                    schema,
                    plvars,
                    phantom_qualifiers,
                    Some(&outer),
                    ctes,
                )
            } else {
                check_select_scoped(query, schema, plvars, phantom_qualifiers, scope.outer, ctes)
            };
            diags.extend(sub_diags);
            if let Some(a) = alias {
                let cols = renamed(output_names(query, schema, ctes));
                scope
                    .relations
                    .insert(a.clone(), cols.map(|c| Cow::Owned(synthetic_schema(&a, c))));
            }
        }
        TableSource::Function { args, .. } => {
            for arg in args {
                check_expr(arg, scope, schema, plvars, phantom_qualifiers, diags);
            }
            if let Some(visible) = table_ref.visible_name().map(str::to_string) {
                // Les colonnes d'une fonction ne sont connues que par ses
                // alias de colonnes ; sinon la source reste opaque.
                let cols = renamed(None);
                scope.relations.insert(
                    visible.clone(),
                    cols.map(|c| Cow::Owned(synthetic_schema(&visible, c))),
                );
            }
        }
    }
}

/// Recursively check an expression for column/function validity.
///
/// Attache à chaque diagnostic la position de l'expression concernée. La
/// récursion se dénouant de l'intérieur vers l'extérieur, un nœud enfant a déjà
/// posé son span — plus précis — quand le parent complète : on ne remplit donc
/// que ce qui manque, et le diagnostic pointe l'expression la plus fine
/// possible plutôt que le début du statement.
fn check_expr(
    expr: &Expr,
    ctx: &Scope<'_>,
    schema: &dyn SchemaProvider,
    plvars: &HashSet<String>,
    phantom_qualifiers: &HashSet<String>,
    diags: &mut Vec<Diagnostic>,
) {
    let first_new = diags.len();
    check_expr_inner(expr, ctx, schema, plvars, phantom_qualifiers, diags);

    if let Some(span) = expr.real_span() {
        let diag_span = pgweave_diagnostics::Span {
            start: span.start as usize,
            end: span.end as usize,
        };
        for d in &mut diags[first_new..] {
            if d.span.is_none() {
                d.span = Some(diag_span.clone());
            }
        }
    }
}

fn check_expr_inner(
    expr: &Expr,
    ctx: &Scope<'_>,
    schema: &dyn SchemaProvider,
    plvars: &HashSet<String>,
    phantom_qualifiers: &HashSet<String>,
    diags: &mut Vec<Diagnostic>,
) {
    match &expr.kind {
        ExprKind::Column(col_ref) => {
            let col_name = &col_ref.column.value;

            match &col_ref.table {
                Some(tbl_ident) => {
                    // Qualified reference: table.column
                    let tbl_key = tbl_ident.value.to_string();
                    match ctx.get(&tbl_key) {
                        Some(Some(table_schema)) => {
                            if table_schema.get_column(col_name).is_none() {
                                diags.push(
                                    Diagnostic::error(
                                        codes::COLUMN_NOT_FOUND,
                                        format!(
                                            "column '{}' does not exist in table '{}'",
                                            col_name, tbl_ident.value
                                        ),
                                    )
                                    .with_hint(format!(
                                        "available columns: {}",
                                        table_schema
                                            .columns
                                            .iter()
                                            .map(|c| c.name.as_str())
                                            .collect::<Vec<_>>()
                                            .join(", ")
                                    )),
                                );
                            }
                        }
                        // Table alias is known but the table itself fired E003 — skip
                        // to avoid cascading noise on every `alias.col` reference.
                        Some(None) => {}
                        None => {
                            // Phantom qualifiers (e.g. NEW/OLD in trigger bodies without
                            // a known trigger table) suppress both E002 and column checks.
                            if phantom_qualifiers.contains(&tbl_key) {
                                // Known alias but schema unavailable — skip silently.
                            } else {
                                // Not in the FROM-clause context — fall back to the schema
                                // provider. This handles implicit row variables like NEW and
                                // OLD in PL/pgSQL trigger bodies when the trigger table IS
                                // known and registered via BodySchemaProvider.
                                match schema.get_table(&tbl_key) {
                                    Some(ts) => {
                                        if ts.get_column(col_name).is_none() {
                                            diags.push(
                                                Diagnostic::error(
                                                    codes::COLUMN_NOT_FOUND,
                                                    format!(
                                                        "column '{}' does not exist in table '{}'",
                                                        col_name, tbl_ident.value
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
                                    None => {
                                        diags.push(Diagnostic::error(
                                            codes::TABLE_NOT_IN_SCOPE,
                                            format!(
                                                "table or alias '{}' not found in FROM clause",
                                                tbl_ident.value
                                            ),
                                        ));
                                    }
                                }
                            }
                        }
                    }
                }
                None => {
                    // Unqualified reference — skip if it's a known PL/pgSQL variable/param.
                    let col_lower = col_name.to_string();
                    if plvars.contains(&col_lower) {
                        return;
                    }

                    // Chercher dans les sources de chaque niveau, de l'intérieur
                    // vers l'extérieur.
                    let found = ctx
                        .levels()
                        .flat_map(|l| l.values())
                        .filter_map(|v| v.as_deref())
                        .any(|ts| ts.get_column(col_name).is_some());

                    // Ne conclure à l'absence que si toutes les sources sont
                    // connues : une source opaque peut fournir la colonne.
                    let all_known = ctx.levels().flat_map(|l| l.values()).all(|v| v.is_some());
                    let local_sources = ctx.levels().next().is_some_and(|l| !l.is_empty());
                    if !found && all_known && local_sources {
                        // Build a hint listing all available columns across resolved tables.
                        let all_cols: Vec<String> = ctx
                            .levels()
                            .next()
                            .into_iter()
                            .flat_map(|l| l.values())
                            .filter_map(|v| v.as_deref())
                            .flat_map(|ts| ts.columns.iter().map(|c| c.name.clone()))
                            .collect();
                        let hint = if all_cols.is_empty() {
                            None
                        } else {
                            Some(format!("available columns: {}", all_cols.join(", ")))
                        };
                        let mut d = Diagnostic::error(
                            codes::COLUMN_NOT_FOUND,
                            format!("column '{}' does not exist", col_name),
                        );
                        if let Some(h) = hint {
                            d = d.with_hint(h);
                        }
                        diags.push(d);
                    }
                }
            }
        }

        ExprKind::FunctionCall { name, .. } => {
            let func_name = pgweave_schema::provider::object_name_to_string(name);
            // Fonction inconnue du catalogue (intégrées et extensions
            // comprises) : avertissement. Les appels qualifiés sont admis.
            if name.0.len() == 1
                && !crate::is_sql_special_form(&func_name)
                && schema.get_function(&func_name).is_none()
            {
                let mut d = Diagnostic::warning(
                    codes::FUNCTION_NOT_FOUND,
                    format!(
                        "function '{}' is not defined in the known schema",
                        func_name
                    ),
                );
                // Fournie par une extension non activée ? (D9)
                let from = pgweave_catalog::builtin::extensions_providing(
                    pgweave_catalog::builtin::ObjectSort::Routine,
                    &func_name,
                );
                if let Some(ext) = from.first() {
                    let ext_name = ext.split('@').next().unwrap_or(ext);
                    d = d.with_hint(format!(
                        "'{func_name}' is provided by extension {ext}, which is not enabled: add `CREATE EXTENSION {ext_name}`"
                    ));
                }
                diags.push(d);
            }
            // Arguments, ORDER BY d'agrégat, FILTER, WITHIN GROUP, fenêtre :
            // tous évalués dans la portée de la requête.
            for child in expr.children() {
                check_expr(child, ctx, schema, plvars, phantom_qualifiers, diags);
            }
        }

        ExprKind::BinaryOp { left, right, .. } | ExprKind::Quantified { left, right, .. } => {
            check_expr(left, ctx, schema, plvars, phantom_qualifiers, diags);
            check_expr(right, ctx, schema, plvars, phantom_qualifiers, diags);
        }

        ExprKind::UnaryOp { expr, .. } => {
            check_expr(expr, ctx, schema, plvars, phantom_qualifiers, diags);
        }

        ExprKind::Cast { expr, .. } => {
            check_expr(expr, ctx, schema, plvars, phantom_qualifiers, diags);
        }

        ExprKind::Case {
            operand,
            when_clauses,
            else_clause,
        } => {
            if let Some(op) = operand {
                check_expr(op, ctx, schema, plvars, phantom_qualifiers, diags);
            }
            for (when, then) in when_clauses {
                check_expr(when, ctx, schema, plvars, phantom_qualifiers, diags);
                check_expr(then, ctx, schema, plvars, phantom_qualifiers, diags);
            }
            if let Some(el) = else_clause {
                check_expr(el, ctx, schema, plvars, phantom_qualifiers, diags);
            }
        }

        // Sous-requêtes corrélées : la requête courante est leur niveau
        // englobant.
        ExprKind::Subquery(sub) => {
            diags.extend(check_correlated(
                sub,
                ctx,
                schema,
                plvars,
                phantom_qualifiers,
            ));
        }

        ExprKind::InSubquery { expr, subquery, .. } => {
            check_expr(expr, ctx, schema, plvars, phantom_qualifiers, diags);
            diags.extend(check_correlated(
                subquery,
                ctx,
                schema,
                plvars,
                phantom_qualifiers,
            ));
        }

        ExprKind::InList { expr, list, .. } => {
            check_expr(expr, ctx, schema, plvars, phantom_qualifiers, diags);
            for item in list {
                check_expr(item, ctx, schema, plvars, phantom_qualifiers, diags);
            }
        }

        ExprKind::Between {
            expr, low, high, ..
        } => {
            check_expr(expr, ctx, schema, plvars, phantom_qualifiers, diags);
            check_expr(low, ctx, schema, plvars, phantom_qualifiers, diags);
            check_expr(high, ctx, schema, plvars, phantom_qualifiers, diags);
        }

        // Literals and wildcards carry no references to resolve.
        // Descendre dans les constructions ajoutées : une colonne inexistante
        // dans un `EXISTS`, un constructeur de ligne ou un `COLLATE` était
        // invisible tant que ces expressions devenaient `Literal(Null)`.
        ExprKind::Exists { subquery, .. } => {
            diags.extend(check_correlated(
                subquery,
                ctx,
                schema,
                plvars,
                phantom_qualifiers,
            ));
        }
        ExprKind::AtTimeZone {
            timestamp,
            time_zone,
        } => {
            check_expr(timestamp, ctx, schema, plvars, phantom_qualifiers, diags);
            check_expr(time_zone, ctx, schema, plvars, phantom_qualifiers, diags);
        }
        ExprKind::Collate { expr, .. } => {
            check_expr(expr, ctx, schema, plvars, phantom_qualifiers, diags)
        }
        ExprKind::Tuple(items) => {
            for e in items {
                check_expr(e, ctx, schema, plvars, phantom_qualifiers, diags);
            }
        }
        ExprKind::GroupingSets { .. } => {
            for e in expr.children() {
                check_expr(e, ctx, schema, plvars, phantom_qualifiers, diags);
            }
        }
        ExprKind::FieldAccess { root, .. } => {
            check_expr(root, ctx, schema, plvars, phantom_qualifiers, diags)
        }
        ExprKind::NamedArgument { value, .. } => {
            check_expr(value, ctx, schema, plvars, phantom_qualifiers, diags)
        }

        ExprKind::Literal(_)
        | ExprKind::Wildcard
        | ExprKind::QualifiedWildcard(_)
        | ExprKind::Interval(_)
        | ExprKind::Parameter(_)
        | ExprKind::Unsupported(_) => {}
    }
}

/// Vérifie une sous-requête d'expression avec `ctx` pour niveau englobant.
fn check_correlated(
    sub: &SelectStmt,
    ctx: &Scope<'_>,
    schema: &dyn SchemaProvider,
    plvars: &HashSet<String>,
    phantom_qualifiers: &HashSet<String>,
) -> Vec<Diagnostic> {
    // Recopie des sources du niveau courant : la portée chaînée emprunte avec
    // une seule durée de vie, plus simple qu'un empilement d'emprunts imbriqués.
    let relations: HashMap<String, Option<Cow<'_, TableSchema>>> = ctx
        .levels()
        .flat_map(|l| l.iter())
        .map(|(k, v)| (k.clone(), v.as_deref().map(|t| Cow::Owned(t.clone()))))
        .collect();
    let outer = Scope {
        relations,
        outer: None,
    };
    check_select_scoped(
        sub,
        schema,
        plvars,
        phantom_qualifiers,
        Some(&outer),
        &HashMap::new(),
    )
}
