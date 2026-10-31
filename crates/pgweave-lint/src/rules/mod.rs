pub mod add_column_not_null_no_default;
pub mod aggregate_without_group_by;
pub mod missing_fk_index;
pub mod naming;
pub mod no_leading_wildcard;
pub mod no_not_in;
pub mod no_select_star;
pub mod require_pk;
pub mod unused_join;

use pgweave_ast::stmt::{Statement, stmt_span};
use pgweave_diagnostics::Diagnostic;

use crate::config::LintConfig;

/// Convertit un span AST (u32) en span de diagnostic (usize).
fn to_diag_span(span: pgweave_ast::Span) -> pgweave_diagnostics::Span {
    pgweave_diagnostics::Span {
        start: span.start as usize,
        end: span.end as usize,
    }
}

/// Attache le span de `stmt` aux diagnostics qui n'en ont pas.
///
/// Les règles n'ont pas à s'en occuper : elles inspectent des nœuds qui, pour
/// la plupart (`Expr`, `Ident`, `ColumnRef`), ne portent aucun span. Sans ça
/// tous les diagnostics de lint se rapportaient en `1:1`, ce qui les rendait
/// inexploitables dans l'éditeur et impossibles à cibler avec une directive
/// `-- pgweave-disable-next-line`.
///
/// Un span déjà présent est conservé : une règle capable d'être plus précise
/// que le statement garde sa précision.
fn attach_span(diags: &mut [Diagnostic], stmt: &Statement) {
    let Some(span) = stmt_span(stmt) else { return };
    let diag_span = to_diag_span(span);
    for d in diags.iter_mut() {
        if d.span.is_none() {
            d.span = Some(diag_span.clone());
        }
    }
}

/// Run all enabled rules against a single statement and collect diagnostics.
pub fn check_statement(stmt: &Statement, config: &LintConfig) -> Vec<Diagnostic> {
    let mut diags = vec![];
    diags.extend(no_select_star::check(stmt, config));
    diags.extend(require_pk::check(stmt, config));
    diags.extend(naming::check(stmt, config));
    diags.extend(no_not_in::check(stmt, config));
    diags.extend(no_leading_wildcard::check(stmt, config));
    diags.extend(aggregate_without_group_by::check(stmt, config));
    diags.extend(add_column_not_null_no_default::check(stmt, config));
    diags.extend(unused_join::check(stmt, config));
    attach_span(&mut diags, stmt);
    diags
}

/// Run rules that need cross-statement context (e.g. FK index check).
pub fn check_all_statements(stmts: &[Statement], config: &LintConfig) -> Vec<Diagnostic> {
    missing_fk_index::check_all(stmts, config)
}
