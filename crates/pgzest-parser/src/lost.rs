//! Syntax read but not held by the AST ([`crate::LostSyntax`]).
//!
//! A conversion from sqlparser's AST that leaves a field aside loses what it
//! held, silently: `DEFERRABLE` on a foreign key, `SET search_path` on a
//! function. Rather than trusting every conversion, each statement is
//! checked as a whole: every word of its source must come back in what
//! pgzest emits from its AST. Punctuation and spelling may change
//! (`::` / `CAST`, `TIMESTAMPTZ` / `timestamp with time zone`); a word that
//! disappears is information the analysis, the migrations and the emitted
//! SQL no longer have.

use pgzest_ast::Statement;
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::tokenizer::{Token, TokenWithSpan, Tokenizer};

/// Words of a statement: unquoted ones lowercased, quoted ones as written.
fn word(t: &Token) -> Option<String> {
    match t {
        Token::Word(w) if w.quote_style.is_some() => Some(w.value.clone()),
        Token::Word(w) => Some(w.value.to_lowercase()),
        _ => None,
    }
}

/// Spellings the emission replaces with an equivalent one: the type
/// aliases PostgreSQL resolves to the same type.
fn spelled_as(w: &str) -> &'static [&'static str] {
    match w {
        "timestamptz" => &["timestamp"],
        "timetz" => &["time"],
        "int" | "int4" => &["integer"],
        "int2" => &["smallint"],
        "int8" => &["bigint"],
        "decimal" => &["numeric"],
        "bool" => &["boolean"],
        "float4" => &["real"],
        "float8" => &["double"],
        "varchar" | "varbit" => &["varying"],
        "char" => &["character"],
        _ => &[],
    }
}

/// Leading keywords of a statement, uppercased, up to the first other
/// token, at most three (`CREATE TABLE`, `ALTER TABLE`).
pub(crate) fn statement_kind(tokens: &[TokenWithSpan]) -> String {
    tokens
        .iter()
        .map(|t| &t.token)
        .filter(|t| !matches!(t, Token::Whitespace(_)))
        .map_while(|t| match t {
            Token::Word(w)
                if w.quote_style.is_none()
                    && w.keyword != sqlparser::keywords::Keyword::NoKeyword =>
            {
                Some(w.value.to_uppercase())
            }
            _ => None,
        })
        .take(3)
        .collect::<Vec<_>>()
        .join(" ")
}

/// Words of `tokens` (the source of `stmt`) missing from its emission, in
/// source order, each once.
pub(crate) fn lost_words(tokens: &[TokenWithSpan], stmt: &Statement) -> Vec<String> {
    let emitted = pgzest_ast::emit::emit_statement(stmt);
    let Ok(emitted) = Tokenizer::new(&PostgreSqlDialect {}, &emitted).tokenize() else {
        // An emission the tokenizer rejects is a defect of the emission;
        // nothing can be compared.
        return Vec::new();
    };
    let have: std::collections::HashSet<String> = emitted.iter().filter_map(word).collect();
    let mut out: Vec<String> = Vec::new();
    for w in tokens.iter().filter_map(|t| word(&t.token)) {
        let kept = have.contains(&w) || spelled_as(&w).iter().any(|a| have.contains(*a));
        if !kept && !out.contains(&w) {
            out.push(w);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    fn lost(sql: &str) -> Vec<String> {
        let out = crate::parse_with_outcome(sql).expect("parse");
        assert!(out.skipped.is_empty(), "{:?}", out.skipped);
        out.lost.into_iter().flat_map(|l| l.words).collect()
    }

    #[test]
    fn words_left_aside_by_the_ast_are_found() {
        assert_eq!(
            lost(
                "CREATE TABLE c (a INT, FOREIGN KEY (a) REFERENCES p MATCH FULL \
                 DEFERRABLE INITIALLY DEFERRED)"
            ),
            ["match", "full", "deferrable", "initially", "deferred"]
        );
        let out =
            crate::parse_with_outcome("SELECT 1;\nCREATE TABLE c (a INT REFERENCES p DEFERRABLE);")
                .unwrap();
        assert_eq!(out.lost.len(), 1);
        assert_eq!(out.lost[0].statement, "CREATE TABLE");
        assert_eq!(out.lost[0].start, 10);
    }

    #[test]
    fn equivalent_spellings_are_not_losses() {
        for sql in [
            "CREATE TABLE t (a TIMESTAMPTZ, b INT4, c DECIMAL(10, 2), d VARCHAR(3), e BOOL)",
            "SELECT x::date, CAST(y AS int) FROM t WHERE (a = 1)",
            "ALTER TABLE t ADD COLUMN n INT NOT NULL DEFAULT 0",
            "CREATE INDEX CONCURRENTLY i ON t USING btree (a DESC NULLS LAST)",
            "CREATE TABLE \"Quoted\" (\"Col\" TEXT, lower_col TEXT)",
        ] {
            assert_eq!(lost(sql), Vec::<String>::new(), "{sql}");
        }
    }
}
