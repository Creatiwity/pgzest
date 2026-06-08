mod convert;
pub mod error;
pub mod normalize;
pub mod plpgsql;

use std::sync::atomic::{AtomicU32, Ordering};

static NODE_COUNTER: AtomicU32 = AtomicU32::new(1);

pub(crate) fn next_node_id() -> pgsql_ast::NodeId {
    pgsql_ast::NodeId(NODE_COUNTER.fetch_add(1, Ordering::Relaxed))
}

pub use error::ParseError;
pub use normalize::{NormalizedSelect, normalize_select};

use pgsql_ast::Statement;
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::parser::Parser as SqlParser;

/// Metadata extracted from a CREATE FUNCTION/PROCEDURE statement before
/// handing the SQL off to sqlparser, which does not understand these tokens.
#[derive(Debug, Default)]
struct FunctionMeta {
    security_definer: bool,
    cost: Option<u64>,
    rows: Option<u64>,
    /// If the original return type was `SETOF <type>`, stores the inner type string.
    setof_type: Option<String>,
    /// If the original return type was `TABLE(col type, ...)`, stores the parsed columns.
    table_columns: Option<Vec<(String, String)>>, // (col_name, type_name)
    /// True if the original statement was CREATE PROCEDURE (rewritten as CREATE FUNCTION).
    is_procedure: bool,
}

/// Pre-process a single SQL statement string:
/// - detects SECURITY DEFINER / SECURITY INVOKER
/// - detects COST n / ROWS n
/// - strips those tokens so sqlparser does not choke
///
/// Returns the cleaned SQL and the extracted metadata.
fn preprocess_function_sql(sql: &str) -> (String, FunctionMeta) {
    let mut meta = FunctionMeta::default();

    // We only need to scan the portion *outside* the body delimiters ($$...$$).
    // Strategy: split on the first $$ to get the "header" portion, process it,
    // then re-join. This avoids accidental matches inside the body string.

    // Find the first occurrence of $$ (or $tag$) and work only in the header.
    // For simplicity we scan tokens in a case-insensitive manner before the first $$.

    let header_end = find_body_start(sql);
    let header = &sql[..header_end];
    let body_and_rest = &sql[header_end..];

    // sqlparser 0.54 does not support CREATE PROCEDURE with a PL/pgSQL body.
    // Workaround: rewrite CREATE [OR REPLACE] PROCEDURE ... to
    // CREATE OR REPLACE FUNCTION ... RETURNS VOID so sqlparser can parse it.
    // The convert layer will produce a CreateProcedure by detecting the rewrite flag.
    let (header, body_and_rest, is_procedure) =
        rewrite_procedure_as_function(header, body_and_rest);
    meta.is_procedure = is_procedure;

    // Detect and rewrite `RETURNS TABLE(col type, ...)` → `RETURNS VOID`
    // Must happen before strip_function_options so the column info is captured.
    let header = rewrite_returns_table(&header, &mut meta);

    let cleaned_header = strip_function_options(&header, &mut meta);
    (format!("{}{}", cleaned_header, body_and_rest), meta)
}

