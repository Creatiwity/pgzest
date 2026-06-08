use std::collections::HashMap;
use tower_lsp::lsp_types::{
    SemanticToken, SemanticTokenType, SemanticTokensFullOptions, SemanticTokensLegend,
    SemanticTokensOptions, SemanticTokensServerCapabilities,
};

pub const N_COLORS: usize = 6;

/// Token types used as the 6 color slots. These are standard VS Code semantic
/// token type strings that most themes actually colorize distinctly.
static COLOR_TOKEN_TYPES: [SemanticTokenType; N_COLORS] = [
    SemanticTokenType::TYPE,
    SemanticTokenType::FUNCTION,
    SemanticTokenType::CLASS,
    SemanticTokenType::ENUM_MEMBER,
    SemanticTokenType::INTERFACE,
    SemanticTokenType::NAMESPACE,
];

pub fn legend() -> SemanticTokensLegend {
    SemanticTokensLegend {
        token_types: COLOR_TOKEN_TYPES.to_vec(),
        token_modifiers: vec![],
    }
}

pub fn server_capability() -> SemanticTokensServerCapabilities {
    SemanticTokensServerCapabilities::SemanticTokensOptions(SemanticTokensOptions {
        legend: legend(),
        full: Some(SemanticTokensFullOptions::Bool(true)),
        range: Some(false),
        work_done_progress_options: Default::default(),
    })
}

// ── Tokenizer ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
enum TokKind {
    Ident,
    Dot,
    Comma,
    LParen,
    RParen,
    Other,
}

#[derive(Debug, Clone)]
struct Tok {
    kind: TokKind,
    text: String,
    line: u32,
    col: u32,
    /// Source span length in bytes (includes quotes for quoted identifiers).
    len: u32,
}

fn tokenize(sql: &str) -> Vec<Tok> {
    let bytes = sql.as_bytes();
    let len = bytes.len();
    let mut i = 0;
    let mut line = 0u32;
    let mut line_start = 0usize;
    let mut tokens = Vec::new();

    macro_rules! col {
        () => {
            (i - line_start) as u32
        };
    }

    while i < len {
        // Skip line comment
        if i + 1 < len && bytes[i] == b'-' && bytes[i + 1] == b'-' {
            while i < len && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }

        // Skip block comment
        if i + 1 < len && bytes[i] == b'/' && bytes[i + 1] == b'*' {
            i += 2;
            while i + 1 < len && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                if bytes[i] == b'\n' {
                    line += 1;
                    line_start = i + 1;
                }
                i += 1;
            }
            i += 2; // consume */
            continue;
        }

        // Skip single-quoted strings
        if bytes[i] == b'\'' {
            i += 1;
            while i < len {
                if bytes[i] == b'\'' {
                    i += 1;
                    // escaped '' inside string
                    if i < len && bytes[i] == b'\'' {
                        i += 1;
                        continue;
                    }
                    break;
                }
                if bytes[i] == b'\n' {
                    line += 1;
                    line_start = i + 1;
                }
                i += 1;
            }
            continue;
        }

        // Dollar sign — treat as Other (we intentionally DO NOT skip dollar-quoted
        // string bodies here; the semantic tokenizer needs to see the SQL inside
        // $$ function bodies to color FROM/JOIN aliases correctly).
        if bytes[i] == b'$' {
            tokens.push(Tok { kind: TokKind::Other, text: "$".into(), line, col: col!(), len: 1 });
            i += 1;
            continue;
        }

        // Newline
        if bytes[i] == b'\n' {
            line += 1;
            line_start = i + 1;
            i += 1;
            continue;
        }

        // Whitespace
        if bytes[i].is_ascii_whitespace() {
            i += 1;
            continue;
        }

        // Dot
        if bytes[i] == b'.' {
            tokens.push(Tok { kind: TokKind::Dot, text: ".".into(), line, col: col!(), len: 1 });
            i += 1;
            continue;
        }

        // Comma
        if bytes[i] == b',' {
            tokens.push(Tok { kind: TokKind::Comma, text: ",".into(), line, col: col!(), len: 1 });
            i += 1;
            continue;
        }

        // Parens
        if bytes[i] == b'(' {
            tokens.push(Tok { kind: TokKind::LParen, text: "(".into(), line, col: col!(), len: 1 });
            i += 1;
            continue;
        }
        if bytes[i] == b')' {
            tokens.push(Tok { kind: TokKind::RParen, text: ")".into(), line, col: col!(), len: 1 });
            i += 1;
            continue;
        }

        // Quoted identifier "foo"
        if bytes[i] == b'"' {
            let t_line = line;
            let t_col = col!();
            i += 1;
            let start = i;
            while i < len && bytes[i] != b'"' {
                if bytes[i] == b'\n' {
                    line += 1;
                    line_start = i + 1;
                }
                i += 1;
            }
            let text = sql[start..i].to_lowercase();
            // +2 to include both the opening and closing double-quote in the span
            let src_len = (text.len() + 2) as u32;
            if i < len { i += 1; } // closing "
            tokens.push(Tok { kind: TokKind::Ident, text, line: t_line, col: t_col, len: src_len });
            continue;
        }

        // Identifier or keyword
        if bytes[i].is_ascii_alphabetic() || bytes[i] == b'_' {
            let t_line = line;
            let t_col = col!();
            let start = i;
            while i < len && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            let text = sql[start..i].to_lowercase();
            let src_len = text.len() as u32;
            tokens.push(Tok { kind: TokKind::Ident, text, line: t_line, col: t_col, len: src_len });
            continue;
        }

        // Everything else
        tokens.push(Tok { kind: TokKind::Other, text: (bytes[i] as char).to_string(), line, col: col!(), len: 1 });
        i += 1;
    }

    tokens
}

