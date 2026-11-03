//! Fragments nommés (PLAN.md, P6) : un morceau de SQL écrit une fois —
//! liste de colonnes, expression, prédicat — et inséré partout où il sert.
//!
//! ```sql
//! -- fragment: search_vector(t)
//! -- Doit rester identique à l'expression de l'index GIN.
//! to_tsvector('french'::regconfig, coalesce(t.nom, '') || ' ' || coalesce(t.enseigne, ''))
//! ```
//!
//! - Un fichier `.sql` de `[macros] directory` en déclare un ou plusieurs,
//!   chacun sous un en-tête `-- fragment: nom` ou `-- fragment: nom(t, …)`.
//!   Le corps est du SQL ordinaire ; les commentaires qui suivent l'en-tête
//!   le documentent.
//! - `@nom` l'insère, n'importe où dans un statement. Seul un nom de fragment
//!   déclaré est une invocation : ailleurs, `@` reste l'opérateur valeur
//!   absolue de PostgreSQL.
//! - Les paramètres sont des **qualificatifs** de relation : `@nom(e)`
//!   remplace chaque `t.` du corps par `e.`, `@nom` sans argument le retire
//!   (`t.col` → `col`). Rien d'autre n'est substitué : le format reste du
//!   SQL, pas un langage de gabarits.
//! - Un corps qui est une expression unique non atomique (opérateur, `IS`,
//!   `IN`, `BETWEEN`…) est inséré entre parenthèses, pour que l'opérateur qui
//!   l'entoure ne le découpe pas ; une liste, un élément aliasé ou un atome
//!   (colonne, appel, littéral) est inséré tel quel. La décision vient du
//!   parser, pas d'une heuristique textuelle.
//! - Un fragment peut en utiliser un autre ; un cycle est une erreur.
//! - Le fragment n'est pas analysé seul : il l'est dans chaque statement qui
//!   l'utilise, après insertion, et une erreur y est rapportée à l'invocation.

use std::collections::HashMap;

use crate::lex::{Kind, Token, tokenize};

/// Un fragment déclaré.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FragmentDef {
    pub name: String,
    /// Qualificatifs, dans l'ordre des arguments.
    pub params: Vec<String>,
    /// SQL du corps, sans la documentation ni les blancs de bord.
    pub body: String,
    /// Commentaires entre l'en-tête et le corps, sans `-- `.
    pub doc: Vec<String>,
    /// Fichier de définition.
    pub origin: String,
    /// Ligne de l'en-tête (1-based).
    pub line: usize,
}

fn is_snake(s: &str) -> bool {
    let mut b = s.bytes();
    matches!(b.next(), Some(b'a'..=b'z' | b'_'))
        && b.all(|c| matches!(c, b'a'..=b'z' | b'0'..=b'9' | b'_'))
}

/// En-tête lu : nom et qualificatifs.
type Header = (String, Vec<String>);

/// `nom` ou `nom(a, b)`.
fn parse_header(payload: &str) -> Result<Header, String> {
    let payload = payload.trim();
    let (name, params) = match payload.split_once('(') {
        None => (payload, Vec::new()),
        Some((n, rest)) => {
            let inner = rest
                .trim_end()
                .strip_suffix(')')
                .ok_or("`)` manquante après les qualificatifs")?;
            let params: Vec<String> = inner
                .split(',')
                .map(|p| p.trim().to_string())
                .filter(|p| !p.is_empty())
                .collect();
            if params.is_empty() {
                return Err("`()` sans qualificatif : écrire `-- fragment: nom`".into());
            }
            (n.trim(), params)
        }
    };
    if !is_snake(name) {
        return Err(format!(
            "nom de fragment invalide : `{name}` (snake_case ASCII attendu)"
        ));
    }
    for (i, p) in params.iter().enumerate() {
        if !is_snake(p) {
            return Err(format!("qualificatif invalide : `{p}`"));
        }
        if params[..i].contains(p) {
            return Err(format!("qualificatif `{p}` déclaré deux fois"));
        }
    }
    Ok((name.to_string(), params))
}