/// Rewrite `CREATE [OR REPLACE] PROCEDURE name(...) LANGUAGE plpgsql AS $$...$$`
/// to `CREATE OR REPLACE FUNCTION name(...) RETURNS VOID LANGUAGE plpgsql AS $$...$$`
/// so that sqlparser (which can't parse PL/pgSQL procedure bodies) can handle it.
///
/// Returns `(new_header, new_body_and_rest, was_procedure)`.
fn rewrite_procedure_as_function(
    header: &str,
    body_and_rest: &str,
) -> (String, String, bool) {
    let upper = header.to_uppercase();

    // Only rewrite PROCEDURE statements — check outside comment sections so that
    // a comment like "-- helper for procedures" doesn't trigger a false rewrite.
    let proc_pos = match find_keyword_outside_comment(upper.as_bytes(), b"PROCEDURE") {
        Some(p) => p,
        None => return (header.to_string(), body_and_rest.to_string(), false),
    };

    // Replace PROCEDURE keyword with FUNCTION and inject RETURNS VOID before LANGUAGE.
    // Strategy:
    //   1. Find "PROCEDURE" in the header and replace with "FUNCTION "
    //   2. Handle OR REPLACE: ensure "CREATE OR REPLACE FUNCTION"
    //   3. Insert "RETURNS VOID " before LANGUAGE (or before AS if no LANGUAGE in header)
    let mut new_header = String::new();
    new_header.push_str(&header[..proc_pos]);
    new_header.push_str("FUNCTION ");
    new_header.push_str(&header[proc_pos + 9..]); // len("PROCEDURE") = 9

    // Step 2: Ensure OR REPLACE is present (sqlparser needs it for CREATE OR REPLACE FUNCTION)
    let new_upper = new_header.to_uppercase();
    let new_header = if !new_upper.contains("OR REPLACE") {
        new_header.replacen("CREATE ", "CREATE OR REPLACE ", 1)
    } else {
        new_header
    };

    // Step 3: Insert RETURNS VOID before LANGUAGE (or before AS $$ if no LANGUAGE)
    let nu = new_header.to_uppercase();
    let new_header = if let Some(lang_pos) = nu.rfind("LANGUAGE") {
        format!("{} RETURNS VOID {}", &new_header[..lang_pos].trim_end(), &new_header[lang_pos..])
    } else {
        // No LANGUAGE in header — body_and_rest starts with $$
        // Insert RETURNS VOID at end of header
        format!("{} RETURNS VOID ", new_header.trim_end())
    };

    (new_header, body_and_rest.to_string(), true)
}

/// Detect `RETURNS TABLE(col1 type1, col2 type2, ...)` in a function header.
/// If found, stores the parsed columns in `meta.table_columns` and rewrites the
/// header so that `RETURNS TABLE(...)` becomes `RETURNS VOID`, which sqlparser
/// can parse without issues.
fn rewrite_returns_table(header: &str, meta: &mut FunctionMeta) -> String {
    let upper = header.to_uppercase();
    let upper_bytes = upper.as_bytes();
    let len = header.len();

    let mut i = 0;
    while i < len {
        if starts_with_word(upper_bytes, i, b"RETURNS") {
            let after_returns = i + 7;
            if let Some(j) = skip_spaces(upper_bytes, after_returns) {
                if starts_with_word(upper_bytes, j, b"TABLE") {
                    let after_table = j + 5;
                    if let Some(paren_start) = skip_spaces(upper_bytes, after_table) {
                        if paren_start < len && header.as_bytes()[paren_start] == b'(' {
                            // Find matching closing paren
                            let mut depth = 0usize;
                            let mut k = paren_start;
                            let hbytes = header.as_bytes();
                            while k < len {
                                if hbytes[k] == b'(' { depth += 1; }
                                else if hbytes[k] == b')' {
                                    depth -= 1;
                                    if depth == 0 { break; }
                                }
                                k += 1;
                            }
                            let paren_end = k; // index of ')'
                            // Extract and parse the column list
                            let col_list = &header[paren_start + 1..paren_end];
                            let cols = parse_table_columns(col_list);
                            meta.table_columns = Some(cols);
                            // Rewrite: keep everything before RETURNS, add RETURNS VOID,
                            // then keep everything after the closing paren.
                            let before = &header[..i];
                            let after = &header[paren_end + 1..];
                            return format!("{}RETURNS VOID{}", before, after);
                        }
                    }
                }
            }
        }
        i += 1;
    }
    header.to_string()
}

/// Parse a `col1 type1, col2 type2, ...` column list from a RETURNS TABLE(...) clause.
/// Returns a Vec of (column_name, type_name) pairs.
fn parse_table_columns(col_list: &str) -> Vec<(String, String)> {
    let mut result = Vec::new();
    // Split by commas (simple approach — doesn't handle nested types with commas,
    // but sufficient for common PostgreSQL scalar types).
    for part in col_list.split(',') {
        let trimmed = part.trim();
        if trimmed.is_empty() {
            continue;
        }
        // Each part is "col_name type_name [...]"
        // Split on first whitespace to get name and type.
        let mut words = trimmed.splitn(2, |c: char| c.is_ascii_whitespace());
        if let (Some(col_name), Some(type_part)) = (words.next(), words.next()) {
            let col_name = col_name.trim().to_string();
            // The type might have qualifiers; take the first word as the base type.
            let type_name = type_part.trim().split_whitespace().next()
                .unwrap_or(type_part.trim())
                .to_string();
            if !col_name.is_empty() {
                result.push((col_name, type_name));
            }
        }
    }
    result
}

