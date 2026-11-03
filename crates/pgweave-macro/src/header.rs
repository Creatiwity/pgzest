//! Header pattern: what follows `@name` up to the opening brace.
//!
//! The set of slot kinds is **closed** and deliberately small. That is what
//! sets this format apart from a template language: an arbitrary grammar
//! cannot be expressed, only a few forms assembled that cover real SQL
//! boilerplate.
//!
//! Pattern: literals, slots `{name:kind}`, optional groups between `[ ]`.
//!
//! ```text
//! {name:ident}({params:parens})[: {ret:type}]
//! [{name:ident} ]{timing:word} {events:words} on {tables:list} for each {each:word}
//! ```

use std::collections::HashMap;

/// What a slot can consume.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotKind {
    /// An identifier, possibly qualified: `users`, `public.users`.
    Ident,
    /// A single word: `before`, `row`.
    Word,
    /// Several words separated by spaces or `or`: `insert or update`.
    Words,
    /// The content up to the matching closing parenthesis, copied verbatim,
    /// honoring nesting and strings: `a INT, b NUMERIC(10,2)`.
    ///
    /// The slot takes the **content**, the pattern carries the delimiters:
    /// `({params:parens})`. The content may be empty.
    Parens,
    /// A SQL type, including `SETOF x` and `TABLE (...)`.
    Type,
    /// An identifier, or a parenthesized list: `users` / `(a, b, c)`.
    List,
    /// Everything else up to the brace — a sequence of keywords.
    Keywords,
}

impl SlotKind {
    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "ident" => Self::Ident,
            "word" => Self::Word,
            "words" => Self::Words,
            "parens" => Self::Parens,
            "type" => Self::Type,
            "list" => Self::List,
            "keywords" => Self::Keywords,
            _ => return None,
        })
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Ident => "ident",
            Self::Word => "word",
            Self::Words => "words",
            Self::Parens => "parens",
            Self::Type => "type",
            Self::List => "list",
            Self::Keywords => "keywords",
        }
    }
}

#[derive(Debug, Clone)]
enum Piece {
    Literal(String),
    Slot {
        name: String,
        kind: SlotKind,
    },
    /// Optional group: consumed if it matches, ignored otherwise.
    Optional(Vec<Piece>),
}

/// Compiled header pattern.
#[derive(Debug, Clone)]
pub struct HeaderPattern {
    pieces: Vec<Piece>,
}

impl HeaderPattern {
    pub fn parse(pattern: &str) -> Result<Self, String> {
        let (pieces, rest) = parse_pieces(pattern, false)?;
        if !rest.is_empty() {
            return Err(format!("`]` without `[` near {rest:?}"));
        }
        Ok(Self { pieces })
    }

    /// The declared slots, in order.
    pub fn slots(&self) -> Vec<(&str, SlotKind)> {
        let mut out = Vec::new();
        collect_slots(&self.pieces, &mut out);
        out
    }

    /// Applies the pattern to `input`, the raw header of the invocation.
    ///
    /// Returns the captures. The pattern must consume all of `input`: an
    /// unconsumed remainder is a user-side syntax error, not a silent partial
    /// success.
    pub fn match_header(&self, input: &str) -> Result<HashMap<String, String>, String> {
        let mut caps = HashMap::new();
        let rest = match_pieces(&self.pieces, input.trim(), &mut caps)?;
        if !rest.trim().is_empty() {
            return Err(format!("unexpected text in header: {:?}", rest.trim()));
        }
        Ok(caps)
    }
}

fn collect_slots<'a>(pieces: &'a [Piece], out: &mut Vec<(&'a str, SlotKind)>) {
    for p in pieces {
        match p {
            Piece::Slot { name, kind } => out.push((name.as_str(), *kind)),
            Piece::Optional(inner) => collect_slots(inner, out),
            Piece::Literal(_) => {}
        }
    }
}