// ── Colored span (absolute positions) ─────────────────────────────────────────

#[derive(Debug, Clone)]
struct ColoredSpan {
    line: u32,
    col: u32,
    len: u32,
    color_index: u32,
}

// ── SQL keywords that terminate a FROM/JOIN binding ───────────────────────────

fn is_join_modifier(s: &str) -> bool {
    matches!(s, "inner" | "left" | "right" | "full" | "cross" | "outer" | "natural")
}

fn is_from_or_join(s: &str) -> bool {
    matches!(s, "from" | "join")
}

fn terminates_binding(s: &str) -> bool {
    matches!(
        s,
        "on" | "using" | "where" | "group" | "order" | "having" | "limit" | "offset"
        | "select" | "set" | "returning" | "union" | "intersect" | "except" | "with"
        | "insert" | "update" | "delete"
    )
}

// ── Color assignment ──────────────────────────────────────────────────────────

fn assign_color(name: &str, next_color: &mut u32, color_map: &mut HashMap<String, u32>) -> u32 {
    if let Some(&c) = color_map.get(name) {
        return c;
    }
    let c = *next_color % N_COLORS as u32;
    *next_color += 1;
    color_map.insert(name.to_string(), c);
    c
}

// ── Table name parser (schema.table or plain table) ──────────────────────────

/// Consume a table reference starting at `tokens[i]`, push a colored span, and
/// return the local table name (after the dot for schema-qualified names).
/// Advances `i` past the consumed tokens.
fn consume_table(
    tokens: &[Tok],
    i: &mut usize,
    next_color: &mut u32,
    color_map: &mut HashMap<String, u32>,
    spans: &mut Vec<ColoredSpan>,
) -> Option<String> {
    let n = tokens.len();
    if *i >= n || tokens[*i].kind != TokKind::Ident { return None; }

    let mut table_name = tokens[*i].text.clone();
    let table_line = tokens[*i].line;
    let table_col  = tokens[*i].col;
    let table_len  = tokens[*i].len;
    *i += 1;

    if *i + 1 < n && tokens[*i].kind == TokKind::Dot && tokens[*i + 1].kind == TokKind::Ident {
        // schema.table — color only the table part
        table_name = tokens[*i + 1].text.clone();
        let tbl = &tokens[*i + 1];
        let color = assign_color(&table_name, next_color, color_map);
        spans.push(ColoredSpan { line: tbl.line, col: tbl.col, len: tbl.len, color_index: color });
        *i += 2;
    } else {
        let color = assign_color(&table_name, next_color, color_map);
        spans.push(ColoredSpan { line: table_line, col: table_col, len: table_len, color_index: color });
    }
    Some(table_name)
}

// ── First pass: table/alias discovery (FROM/JOIN/INSERT/UPDATE) ───────────────