fn header_payload<'a>(src: &'a str, t: &Token) -> Option<&'a str> {
    src[t.range.clone()]
        .strip_prefix("--")?
        .trim_start()
        .strip_prefix("fragment:")
}

/// Fragments d'un fichier, et les erreurs de format (positionnées
/// `fichier:ligne`).
pub fn parse_fragment_file(text: &str, origin: &str) -> (Vec<FragmentDef>, Vec<String>) {
    let line_of = |offset: usize| text[..offset].matches('\n').count() + 1;
    let (defs, errors) = parse_fragments(text, origin);
    let errors = errors
        .into_iter()
        .map(|(at, msg)| format!("{origin}:{}: {msg}", line_of(at)))
        .collect();
    (defs, errors)
}

/// Erreurs de format d'un fichier de fragments, positionnées dans le texte
/// (éditeur).
pub fn fragment_file_diagnostics(text: &str) -> Vec<pgweave_diagnostics::Diagnostic> {
    parse_fragments(text, "")
        .1
        .into_iter()
        .map(|(at, msg)| {
            let end = text[at..].find('\n').map_or(text.len(), |k| at + k);
            pgweave_diagnostics::Diagnostic::error(crate::expand::MACRO_ERROR, msg)
                .with_span(pgweave_diagnostics::Span { start: at, end })
        })
        .collect()
}

fn parse_fragments(text: &str, origin: &str) -> (Vec<FragmentDef>, Vec<(usize, String)>) {
    let tokens = tokenize(text);
    let line_of = |offset: usize| text[..offset].matches('\n').count() + 1;
    let at_line_start = |offset: usize| {
        text[..offset]
            .rsplit('\n')
            .next()
            .is_none_or(|l| l.trim().is_empty())
    };

    let mut defs = Vec::new();
    let mut errors = Vec::new();
    // En-têtes : (index du token, nom, qualificatifs).
    let mut headers: Vec<(usize, Result<Header, String>)> = Vec::new();
    for (i, t) in tokens.iter().enumerate() {
        if t.kind == Kind::LineComment
            && let Some(payload) = header_payload(text, t)
        {
            if at_line_start(t.range.start) {
                headers.push((i, parse_header(payload)));
            } else {
                errors.push((
                    t.range.start,
                    "un en-tête `-- fragment:` doit commencer la ligne".to_string(),
                ));
            }
        }
    }
    // Du SQL avant le premier en-tête n'appartient à aucun fragment.
    let first = headers.first().map_or(tokens.len(), |(i, _)| *i);
    if let Some(t) = tokens[..first].iter().find(|t| {
        !matches!(
            t.kind,
            Kind::Whitespace | Kind::LineComment | Kind::BlockComment
        )
    }) {
        errors.push((
            t.range.start,
            "SQL hors d'un fragment (chaque fragment commence par `-- fragment: nom`)".to_string(),
        ));
    }

    for (k, (hi, header)) in headers.iter().enumerate() {
        let at = tokens[*hi].range.start;
        let line = line_of(at);
        let (name, params) = match header {
            Ok(h) => h.clone(),
            Err(e) => {
                errors.push((at, e.clone()));
                continue;
            }
        };
        let end = headers.get(k + 1).map_or(tokens.len(), |(i, _)| *i);
        let span = &tokens[hi + 1..end];
        // Documentation : commentaires de ligne avant le premier token de code.
        let code: Vec<&Token> = span
            .iter()
            .filter(|t| {
                !matches!(
                    t.kind,
                    Kind::Whitespace | Kind::LineComment | Kind::BlockComment
                )
            })
            .collect();
        let (Some(first), Some(last)) = (code.first(), code.last()) else {
            errors.push((at, format!("le fragment `{name}` est vide")));
            continue;
        };
        let doc = span
            .iter()
            .take_while(|t| t.range.start < first.range.start)
            .filter(|t| t.kind == Kind::LineComment)
            .map(|t| {
                let c = &text[t.range.clone()][2..];
                c.strip_prefix(' ').unwrap_or(c).trim_end().to_string()
            })
            .collect();
        if &text[last.range.clone()] == ";" {
            errors.push((
                last.range.start,
                "un fragment n'est pas un statement : retirer le `;`".to_string(),
            ));
            continue;
        }
        defs.push(FragmentDef {
            name,
            params,
            body: text[first.range.start..last.range.end].to_string(),
            doc,
            origin: origin.to_string(),
            line,
        });
    }
    (defs, errors)
}