fn parse_pieces(s: &str, inside_optional: bool) -> Result<(Vec<Piece>, &str), String> {
    let mut pieces = Vec::new();
    let mut lit = String::new();
    let mut rest = s;

    macro_rules! flush {
        () => {
            if !lit.is_empty() {
                pieces.push(Piece::Literal(std::mem::take(&mut lit)));
            }
        };
    }

    while let Some(c) = rest.chars().next() {
        match c {
            '{' => {
                flush!();
                let end = rest
                    .find('}')
                    .ok_or_else(|| format!("`{{` without `}}` near {rest:?}"))?;
                let inner = &rest[1..end];
                let (name, kind_str) = inner
                    .split_once(':')
                    .ok_or_else(|| format!("slot {inner:?}: expected `{{name:kind}}`"))?;
                let kind = SlotKind::parse(kind_str.trim()).ok_or_else(|| {
                    format!(
                        "unknown slot kind {:?}; available: ident, word, words, parens, type, list, keywords",
                        kind_str.trim()
                    )
                })?;
                pieces.push(Piece::Slot {
                    name: name.trim().to_string(),
                    kind,
                });
                rest = &rest[end + 1..];
            }
            '[' => {
                flush!();
                let (inner, r) = parse_pieces(&rest[1..], true)?;
                pieces.push(Piece::Optional(inner));
                rest = r;
            }
            ']' if inside_optional => {
                flush!();
                return Ok((pieces, &rest[1..]));
            }
            _ => {
                lit.push(c);
                rest = &rest[c.len_utf8()..];
            }
        }
    }
    if inside_optional {
        return Err("`[` without `]`".to_string());
    }
    flush!();
    Ok((pieces, rest))
}

/// Applies a sequence of pieces, returns the unconsumed remainder.
fn match_pieces<'a>(
    pieces: &[Piece],
    input: &'a str,
    caps: &mut HashMap<String, String>,
) -> Result<&'a str, String> {
    let mut rest = input;
    for (i, piece) in pieces.iter().enumerate() {
        match piece {
            Piece::Literal(lit) => {
                let want = lit.trim();
                if want.is_empty() {
                    rest = rest.trim_start();
                    continue;
                }
                let r = rest.trim_start();
                let lower_r = r.to_lowercase();
                let lower_w = want.to_lowercase();
                if !lower_r.starts_with(&lower_w) {
                    return Err(format!("expected {want:?}, found {:?}", head(r)));
                }
                rest = &r[want.len()..];
            }
            Piece::Slot { name, kind } => {
                let (value, r) = consume_slot(*kind, rest.trim_start())
                    .map_err(|e| format!("slot `{name}`: {e}"))?;
                caps.insert(name.clone(), value);
                rest = r;
            }
            Piece::Optional(inner) => {
                // Attempt: if the group does not match, it is skipped without
                // consuming. Work on a copy of the captures so that a failed
                // attempt leaves no trace.
                let mut trial = caps.clone();
                match match_pieces(inner, rest, &mut trial) {
                    Ok(r) => {
                        // An optional group that succeeds while consuming
                        // nothing would block what follows: accept it anyway,
                        // it simply has no effect.
                        *caps = trial;
                        rest = r;
                    }
                    Err(_) => {
                        // Group absent: if mandatory pieces follow, they will
                        // report what is missing themselves.
                        let _ = i;
                    }
                }
            }
        }
    }
    Ok(rest)
}

fn head(s: &str) -> String {
    s.chars().take(20).collect()
}

