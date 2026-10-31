//! Motif d'en-tête : ce qui suit `@nom` jusqu'à l'accolade ouvrante.
//!
//! Le jeu de types de trou est **fermé** et volontairement petit. C'est ce qui
//! distingue ce format d'un langage de gabarits : on ne peut pas exprimer une
//! grammaire arbitraire, seulement assembler quelques formes qui couvrent le
//! boilerplate SQL réel.
//!
//! Motif : littéraux, trous `{nom:type}`, groupes optionnels entre `[ ]`.
//!
//! ```text
//! {name:ident}({params:parens})[: {ret:type}]
//! [{name:ident} ]{timing:word} {events:words} on {tables:list} for each {each:word}
//! ```

use std::collections::HashMap;

/// Ce qu'un trou sait consommer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotKind {
    /// Un identifiant, éventuellement qualifié : `users`, `public.users`.
    Ident,
    /// Un mot unique : `before`, `row`.
    Word,
    /// Plusieurs mots séparés par des espaces ou `or` : `insert or update`.
    Words,
    /// Le contenu jusqu'à la parenthèse fermante correspondante, recopié
    /// verbatim, imbrication et chaînes respectées : `a INT, b NUMERIC(10,2)`.
    ///
    /// Le trou prend le **contenu**, le motif porte les délimiteurs :
    /// `({params:parens})`. Le contenu peut être vide.
    Parens,
    /// Un type SQL, y compris `SETOF x` et `TABLE (...)`.
    Type,
    /// Un identifiant, ou une liste entre parenthèses : `users` / `(a, b, c)`.
    List,
    /// Tout le reste jusqu'à l'accolade — suite de mots-clés.
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
    /// Groupe facultatif : consommé s'il correspond, ignoré sinon.
    Optional(Vec<Piece>),
}

/// Motif d'en-tête compilé.
#[derive(Debug, Clone)]
pub struct HeaderPattern {
    pieces: Vec<Piece>,
}

impl HeaderPattern {
    pub fn parse(pattern: &str) -> Result<Self, String> {
        let (pieces, rest) = parse_pieces(pattern, false)?;
        if !rest.is_empty() {
            return Err(format!("`]` sans `[` près de {rest:?}"));
        }
        Ok(Self { pieces })
    }

    /// Les trous déclarés, dans l'ordre.
    pub fn slots(&self) -> Vec<(&str, SlotKind)> {
        let mut out = Vec::new();
        collect_slots(&self.pieces, &mut out);
        out
    }

    /// Applique le motif à `input`, qui est l'en-tête brut de l'invocation.
    ///
    /// Renvoie les captures. Le motif doit consommer tout `input` : un reste
    /// non consommé est une erreur de syntaxe côté utilisateur, pas un succès
    /// partiel silencieux.
    pub fn match_header(&self, input: &str) -> Result<HashMap<String, String>, String> {
        let mut caps = HashMap::new();
        let rest = match_pieces(&self.pieces, input.trim(), &mut caps)?;
        if !rest.trim().is_empty() {
            return Err(format!(
                "texte inattendu dans l'en-tête : {:?}",
                rest.trim()
            ));
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
                    .ok_or_else(|| format!("`{{` sans `}}` près de {rest:?}"))?;
                let inner = &rest[1..end];
                let (name, kind_str) = inner
                    .split_once(':')
                    .ok_or_else(|| format!("trou {inner:?} : attendu `{{nom:type}}`"))?;
                let kind = SlotKind::parse(kind_str.trim()).ok_or_else(|| {
                    format!(
                        "type de trou inconnu {:?} ; disponibles : ident, word, words, parens, type, list, keywords",
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
        return Err("`[` sans `]`".to_string());
    }
    flush!();
    Ok((pieces, rest))
}

/// Applique une suite de pièces, renvoie le reste non consommé.
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
                    return Err(format!("attendu {want:?}, trouvé {:?}", head(r)));
                }
                rest = &r[want.len()..];
            }
            Piece::Slot { name, kind } => {
                let (value, r) = consume_slot(*kind, rest.trim_start())
                    .map_err(|e| format!("trou `{name}` : {e}"))?;
                caps.insert(name.clone(), value);
                rest = r;
            }
            Piece::Optional(inner) => {
                // Tentative : si le groupe ne correspond pas, on l'ignore sans
                // consommer. On travaille sur une copie des captures pour ne
                // pas laisser de trace d'un essai raté.
                let mut trial = caps.clone();
                match match_pieces(inner, rest, &mut trial) {
                    Ok(r) => {
                        // Un groupe optionnel qui réussit en ne consommant rien
                        // boucherait la suite : on l'accepte quand même, il est
                        // simplement sans effet.
                        *caps = trial;
                        rest = r;
                    }
                    Err(_) => {
                        // Groupe absent : si des pièces obligatoires suivent,
                        // elles diront elles-mêmes ce qui manque.
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
                return Err(format!("identifiant attendu, trouvé {:?}", head(s)));
            }
            Ok((s[..end].to_string(), &s[end..]))
        }
        SlotKind::Word => {
            let end = s.find(char::is_whitespace).unwrap_or(s.len());
            if end == 0 {
                return Err("mot attendu".to_string());
            }
            Ok((s[..end].to_string(), &s[end..]))
        }
        SlotKind::Words => {
            // Mots et `or`, jusqu'à un mot-clé qui n'en fait plus partie.
            // On s'arrête devant `on`, qui suit toujours la liste d'événements.
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
                return Err("au moins un mot attendu".to_string());
            }
            Ok((words.join(" "), &s[taken..]))
        }
        SlotKind::Parens => {
            // On est déjà passé la parenthèse ouvrante (littéral du motif) :
            // on consomme jusqu'à la fermante correspondante, exclue.
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
            // Type simple, éventuellement paramétré ou en tableau.
            let end = s
                .find(|c: char| !(c.is_alphanumeric() || c == '_' || c == '.'))
                .unwrap_or(s.len());
            if end == 0 {
                return Err(format!("type attendu, trouvé {:?}", head(s)));
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
                    return Err("liste vide".to_string());
                }
                return Ok((items.join(", "), &s[consumed..]));
            }
            consume_slot(SlotKind::Ident, s)
        }
        SlotKind::Keywords => Ok((s.trim().to_string(), "")),
    }
}

