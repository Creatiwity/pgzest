use pgweave_schema::provider::SchemaProvider;
use pgweave_semantic::data_type_to_ts;
use tower_lsp::lsp_types::*;

// ── Cursor context ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub enum ClauseCtx {
    /// Beginning of a statement (nothing typed yet, or after `;`)
    StatementStart,
    /// Inside a SELECT / RETURNING list
    SelectList,
    /// After FROM or JOIN — expect table names
    FromJoin,
    /// Inside WHERE, HAVING, ON, AND/OR
    WhereHaving,
    /// Inside SET (UPDATE)
    SetClause,
    /// ORDER BY / GROUP BY
    OrderGroup,
}

pub struct TableBinding {
    pub alias: String,
    pub table: String,
}

pub struct CursorCtx {
    pub clause: ClauseCtx,
    /// Alias before the "." that immediately precedes the cursor
    pub qualifier: Option<String>,
    /// All FROM/JOIN bindings found in the document
    pub bindings: Vec<TableBinding>,
}

// ── Public entry point ────────────────────────────────────────────────────────

pub fn analyze_cursor(text: &str, cursor_offset: usize) -> CursorCtx {
    let safe_offset = cursor_offset.min(text.len());
    let prefix = &text[..safe_offset];

    let qualifier = detect_qualifier(prefix);
    let clause = detect_clause(prefix);
    let bindings = extract_table_bindings(text);

    CursorCtx {
        clause,
        qualifier,
        bindings,
    }
}

pub fn build_completions(ctx: &CursorCtx, schema: &dyn SchemaProvider) -> Vec<CompletionItem> {
    let mut items = Vec::new();
    let mut seen = std::collections::HashSet::<String>::new();

    match &ctx.clause {
        ClauseCtx::StatementStart => {
            for kw in &[
                "SELECT",
                "INSERT INTO",
                "UPDATE",
                "DELETE FROM",
                "CREATE TABLE",
                "ALTER TABLE",
                "DROP TABLE",
                "WITH",
            ] {
                items.push(kw_item(kw));
            }
        }

        ClauseCtx::FromJoin => {
            for name in schema.all_table_names() {
                // Strip schema prefix from label — "public.users" → "users".
                // The checker's 3-step fallback resolves unqualified names correctly.
                let label = name.split('.').next_back().unwrap_or(name);
                if seen.insert(label.to_string()) {
                    items.push(CompletionItem {
                        label: label.to_string(),
                        kind: Some(CompletionItemKind::CLASS),
                        detail: Some("table".into()),
                        ..Default::default()
                    });
                }
            }
        }

        ClauseCtx::SelectList
        | ClauseCtx::WhereHaving
        | ClauseCtx::SetClause
        | ClauseCtx::OrderGroup => {
            if let Some(alias) = &ctx.qualifier {
                // Qualified "alias." → show only columns of that alias
                for binding in &ctx.bindings {
                    if binding.alias == *alias
                        && let Some(ts) = schema.get_table(&binding.table)
                    {
                        for col in &ts.columns {
                            items.push(col_item(
                                &col.name,
                                data_type_to_ts(&col.data_type),
                                &binding.alias,
                            ));
                        }
                    }
                }
            } else {
                // Unqualified → all columns from referenced tables
                for binding in &ctx.bindings {
                    if let Some(ts) = schema.get_table(&binding.table) {
                        for col in &ts.columns {
                            let ts_type = data_type_to_ts(&col.data_type);
                            // Unqualified name (first binding wins on collision)
                            if seen.insert(col.name.to_lowercase()) {
                                items.push(col_item(&col.name, ts_type, &binding.alias));
                            }
                            // Qualified alias.col (always unique)
                            let qualified = format!("{}.{}", binding.alias, col.name);
                            if seen.insert(qualified.clone()) {
                                items.push(col_item(&qualified, ts_type, &binding.alias));
                            }
                        }
                    }
                }
                // Clause-specific keyword suggestions
                let kws: &[&str] = match ctx.clause {
                    ClauseCtx::SelectList => &["DISTINCT", "AS", "COUNT(*)", "COALESCE", "NULLIF"],
                    ClauseCtx::WhereHaving => &[
                        "AND",
                        "OR",
                        "NOT",
                        "IS NULL",
                        "IS NOT NULL",
                        "IN",
                        "NOT IN",
                        "BETWEEN",
                        "LIKE",
                        "ILIKE",
                        "EXISTS",
                    ],
                    ClauseCtx::OrderGroup => &["ASC", "DESC", "NULLS FIRST", "NULLS LAST"],
                    _ => &[],
                };
                for kw in kws {
                    items.push(kw_item(kw));
                }
            }
        }
    }

    items
}