fn consume_slot(kind: SlotKind, s: &str) -> Result<(String, &str), String> {
    match kind {
        SlotKind::Ident => {
            let end = s
                .find(|c: char| !(c.is_alphanumeric() || c == '_' || c == '.' || c == '"'))
                .unwrap_or(s.len());
            if end == 0 {
                return Err(format!("expected identifier, found {:?}", head(s)));
            }
            Ok((s[..end].to_string(), &s[end..]))
        }
        SlotKind::Word => {
            let end = s.find(char::is_whitespace).unwrap_or(s.len());
            if end == 0 {
                return Err("expected a word".to_string());
            }
            Ok((s[..end].to_string(), &s[end..]))
        }
        SlotKind::Words => {
            // Words and `or`, up to a keyword that is no longer part of them.
            // Stop before `on`, which always follows the event list.
            let mut taken = 0usize;
            let mut words = Vec::new();
            for w in s.split_inclusive(char::is_whitespace) {
                let t = w.trim();
                if t.is_empty() {
                    taken += w.len();
                    continue;
                }
                if t.eq_ignore_ascii_case("on") || t.starts_with('{') {
                    break;
                }
                words.push(t.to_string());
                taken += w.len();
            }
            if words.is_empty() {
                return Err("expected at least one word".to_string());
            }
            Ok((words.join(" "), &s[taken..]))
        }
        SlotKind::Parens => {
            // The opening parenthesis (pattern literal) is already consumed:
            // consume up to the matching closing one, excluded.
            let end = until_unbalanced_close(s)?;
            Ok((s[..end].trim().to_string(), &s[end..]))
        }
        SlotKind::Type => {
            let lower = s.to_lowercase();
            if lower.starts_with("table") {
                let after = s["table".len()..].trim_start();
                let skipped = s.len() - after.len();
                let inner = balanced(after, '(', ')')?;
                let consumed = skipped + inner.len() + 2;
                return Ok((format!("TABLE ({inner})"), &s[consumed..]));
            }
            if lower.starts_with("setof ") {
                let after = &s["setof ".len()..];
                let (id, r) = consume_slot(SlotKind::Ident, after.trim_start())?;
                return Ok((format!("SETOF {id}"), r));
            }
            // Simple type, possibly parameterized or an array.
            let end = s
                .find(|c: char| !(c.is_alphanumeric() || c == '_' || c == '.'))
                .unwrap_or(s.len());
            if end == 0 {
                return Err(format!("expected type, found {:?}", head(s)));
            }
            let mut ty = s[..end].to_string();
            let mut rest = &s[end..];
            if rest.starts_with('(') {
                let inner = balanced(rest, '(', ')')?;
                ty.push_str(&format!("({inner})"));
                rest = &rest[inner.len() + 2..];
            }
            while rest.starts_with("[]") {
                ty.push_str("[]");
                rest = &rest[2..];
            }
            Ok((ty, rest))
        }
        SlotKind::List => {
            if s.starts_with('(') {
                let inner = balanced(s, '(', ')')?;
                let consumed = inner.len() + 2;
                let items: Vec<String> = inner
                    .split(',')
                    .map(|x| x.trim().to_string())
                    .filter(|x| !x.is_empty())
                    .collect();
                if items.is_empty() {
                    return Err("empty list".to_string());
                }
                return Ok((items.join(", "), &s[consumed..]));
            }
            consume_slot(SlotKind::Ident, s)
        }
        SlotKind::Keywords => Ok((s.trim().to_string(), "")),
    }
}

/// Offset of the unmatched closing parenthesis that ends the current group.
///
/// `s` starts right after an already consumed opening parenthesis. Advance
/// following nesting and skipping string contents, up to the `)` that closes
/// the group.
fn until_unbalanced_close(s: &str) -> Result<usize, String> {
    let mut depth = 0usize;
    let mut in_str = false;
    for (i, c) in s.char_indices() {
        if in_str {
            if c == '\'' {
                in_str = false;
            }
            continue;
        }
        match c {
            '\'' => in_str = true,
            '(' => depth += 1,
            ')' => {
                if depth == 0 {
                    return Ok(i);
                }
                depth -= 1;
            }
            _ => {}
        }
    }
    Err("missing `)`".to_string())
}

