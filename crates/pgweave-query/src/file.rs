//! Format de fichier des requêtes nommées (`docs/named-queries.md`).
//!
//! Le découpage repose sur le tokenizer de sqlparser, pas sur un balayage de
//! lignes : un `-- name:` à l'intérieur d'une chaîne ou d'un corps `$$…$$`
//! n'est pas un en-tête, et un `$name` dans une chaîne n'est pas un
//! paramètre. Le tokenizer émet aussi les espaces et commentaires, donc les
//! tokens pavent la source : la fin d'un token est le début du suivant.

use std::collections::HashMap;
use std::ops::Range;

use pgweave_diagnostics::{Diagnostic, Span, codes};
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::tokenizer::{Token, Tokenizer, Whitespace};

use crate::line_index::LineIndex;

/// Nombre de lignes renvoyées, déclaré dans l'en-tête (`-- name: x :one`).
///
/// Optionnel : à partir de P4, la cardinalité est inférée et la déclaration
/// sert de contrat vérifié contre l'inférence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Cardinality {
    /// Exactement une ligne.
    One,
    /// Zéro ou une ligne.
    Optional,
    /// Zéro, une ou plusieurs lignes.
    Many,
    /// Aucune ligne renvoyée (statement sans `RETURNING`).
    None,
}

impl Cardinality {
    pub const ALL: [Cardinality; 4] = [
        Cardinality::One,
        Cardinality::Optional,
        Cardinality::Many,
        Cardinality::None,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Cardinality::One => "one",
            Cardinality::Optional => "optional",
            Cardinality::Many => "many",
            Cardinality::None => "none",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.as_str() == s)
    }
}

/// Paramètre nommé et ses occurrences dans le fichier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamedParam {
    pub name: String,
    /// Plages octets de chaque `$name` dans le fichier, dans l'ordre ; une
    /// occurrence marquée `$name?` inclut le `?`.
    pub occurrences: Vec<Range<usize>>,
    /// `$name?` : la valeur peut être absente (NULL, `Option` en Rust), et
    /// la requête est écrite pour (`$name::type IS NULL OR …`).
    pub optional: bool,
}

/// Paramètres d'une requête. Les deux notations ne se mélangent pas.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Params {
    None,
    /// `$1 … $n`, sans trou.
    Positional {
        count: u32,
    },
    /// `$name`, dans l'ordre de première apparition — c'est l'ordre des
    /// paramètres positionnels après réécriture.
    Named(Vec<NamedParam>),
}

/// SQL d'une requête après insertion de ses fragments (P6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpandedSql {
    pub sql: String,
    /// Positions du SQL inséré → positions dans le fichier.
    pub map: pgweave_ast::SourceMap,
}

/// Une requête nommée, telle qu'écrite dans le fichier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamedQuery {
    pub name: String,
    /// Plage du nom dans l'en-tête.
    pub name_range: Range<usize>,
    pub cardinality: Option<Cardinality>,
    /// `-> autre` (même fichier) ou `-> a::b::autre` (depuis la racine du
    /// répertoire de requêtes) : les lignes ont la forme de celles de cette
    /// requête (même struct générée), et sa plage dans l'en-tête.
    pub row: Option<(String, Range<usize>)>,
    /// Lignes de commentaire entre l'en-tête et le SQL, sans le `-- `.
    pub doc: Vec<String>,
    /// Le statement, sans le `;` final.
    pub sql: String,
    /// Plage de `sql` dans le fichier.
    pub sql_range: Range<usize>,
    /// Fragments insérés : le SQL analysé et envoyé. `None` sans fragment.
    pub expanded: Option<ExpandedSql>,
    /// Paramètres du SQL envoyé. Avec des fragments, leurs occurrences sont
    /// des positions dans [`ExpandedSql::sql`] décalées de
    /// `sql_range.start` ; sinon, des positions dans le fichier.
    pub params: Params,
}

impl NamedQuery {
    /// SQL analysé : fragments insérés s'il y en a.
    pub fn text(&self) -> &str {
        self.expanded.as_ref().map_or(&self.sql, |e| &e.sql)
    }

