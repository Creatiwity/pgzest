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

    // Only rewrite PROCEDURE statements
    if !upper.contains("PROCEDURE") {
        return (header.to_string(), body_and_rest.to_string(), false);
    }

    // Replace PROCEDURE keyword with FUNCTION and inject RETURNS VOID before LANGUAGE.
    // Strategy:
    //   1. Find "PROCEDURE" in the header and replace with "FUNCTION "
    //   2. Handle OR REPLACE: ensure "CREATE OR REPLACE FUNCTION"
    //   3. Insert "RETURNS VOID " before LANGUAGE (or before AS if no LANGUAGE in header)

    // Step 1: Replace "PROCEDURE" with "FUNCTION"
    let proc_pos = upper.find("PROCEDURE").unwrap();
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

/// Returns true if the SQL statement (uppercased, trimmed) looks like a
/// CREATE FUNCTION or CREATE OR REPLACE FUNCTION / PROCEDURE statement.
/// Leading `--` comment lines are skipped so that comments attached to a
/// function definition don't prevent detection.
fn is_create_function_sql(sql: &str) -> bool {
    // Skip leading comment lines (-- ...) before checking for CREATE.
    let first_non_comment_line = sql
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with("--"));

    let s = match first_non_comment_line {
        Some(l) => l.to_uppercase(),
        None => return false,
    };

    if !s.starts_with("CREATE") {
        return false;
    }
    // Also check the full SQL (not just the first line) for FUNCTION/PROCEDURE
    // in case the keyword appears on a later line.
    let full_upper = sql.to_uppercase();
    full_upper.contains("FUNCTION") || full_upper.contains("PROCEDURE")
}

pub fn parse(sql: &str) -> Result<Vec<Statement>, ParseError> {
    let dialect = PostgreSqlDialect {};

    // Split on ';' to handle each statement individually so we can pre-process
    // CREATE FUNCTION statements independently.
    // We do a simple split — statements inside $$ bodies can contain ';' but sqlparser
    // handles the final split correctly. We pre-process per-statement by re-joining
    // after stripping problematic tokens only from CREATE FUNCTION headers.

    // Strategy: parse the whole SQL at once after preprocessing each statement.
    // Since sqlparser splits on ';' itself (respecting dollar-quotes), we do a
    // two-pass approach:
    //   1. Collect raw statement strings by splitting on ';' outside dollar-quotes.
    //   2. For each CREATE FUNCTION statement, preprocess it and store its meta.
    //   3. Re-join all cleaned statements and parse at once.
    //   4. Walk the resulting parsed statements, injecting meta back for CreateFunction nodes.

    let raw_stmts = split_statements(sql);
    let mut metas: Vec<Option<FunctionMeta>> = Vec::with_capacity(raw_stmts.len());
    let mut cleaned_stmts: Vec<String> = Vec::with_capacity(raw_stmts.len());

    for stmt_sql in &raw_stmts {
        if is_create_function_sql(stmt_sql) {
            let (cleaned, meta) = preprocess_function_sql(stmt_sql);
            cleaned_stmts.push(cleaned);
            metas.push(Some(meta));
        } else {
            cleaned_stmts.push(stmt_sql.clone());
            metas.push(None);
        }
    }

    let reassembled = cleaned_stmts.join("; ");

    let sp_stmts = SqlParser::parse_sql(&dialect, &reassembled)
        .map_err(|e| ParseError::Syntax(e.to_string()))?;

    // Zip with metas and inject
    let mut result = Vec::with_capacity(sp_stmts.len());
    for (sp_stmt, meta) in sp_stmts.into_iter().zip(metas.into_iter()) {
        let mut stmt = match convert::convert_statement(sp_stmt) {
            Ok(s) => s,
            // Silently skip DDL statements not yet supported by the AST
            // (e.g. CREATE EXTENSION, unsupported ALTER TABLE variants).
            Err(ParseError::Unsupported(_)) => continue,
            Err(e) => return Err(e),
        };
        // Inject function meta if available
        if let Some(meta) = meta {
            if let Statement::Ddl(pgsql_ast::ddl::DdlStatement::CreateFunction(ref mut cf)) =
                stmt
            {
                cf.security_definer = meta.security_definer;
                cf.cost = meta.cost;
                cf.rows = meta.rows;
                // Override return type to SetOf if we detected RETURNS SETOF <type>
                if let Some(ref inner_type_str) = meta.setof_type {
                    use pgsql_ast::ddl::ReturnType;
                    cf.return_type =
                        ReturnType::SetOf(convert::parse_simple_type_pub(inner_type_str));
                }
            }

            // If the original statement was CREATE PROCEDURE (rewritten as FUNCTION for sqlparser),
            // convert back to CreateProcedure.
            if meta.is_procedure {
                if let Statement::Ddl(pgsql_ast::ddl::DdlStatement::CreateFunction(cf)) = stmt {
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
                    stmt = Statement::Ddl(pgsql_ast::ddl::DdlStatement::CreateProcedure(proc));
                }
            }
        }
        result.push(stmt);
    }
    Ok(result)
}

/// Split SQL into individual statement strings by ';', respecting dollar-quoted strings.
/// The returned strings do NOT include the terminating ';'.
fn split_statements(sql: &str) -> Vec<String> {
    let mut stmts = Vec::new();
    let mut current = String::new();
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
                current.push(bytes[i] as char);
                i += 1;
                continue;
            }
        }

        if bytes[i] == b'\'' {
            // Single-quoted string — copy until closing quote (handling '')
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
                stmts.push(trimmed);
            }
            current.clear();
            i += 1;
            continue;
        }

        current.push(bytes[i] as char);
        i += 1;
    }

    let trimmed = current.trim().to_string();
    if !trimmed.is_empty() {
        stmts.push(trimmed);
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
