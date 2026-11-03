//! Requêtes écrites dans un fichier Rust (`sql!`, `query!`) : extraites pour
//! être analysées comme une requête nommée, avec les positions du fichier.
//!
//! Un balayage lexical, pas un parse Rust : un fichier en cours de frappe
//! reste exploitable. Commentaires, chaînes, chaînes brutes et caractères
//! sont sautés ; dans une invocation, les attributs, la visibilité, le nom,
//! la cardinalité et la ligne reprise sont lus, puis le littéral de la
//! requête, décodé (échappements compris) avec la correspondance exacte de
//! chaque octet vers le fichier.

use std::ops::Range;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// `sql! { pub nom: cardinalité -> Chemin, "…" }`.
    Sql,
    /// `query!("…", liaisons)`.
    Query,
}

/// Une requête trouvée dans le fichier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Embedded {
    pub kind: Kind,
    /// Nom écrit (`sql!`) et sa plage.
    pub name: Option<(String, Range<usize>)>,
    pub cardinality: Option<String>,
    /// SQL décodé.
    pub sql: String,
    /// Plage du contenu du littéral dans le fichier.
    pub content: Range<usize>,
    /// Segments (position dans `sql`, position dans le fichier, longueur) :
    /// le texte recopié tel quel. Un échappement n'en fait pas partie.
    segments: Vec<(usize, usize, usize)>,
}

impl Embedded {
    /// Position dans le fichier d'une position du SQL.
    pub fn to_file(&self, sql_offset: usize) -> usize {
        for &(s, f, len) in self.segments.iter().rev() {
            if sql_offset >= s {
                return f + (sql_offset - s).min(len);
            }
        }
        self.content.start
    }

    /// Position dans le SQL d'une position du fichier, si elle est dans le
    /// littéral.
    pub fn to_sql(&self, file_offset: usize) -> Option<usize> {
        if !(self.content.start..=self.content.end).contains(&file_offset) {
            return None;
        }
        for &(s, f, len) in self.segments.iter().rev() {
            if file_offset >= f {
                return Some(s + (file_offset - f).min(len));
            }
        }
        Some(0)
    }
}

/// Fin du commentaire, de la chaîne ou du caractère qui commence à `i`, s'il
/// y en a un.
fn skip_trivia(b: &[u8], i: usize) -> Option<usize> {
    let at = |k: usize| b.get(k).copied();
    match (at(i), at(i + 1)) {
        (Some(b'/'), Some(b'/')) => Some(
            b[i..]
                .iter()
                .position(|&c| c == b'\n')
                .map_or(b.len(), |k| i + k),
        ),
        (Some(b'/'), Some(b'*')) => {
            let (mut depth, mut k) = (0usize, i);
            while k < b.len() {
                if b[k..].starts_with(b"/*") {
                    depth += 1;
                    k += 2;
                } else if b[k..].starts_with(b"*/") {
                    depth -= 1;
                    k += 2;
                    if depth == 0 {
                        return Some(k);
                    }
                } else {
                    k += 1;
                }
            }
            Some(b.len())
        }
        _ => string_at(b, i)
            .map(|(_, end)| end)
            .or_else(|| char_at(b, i)),
    }
}

/// Littéral `'x'`, `'\n'`, `'\u{…}'` ; `None` pour une durée de vie (`'a`).
fn char_at(b: &[u8], i: usize) -> Option<usize> {
    if b.get(i) != Some(&b'\'') {
        return None;
    }
    if b.get(i + 1) == Some(&b'\\') {
        let close = b[i + 2..].iter().position(|&c| c == b'\'')?;
        return Some(i + 2 + close + 1);
    }
    let ch = std::str::from_utf8(&b[i + 1..(i + 5).min(b.len())])
        .ok()
        .or_else(|| std::str::from_utf8(&b[i + 1..(i + 2).min(b.len())]).ok())?
        .chars()
        .next()?;
    let after = i + 1 + ch.len_utf8();
    (b.get(after) == Some(&b'\'')).then_some(after + 1)
}

/// Littéral chaîne qui commence à `i` (`"…"`, `r#"…"#`, `b"…"`, `br"…"`) :
/// (plage du contenu, fin).
fn string_at(b: &[u8], i: usize) -> Option<(Range<usize>, usize)> {
    let mut k = i;
    if b.get(k) == Some(&b'b') {
        k += 1;
    }
    let raw = b.get(k) == Some(&b'r');
    if raw {
        k += 1;
        let hashes = b[k..].iter().take_while(|&&c| c == b'#').count();
        k += hashes;
        if b.get(k) != Some(&b'"') {
            return None;
        }
        let start = k + 1;
        let mut close = vec![b'"'];
        close.extend(std::iter::repeat_n(b'#', hashes));
        let end = b[start..]
            .windows(close.len())
            .position(|w| w == close.as_slice())
            .map_or(b.len(), |p| start + p);
        return Some((start..end, (end + close.len()).min(b.len())));
    }
    if b.get(k) != Some(&b'"') {
        return None;
    }
    // Un `b` ou `r` isolé n'ouvre une chaîne que s'il n'est pas la fin d'un
    // identifiant ; l'appelant ne l'appelle qu'en début de token.
    let start = k + 1;
    let mut j = start;
    while j < b.len() {
        match b[j] {
            b'\\' => j += 2,
            b'"' => return Some((start..j, j + 1)),
            _ => j += 1,
        }
    }
    Some((start..b.len(), b.len()))
}