    /// [`Self::text`], marqueurs `?` des paramètres optionnels remplacés par
    /// une espace : SQL lisible par le parser, positions inchangées.
    pub fn analysis_text(&self) -> String {
        let mut bytes = self.text().as_bytes().to_vec();
        if let Params::Named(params) = &self.params {
            for r in params.iter().flat_map(|p| &p.occurrences) {
                let end = r.end - self.sql_range.start;
                if bytes.get(end.wrapping_sub(1)) == Some(&b'?') {
                    bytes[end - 1] = b' ';
                }
            }
        }
        String::from_utf8(bytes).expect("un `?` remplacé par une espace")
    }

    /// Le SQL à envoyer au serveur : chaque `$name` remplacé par `$n`, `n`
    /// étant le rang de première apparition du nom. Inchangé pour une
    /// requête positionnelle ou sans paramètre.
    pub fn positional_sql(&self) -> String {
        let text = self.text();
        let Params::Named(params) = &self.params else {
            return text.to_string();
        };
        let mut replacements: Vec<(Range<usize>, usize)> = params
            .iter()
            .enumerate()
            .flat_map(|(i, p)| p.occurrences.iter().map(move |r| (r.clone(), i + 1)))
            .collect();
        replacements.sort_by_key(|(r, _)| r.start);

        let base = self.sql_range.start;
        let mut out = String::with_capacity(text.len());
        let mut cursor = 0;
        for (range, n) in replacements {
            out.push_str(&text[cursor..range.start - base]);
            out.push_str(&format!("${n}"));
            cursor = range.end - base;
        }
        out.push_str(&text[cursor..]);
        out
    }
}

/// Résultat du découpage d'un fichier.
#[derive(Debug, Clone)]
pub struct QueryFile {
    /// Requêtes valides, dans l'ordre du fichier. Une requête dont l'en-tête
    /// est invalide n'y figure pas (elle produit un diagnostic).
    pub queries: Vec<NamedQuery>,
    pub diagnostics: Vec<Diagnostic>,
}

/// Découpe un fichier de requêtes nommées et vérifie son format. Sans
/// fragments : un `@nom` reste tel quel (voir [`parse_query_file_with`]).
pub fn parse_query_file(source: &str) -> QueryFile {
    parse_query_file_impl(source, None)
}

/// Comme [`parse_query_file`], les fragments de `macros` insérés dans
/// chaque requête (P6). Les diagnostics restent positionnés dans le fichier.
pub fn parse_query_file_with(source: &str, macros: &pgweave_macro::MacroSet) -> QueryFile {
    parse_query_file_impl(source, Some(macros))
}

fn parse_query_file_impl(source: &str, macros: Option<&pgweave_macro::MacroSet>) -> QueryFile {
    let mut diagnostics = Vec::new();
    let index = LineIndex::new(source);

    let tokens = match Tokenizer::new(&PostgreSqlDialect {}, source)
        .with_unescape(false)
        .tokenize_with_location()
    {
        Ok(t) => t,
        Err(e) => {
            let at = index.offset(e.location).unwrap_or(0);
            diagnostics.push(
                Diagnostic::error(
                    codes::PARSE_ERROR,
                    format!("fichier de requêtes illisible : {}", e.message),
                )
                .with_span(span(at..at)),
            );
            return QueryFile {
                queries: Vec::new(),
                diagnostics,
            };
        }
    };

    let tokens: Vec<_> = tokens
        .into_iter()
        .filter(|t| t.token != Token::EOF)
        .collect();
    let starts: Vec<usize> = tokens
        .iter()
        .map(|t| index.offset(t.span.start).unwrap_or(source.len()))
        .collect();

    let mut parser = FileParser {
        source,
        macros,
        diagnostics,
        queries: Vec::new(),
        current: None,
        outside_reported: false,
    };

    for (i, tok) in tokens.iter().enumerate() {
        let range = starts[i]..starts.get(i + 1).copied().unwrap_or(source.len());
        let line_start = tok.span.start.column == 1;
        parser.token(&tok.token, range, line_start);
    }
    parser.finish_current();

    let FileParser {
        mut queries,
        mut diagnostics,
        ..
    } = parser;
    report_duplicates(&mut queries, &mut diagnostics);
    QueryFile {
        queries,
        diagnostics,
    }
}