// ── Clause detection ──────────────────────────────────────────────────────────

fn detect_clause(prefix: &str) -> ClauseCtx {
    // Work only on the last statement (after the last `;`)
    let stmt = prefix.rsplit(';').next().unwrap_or(prefix);
    let upper = stmt.to_uppercase();
    let bytes = upper.as_bytes();

    let is_word_boundary = |pos: usize, len: usize| -> bool {
        let before =
            pos == 0 || (!bytes[pos - 1].is_ascii_alphanumeric() && bytes[pos - 1] != b'_');
        let after_pos = pos + len;
        let after = after_pos >= bytes.len()
            || (!bytes[after_pos].is_ascii_alphanumeric() && bytes[after_pos] != b'_');
        before && after
    };

    // Pairs of (keyword, context) ordered by priority — last occurrence wins
    let candidates: &[(&str, ClauseCtx)] = &[
        ("SELECT", ClauseCtx::SelectList),
        ("DISTINCT", ClauseCtx::SelectList),
        ("RETURNING", ClauseCtx::SelectList),
        ("FROM", ClauseCtx::FromJoin),
        ("JOIN", ClauseCtx::FromJoin),
        ("WHERE", ClauseCtx::WhereHaving),
        ("HAVING", ClauseCtx::WhereHaving),
        ("AND", ClauseCtx::WhereHaving),
        ("OR", ClauseCtx::WhereHaving),
        ("ON", ClauseCtx::WhereHaving),
        ("SET", ClauseCtx::SetClause),
        ("GROUP", ClauseCtx::OrderGroup),
        ("ORDER", ClauseCtx::OrderGroup),
    ];

    let mut best_pos: Option<usize> = None;
    let mut best_ctx = ClauseCtx::StatementStart;

    for (kw, ctx) in candidates {
        let klen = kw.len();
        if upper.len() < klen {
            continue;
        }
        // Scan from the right for the rightmost word-boundary match
        let mut pos = upper.len() - klen;
        loop {
            if upper[pos..pos + klen] == **kw && is_word_boundary(pos, klen) {
                if best_pos.is_none_or(|p| pos > p) {
                    best_pos = Some(pos);
                    best_ctx = ctx.clone();
                }
                break;
            }
            if pos == 0 {
                break;
            }
            pos -= 1;
        }
    }

    best_ctx
}

// ── Qualifier detection ───────────────────────────────────────────────────────

fn detect_qualifier(prefix: &str) -> Option<String> {
    // Check if the text right before the cursor looks like "alias."
    // (allowing optional trailing spaces between alias and dot, but not before cursor)
    let trimmed = prefix.trim_end_matches([' ', '\t']);
    if !trimmed.ends_with('.') {
        return None;
    }
    let before_dot = &trimmed[..trimmed.len() - 1];
    let start = before_dot
        .rfind(|c: char| !c.is_alphanumeric() && c != '_')
        .map(|i| i + 1)
        .unwrap_or(0);
    let alias = &before_dot[start..];
    if alias.is_empty() {
        None
    } else {
        Some(alias.to_lowercase())
    }
}

// ── Table binding extraction ──────────────────────────────────────────────────