/// Returns the byte offset of the first `$$` or `$tag$` delimiter in `sql`,
/// or `sql.len()` if none found.
fn find_body_start(sql: &str) -> usize {
    let bytes = sql.as_bytes();
    let len = sql.len();
    let mut i = 0;
    while i < len {
        if bytes[i] == b'$' {
            // Scan forward to find the closing $
            let mut j = i + 1;
            while j < len && bytes[j] != b'$' {
                j += 1;
            }
            if j < len && bytes[j] == b'$' {
                // Found a dollar-quote delimiter at [i..=j]
                return i;
            }
        }
        i += 1;
    }
    len
}

/// Strip SECURITY DEFINER/INVOKER, COST n, ROWS n, and RETURNS SETOF <type> from a
/// header string. Fills `meta` with the detected values and replaces tokens with
/// spaces so the SQL remains valid for sqlparser.
///
/// For `RETURNS SETOF <type>`: rewrites to `RETURNS <type>` so sqlparser can parse
/// the return type normally, and records that it was a SETOF in meta.setof_type.
fn strip_function_options(header: &str, meta: &mut FunctionMeta) -> String {
    // Work on an uppercase copy for matching, but build output from the original.
    let upper = header.to_uppercase();
    let bytes = header.as_bytes();
    let len = header.len();

    // We build a list of byte ranges to blank out (replace with spaces).
    let mut blank_ranges: Vec<(usize, usize)> = Vec::new();

    let mut i = 0;
    let upper_bytes = upper.as_bytes();
    while i < len {
        // Detect `RETURNS SETOF <type>` — rewrite to `RETURNS <type>` by blanking
        // only the "SETOF " part.
        if starts_with_word(upper_bytes, i, b"RETURNS") {
            let after_returns = i + 7; // len("RETURNS")
            if let Some(j) = skip_spaces(upper_bytes, after_returns) {
                if starts_with_word(upper_bytes, j, b"SETOF") {
                    let after_setof = j + 5; // len("SETOF")
                    // Find the inner type name (skip spaces after SETOF)
                    if let Some(type_start) = skip_spaces(upper_bytes, after_setof) {
                        // Collect the type name (word chars until whitespace)
                        let type_end = {
                            let mut k = type_start;
                            while k < len
                                && (upper_bytes[k].is_ascii_alphanumeric()
                                    || upper_bytes[k] == b'_')
                            {
                                k += 1;
                            }
                            k
                        };
                        let inner_type = &header[type_start..type_end];
                        meta.setof_type = Some(inner_type.to_string());
                        // Blank out just "SETOF " (from j to type_start), keeping the type name
                        blank_ranges.push((j, type_start));
                        i = type_end;
                        continue;
                    } else {
                        // SETOF with no type — blank it and record as "record"
                        meta.setof_type = Some("record".to_string());
                        blank_ranges.push((j, after_setof));
                        i = after_setof;
                        continue;
                    }
                }
            }
        }

        // Try to match "SECURITY" at position i.
        if starts_with_word(upper_bytes, i, b"SECURITY") {
            let after_sec = i + 8; // len("SECURITY")
            if let Some(j) = skip_spaces(upper_bytes, after_sec) {
                if starts_with_word(upper_bytes, j, b"DEFINER") {
                    meta.security_definer = true;
                    blank_ranges.push((i, j + 7));
                    i = j + 7;
                    continue;
                } else if starts_with_word(upper_bytes, j, b"INVOKER") {
                    // SECURITY INVOKER is the default — just strip it
                    blank_ranges.push((i, j + 7));
                    i = j + 7;
                    continue;
                }
            }
        }
        // Try to match "COST" followed by whitespace and a number.
        if starts_with_word(upper_bytes, i, b"COST") {
            let after = i + 4;
            if let Some(j) = skip_spaces(upper_bytes, after) {
                if let Some((n, end)) = parse_u64(bytes, j) {
                    meta.cost = Some(n);
                    blank_ranges.push((i, end));
                    i = end;
                    continue;
                }
            }
        }
        // Try to match "ROWS" followed by whitespace and a number.
        if starts_with_word(upper_bytes, i, b"ROWS") {
            let after = i + 4;
            if let Some(j) = skip_spaces(upper_bytes, after) {
                if let Some((n, end)) = parse_u64(bytes, j) {
                    meta.rows = Some(n);
                    blank_ranges.push((i, end));
                    i = end;
                    continue;
                }
            }
        }
        i += 1;
    }

    if blank_ranges.is_empty() {
        return header.to_string();
    }

    // Build output with blanked ranges replaced by spaces.
    let mut out = header.to_string().into_bytes();
    for (start, end) in blank_ranges {
        for b in &mut out[start..end] {
            *b = b' ';
        }
    }
    String::from_utf8(out).unwrap_or_else(|_| header.to_string())
}

