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

// ── First pass: FROM/JOIN table + alias discovery ─────────────────────────────

/// Returns (color_map, spans).
/// color_map maps lowercased table names and aliases → color index.
fn first_pass(tokens: &[Tok]) -> (HashMap<String, u32>, Vec<ColoredSpan>) {
    let mut color_map: HashMap<String, u32> = HashMap::new();
    let mut spans: Vec<ColoredSpan> = Vec::new();
    let mut next_color: u32 = 0;

    let n = tokens.len();
    let mut i = 0;

    // Assigns a color to `name` (reusing existing assignment if present).
    let assign_color = |name: &str, next_color: &mut u32, color_map: &mut HashMap<String, u32>| -> u32 {
        if let Some(&c) = color_map.get(name) {
            return c;
        }
        let c = *next_color % N_COLORS as u32;
        *next_color += 1;
        color_map.insert(name.to_string(), c);
        c
    };

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

        if !is_from_or_join(&tok.text) {
            i += 1;
            continue;
        }

        // Advance past the FROM/JOIN keyword
        i += 1;

        // Expect an identifier (possibly schema.table)
        if i >= n || tokens[i].kind != TokKind::Ident {
            continue;
        }

        // Skip subqueries starting with (
        // (already filtered by tokenizer — LParen will be next tok)
        if i < n && tokens[i].kind == TokKind::LParen {
            // subquery — skip until matching )
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

        if i >= n || tokens[i].kind != TokKind::Ident {
            continue;
        }

        // Collect schema.table (two idents separated by a dot)
        let mut table_name = tokens[i].text.clone();
        let table_tok_line = tokens[i].line;
        let table_tok_col = tokens[i].col;
        let table_tok_len = tokens[i].len;
        i += 1;

        if i + 1 < n && tokens[i].kind == TokKind::Dot && tokens[i + 1].kind == TokKind::Ident {
            // schema.table — use only the table part as the color key
            table_name = tokens[i + 1].text.clone();
            let tbl_tok = &tokens[i + 1];
            let color = assign_color(&table_name, &mut next_color, &mut color_map);
            spans.push(ColoredSpan {
                line: tbl_tok.line,
                col: tbl_tok.col,
                len: tbl_tok.len,
                color_index: color,
            });
            i += 2;
        } else {
            // Plain table name
            let color = assign_color(&table_name, &mut next_color, &mut color_map);
            spans.push(ColoredSpan {
                line: table_tok_line,
                col: table_tok_col,
                len: table_tok_len,
                color_index: color,
            });
        }

        let color = *color_map.get(&table_name).unwrap();

        // Now look for an alias.
        // Skip if next token terminates the clause or is another join keyword.
        if i >= n { continue; }

        let maybe_as = &tokens[i];

        if maybe_as.kind == TokKind::Ident && maybe_as.text == "as" {
            // Explicit AS alias
            i += 1;
            if i < n && tokens[i].kind == TokKind::Ident && !terminates_binding(&tokens[i].text) {
                let alias_tok = &tokens[i];
                let alias = alias_tok.text.clone();
                let color = *color_map.entry(alias.clone()).or_insert(color);
                spans.push(ColoredSpan {
                    line: alias_tok.line,
                    col: alias_tok.col,
                    len: alias_tok.len,
                    color_index: color,
                });
                i += 1;
            }
        } else if maybe_as.kind == TokKind::Ident
            && !terminates_binding(&maybe_as.text)
            && !is_from_or_join(&maybe_as.text)
            && !is_join_modifier(&maybe_as.text)
        {
            // Implicit alias — but must not be a join modifier or from keyword that
            // starts the next binding
            let alias_tok = &tokens[i];
            let alias = alias_tok.text.clone();
            let color = *color_map.entry(alias.clone()).or_insert(color);
            spans.push(ColoredSpan {
                line: alias_tok.line,
                col: alias_tok.col,
                len: alias_tok.len,
                color_index: color,
            });
            i += 1;
        }
        // If neither, the table has no alias; table_name itself is in the map already.
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

    let (color_map, mut spans) = first_pass(&tokens);
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
