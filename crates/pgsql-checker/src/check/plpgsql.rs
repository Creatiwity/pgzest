use crate::diagnostic::Diagnostic;
use crate::schema::{SchemaProvider, TableSchema};

/// Lightweight best-effort checker for PL/pgSQL function bodies stored as raw
/// strings (`FunctionBody::Raw`).  This is NOT a full PL/pgSQL parser — it
/// scans the body line-by-line and applies heuristics.
///
/// Diagnostic codes used here:
/// - W002  empty body
/// - E001  column referenced via OLD./NEW. does not exist on the trigger table
pub fn check_plpgsql_body(
    body: &str,
    function_name: &str,
    // Optional: name of the table this trigger operates on (None for non-trigger functions).
    trigger_table: Option<&str>,
    schema: &dyn SchemaProvider,
    diags: &mut Vec<Diagnostic>,
) {
    // W002 — empty body.
    if body.trim().is_empty() {
        diags.push(
            Diagnostic::warning(
                "W002",
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
        if contains_dml_keyword(&upper) {
            if let Some(sql_fragment) = extract_sql_fragment(trimmed) {
                match pgsql_parser::parse(sql_fragment) {
                    Ok(stmts) => {
                        let inner = super::check_statements(&stmts, schema);
                        for mut d in inner {
                            d.message =
                                format!("[in {}] {}", function_name, d.message);
                            diags.push(d);
                        }
                    }
                    // Fragment parse failures are expected for multi-line or
                    // PL/pgSQL-variable-containing queries — ignore silently.
                    Err(_) => {}
                }
            }
        }

        // --- OLD.column / NEW.column references in trigger bodies.
        if let Some(ts) = trigger_table_schema {
            check_old_new_refs(trimmed, ts, function_name, diags);
        }
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

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
/// Strips leading assignment targets (e.g. `result :=`), trailing semicolons,
/// and returns the remaining candidate string.
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
    if t.is_empty() {
        None
    } else {
        Some(t)
    }
}

/// Scan a single body line for `OLD.<col>` and `NEW.<col>` references.
/// If a trigger table schema is available, verify those column names exist.
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

            if !col_name.is_empty()
                && trigger_table_schema.get_column(&col_name).is_none()
            {
                diags.push(
                    Diagnostic::error(
                        "E001",
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