/// Returns true if `buf[pos..]` starts with `needle` and the character
/// immediately after `needle` (if any) is not alphanumeric or underscore.
fn starts_with_word(buf: &[u8], pos: usize, needle: &[u8]) -> bool {
    let end = pos + needle.len();
    if end > buf.len() {
        return false;
    }
    if &buf[pos..end] != needle {
        return false;
    }
    // Check word boundary after needle
    if end < buf.len() {
        let c = buf[end];
        if c.is_ascii_alphanumeric() || c == b'_' {
            return false;
        }
    }
    true
}

/// Skip ASCII whitespace starting at `pos`. Returns `Some(first_non_space)`
/// or `None` if end of slice reached.
fn skip_spaces(buf: &[u8], pos: usize) -> Option<usize> {
    let mut i = pos;
    while i < buf.len() {
        if buf[i] == b' ' || buf[i] == b'\t' || buf[i] == b'\n' || buf[i] == b'\r' {
            i += 1;
        } else {
            return Some(i);
        }
    }
    None
}

/// Parse an ASCII decimal integer starting at `pos` in `buf`.
/// Returns `Some((value, end_pos))` where `end_pos` is the index after the last digit.
fn parse_u64(buf: &[u8], pos: usize) -> Option<(u64, usize)> {
    let mut i = pos;
    if i >= buf.len() || !buf[i].is_ascii_digit() {
        return None;
    }
    let mut n: u64 = 0;
    while i < buf.len() && buf[i].is_ascii_digit() {
        n = n.saturating_mul(10).saturating_add((buf[i] - b'0') as u64);
        i += 1;
    }
    Some((n, i))
}