/// Offset de la parenthèse fermante non appariée qui termine le groupe courant.
///
/// `s` commence juste après une parenthèse ouvrante déjà consommée. On avance
/// en suivant l'imbrication et en ignorant le contenu des chaînes, jusqu'au `)`
/// qui referme le groupe.
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
    Err("`)` manquant".to_string())
}

/// Contenu entre `open` et son `close` correspondant, chaînes et imbrication
/// respectées. `s` doit commencer par `open`.
fn balanced(s: &str, open: char, close: char) -> Result<String, String> {
    let mut chars = s.char_indices();
    match chars.next() {
        Some((_, c)) if c == open => {}
        _ => return Err(format!("`{open}` attendu, trouvé {:?}", head(s))),
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
    Err(format!("`{close}` manquant"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(pattern: &str, input: &str) -> HashMap<String, String> {
        HeaderPattern::parse(pattern)
            .expect("motif")
            .match_header(input)
            .expect("correspondance")
    }

    #[test]
    fn nom_et_parametres() {
        let c = caps(
            "{name:ident}({params:parens})",
            "user_count(target_org UUID)",
        );
        assert_eq!(c["name"], "user_count");
        assert_eq!(c["params"], "target_org UUID");
    }

    #[test]
    fn parentheses_imbriquees_dans_les_parametres() {
        let c = caps(
            "{name:ident}({params:parens})",
            "f(a NUMERIC(10,2), b TEXT[])",
        );
        assert_eq!(c["params"], "a NUMERIC(10,2), b TEXT[]");
    }

    #[test]
    fn groupe_optionnel_present_et_absent() {
        let p = HeaderPattern::parse("{name:ident}({params:parens})[: {ret:type}]").unwrap();

        let with = p.match_header("f(): INTEGER").unwrap();
        assert_eq!(with["ret"], "INTEGER");

        let without = p.match_header("f()").unwrap();
        assert!(!without.contains_key("ret"), "le trou doit rester absent");
    }

    #[test]
    fn type_setof_et_table() {
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
    fn type_parametre_et_tableau() {
        let p = HeaderPattern::parse("{n:ident}: {ret:type}").unwrap();
        assert_eq!(
            p.match_header("f: NUMERIC(10,2)").unwrap()["ret"],
            "NUMERIC(10,2)"
        );
        assert_eq!(p.match_header("f: TEXT[]").unwrap()["ret"], "TEXT[]");
    }

    #[test]
    fn liste_simple_ou_parenthesee() {
        let p = HeaderPattern::parse("on {tables:list}").unwrap();
        assert_eq!(p.match_header("on users").unwrap()["tables"], "users");
        assert_eq!(
            p.match_header("on (organizations, users, projects)")
                .unwrap()["tables"],
            "organizations, users, projects"
        );
    }

    #[test]
    fn les_mots_sarretent_devant_on() {
        let p = HeaderPattern::parse("{events:words} on {tables:list}").unwrap();
        let c = p.match_header("insert or update on users").unwrap();
        assert_eq!(c["events"], "insert or update");
        assert_eq!(c["tables"], "users");
    }

    #[test]
    fn les_litteraux_sont_insensibles_a_la_casse() {
        let p = HeaderPattern::parse("on {t:list}").unwrap();
        assert_eq!(p.match_header("ON users").unwrap()["t"], "users");
    }

    #[test]
    fn un_reste_non_consomme_est_une_erreur() {
        let p = HeaderPattern::parse("{n:ident}").unwrap();
        let e = p.match_header("f trailing junk").unwrap_err();
        assert!(e.contains("inattendu"), "obtenu: {e}");
    }

    #[test]
    fn un_litteral_manquant_est_signale() {
        let p = HeaderPattern::parse("{n:ident} on {t:list}").unwrap();
        let e = p.match_header("f users").unwrap_err();
        assert!(e.contains("attendu"), "obtenu: {e}");
    }

    #[test]
    fn un_type_de_trou_inconnu_est_refuse() {
        let e = HeaderPattern::parse("{n:machin}").unwrap_err();
        assert!(e.contains("type de trou inconnu"), "obtenu: {e}");
        assert!(e.contains("ident"), "doit lister les types: {e}");
    }

    #[test]
    fn une_parenthese_non_fermee_est_signalee() {
        let p = HeaderPattern::parse("{n:ident}({a:parens})").unwrap();
        assert!(p.match_header("f(a INT").unwrap_err().contains("manquant"));
    }

    #[test]
    fn une_parenthese_dans_une_chaine_ne_compte_pas() {
        let p = HeaderPattern::parse("{n:ident}({a:parens})").unwrap();
        let c = p.match_header("f(sep TEXT = ')')").unwrap();
        assert_eq!(c["a"], "sep TEXT = ')'");
    }

    #[test]
    fn slots_enumere_les_trous_y_compris_optionnels() {
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