// ── Machine à états ───────────────────────────────────────────────────────────

struct FileParser<'a> {
    source: &'a str,
    macros: Option<&'a pgweave_macro::MacroSet>,
    diagnostics: Vec<Diagnostic>,
    queries: Vec<NamedQuery>,
    current: Option<Builder>,
    /// Du SQL hors requête n'est signalé qu'une fois par fichier.
    outside_reported: bool,
}

/// Requête en cours de lecture.
struct Builder {
    /// `None` si l'en-tête est invalide : le corps est consommé (pour ne pas
    /// être signalé comme SQL hors requête) mais la requête n'est pas rendue.
    name: Option<(String, Range<usize>)>,
    header_range: Range<usize>,
    cardinality: Option<Cardinality>,
    row: Option<(String, Range<usize>)>,
    doc: Vec<String>,
    body: Option<Range<usize>>,
    after_semicolon: bool,
    extra_reported: bool,
    placeholders: Vec<(String, Range<usize>)>,
}

impl FileParser<'_> {
    fn token(&mut self, token: &Token, range: Range<usize>, line_start: bool) {
        match token {
            Token::Whitespace(Whitespace::SingleLineComment { comment, prefix })
                if prefix == "--" =>
            {
                if let Some(rest) = header_payload(comment) {
                    if line_start {
                        self.header(rest, range);
                    } else {
                        self.diagnostics.push(
                            Diagnostic::error(
                                codes::QUERY_HEADER_INVALID,
                                "un en-tête `-- name:` doit commencer la ligne",
                            )
                            .with_span(span(range)),
                        );
                    }
                } else if let Some(b) = self.current.as_mut()
                    && b.body.is_none()
                {
                    b.doc.push(doc_line(comment));
                }
            }
            Token::Whitespace(_) => {}
            Token::SemiColon => match self.current.as_mut() {
                Some(b) => b.after_semicolon = true,
                None => self.outside(range),
            },
            other => {
                let Some(b) = self.current.as_mut() else {
                    self.outside(range);
                    return;
                };
                if b.after_semicolon {
                    if !b.extra_reported {
                        b.extra_reported = true;
                        self.diagnostics.push(
                            Diagnostic::error(
                                codes::QUERY_MULTIPLE_STATEMENTS,
                                "une requête nommée contient un seul statement",
                            )
                            .with_span(span(range))
                            .with_hint("ajouter un en-tête `-- name: …` avant ce statement"),
                        );
                    }
                    return;
                }
                match &mut b.body {
                    Some(body) => body.end = range.end,
                    None => b.body = Some(range.clone()),
                }
                if let Token::Placeholder(p) = other
                    && p.starts_with('$')
                {
                    b.placeholders.push((p.clone(), range));
                }
            }
        }
    }

    fn header(&mut self, payload: &str, range: Range<usize>) {
        self.finish_current();

        // Position du payload dans le fichier, pour viser le nom précisément.
        let payload_start = range.start + self.source[range.clone()].find(payload).unwrap_or(0);
        let mut words = payload.split_whitespace().peekable();
        let name_word = words.next();
        let marker = words.next_if(|w| w.starts_with(':'));
        let arrow = words.next_if(|w| *w == "->");
        let row_word = arrow.and_then(|_| words.next());
        let extra = words.next();

        let mut builder = Builder {
            name: None,
            header_range: range.clone(),
            cardinality: None,
            row: None,
            doc: Vec::new(),
            body: None,
            after_semicolon: false,
            extra_reported: false,
            placeholders: Vec::new(),
        };

        match name_word {
            None => self.diagnostics.push(
                Diagnostic::error(codes::QUERY_HEADER_INVALID, "en-tête sans nom de requête")
                    .with_span(span(range.clone()))
                    .with_hint(
                        "forme attendue : `-- name: nom_de_requete [:one|:optional|:many|:none]`",
                    ),
            ),
            Some(name) => {
                let start = payload_start + payload.find(name).unwrap_or(0);
                let name_range = start..start + name.len();
                if is_snake_ident(name) {
                    builder.name = Some((name.to_string(), name_range));
                } else {
                    self.diagnostics.push(
                        Diagnostic::error(
                            codes::QUERY_HEADER_INVALID,
                            format!("nom de requête invalide : `{name}`"),
                        )
                        .with_span(span(name_range))
                        .with_hint("snake_case ASCII : lettres minuscules, chiffres, `_`, sans chiffre initial"),
                    );
                }
            }
        }

        if let Some(m) = marker {
            match m.strip_prefix(':').and_then(Cardinality::parse) {
                Some(c) => builder.cardinality = Some(c),
                None => self.diagnostics.push(
                    Diagnostic::error(
                        codes::QUERY_HEADER_INVALID,
                        format!("cardinalité inconnue : `{m}`"),
                    )
                    .with_span(span(range.clone()))
                    .with_hint("valeurs : :one, :optional, :many, :none"),
                ),
            }
        }
        match (arrow, row_word) {
            (Some(_), Some(r)) if r.split("::").all(is_snake_ident) => {
                let after = payload.find("->").map_or(0, |k| k + 2);
                let start = payload_start + after + payload[after..].find(r).unwrap_or(0);
                builder.row = Some((r.to_string(), start..start + r.len()));
            }
            (Some(_), r) => self.diagnostics.push(
                Diagnostic::error(
                    codes::QUERY_HEADER_INVALID,
                    match r {
                        Some(r) => format!("`-> {r}` : nom de requête invalide"),
                        None => "`->` sans nom de requête".to_string(),
                    },
                )
                .with_span(span(range.clone()))
                .with_hint(
                    "`-> autre_requete` : les lignes ont la forme de celles de `autre_requete`",
                ),
            ),
            (None, _) => {}
        }
        if extra.is_some() {
            self.diagnostics.push(
                Diagnostic::error(
                    codes::QUERY_HEADER_INVALID,
                    "texte inattendu à la fin de l'en-tête",
                )
                .with_span(span(range))
                .with_hint("forme : `-- name: nom [:cardinalité] [-> requete_de_reference]`"),
            );
        }

        self.current = Some(builder);
    }

    fn outside(&mut self, range: Range<usize>) {
        if self.outside_reported {
            return;
        }
        self.outside_reported = true;
        self.diagnostics.push(
            Diagnostic::error(codes::QUERY_WITHOUT_NAME, "SQL hors d'une requête nommée")
                .with_span(span(range))
                .with_hint("chaque requête commence par un en-tête `-- name: nom_de_requete`"),
        );
    }

    fn finish_current(&mut self) {
        let Some(b) = self.current.take() else {
            return;
        };
        let Some((name, name_range)) = b.name else {
            return;
        };
        let Some(body) = b.body else {
            self.diagnostics.push(
                Diagnostic::error(
                    codes::QUERY_EMPTY,
                    format!("la requête `{name}` n'a pas de SQL"),
                )
                .with_span(span(b.header_range)),
            );
            return;
        };

        let sql = self.source[body.clone()].to_string();
        let expansion = self
            .macros
            .filter(|_| sql.contains('@'))
            .map(|m| pgweave_macro::expand_source_at(&sql, body.start, m))
            .filter(|e| !e.is_passthrough() || !e.diagnostics.is_empty());
        let (expanded, placeholders) = match expansion {
            None => (None, b.placeholders),
            Some(e) => {
                let failed = !e.diagnostics.is_empty();
                self.diagnostics.extend(e.diagnostics);
                if failed {
                    return;
                }
                // Paramètres du SQL envoyé : un fragment peut en apporter.
                let Some(placeholders) = placeholders_of(&e.sql, body.start) else {
                    return;
                };
                (
                    Some(ExpandedSql {
                        sql: e.sql,
                        map: e.map,
                    }),
                    placeholders,
                )
            }
        };
        // `$nom?` : paramètre optionnel ; le `?` (collé) fait partie de
        // l'occurrence, retiré du SQL envoyé.
        let text_of = |r: &Range<usize>| -> Option<u8> {
            match &expanded {
                None => self.source.as_bytes().get(r.end).copied(),
                Some(e) => e.sql.as_bytes().get(r.end - body.start).copied(),
            }
        };
        let placeholders: Vec<(String, Range<usize>, bool)> = placeholders
            .into_iter()
            .map(|(t, r)| {
                if text_of(&r) == Some(b'?') {
                    let end = r.end + 1;
                    (t, r.start..end, true)
                } else {
                    (t, r, false)
                }
            })
            .collect();
        let to_file = |r: Range<usize>| match &expanded {
            None => r,
            Some(e) => e
                .map
                .remap(r.start - body.start, r.end - body.start)
                .map_or(body.clone(), |(s, t)| s..t),
        };
        let Some(params) = self.params(&name, &placeholders, &to_file) else {
            return;
        };
        self.queries.push(NamedQuery {
            name,
            name_range,
            cardinality: b.cardinality,
            row: b.row,
            doc: b.doc,
            sql,
            sql_range: body,
            expanded,
            params,
        });
    }

    /// Classe les paramètres d'une requête. `None` si leur usage est invalide
    /// (diagnostics émis) : une requête dont les paramètres ne sont pas sûrs
    /// n'est pas rendue, pour qu'aucun code ne soit généré à partir d'elle.
    fn params(
        &mut self,
        query: &str,
        placeholders: &[(String, Range<usize>, bool)],
        to_file: &dyn Fn(Range<usize>) -> Range<usize>,
    ) -> Option<Params> {
        let mut positional: Vec<(u32, Range<usize>)> = Vec::new();
        let mut named: Vec<NamedParam> = Vec::new();
        let mut valid = true;

        for (text, range, optional) in placeholders {
            let raw = &text[1..];
            if *optional && raw.bytes().all(|b| b.is_ascii_digit()) {
                valid = false;
                self.diagnostics.push(
                    Diagnostic::error(
                        codes::QUERY_PARAMETER_INVALID,
                        format!("paramètre positionnel optionnel : `{text}?`"),
                    )
                    .with_span(span(to_file(range.clone())))
                    .with_hint("seul un paramètre nommé est optionnel (`$statut?`) ; pour l'opérateur jsonb `?`, l'entourer d'espaces"),
                );
                continue;
            }
            if !raw.is_empty() && raw.bytes().all(|b| b.is_ascii_digit()) {
                match raw.parse::<u32>() {
                    Ok(n) if n >= 1 => positional.push((n, range.clone())),
                    _ => {
                        valid = false;
                        self.diagnostics.push(
                            Diagnostic::error(
                                codes::QUERY_PARAMETER_INVALID,
                                format!("paramètre positionnel invalide : `{text}`"),
                            )
                            .with_span(span(to_file(range.clone())))
                            .with_hint("les paramètres positionnels commencent à $1"),
                        );
                    }
                }
            } else if is_snake_ident(raw) {
                match named.iter_mut().find(|p| p.name == raw) {
                    Some(p) => {
                        p.occurrences.push(range.clone());
                        p.optional |= *optional;
                    }
                    None => named.push(NamedParam {
                        name: raw.to_string(),
                        occurrences: vec![range.clone()],
                        optional: *optional,
                    }),
                }
            } else {
                valid = false;
                self.diagnostics.push(
                    Diagnostic::error(
                        codes::QUERY_PARAMETER_INVALID,
                        format!("nom de paramètre invalide : `{text}`"),
                    )
                    .with_span(span(to_file(range.clone())))
                    .with_hint("snake_case ASCII : `$page_id`, pas `$pageId`"),
                );
            }
        }

        if !positional.is_empty() && !named.is_empty() {
            let (_, first_positional) = &positional[0];
            self.diagnostics.push(
                Diagnostic::error(
                    codes::QUERY_MIXED_PARAMETERS,
                    format!("la requête `{query}` mélange paramètres nommés et positionnels"),
                )
                .with_span(span(to_file(first_positional.clone())))
                .with_hint("utiliser uniquement `$nom` (recommandé) ou uniquement `$1 … $n`"),
            );
            return None;
        }

        if !valid {
            return None;
        }

        if !positional.is_empty() {
            let max = positional.iter().map(|(n, _)| *n).max().unwrap_or(0);
            let missing: Vec<String> = (1..=max)
                .filter(|n| !positional.iter().any(|(p, _)| p == n))
                .map(|n| format!("${n}"))
                .collect();
            if !missing.is_empty() {
                let last = positional
                    .iter()
                    .find(|(n, _)| *n == max)
                    .map(|(_, r)| r.clone())
                    .unwrap_or(0..0);
                self.diagnostics.push(
                    Diagnostic::error(
                        codes::QUERY_PARAMETER_GAP,
                        format!(
                            "la requête `{query}` utilise ${max} sans {}",
                            missing.join(", ")
                        ),
                    )
                    .with_span(span(to_file(last))),
                );
                return None;
            }
            return Some(Params::Positional { count: max });
        }

        if named.is_empty() {
            Some(Params::None)
        } else {
            Some(Params::Named(named))
        }
    }
}