/// Qualificatif lié pendant un rendu : relation donnée, ou retirée.
type Env = HashMap<String, Option<String>>;

/// Une invocation reconnue dans un texte.
pub(crate) struct Call<'a> {
    pub def: &'a FragmentDef,
    /// Arguments écrits (identifiants), vide sans parenthèses.
    pub args: Vec<String>,
    /// Fin de l'invocation (après `)` ou le nom).
    pub end: usize,
}

/// Invocation de fragment qui commence au token `i` (un `@`), s'il y en a
/// une : `Ok(None)` si ce n'est pas un nom de fragment.
pub(crate) fn call_at<'a>(
    src: &str,
    tokens: &[Token],
    i: usize,
    fragments: &'a HashMap<String, FragmentDef>,
) -> Result<Option<(Call<'a>, usize)>, String> {
    if &src[tokens[i].range.clone()] != "@" {
        return Ok(None);
    }
    let Some(name_tok) = tokens
        .get(i + 1)
        .filter(|t| t.kind == Kind::Word && t.range.start == tokens[i].range.end)
    else {
        return Ok(None);
    };
    let Some(def) = fragments.get(&src[name_tok.range.clone()]) else {
        return Ok(None);
    };
    let mut next = i + 2;
    let mut args = Vec::new();
    if tokens
        .get(next)
        .is_some_and(|t| t.range.start == name_tok.range.end && &src[t.range.clone()] == "(")
    {
        next += 1;
        let mut expect_arg = true;
        loop {
            let Some(t) = tokens.get(next) else {
                return Err(format!("@{} : `)` manquante", def.name));
            };
            next += 1;
            let text = &src[t.range.clone()];
            match t.kind {
                Kind::Whitespace => {}
                Kind::Word | Kind::QuotedIdent if expect_arg => {
                    args.push(text.to_string());
                    expect_arg = false;
                }
                Kind::Punct if text == "," && !expect_arg => expect_arg = true,
                Kind::Punct if text == ")" && (!expect_arg || args.is_empty()) => break,
                _ => {
                    return Err(format!(
                        "@{} : un argument est un nom de relation (alias, table), pas `{text}`",
                        def.name
                    ));
                }
            }
        }
    }
    let end = tokens[next - 1].range.end;
    if !args.is_empty() && args.len() != def.params.len() {
        return Err(format!(
            "@{} attend {} qualificatif(s) ({}) ou aucun, {} donné(s)",
            def.name,
            def.params.len(),
            def.params.join(", "),
            args.len()
        ));
    }
    Ok(Some((Call { def, args, end }, next)))
}

/// SQL inséré pour `call` : corps qualifié, fragments imbriqués rendus,
/// parenthésé s'il le faut.
pub(crate) fn render_call(
    call: &Call,
    env: &Env,
    fragments: &HashMap<String, FragmentDef>,
    stack: &mut Vec<String>,
) -> Result<String, String> {
    // Un argument qui est lui-même un qualificatif du fragment englobant
    // prend sa valeur.
    let mut bound: Env = HashMap::new();
    for (i, p) in call.def.params.iter().enumerate() {
        let v = match call.args.get(i) {
            None => None,
            Some(a) => match env.get(a) {
                Some(outer) => outer.clone(),
                None => Some(a.clone()),
            },
        };
        bound.insert(p.clone(), v);
    }
    let given = bound.values().filter(|v| v.is_some()).count();
    if given != 0 && given != call.def.params.len() {
        return Err(format!(
            "@{} : qualificatifs en partie retirés, en partie donnés",
            call.def.name
        ));
    }
    if stack.contains(&call.def.name) {
        return Err(format!(
            "fragments récursifs : {} → {}",
            stack.join(" → "),
            call.def.name
        ));
    }
    stack.push(call.def.name.clone());
    let body = render_body(&call.def.body, &bound, fragments, stack);
    stack.pop();
    let body = body?;
    Ok(if needs_parens(&body) {
        format!("({body})")
    } else {
        body
    })
}