/// Scan the document for `FROM table [AS alias]` and `JOIN table [AS alias]`,
/// returning all resolved alias → table_name pairs.
pub fn extract_table_bindings(text: &str) -> Vec<TableBinding> {
    let mut bindings = Vec::new();
    let upper = text.to_uppercase();

    for trigger in &["FROM", "JOIN"] {
        let tlen = trigger.len();
        let mut search_from = 0;
        while search_from + tlen <= upper.len() {
            let Some(rel) = upper[search_from..].find(trigger) else {
                break;
            };
            let pos = search_from + rel;
            search_from = pos + 1;

            if !is_word_boundary_at(&upper, pos, tlen) {
                continue;
            }

            let mut i = pos + tlen;
            // Skip whitespace
            i = skip_ws(text, i);

            // Skip leading `(` for subqueries — ignore that token
            if i < text.len() && text.as_bytes()[i] == b'(' {
                continue;
            }

            // Read table name (may be schema-qualified: schema.table)
            let table_start = i;
            while i < text.len() && is_ident_char(text.as_bytes()[i]) {
                i += 1;
            }
            let table_name = text[table_start..i].to_lowercase();
            if table_name.is_empty() || is_sql_keyword(&table_name) {
                continue;
            }

            i = skip_ws(text, i);

            // Determine alias
            let alias = if i + 3 <= text.len() && text[i..].to_uppercase().starts_with("AS ") {
                i += 3;
                i = skip_ws(text, i);
                let a_start = i;
                while i < text.len() && is_ident_char(text.as_bytes()[i]) {
                    i += 1;
                }
                text[a_start..i].to_lowercase()
            } else {
                // Implicit alias: next word if it's not a SQL keyword
                let a_start = i;
                while i < text.len() && is_ident_char(text.as_bytes()[i]) {
                    i += 1;
                }
                let next = text[a_start..i].to_lowercase();
                if !next.is_empty() && !is_sql_keyword(&next) {
                    next
                } else {
                    // Fall back to the table name (without schema prefix)
                    table_name
                        .rsplit('.')
                        .next()
                        .unwrap_or(&table_name)
                        .to_string()
                }
            };

            bindings.push(TableBinding {
                alias,
                table: table_name,
            });
        }
    }

    bindings
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn is_word_boundary_at(upper: &str, pos: usize, len: usize) -> bool {
    let bytes = upper.as_bytes();
    let before = pos == 0 || (!bytes[pos - 1].is_ascii_alphanumeric() && bytes[pos - 1] != b'_');
    let after_pos = pos + len;
    let after = after_pos >= bytes.len()
        || (!bytes[after_pos].is_ascii_alphanumeric() && bytes[after_pos] != b'_');
    before && after
}

fn skip_ws(text: &str, mut i: usize) -> usize {
    let bytes = text.as_bytes();
    while i < text.len() && matches!(bytes[i], b' ' | b'\t' | b'\n' | b'\r') {
        i += 1;
    }
    i
}

fn is_ident_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'.'
}

fn is_sql_keyword(s: &str) -> bool {
    matches!(
        s.to_uppercase().as_str(),
        "WHERE"
            | "ON"
            | "AND"
            | "OR"
            | "NOT"
            | "GROUP"
            | "ORDER"
            | "HAVING"
            | "LIMIT"
            | "OFFSET"
            | "INNER"
            | "LEFT"
            | "RIGHT"
            | "FULL"
            | "CROSS"
            | "SET"
            | "RETURNING"
            | "INTO"
            | "VALUES"
            | "SELECT"
            | "INSERT"
            | "UPDATE"
            | "DELETE"
            | "CREATE"
            | "ALTER"
            | "DROP"
            | "WITH"
            | "AS"
            | "JOIN"
            | "FROM"
            | "DISTINCT"
            | "ALL"
            | "UNION"
            | "EXCEPT"
            | "INTERSECT"
            | "LATERAL"
            | "NATURAL"
            | "USING"
    )
}

fn kw_item(label: &str) -> CompletionItem {
    CompletionItem {
        label: label.to_string(),
        kind: Some(CompletionItemKind::KEYWORD),
        ..Default::default()
    }
}

fn col_item(label: &str, type_detail: &'static str, from_alias: &str) -> CompletionItem {
    CompletionItem {
        label: label.to_string(),
        kind: Some(CompletionItemKind::FIELD),
        detail: Some(type_detail.to_string()),
        documentation: Some(Documentation::String(format!("from {}", from_alias))),
        ..Default::default()
    }
}