/// Noms en double dans un même fichier : la seconde occurrence est signalée
/// et retirée, pour qu'aucune génération ne choisisse arbitrairement.
fn report_duplicates(queries: &mut Vec<NamedQuery>, diagnostics: &mut Vec<Diagnostic>) {
    let mut seen: HashMap<String, Range<usize>> = HashMap::new();
    queries.retain(|q| {
        if seen.contains_key(&q.name) {
            diagnostics.push(
                Diagnostic::error(
                    codes::QUERY_DUPLICATE_NAME,
                    format!("requête `{}` déjà définie dans ce fichier", q.name),
                )
                .with_span(span(q.name_range.clone())),
            );
            false
        } else {
            seen.insert(q.name.clone(), q.name_range.clone());
            true
        }
    });
}

// ── Utilitaires ───────────────────────────────────────────────────────────────

/// `sql` où chaque marqueur `$nom?` (paramètre optionnel, `?` collé) a son
/// `?` remplacé par une espace : lisible par le parser, positions
/// inchangées. Chaînes et commentaires ne sont pas touchés.
pub fn blank_optional_markers(sql: &str) -> String {
    let index = LineIndex::new(sql);
    let Ok(tokens) = Tokenizer::new(&PostgreSqlDialect {}, sql)
        .with_unescape(false)
        .tokenize_with_location()
    else {
        return sql.to_string();
    };
    let mut bytes = sql.as_bytes().to_vec();
    for pair in tokens.windows(2) {
        if let Token::Placeholder(p) = &pair[0].token
            && p.starts_with('$')
            && let Some(start) = index.offset(pair[0].span.start)
            && bytes.get(start + p.len()) == Some(&b'?')
            && index.offset(pair[1].span.start) == Some(start + p.len())
        {
            bytes[start + p.len()] = b' ';
        }
    }
    String::from_utf8(bytes).unwrap_or_else(|_| sql.to_string())
}