fn render_body(
    src: &str,
    env: &Env,
    fragments: &HashMap<String, FragmentDef>,
    stack: &mut Vec<String>,
) -> Result<String, String> {
    let tokens = tokenize(src);
    let mut out = String::with_capacity(src.len());
    let mut i = 0;
    while i < tokens.len() {
        let t = &tokens[i];
        let text = &src[t.range.clone()];
        if let Some((call, next)) = call_at(src, &tokens, i, fragments)? {
            out.push_str(&render_call(&call, env, fragments, stack)?);
            i = next;
            continue;
        }
        // `t.` → `e.` ou rien, `t` étant un qualificatif ; jamais après un
        // `.` (`s.t.col` désigne une table `t` du schéma `s`).
        let qualifies = t.kind == Kind::Word
            && tokens
                .get(i + 1)
                .is_some_and(|n| &src[n.range.clone()] == ".")
            && (i == 0 || &src[tokens[i - 1].range.clone()] != ".");
        if qualifies && let Some(v) = env.get(text) {
            if let Some(rel) = v {
                out.push_str(rel);
                out.push('.');
            }
            i += 2;
            continue;
        }
        out.push_str(text);
        i += 1;
    }
    Ok(out)
}

/// Le corps est-il une expression unique dont un opérateur voisin pourrait
/// changer le sens ? Lu par le parser : `SELECT <corps>` à un seul élément
/// non aliasé, d'une autre nature qu'un atome.
fn needs_parens(body: &str) -> bool {
    use pgweave_ast::expr::ExprKind as K;
    let Ok(stmts) = pgweave_parser::parse(&format!("SELECT {body}")) else {
        return false;
    };
    let [pgweave_ast::Statement::Select(s)] = stmts.as_slice() else {
        return false;
    };
    let [pgweave_ast::SelectItem::Expr { expr, alias: None }] = s.columns.as_slice() else {
        return false;
    };
    !matches!(
        expr.kind,
        K::Literal(_)
            | K::Column(_)
            | K::FunctionCall { .. }
            | K::Cast { .. }
            | K::Case { .. }
            | K::Subquery(_)
            | K::Exists { .. }
            | K::Tuple(_)
            | K::FieldAccess { .. }
            | K::Parameter(_)
            | K::Interval(_)
            | K::Wildcard
            | K::QualifiedWildcard(_)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(defs: &str) -> HashMap<String, FragmentDef> {
        let (defs, errors) = parse_fragment_file(defs, "f.sql");
        assert!(errors.is_empty(), "{errors:?}");
        defs.into_iter().map(|d| (d.name.clone(), d)).collect()
    }

    fn expand(src: &str, fragments: &HashMap<String, FragmentDef>) -> Result<String, String> {
        render_body(src, &HashMap::new(), fragments, &mut Vec::new())
    }

    #[test]
    fn en_tetes_documentation_et_corps() {
        let (defs, errors) = parse_fragment_file(
            "-- Préambule libre.\n\n\
             -- fragment: cols(t)\n-- Les colonnes.\nt.a, t.b -- fin\n\n\
             -- fragment: actif\nstatus = 'actif'\n",
            "f.sql",
        );
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(defs.len(), 2);
        assert_eq!(defs[0].name, "cols");
        assert_eq!(defs[0].params, vec!["t"]);
        assert_eq!(defs[0].doc, vec!["Les colonnes."]);
        assert_eq!(defs[0].body, "t.a, t.b");
        assert_eq!(defs[0].line, 3);
        assert_eq!(defs[1].body, "status = 'actif'");
    }

    #[test]
    fn erreurs_de_format() {
        for (src, attendu) in [
            ("SELECT 1;\n-- fragment: x\na", "hors d'un fragment"),
            ("-- fragment: X\na", "nom de fragment invalide"),
            ("-- fragment: x(t, t)\na", "deux fois"),
            ("-- fragment: x\n", "vide"),
            ("-- fragment: x\nSELECT 1;", "retirer le `;`"),
            ("SELECT 1 -- fragment: x\n", "commencer la ligne"),
        ] {
            let (_, errors) = parse_fragment_file(src, "f.sql");
            assert!(
                errors.iter().any(|e| e.contains(attendu)),
                "{src:?} : {errors:?}"
            );
        }
    }

    #[test]
    fn qualificatifs_substitues_ou_retires() {
        let f = set("-- fragment: cols(t)\nt.a, t.\"B\", s.t.c, t");
        assert_eq!(
            expand("SELECT @cols(e)", &f).unwrap(),
            "SELECT e.a, e.\"B\", s.t.c, t"
        );
        assert_eq!(
            expand("SELECT @cols", &f).unwrap(),
            "SELECT a, \"B\", s.t.c, t"
        );
        assert_eq!(
            expand("SELECT @cols(\"Ma table\")", &f).unwrap(),
            "SELECT \"Ma table\".a, \"Ma table\".\"B\", s.t.c, t"
        );
    }

    #[test]
    fn expression_parenthesee_atome_et_liste_inseres_tels_quels() {
        let f = set("-- fragment: p\na = 1 OR b = 2\n\
             -- fragment: f(t)\nlower(t.nom)\n\
             -- fragment: l\na, b\n\
             -- fragment: alias\nlower(nom) AS n\n\
             -- fragment: ordre\ncreated_at DESC");
        assert_eq!(
            expand("WHERE @p AND c", &f).unwrap(),
            "WHERE (a = 1 OR b = 2) AND c"
        );
        assert_eq!(expand("@f(x) = 'a'", &f).unwrap(), "lower(x.nom) = 'a'");
        assert_eq!(expand("SELECT @l", &f).unwrap(), "SELECT a, b");
        assert_eq!(
            expand("SELECT @alias", &f).unwrap(),
            "SELECT lower(nom) AS n"
        );
        assert_eq!(
            expand("ORDER BY @ordre", &f).unwrap(),
            "ORDER BY created_at DESC"
        );
    }

    #[test]
    fn imbrication_et_cycle() {
        let f = set("-- fragment: inner(u)\nu.a\n\
             -- fragment: outer(t)\n@inner(t), t.b\n\
             -- fragment: x\n@y\n-- fragment: y\n@x");
        assert_eq!(expand("SELECT @outer(e)", &f).unwrap(), "SELECT e.a, e.b");
        assert_eq!(expand("SELECT @outer", &f).unwrap(), "SELECT a, b");
        let e = expand("SELECT @x", &f).unwrap_err();
        assert!(e.contains("récursifs"), "{e}");
    }

    #[test]
    fn arobase_hors_fragment_inchange() {
        let f = set("-- fragment: cols\na");
        // Valeur absolue, nom inconnu, chaîne, identifiant quoté.
        let src = "SELECT @ -5, @x, '@cols', \"@cols\"";
        assert_eq!(expand(src, &f).unwrap(), src);
    }

    #[test]
    fn arguments_verifies() {
        let f = set("-- fragment: j(a, b)\na.x = b.x");
        assert!(expand("@j(e)", &f).unwrap_err().contains("attend 2"));
        assert!(
            expand("@j(e, 1)", &f)
                .unwrap_err()
                .contains("nom de relation")
        );
        assert!(expand("@j(e", &f).unwrap_err().contains("manquante"));
    }
}