/// Returns (color_map, spans).
/// color_map maps lowercased table names and aliases → color index.
fn first_pass(tokens: &[Tok]) -> (HashMap<String, u32>, Vec<ColoredSpan>) {
    let mut color_map: HashMap<String, u32> = HashMap::new();
    let mut spans: Vec<ColoredSpan> = Vec::new();
    let mut next_color: u32 = 0;

    let n = tokens.len();
    let mut i = 0;

    while i < n {
        let tok = &tokens[i];
        if tok.kind != TokKind::Ident {
            i += 1;
            continue;
        }

        // Skip join modifiers (LEFT, RIGHT, INNER, etc.)
        if is_join_modifier(&tok.text) {
            i += 1;
            continue;
        }

        // ── FROM / JOIN ───────────────────────────────────────────────────────
        if is_from_or_join(&tok.text) {
            i += 1;
            if i >= n || tokens[i].kind != TokKind::Ident { continue; }

            // Skip subqueries starting with (
            if tokens[i].kind == TokKind::LParen {
                let mut depth = 0i32;
                while i < n {
                    match tokens[i].kind {
                        TokKind::LParen => { depth += 1; i += 1; }
                        TokKind::RParen => { depth -= 1; i += 1; if depth <= 0 { break; } }
                        _ => { i += 1; }
                    }
                }
                continue;
            }

            let table_name = match consume_table(tokens, &mut i, &mut next_color, &mut color_map, &mut spans) {
                Some(n) => n,
                None => continue,
            };

            let color = *color_map.get(&table_name).unwrap();

            if i >= n { continue; }
            let maybe_as = &tokens[i];

            if maybe_as.kind == TokKind::Ident && maybe_as.text == "as" {
                i += 1;
                if i < n && tokens[i].kind == TokKind::Ident && !terminates_binding(&tokens[i].text) {
                    let alias_tok = &tokens[i];
                    let alias = alias_tok.text.clone();
                    let color = *color_map.entry(alias.clone()).or_insert(color);
                    spans.push(ColoredSpan { line: alias_tok.line, col: alias_tok.col, len: alias_tok.len, color_index: color });
                    i += 1;
                }
            } else if maybe_as.kind == TokKind::Ident
                && !terminates_binding(&maybe_as.text)
                && !is_from_or_join(&maybe_as.text)
                && !is_join_modifier(&maybe_as.text)
            {
                let alias_tok = &tokens[i];
                let alias = alias_tok.text.clone();
                let color = *color_map.entry(alias.clone()).or_insert(color);
                spans.push(ColoredSpan { line: alias_tok.line, col: alias_tok.col, len: alias_tok.len, color_index: color });
                i += 1;
            }
            continue;
        }

        // ── INSERT [INTO] table [(col1, col2, ...)] ───────────────────────────
        if tok.text == "insert" {
            i += 1;
            // Skip optional INTO keyword
            if i < n && tokens[i].kind == TokKind::Ident && tokens[i].text == "into" {
                i += 1;
            }
            // Skip subqueries
            if i < n && tokens[i].kind == TokKind::LParen {
                i += 1;
                continue;
            }
            if let Some(table_name) = consume_table(tokens, &mut i, &mut next_color, &mut color_map, &mut spans) {
                let color = *color_map.get(&table_name).unwrap();
                // Color the explicit column list: INSERT INTO t (col1, col2, ...)
                if i < n && tokens[i].kind == TokKind::LParen {
                    i += 1; // consume (
                    while i < n {
                        match tokens[i].kind {
                            TokKind::RParen => { i += 1; break; }
                            TokKind::Ident => {
                                let t = &tokens[i];
                                spans.push(ColoredSpan { line: t.line, col: t.col, len: t.len, color_index: color });
                                i += 1;
                            }
                            _ => { i += 1; }
                        }
                    }
                }
            }
            continue;
        }

        // ── UPDATE table SET col1 = ..., col2 = ... ───────────────────────────
        if tok.text == "update" {
            i += 1;
            if let Some(table_name) = consume_table(tokens, &mut i, &mut next_color, &mut color_map, &mut spans) {
                let color = *color_map.get(&table_name).unwrap();
                // Skip optional alias (same logic as FROM/JOIN) then find SET
                while i < n && !(tokens[i].kind == TokKind::Ident && tokens[i].text == "set") {
                    i += 1;
                }
                if i < n { i += 1; } // consume SET
                // Color the target column in each `col = expr` assignment.
                // Stop at WHERE / FROM / RETURNING / JOIN which end the SET clause.
                while i < n {
                    let t = &tokens[i];
                    if t.kind == TokKind::Ident
                        && matches!(t.text.as_str(), "where" | "from" | "join" | "returning")
                    {
                        break;
                    }
                    // `col =` or `alias.col =`: color the ident immediately before `=`
                    if t.kind == TokKind::Ident
                        && i + 1 < n
                        && tokens[i + 1].kind == TokKind::Other
                        && tokens[i + 1].text == "="
                    {
                        spans.push(ColoredSpan { line: t.line, col: t.col, len: t.len, color_index: color });
                    }
                    i += 1;
                }
            }
            continue;
        }

        i += 1;
    }

    (color_map, spans)
}