/// `$…` de `sql` (SQL après insertion des fragments), positions décalées de
/// `base`. `None` si le tokenizer ne sait pas le lire (le parse le signalera).
fn placeholders_of(sql: &str, base: usize) -> Option<Vec<(String, Range<usize>)>> {
    let index = LineIndex::new(sql);
    let tokens = Tokenizer::new(&PostgreSqlDialect {}, sql)
        .with_unescape(false)
        .tokenize_with_location()
        .ok()?;
    Some(
        tokens
            .iter()
            .filter_map(|t| match &t.token {
                Token::Placeholder(p) if p.starts_with('$') => {
                    let start = index.offset(t.span.start)?;
                    Some((p.clone(), base + start..base + start + p.len()))
                }
                _ => None,
            })
            .collect(),
    )
}

/// Contenu d'un en-tête si `comment` (texte après `--`) en est un.
fn header_payload(comment: &str) -> Option<&str> {
    comment.trim_start().strip_prefix("name:").map(str::trim)
}

/// Ligne de documentation : le texte du commentaire sans l'espace qui suit
/// `--` ni le saut de ligne.
fn doc_line(comment: &str) -> String {
    let text = comment.trim_end_matches(['\n', '\r']);
    text.strip_prefix(' ').unwrap_or(text).to_string()
}

/// `[a-z_][a-z0-9_]*` : utilisable tel quel comme identifiant Rust, et
/// convertible sans perte en camelCase pour TypeScript.
fn is_snake_ident(s: &str) -> bool {
    let mut bytes = s.bytes();
    matches!(bytes.next(), Some(b'a'..=b'z' | b'_'))
        && bytes.all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_'))
}

fn span(range: Range<usize>) -> Span {
    Span {
        start: range.start,
        end: range.end,
    }
}