/// Strip `--` line comments from SQL text.
/// For each line, everything from the first `--` to end-of-line is removed.
/// This avoids false keyword matches inside comments.
fn strip_line_comments(sql: &str) -> String {
    sql.lines()
        .map(|line| {
            // Naive scan: find -- not inside single-quoted strings.
            let bytes = line.as_bytes();
            let mut in_quote = false;
            let mut i = 0;
            while i < bytes.len() {
                match bytes[i] {
                    b'\'' => in_quote = !in_quote,
                    b'-' if !in_quote && i + 1 < bytes.len() && bytes[i + 1] == b'-' => {
                        return &line[..i];
                    }
                    _ => {}
                }
                i += 1;
            }
            line
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Find the byte position of `keyword` (already uppercased bytes) in `text_upper`,
/// skipping `--` line comment sections. Returns `None` if not found outside a comment.
fn find_keyword_outside_comment(text_upper: &[u8], keyword: &[u8]) -> Option<usize> {
    let mut i = 0;
    while i < text_upper.len() {
        // Skip line comment
        if i + 1 < text_upper.len() && text_upper[i] == b'-' && text_upper[i + 1] == b'-' {
            while i < text_upper.len() && text_upper[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if starts_with_word(text_upper, i, keyword) {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Returns true if the SQL statement (uppercased, trimmed) looks like a
/// CREATE FUNCTION or CREATE OR REPLACE FUNCTION / PROCEDURE statement.
/// Leading `--` comment lines are skipped so that comments attached to a
/// function definition don't prevent detection.
fn is_create_function_sql(sql: &str) -> bool {
    // Strip comments before any keyword matching so that e.g.
    // "-- triggers/procedures..." above a CREATE EXTENSION doesn't trigger a
    // false positive.
    let no_comments = strip_line_comments(sql);

    let first_non_empty = no_comments
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty());

    let s = match first_non_empty {
        Some(l) => l.to_uppercase(),
        None => return false,
    };

    if !s.starts_with("CREATE") {
        return false;
    }
    // Check comment-stripped SQL for FUNCTION/PROCEDURE keywords.
    let full_upper = no_comments.to_uppercase();
    full_upper.contains("FUNCTION") || full_upper.contains("PROCEDURE")
}

/// Replace each `CHECK (...)` body in a CREATE TABLE statement with `CHECK (true)`.
///
/// This allows pg_dump output to parse correctly even when the CHECK expression
/// uses PostgreSQL-specific syntax not yet supported by our expression converter
/// (e.g. `TRIM(BOTH FROM x)`, `ARRAY[...]`, `= ANY(...)`).
/// The CHECK expression is irrelevant for schema information extraction.
fn neutralize_check_constraints(sql: &str) -> String {
    // All scanning uses byte indices over ASCII characters (parens, quotes, 'C','H','E','C','K').
    // Non-ASCII bytes always have the high bit set, so they are never mistaken for ASCII
    // control characters.  We copy through non-ASCII content verbatim by slicing `sql`.
    let bytes = sql.as_bytes();
    let len = bytes.len();
    let mut result = String::with_capacity(len);
    let mut i = 0;

    // Copy a byte-range from `sql` (guaranteed valid UTF-8 slice) into result.
    macro_rules! copy_range {
        ($start:expr, $end:expr) => {
            result.push_str(&sql[$start..$end]);
        };
    }

    while i < len {
        // Look for the ASCII word "CHECK" (case-insensitive via byte comparison).
        if i + 5 <= len {
            let b = &bytes[i..i + 5];
            if (b[0] == b'C' || b[0] == b'c')
                && (b[1] == b'H' || b[1] == b'h')
                && (b[2] == b'E' || b[2] == b'e')
                && (b[3] == b'C' || b[3] == b'c')
                && (b[4] == b'K' || b[4] == b'k')
            {
                let before_ok = i == 0
                    || (!bytes[i - 1].is_ascii_alphanumeric() && bytes[i - 1] != b'_');
                let after_ok = i + 5 >= len
                    || (!bytes[i + 5].is_ascii_alphanumeric() && bytes[i + 5] != b'_');
                if before_ok && after_ok {
                    // Emit "CHECK"
                    copy_range!(i, i + 5);
                    i += 5;
                    // Copy any whitespace between CHECK and '('
                    let ws_start = i;
                    while i < len && bytes[i].is_ascii_whitespace() {
                        i += 1;
                    }
                    copy_range!(ws_start, i);
                    // Scan a balanced paren group starting at '('
                    if i < len && bytes[i] == b'(' {
                        let paren_start = i;
                        let mut depth = 0i32;
                        while i < len {
                            match bytes[i] {
                                b'(' => { depth += 1; i += 1; }
                                b')' => {
                                    depth -= 1;
                                    i += 1;
                                    if depth <= 0 { break; }
                                }
                                b'\'' => {
                                    // Skip single-quoted string (handles '' escaping)
                                    i += 1;
                                    while i < len {
                                        if bytes[i] == b'\'' {
                                            i += 1;
                                            if i < len && bytes[i] == b'\'' {
                                                i += 1;
                                                continue;
                                            }
                                            break;
                                        }
                                        i += 1;
                                    }
                                }
                                _ => { i += 1; }
                            }
                        }
                        let body = &sql[paren_start..i];
                        let inner = body
                            .trim_start_matches('(')
                            .trim_end_matches(')')
                            .trim();
                        if inner.eq_ignore_ascii_case("true")
                            || inner.eq_ignore_ascii_case("false")
                        {
                            result.push_str(body);
                        } else {
                            result.push_str("(true)");
                        }
                    }
                    continue;
                }
            }
        }
        // Advance by one UTF-8 char (so we never split a multi-byte sequence).
        let ch_len = len - i; // remaining bytes
        let step = if bytes[i] < 0x80 {
            1
        } else if bytes[i] < 0xE0 {
            2usize.min(ch_len)
        } else if bytes[i] < 0xF0 {
            3usize.min(ch_len)
        } else {
            4usize.min(ch_len)
        };
        copy_range!(i, i + step);
        i += step;
    }
    result
}

pub fn parse(sql: &str) -> Result<Vec<Statement>, ParseError> {
    let dialect = PostgreSqlDialect {};

    // Split into individual statement strings (respecting dollar-quotes).
    // Parse each statement separately so that a single unsupported statement
    // (e.g. CREATE SEQUENCE with multiple options, CREATE EXTENSION, GRANT, …)
    // does not abort the entire parse — it is silently skipped instead.
    let raw_stmts = split_statements(sql);
    let mut result = Vec::new();

    for (stmt_sql, byte_offset) in &raw_stmts {
        let (cleaned, meta) = if is_create_function_sql(stmt_sql) {
            let (c, m) = preprocess_function_sql(stmt_sql);
            (c, Some(m))
        } else {
            // Neutralize complex CHECK constraint bodies so pg_dump schema files
            // parse correctly regardless of the expression syntax used.
            (neutralize_check_constraints(stmt_sql), None)
        };

        // Parse single statement; skip on syntax error (unsupported SQL).
        let sp_stmts = match SqlParser::parse_sql(&dialect, &cleaned) {
            Ok(s) => s,
            Err(_) => continue,
        };

        for sp_stmt in sp_stmts {
            let mut stmt = match convert::convert_statement(sp_stmt) {
                Ok(s) => s,
                Err(ParseError::Unsupported(_)) => continue,
                Err(e) => return Err(e),
            };

            if let Some(ref meta) = meta {
                if let Statement::Ddl(pgsql_ast::ddl::DdlStatement::CreateFunction(ref mut cf)) =
                    stmt
                {
                    cf.security_definer = meta.security_definer;
                    cf.cost = meta.cost;
                    cf.rows = meta.rows;
                    if let Some(ref inner_type_str) = meta.setof_type {
                        use pgsql_ast::ddl::ReturnType;
                        cf.return_type =
                            ReturnType::SetOf(convert::parse_simple_type_pub(inner_type_str));
                    }
                    if let Some(ref cols) = meta.table_columns {
                        use pgsql_ast::ddl::ReturnType;
                        use pgsql_ast::expr::Ident;
                        let table_cols: Vec<(Ident, pgsql_ast::types::DataType)> = cols
                            .iter()
                            .map(|(col_name, type_name)| {
                                let ident = Ident { value: col_name.clone(), quoted: false };
                                let dt = convert::parse_simple_type_pub(type_name);
                                (ident, dt)
                            })
                            .collect();
                        cf.return_type = ReturnType::Table(table_cols);
                    }
                }

                if meta.is_procedure {
                    if let Statement::Ddl(pgsql_ast::ddl::DdlStatement::CreateFunction(cf)) = stmt
                    {
                        use pgsql_ast::ddl::CreateProcedure;
                        let proc = CreateProcedure {
                            id: cf.id,
                            span: cf.span,
                            name: cf.name,
                            or_replace: cf.or_replace,
                            params: cf.params,
                            language: cf.language,
                            body: cf.body,
                        };
                        stmt =
                            Statement::Ddl(pgsql_ast::ddl::DdlStatement::CreateProcedure(proc));
                    }
                }
            }
            set_stmt_span(&mut stmt, *byte_offset, byte_offset + stmt_sql.len());
            result.push(stmt);
        }
    }
    Ok(result)
}

fn set_stmt_span(stmt: &mut Statement, start: usize, end: usize) {
    use pgsql_ast::{DdlStatement, Statement::*};
    let span = pgsql_ast::node::Span { start: start as u32, end: end as u32 };
    match stmt {
        Select(s) => s.span = span,
        Insert(s) => s.span = span,
        Update(s) => s.span = span,
        Delete(s) => s.span = span,
        Ddl(ddl) => match ddl {
            DdlStatement::CreateFunction(cf) => cf.span = span,
            DdlStatement::CreateProcedure(cp) => cp.span = span,
            DdlStatement::CreatePolicy(p) => p.span = span,
            DdlStatement::CreateTable(ct) => ct.span = span,
            _ => {}
        },
    }
}

/// Split SQL into individual statement strings by ';', respecting dollar-quoted strings.
/// The returned tuples are (statement_text, byte_offset) where byte_offset is the
/// position of the first non-whitespace character of the statement in the original `sql`.
/// The returned strings do NOT include the terminating ';'.
fn split_statements(sql: &str) -> Vec<(String, usize)> {
    let mut stmts = Vec::new();
    let mut current = String::new();
    let mut stmt_start: Option<usize> = None;
    let bytes = sql.as_bytes();
    let len = bytes.len();
    let mut i = 0;

    while i < len {
        if bytes[i] == b'$' {
            // Possibly the start of a dollar-quoted string — scan the tag.
            let tag_start = i;
            let mut j = i + 1;
            while j < len && bytes[j] != b'$' && bytes[j] != b'\n' {
                j += 1;
            }
            if j < len && bytes[j] == b'$' {
                // We have a valid dollar-quote opening tag: bytes[tag_start..=j]
                let tag = &sql[tag_start..=j];
                if stmt_start.is_none() {
                    stmt_start = Some(i);
                }
                // Copy the opening tag into current
                current.push_str(tag);
                let content_start = j + 1;
                // Now scan for the closing tag
                if let Some(close_pos) = sql[content_start..].find(tag) {
                    let abs_close = content_start + close_pos;
                    // Copy everything up to and including closing tag
                    current.push_str(&sql[content_start..abs_close + tag.len()]);
                    i = abs_close + tag.len();
                    continue;
                } else {
                    // No closing tag found — copy the rest verbatim
                    current.push_str(&sql[content_start..]);
                    i = len;
                    continue;
                }
            } else {
                // Not a dollar-quote, just a plain '$'
                if stmt_start.is_none() {
                    stmt_start = Some(i);
                }
                current.push(bytes[i] as char);
                i += 1;
                continue;
            }
        }

        // Skip line comments (-- ...) so that semicolons inside comments
        // do not cause spurious statement splits.
        if bytes[i] == b'-' && i + 1 < len && bytes[i + 1] == b'-' {
            // Copy the comment characters and skip to end of line
            while i < len && bytes[i] != b'\n' {
                if stmt_start.is_none() && !bytes[i].is_ascii_whitespace() {
                    stmt_start = Some(i);
                }
                current.push(bytes[i] as char);
                i += 1;
            }
            continue;
        }

        if bytes[i] == b'\'' {
            // Single-quoted string — copy until closing quote (handling '')
            if stmt_start.is_none() {
                stmt_start = Some(i);
            }
            current.push('\'');
            i += 1;
            while i < len {
                if bytes[i] == b'\'' {
                    current.push('\'');
                    i += 1;
                    if i < len && bytes[i] == b'\'' {
                        // Escaped quote
                        current.push('\'');
                        i += 1;
                    } else {
                        break;
                    }
                } else {
                    current.push(bytes[i] as char);
                    i += 1;
                }
            }
            continue;
        }

        if bytes[i] == b';' {
            let trimmed = current.trim().to_string();
            if !trimmed.is_empty() {
                stmts.push((trimmed, stmt_start.unwrap_or(0)));
            }
            current.clear();
            stmt_start = None;
            i += 1;
            continue;
        }

        // Track the start of non-whitespace content
        if stmt_start.is_none() && !bytes[i].is_ascii_whitespace() {
            stmt_start = Some(i);
        }
        current.push(bytes[i] as char);
        i += 1;
    }

    let trimmed = current.trim().to_string();
    if !trimmed.is_empty() {
        stmts.push((trimmed, stmt_start.unwrap_or(0)));
    }

    stmts
}

/// Expose single-statement conversion for testing purposes.
/// Takes a pre-parsed sqlparser statement and converts it to a pgsql_ast Statement.
pub fn convert_statement_pub(
    stmt: sqlparser::ast::Statement,
) -> Result<Statement, ParseError> {
    convert::convert_statement(stmt)
}