/// Content between `open` and its matching `close`, honoring strings and
/// nesting. `s` must start with `open`.
fn balanced(s: &str, open: char, close: char) -> Result<String, String> {
    let mut chars = s.char_indices();
    match chars.next() {
        Some((_, c)) if c == open => {}
        _ => return Err(format!("expected `{open}`, found {:?}", head(s))),
    }
    let mut depth = 1usize;
    let mut in_str = false;
    for (i, c) in chars {
        if in_str {
            if c == '\'' {
                in_str = false;
            }
            continue;
        }
        match c {
            '\'' => in_str = true,
            _ if c == open => depth += 1,
            _ if c == close => {
                depth -= 1;
                if depth == 0 {
                    return Ok(s[open.len_utf8()..i].to_string());
                }
            }
            _ => {}
        }
    }
    Err(format!("missing `{close}`"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(pattern: &str, input: &str) -> HashMap<String, String> {
        HeaderPattern::parse(pattern)
            .expect("pattern")
            .match_header(input)
            .expect("match")
    }

    #[test]
    fn name_and_parameters() {
        let c = caps(
            "{name:ident}({params:parens})",
            "user_count(target_org UUID)",
        );
        assert_eq!(c["name"], "user_count");
        assert_eq!(c["params"], "target_org UUID");
    }

    #[test]
    fn nested_parentheses_in_parameters() {
        let c = caps(
            "{name:ident}({params:parens})",
            "f(a NUMERIC(10,2), b TEXT[])",
        );
        assert_eq!(c["params"], "a NUMERIC(10,2), b TEXT[]");
    }

    #[test]
    fn optional_group_present_and_absent() {
        let p = HeaderPattern::parse("{name:ident}({params:parens})[: {ret:type}]").unwrap();

        let with = p.match_header("f(): INTEGER").unwrap();
        assert_eq!(with["ret"], "INTEGER");

        let without = p.match_header("f()").unwrap();
        assert!(!without.contains_key("ret"), "the slot must stay absent");
    }

    #[test]
    fn setof_and_table_type() {
        let p = HeaderPattern::parse("{n:ident}({a:parens}): {ret:type}").unwrap();
        assert_eq!(
            p.match_header("f(): SETOF users").unwrap()["ret"],
            "SETOF users"
        );
        assert_eq!(
            p.match_header("f(): TABLE (id UUID, total INT)").unwrap()["ret"],
            "TABLE (id UUID, total INT)"
        );
    }

    #[test]
    fn parameterized_and_array_type() {
        let p = HeaderPattern::parse("{n:ident}: {ret:type}").unwrap();
        assert_eq!(
            p.match_header("f: NUMERIC(10,2)").unwrap()["ret"],
            "NUMERIC(10,2)"
        );
        assert_eq!(p.match_header("f: TEXT[]").unwrap()["ret"], "TEXT[]");
    }

    #[test]
    fn simple_or_parenthesized_list() {
        let p = HeaderPattern::parse("on {tables:list}").unwrap();
        assert_eq!(p.match_header("on users").unwrap()["tables"], "users");
        assert_eq!(
            p.match_header("on (organizations, users, projects)")
                .unwrap()["tables"],
            "organizations, users, projects"
        );
    }

    #[test]
    fn words_stop_before_on() {
        let p = HeaderPattern::parse("{events:words} on {tables:list}").unwrap();
        let c = p.match_header("insert or update on users").unwrap();
        assert_eq!(c["events"], "insert or update");
        assert_eq!(c["tables"], "users");
    }

    #[test]
    fn literals_are_case_insensitive() {
        let p = HeaderPattern::parse("on {t:list}").unwrap();
        assert_eq!(p.match_header("ON users").unwrap()["t"], "users");
    }

    #[test]
    fn unconsumed_remainder_is_an_error() {
        let p = HeaderPattern::parse("{n:ident}").unwrap();
        let e = p.match_header("f trailing junk").unwrap_err();
        assert!(e.contains("unexpected"), "got: {e}");
    }

    #[test]
    fn missing_literal_is_reported() {
        let p = HeaderPattern::parse("{n:ident} on {t:list}").unwrap();
        let e = p.match_header("f users").unwrap_err();
        assert!(e.contains("expected"), "got: {e}");
    }

    #[test]
    fn unknown_slot_kind_is_rejected() {
        let e = HeaderPattern::parse("{n:machin}").unwrap_err();
        assert!(e.contains("unknown slot kind"), "got: {e}");
        assert!(e.contains("ident"), "must list the kinds: {e}");
    }

    #[test]
    fn unclosed_parenthesis_is_reported() {
        let p = HeaderPattern::parse("{n:ident}({a:parens})").unwrap();
        assert!(p.match_header("f(a INT").unwrap_err().contains("missing"));
    }

    #[test]
    fn parenthesis_inside_string_does_not_count() {
        let p = HeaderPattern::parse("{n:ident}({a:parens})").unwrap();
        let c = p.match_header("f(sep TEXT = ')')").unwrap();
        assert_eq!(c["a"], "sep TEXT = ')'");
    }

    #[test]
    fn slots_lists_slots_including_optional_ones() {
        let p = HeaderPattern::parse("{name:ident}({params:parens})[: {ret:type}]").unwrap();
        let slots = p.slots();
        assert_eq!(
            slots
                .iter()
                .map(|(n, k)| (*n, k.as_str()))
                .collect::<Vec<_>>(),
            vec![("name", "ident"), ("params", "parens"), ("ret", "type")]
        );
    }
}