/// Contenu d'un littéral décodé, et ses segments recopiés.
fn decode(src: &str, content: Range<usize>, raw: bool) -> (String, Vec<(usize, usize, usize)>) {
    if raw {
        return (
            src[content.clone()].to_string(),
            vec![(0, content.start, content.len())],
        );
    }
    let mut out = String::new();
    let mut segments = Vec::new();
    let text = &src[content.clone()];
    let mut run_start = 0usize;
    let mut chars = text.char_indices().peekable();
    let flush =
        |out: &mut String, segments: &mut Vec<(usize, usize, usize)>, from: usize, to: usize| {
            if to > from {
                segments.push((out.len(), content.start + from, to - from));
                out.push_str(&text[from..to]);
            }
        };
    while let Some((i, c)) = chars.next() {
        if c != '\\' {
            continue;
        }
        flush(&mut out, &mut segments, run_start, i);
        let Some((_, e)) = chars.next() else {
            run_start = text.len();
            break;
        };
        let mut next = i + 1 + e.len_utf8();
        match e {
            'n' => out.push('\n'),
            't' => out.push('\t'),
            'r' => out.push('\r'),
            '0' => out.push('\0'),
            '\\' | '\'' | '"' => out.push(e),
            '\n' => {
                // Continuation de ligne : le saut et l'indentation suivante
                // disparaissent.
                while let Some(&(k, w)) = chars.peek() {
                    if !w.is_whitespace() {
                        break;
                    }
                    chars.next();
                    next = k + w.len_utf8();
                }
            }
            'x' => {
                let hex = text.get(next..next + 2).unwrap_or("");
                if let Ok(v) = u8::from_str_radix(hex, 16) {
                    out.push(v as char);
                    chars.next();
                    chars.next();
                    next += 2;
                }
            }
            'u' => {
                if let Some(close) = text[next..].find('}') {
                    let hex: String = text[next + 1..next + close]
                        .chars()
                        .filter(|c| *c != '_')
                        .collect();
                    if let Some(ch) = u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
                        out.push(ch);
                    }
                    while chars.peek().is_some_and(|&(k, _)| k <= next + close) {
                        chars.next();
                    }
                    next += close + 1;
                }
            }
            other => out.push(other),
        }
        run_start = next;
    }
    flush(&mut out, &mut segments, run_start, text.len());
    (out, segments)
}