// ── PL/pgSQL DECLARE pass: variable declaration coloring ─────────────────────

/// Returns (var_color_map, spans).
/// Scans DECLARE...BEGIN sections and colors declared variable names.
/// All variables in the DECLARE section share a single color (the last slot)
/// so they don't add visual noise when multiple tables are already colorized.
fn declare_pass(tokens: &[Tok]) -> (HashMap<String, u32>, Vec<ColoredSpan>) {
    let mut color_map: HashMap<String, u32> = HashMap::new();
    let mut spans: Vec<ColoredSpan> = Vec::new();
    const VAR_COLOR: u32 = (N_COLORS - 1) as u32;
    let n = tokens.len();
    let mut i = 0;

    while i < n {
        let tok = &tokens[i];
        if tok.kind != TokKind::Ident || tok.text != "declare" {
            i += 1;
            continue;
        }
        i += 1;

        // Inside DECLARE section: first ident on each declaration is the variable name.
        // A new declaration starts after DECLARE or after a semicolon (Other ";").
        let mut at_decl_start = true;

        while i < n {
            let t = &tokens[i];
            match t.kind {
                TokKind::Ident => {
                    if t.text == "begin" || t.text == "end" {
                        break;
                    }
                    // Skip modifier keywords that prefix a declaration
                    if matches!(t.text.as_str(), "constant" | "not" | "null" | "default") {
                        at_decl_start = false;
                        i += 1;
                        continue;
                    }
                    if at_decl_start {
                        color_map.insert(t.text.clone(), VAR_COLOR);
                        spans.push(ColoredSpan { line: t.line, col: t.col, len: t.len, color_index: VAR_COLOR });
                        at_decl_start = false;
                    }
                    i += 1;
                }
                TokKind::Other => {
                    if t.text == ";" {
                        at_decl_start = true;
                    }
                    i += 1;
                }
                _ => { i += 1; }
            }
        }
    }

    (color_map, spans)
}

// ── Second pass: alias.column references ─────────────────────────────────────

fn second_pass(tokens: &[Tok], color_map: &HashMap<String, u32>) -> Vec<ColoredSpan> {
    let mut spans = Vec::new();
    let n = tokens.len();
    let mut i = 0;

    while i + 2 < n {
        if tokens[i].kind == TokKind::Ident
            && tokens[i + 1].kind == TokKind::Dot
            && tokens[i + 2].kind == TokKind::Ident
        {
            let qualifier = &tokens[i];
            let column = &tokens[i + 2];
            if let Some(&color) = color_map.get(&qualifier.text) {
                spans.push(ColoredSpan {
                    line: qualifier.line,
                    col: qualifier.col,
                    len: qualifier.len,
                    color_index: color,
                });
                spans.push(ColoredSpan {
                    line: column.line,
                    col: column.col,
                    len: column.len,
                    color_index: color,
                });
                i += 3; // skip qualifier . column
                continue;
            }
        }
        i += 1;
    }

    spans
}

// ── Delta encoding ────────────────────────────────────────────────────────────

pub fn compute_semantic_tokens(sql: &str) -> Vec<SemanticToken> {
    let tokens = tokenize(sql);

    let (mut color_map, mut spans) = first_pass(&tokens);
    let (var_color_map, var_spans) = declare_pass(&tokens);

    // Merge variable colors into the main map (table names win on conflict).
    for (k, v) in var_color_map {
        color_map.entry(k).or_insert(v);
    }
    spans.extend(var_spans);

    let second = second_pass(&tokens, &color_map);
    spans.extend(second);

    // Sort by position (line asc, col asc). Deduplicate overlapping spans from
    // the two passes (e.g. the alias in FROM clause is already in spans, and the
    // second pass might re-emit it if used as a qualifier).
    spans.sort_by(|a, b| a.line.cmp(&b.line).then(a.col.cmp(&b.col)));
    spans.dedup_by(|a, b| a.line == b.line && a.col == b.col);

    // Delta-encode
    let mut result = Vec::with_capacity(spans.len());
    let mut prev_line = 0u32;
    let mut prev_col = 0u32;

    for span in &spans {
        let delta_line = span.line - prev_line;
        let delta_start = if delta_line == 0 { span.col - prev_col } else { span.col };
        result.push(SemanticToken {
            delta_line,
            delta_start,
            length: span.len,
            token_type: span.color_index,
            token_modifiers_bitset: 0,
        });
        prev_line = span.line;
        prev_col = span.col;
    }

    result
}
