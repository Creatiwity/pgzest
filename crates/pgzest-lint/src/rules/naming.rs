/// L003 — Table or column name is not snake_case.
///
/// PostgreSQL folds unquoted identifiers to lower-case. Quoted mixed-case
/// names are error-prone and break convention. Enforce snake_case throughout.
use pgzest_ast::ddl::DdlStatement;
use pgzest_ast::expr::Ident;
use pgzest_ast::stmt::Statement;
use pgzest_diagnostics::Diagnostic;

use crate::config::{LintConfig, RuleLevel};

pub const CODE: &str = "L003";

/// Returns `true` when the identifier is valid snake_case:
/// only lowercase ASCII letters, digits, and underscores, and not starting
/// with a digit.
fn is_snake_case(name: &str) -> bool {
    if name.is_empty() {
        return true;
    }
    let mut chars = name.chars();
    // First char: letter or underscore
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

fn diag_for_ident(ident: &Ident, context: &str, level: &RuleLevel) -> Option<Diagnostic> {
    // Quoted identifiers that contain upper-case are suspicious; unquoted
    // identifiers are folded by PG but we still want to enforce convention.
    if is_snake_case(&ident.value) {
        return None;
    }
    let msg = format!("{context} `{}` is not snake_case", ident.value);
    let d = match level {
        RuleLevel::Error => Diagnostic::error(CODE, msg),
        _ => Diagnostic::warning(CODE, msg),
    }
    .with_hint(
        "Rename to snake_case (e.g. `my_table`) to follow PostgreSQL conventions \
         and avoid quoting requirements.",
    );
    Some(d)
}

pub fn check(stmt: &Statement, config: &LintConfig) -> Vec<Diagnostic> {
    if config.rules.naming_snake_case == RuleLevel::Off {
        return vec![];
    }

    let level = &config.rules.naming_snake_case;
    let mut diags = vec![];

    let Statement::Ddl(ddl) = stmt else {
        return vec![];
    };

    match ddl {
        DdlStatement::CreateTable(ct) => {
            // Check table name
            for part in &ct.name.0 {
                if let Some(d) = diag_for_ident(part, "Table", level) {
                    diags.push(d);
                }
            }
            // Check column names
            for col in &ct.columns {
                if let Some(d) = diag_for_ident(&col.name, "Column", level) {
                    diags.push(d);
                }
            }
        }
        DdlStatement::CreateIndex(ci) => {
            if let Some(ref name) = ci.name
                && let Some(d) = diag_for_ident(name, "Index", level)
            {
                diags.push(d);
            }
            for part in &ci.table.0 {
                if let Some(d) = diag_for_ident(part, "Table", level) {
                    diags.push(d);
                }
            }
        }
        DdlStatement::CreateFunction(cf) => {
            for part in &cf.name.0 {
                if let Some(d) = diag_for_ident(part, "Function", level) {
                    diags.push(d);
                }
            }
        }
        DdlStatement::CreateProcedure(cp) => {
            for part in &cp.name.0 {
                if let Some(d) = diag_for_ident(part, "Procedure", level) {
                    diags.push(d);
                }
            }
        }
        _ => {}
    }

    diags
}