fn is_ident_byte(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

/// Requêtes `sql!` et `query!` de `src`.
pub fn find(src: &str) -> Vec<Embedded> {
    let b = src.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < b.len() {
        let token_start = i == 0 || !is_ident_byte(b[i - 1]);
        if token_start && let Some(end) = skip_trivia(b, i) {
            i = end.max(i + 1);
            continue;
        }
        if token_start && b[i].is_ascii_alphabetic() {
            let end = i + b[i..].iter().take_while(|&&c| is_ident_byte(c)).count();
            let word = &src[i..end];
            if (word == "sql" || word == "query")
                && let Some((open, close)) = invocation_args(b, end)
                && let Some(e) = parse_args(src, word, open, close)
            {
                out.push(e);
                i = close;
                continue;
            }
            i = end;
            continue;
        }
        i += 1;
    }
    out
}

/// `! (` après le nom de la macro : plage des arguments (après le délimiteur
/// ouvrant, jusqu'au fermant, ou la fin du fichier en cours de frappe).
fn invocation_args(b: &[u8], mut k: usize) -> Option<(usize, usize)> {
    while b.get(k).is_some_and(|c| c.is_ascii_whitespace()) {
        k += 1;
    }
    if b.get(k) != Some(&b'!') {
        return None;
    }
    k += 1;
    while b.get(k).is_some_and(|c| c.is_ascii_whitespace()) {
        k += 1;
    }
    let close = match b.get(k)? {
        b'(' => b')',
        b'{' => b'}',
        b'[' => b']',
        _ => return None,
    };
    let open = k + 1;
    let mut depth = 0usize;
    let mut j = open;
    while j < b.len() {
        if let Some(end) =
            skip_trivia(b, j).filter(|_| !is_ident_byte(b[j.saturating_sub(1)]) || j == open)
        {
            j = end.max(j + 1);
            continue;
        }
        match b[j] {
            b'(' | b'{' | b'[' => depth += 1,
            c if c == close && depth == 0 => return Some((open, j)),
            b')' | b'}' | b']' => depth = depth.saturating_sub(1),
            _ => {}
        }
        j += 1;
    }
    Some((open, b.len()))
}

fn skip_ws_and_attrs(src: &str, mut k: usize, end: usize) -> usize {
    let b = src.as_bytes();
    loop {
        while k < end && b[k].is_ascii_whitespace() {
            k += 1;
        }
        if src[k..end].starts_with("//") || src[k..end].starts_with("/*") {
            k = skip_trivia(b, k).unwrap_or(end).min(end);
            continue;
        }
        if b.get(k) == Some(&b'#') {
            // `#[…]` : jusqu'au crochet fermant équilibré.
            let mut depth = 0usize;
            let mut j = k + 1;
            while j < end {
                match b[j] {
                    b'[' => depth += 1,
                    b']' => {
                        depth -= 1;
                        if depth == 0 {
                            j += 1;
                            break;
                        }
                    }
                    _ => {}
                }
                j += 1;
            }
            k = j;
            continue;
        }
        return k;
    }
}

fn ident_at(src: &str, k: usize, end: usize) -> Option<(String, Range<usize>)> {
    let b = src.as_bytes();
    let len = b[k..end].iter().take_while(|&&c| is_ident_byte(c)).count();
    (len > 0 && !b[k].is_ascii_digit()).then(|| (src[k..k + len].to_string(), k..k + len))
}

fn parse_args(src: &str, word: &str, open: usize, close: usize) -> Option<Embedded> {
    let b = src.as_bytes();
    let mut k = skip_ws_and_attrs(src, open, close);
    let (kind, name, cardinality) = if word == "sql" {
        // Visibilité.
        if src[k..close].starts_with("pub") && !b.get(k + 3).is_some_and(|c| is_ident_byte(*c)) {
            k += 3;
            while k < close && b[k].is_ascii_whitespace() {
                k += 1;
            }
            if b.get(k) == Some(&b'(') {
                k += b[k..close].iter().position(|&c| c == b')')? + 1;
            }
            k = skip_ws_and_attrs(src, k, close);
        }
        let name = ident_at(src, k, close)?;
        k = name.1.end;
        k = skip_ws_and_attrs(src, k, close);
        let mut cardinality = None;
        if b.get(k) == Some(&b':') && b.get(k + 1) != Some(&b':') {
            k = skip_ws_and_attrs(src, k + 1, close);
            if let Some((c, r)) = ident_at(src, k, close) {
                cardinality = Some(c);
                k = r.end;
            }
        }
        // `-> Chemin` puis la virgule.
        let comma = b[k..close].iter().position(|&c| c == b',')?;
        k = skip_ws_and_attrs(src, k + comma + 1, close);
        (Kind::Sql, Some(name), cardinality)
    } else {
        (Kind::Query, None, None)
    };
    let raw = matches!(b.get(k), Some(b'r')) || b[k..close.min(k + 2)] == *b"br";
    let (content, _) = string_at(b, k)?;
    let (sql, segments) = decode(src, content.clone(), raw);
    Some(Embedded {
        kind,
        name,
        cardinality,
        sql,
        content,
        segments,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requetes_sql_et_query_trouvees() {
        let src = r##"
// sql!("pas une requête")
fn f() {
    let _ = "query!(\"non plus\")";
    let c = 'x';
    pgweave_macros::sql! {
        /// La fiche.
        #[allow(dead_code)]
        pub(crate) fiche: optional -> crate::q::FicheRow,
        r#"SELECT id
           FROM t WHERE id = $id"#
    }
    let r = query!("SELECT \"x\" FROM t WHERE a = $a", a = g(1));
}
"##;
        let found = find(src);
        assert_eq!(found.len(), 2, "{found:?}");
        let s = &found[0];
        assert_eq!(s.kind, Kind::Sql);
        assert_eq!(s.name.as_ref().unwrap().0, "fiche");
        assert_eq!(&src[s.name.clone().unwrap().1], "fiche");
        assert_eq!(s.cardinality.as_deref(), Some("optional"));
        assert!(s.sql.starts_with("SELECT id\n"));
        let at = s.sql.find("$id").unwrap();
        assert_eq!(&src[s.to_file(at)..s.to_file(at) + 3], "$id");
        assert_eq!(s.to_sql(s.to_file(at)), Some(at));

        let q = &found[1];
        assert_eq!(q.kind, Kind::Query);
        assert_eq!(q.sql, "SELECT \"x\" FROM t WHERE a = $a");
        // Après l'échappement, les positions restent exactes.
        let at = q.sql.find("FROM").unwrap();
        assert_eq!(&src[q.to_file(at)..q.to_file(at) + 4], "FROM");
        assert_eq!(q.to_sql(q.to_file(at)), Some(at));
    }

    #[test]
    fn fichier_en_cours_de_frappe() {
        let found = find("sql! { pub x, \"SELECT nom FROM t");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].sql, "SELECT nom FROM t");
        assert!(find("let sql = 1; query(x);").is_empty());
    }
}
