//! Désucrage d'une source `.pgsql` en SQL standard, avec source-map.
//!
//! Le texte hors macro est recopié tel quel. Chaque invocation `@nom … { … }`
//! est remplacée par le rendu de son gabarit, le corps recopié verbatim afin
//! que la source-map reste exacte à l'octet. Les fragments (`@nom`, voir
//! [`crate::fragment`]) sont insérés partout où du code est recopié, corps
//! de macro compris ; leur texte est rapporté à l'invocation entière.

use pgweave_diagnostics::{Diagnostic, Severity, Span};

use crate::def::{Keywords, MacroDef, MacroSet};
use pgweave_ast::SourceMap;

/// Code de diagnostic des erreurs de macro.
pub const MACRO_ERROR: &str = "macro-error";

/// Une invocation désucrée, et où elle se trouve de part et d'autre.
///
/// Permet de retrouver l'expansion correspondant à une position du curseur,
/// exactement plutôt que par heuristique textuelle.
#[derive(Debug, Clone)]
pub struct ExpandedRegion {
    /// Nom de la macro, sans le `@`.
    pub macro_name: String,
    /// Plage de l'invocation dans la source.
    pub source: std::ops::Range<usize>,
    /// Plage du SQL engendré correspondant.
    pub generated: std::ops::Range<usize>,
}

/// Résultat d'un désucrage.
pub struct Expansion {
    /// SQL standard produit.
    pub sql: String,
    /// Correspondance offsets générés → offsets source.
    pub map: SourceMap,
    /// Erreurs de syntaxe de macro, positionnées dans la source.
    pub diagnostics: Vec<Diagnostic>,
    /// Nombre d'invocations désucrées.
    pub expanded: usize,
    /// Une entrée par invocation, dans l'ordre du fichier.
    pub regions: Vec<ExpandedRegion>,
}

impl Expansion {
    /// L'expansion de l'invocation contenant `source_offset`.
    pub fn region_at(&self, source_offset: usize) -> Option<&ExpandedRegion> {
        self.regions
            .iter()
            .find(|r| r.source.contains(&source_offset))
    }

    /// Le SQL engendré par l'invocation contenant `source_offset`.
    pub fn expansion_at(&self, source_offset: usize) -> Option<&str> {
        self.region_at(source_offset)
            .map(|r| self.sql[r.generated.clone()].trim())
    }
}

impl Expansion {
    /// Vrai si la source ne contenait aucune macro — le SQL est alors la source.
    pub fn is_passthrough(&self) -> bool {
        self.expanded == 0
    }
}

/// Désucre `source`.
///
/// Une erreur sur une invocation ne fait pas échouer le tout : l'invocation est
/// recopiée telle quelle et un diagnostic est émis. Le reste du fichier reste
/// donc analysable, comme pour un statement illisible côté parser.
pub fn expand_source(source: &str, macros: &MacroSet) -> Expansion {
    expand_source_at(source, 0, macros)
}

/// Comme [`expand_source`], `source` étant situé à `base` dans son document :
/// source-map, diagnostics et régions sont exprimés dans le repère du
/// document (corps d'une requête nommée dans son fichier).
pub fn expand_source_at(source: &str, base: usize, macros: &MacroSet) -> Expansion {
    let mut x = Out {
        out: String::with_capacity(source.len() + source.len() / 4),
        map: SourceMap::new(),
        diagnostics: Vec::new(),
        expanded: 0,
        regions: Vec::new(),
        base,
        macros,
    };
    let mut i = 0usize;

    while i < source.len() {
        match find_invocation(source, i) {
            None => {
                x.copy_code(source, i, source.len());
                break;
            }
            Some(at) => {
                x.copy_code(source, i, at);
                match parse_invocation(source, at, macros) {
                    Ok(inv) => {
                        let gen_start = x.out.len();
                        let name = inv.def.name.clone();
                        render(&inv, &mut x);
                        x.regions.push(ExpandedRegion {
                            macro_name: name,
                            source: base + at..base + inv.end,
                            generated: gen_start..x.out.len(),
                        });
                        x.expanded += 1;
                        i = inv.end;
                    }
                    Err((msg, span_end)) => {
                        x.diagnostics
                            .push(Diagnostic::error(MACRO_ERROR, msg).with_span(Span {
                                start: base + at,
                                end: base + span_end,
                            }));
                        // Recopier l'invocation fautive telle quelle : le parser
                        // SQL la signalera à son tour, mais le reste du fichier
                        // continue d'être analysé.
                        x.copy(source, at, span_end);
                        i = span_end;
                    }
                }
            }
        }
    }

    Expansion {
        sql: x.out,
        map: x.map,
        diagnostics: x.diagnostics,
        expanded: x.expanded,
        regions: x.regions,
    }
}

/// SQL engendré en cours, et ce qui le rapporte à la source.
struct Out<'m> {
    out: String,
    map: SourceMap,
    diagnostics: Vec<Diagnostic>,
    expanded: usize,
    regions: Vec<ExpandedRegion>,
    /// Position de la source dans son document.
    base: usize,
    macros: &'m MacroSet,
}

impl Out<'_> {
    /// Recopie verbatim `source[from..to]`.
    fn copy(&mut self, source: &str, from: usize, to: usize) {
        if to <= from {
            return;
        }
        self.map.add(self.out.len(), self.base + from, to - from);
        self.out.push_str(&source[from..to]);
    }

    /// Recopie `source[from..to]`, fragments insérés.
    fn copy_code(&mut self, source: &str, from: usize, to: usize) {
        let fragments = self.macros.fragments();
        if fragments.is_empty() || !source[from..to].contains('@') {
            self.copy(source, from, to);
            return;
        }
        let text = &source[from..to];
        let tokens = crate::lex::tokenize(text);
        let mut chunk = 0usize;
        let mut i = 0usize;
        while i < tokens.len() {
            let at = tokens[i].range.start;
            let call = crate::fragment::call_at(text, &tokens, i, fragments);
            let (call, next) = match call {
                Ok(Some(c)) => c,
                Ok(None) => {
                    i += 1;
                    continue;
                }
                Err(msg) => {
                    let end = tokens.get(i + 1).map_or(at + 1, |t| t.range.end);
                    self.diagnostics
                        .push(Diagnostic::error(MACRO_ERROR, msg).with_span(Span {
                            start: self.base + from + at,
                            end: self.base + from + end,
                        }));
                    i += 2;
                    continue;
                }
            };
            let rendered = crate::fragment::render_call(
                &call,
                &Default::default(),
                fragments,
                &mut Vec::new(),
            );
            match rendered {
                Ok(sql) => {
                    self.copy(source, from + chunk, from + at);
                    let gen_start = self.out.len();
                    self.out.push_str(&sql);
                    self.map
                        .add_opaque(gen_start, sql.len(), self.base + from + at, call.end - at);
                    self.regions.push(ExpandedRegion {
                        macro_name: call.def.name.clone(),
                        source: self.base + from + at..self.base + from + call.end,
                        generated: gen_start..self.out.len(),
                    });
                    self.expanded += 1;
                    chunk = call.end;
                }
                Err(msg) => self
                    .diagnostics
                    .push(Diagnostic::error(MACRO_ERROR, msg).with_span(Span {
                        start: self.base + from + at,
                        end: self.base + from + call.end,
                    })),
            }
            i = next;
        }
        self.copy(source, from + chunk, to);
    }
}

/// Une invocation reconnue.
struct Invocation<'a> {
    def: &'a MacroDef,
    /// Captures de l'en-tête.
    caps: std::collections::HashMap<String, String>,
    keywords: Keywords,
    /// Corps : plage dans la source (sans les accolades).
    body: std::ops::Range<usize>,
    /// Déclarations du corps, à remonter avant `BEGIN`.
    ///
    /// PL/pgSQL exige une section `DECLARE` **avant** `BEGIN`. Les écrire près
    /// de leur usage est plus lisible, donc l'expanseur les déplace — un
    /// déplacement mécanique, rien n'est deviné.
    declares: Vec<Declare>,
    /// Offset de fin de l'invocation dans la source (après `}`).
    end: usize,
    source: &'a str,
}

/// Prochain `@` ouvrant un statement.
///
/// Un `@` n'est une invocation que s'il commence un statement : sinon c'est
/// l'opérateur valeur absolue de PostgreSQL (`SELECT @ -5`).
///
/// L'état « début de statement » est suivi pendant le balayage plutôt que
/// déduit en regardant en arrière : regarder en arrière tombe sur le
/// commentaire qui précède la macro et conclut à tort. Blancs et commentaires
/// ne changent pas l'état ; `;` et `}` le rétablissent — `}` parce qu'il ferme
/// une macro précédente.
fn find_invocation(source: &str, from: usize) -> Option<usize> {
    let b = source.as_bytes();
    let mut i = from;
    // Au tout début du fichier on est en début de statement ; en reprise après
    // une macro, l'appelant a positionné `from` juste après le `}`.
    let mut at_stmt_start = true;

    while i < b.len() {
        match b[i] {
            b'\'' => {
                i += 1;
                while i < b.len() && b[i] != b'\'' {
                    i += 1;
                }
                i += 1;
                at_stmt_start = false;
            }
            b'-' if i + 1 < b.len() && b[i + 1] == b'-' => {
                // Commentaire ligne : neutre vis-à-vis de l'état.
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if i + 1 < b.len() && b[i + 1] == b'*' => {
                i += 2;
                while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                    i += 1;
                }
                i += 2;
            }
            b'@' if at_stmt_start && source[i + 1..].starts_with(|c: char| c.is_alphabetic()) => {
                return Some(i);
            }
            b';' | b'}' => {
                at_stmt_start = true;
                i += 1;
            }
            c if c.is_ascii_whitespace() => i += 1,
            _ => {
                at_stmt_start = false;
                i += 1;
            }
        }
    }
    None
}

/// Une déclaration remontée du corps vers la section `DECLARE`.
struct Declare {
    /// Plage à retirer du corps : la ligne entière, saut de ligne compris.
    line: std::ops::Range<usize>,
    /// Plage à émettre dans la section `DECLARE` : après le mot-clé.
    content: std::ops::Range<usize>,
}

type InvocationError = (String, usize);

fn parse_invocation<'a>(
    source: &'a str,
    at: usize,
    macros: &'a MacroSet,
) -> Result<Invocation<'a>, InvocationError> {
    let after_at = &source[at + 1..];
    let name_len = after_at
        .find(|c: char| !(c.is_alphanumeric() || c == '_'))
        .unwrap_or(after_at.len());
    let name = &after_at[..name_len];

    // Fin de l'invocation, pour positionner les diagnostics même en erreur.
    let brace = source[at..].find('{').map(|k| at + k);
    let fallback_end = brace
        .and_then(|b| body_end(source, b))
        .unwrap_or(source.len());

    let def = macros.get(name).ok_or_else(|| {
        (
            format!(
                "macro @{name} inconnue ; disponibles : {}",
                macros
                    .names()
                    .iter()
                    .map(|n| format!("@{n}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            fallback_end,
        )
    })?;

    let brace = brace.ok_or_else(|| {
        (
            format!("@{name} : bloc `{{ … }}` manquant"),
            source.len().min(at + 60),
        )
    })?;
    let end = body_end(source, brace)
        .ok_or_else(|| (format!("@{name} : `}}` manquant"), source.len()))?;

    let header_text = &source[at + 1 + name_len..brace];
    let mut caps = def
        .header
        .match_header(header_text)
        .map_err(|e| (format!("@{name} : {e}"), end))?;

    // Les mots-clés restants (drapeaux, options) sortent du trou `keywords`.
    let kw_slot = def
        .header
        .slots()
        .into_iter()
        .find(|(_, k)| *k == crate::header::SlotKind::Keywords)
        .map(|(n, _)| n.to_string());
    let kw_text = kw_slot
        .as_ref()
        .and_then(|n| caps.remove(n))
        .unwrap_or_default();
    let keywords = Keywords::parse(&kw_text, def).map_err(|e| (format!("@{name} : {e}"), end))?;

    for (slot, value) in &def.defaults {
        caps.entry(slot.clone()).or_insert_with(|| value.clone());
    }

    let body = brace + 1..end - 1;
    let declares = find_declares(source, &body);

    Ok(Invocation {
        def,
        caps,
        keywords,
        declares,
        body,
        end,
        source,
    })
}

/// Plages des `DECLARE …;` de premier niveau dans `body`.
///
/// Reconnaît une ligne dont le premier mot est `DECLARE`, jusqu'au `;`. Les
/// blocs imbriqués (`BEGIN … END`) ont leur propre section DECLARE et ne sont
/// pas touchés : on s'arrête au premier `BEGIN`.
fn find_declares(source: &str, body: &std::ops::Range<usize>) -> Vec<Declare> {
    let mut out = Vec::new();
    let text = &source[body.clone()];
    let mut offset = 0usize;

    for line in text.split_inclusive('\n') {
        let trimmed = line.trim_start();
        let lead = line.len() - trimmed.len();
        let first_word = trimmed
            .split(|c: char| c.is_whitespace())
            .next()
            .unwrap_or("");

        if first_word.eq_ignore_ascii_case("begin") {
            break;
        }
        if first_word.eq_ignore_ascii_case("declare") {
            let kw_end = offset + lead + first_word.len();
            // Contenu : après le mot-clé, jusqu'au `;` inclus.
            if let Some(semi_rel) = text[kw_end..].find(';') {
                let content_start =
                    kw_end + (text[kw_end..].len() - text[kw_end..].trim_start().len());
                let content_end = kw_end + semi_rel + 1;
                out.push(Declare {
                    line: body.start + offset..body.start + offset + line.len(),
                    content: body.start + content_start..body.start + content_end,
                });
            }
        }
        offset += line.len();
    }
    out
}

/// Offset juste après le `}` fermant du bloc ouvert à `open`.
fn body_end(source: &str, open: usize) -> Option<usize> {
    let b = source.as_bytes();
    let mut depth = 0usize;
    let mut i = open;
    while i < b.len() {
        match b[i] {
            b'\'' => {
                i += 1;
                while i < b.len() && b[i] != b'\'' {
                    i += 1;
                }
            }
            b'$' => {
                // Dollar-quote : sauter jusqu'au tag fermant.
                if let Some(tag_end) = source[i + 1..].find('$') {
                    let tag = &source[i..=i + 1 + tag_end];
                    let after = i + tag.len();
                    if let Some(close) = source[after..].find(tag) {
                        i = after + close + tag.len();
                        continue;
                    }
                }
            }
            b'-' if i + 1 < b.len() && b[i + 1] == b'-' => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i + 1);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// Rend une invocation. Une macro dont le trou `tables` contient plusieurs
/// tables est rendue une fois par table.
fn render(inv: &Invocation, x: &mut Out) {
    let tables: Vec<String> = match inv.caps.get("tables") {
        Some(list) => list.split(',').map(|t| t.trim().to_string()).collect(),
        None => vec![String::new()],
    };

    for (idx, table) in tables.iter().enumerate() {
        let mut caps = inv.caps.clone();
        if !table.is_empty() {
            caps.insert("tables".to_string(), table.clone());
            // Nom unique par table quand il y en a plusieurs, sinon le nom donné
            // servirait plusieurs fois.
            if tables.len() > 1
                && let Some(base) = caps.get("name").cloned()
            {
                let suffix = table.rsplit('.').next().unwrap_or(table);
                caps.insert("name".to_string(), format!("{base}_{suffix}"));
            }
        }
        let options = inv.def.render_options(&inv.keywords);
        if idx > 0 {
            x.out.push('\n');
        }
        render_template(inv, &caps, &options, x);
    }
}

fn render_template(
    inv: &Invocation,
    caps: &std::collections::HashMap<String, String>,
    options: &str,
    x: &mut Out,
) {
    let tpl = &inv.def.template;
    let mut rest = tpl.as_str();

    while let Some(open) = rest.find('{') {
        x.out.push_str(&rest[..open]);
        let Some(close_rel) = rest[open..].find('}') else {
            x.out.push_str(&rest[open..]);
            return;
        };
        let close = open + close_rel;
        let key = &rest[open + 1..close];

        match key {
            "body" => {
                // Recopie verbatim, en sautant les DECLARE remontés plus haut.
                // Chaque fragment garde sa correspondance, donc la source-map
                // reste exacte malgré le déplacement.
                let mut at = inv.body.start;
                for d in &inv.declares {
                    x.copy_code(inv.source, at, d.line.start);
                    at = d.line.end;
                }
                x.copy_code(inv.source, at, inv.body.end);
            }
            "declares" => {
                if !inv.declares.is_empty() {
                    x.out.push_str("DECLARE\n");
                    for d in &inv.declares {
                        x.out.push_str("    ");
                        x.copy_code(inv.source, d.content.start, d.content.end);
                        x.out.push('\n');
                    }
                }
            }
            "options" => x.out.push_str(options),
            _ => x
                .out
                .push_str(caps.get(key).map(String::as_str).unwrap_or("")),
        }
        rest = &rest[close + 1..];
    }
    x.out.push_str(rest);
}

/// Les diagnostics d'un désucrage, filtrés sur les erreurs.
pub fn has_errors(diags: &[Diagnostic]) -> bool {
    diags.iter().any(|d| d.severity == Severity::Error)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn expand(src: &str) -> Expansion {
        expand_source(src, &MacroSet::builtin())
    }

    fn sql_of(src: &str) -> String {
        let e = expand(src);
        assert!(
            !has_errors(&e.diagnostics),
            "erreurs inattendues : {:?}",
            e.diagnostics
        );
        e.sql
    }

    // ── @function ─────────────────────────────────────────────────────────────

    #[test]
    fn une_fonction_minimale_se_desucre() {
        let sql = sql_of(
            "@function user_count(target_org UUID): INTEGER {\n\
             \x20   DECLARE total INTEGER;\n\
             \x20   SELECT count(*) INTO total FROM users WHERE org_id = target_org;\n\
             \x20   RETURN total;\n\
             }",
        );
        assert!(sql.contains("CREATE OR REPLACE FUNCTION user_count(target_org UUID)"));
        assert!(sql.contains("RETURNS INTEGER"));
        assert!(sql.contains("LANGUAGE plpgsql"));
        assert!(sql.contains("BEGIN"));
        // La déclaration est remontée en section DECLARE, avant BEGIN :
        // `DECLARE x T;` entre BEGIN et END est du PL/pgSQL invalide.
        let decl = sql.find("DECLARE").expect("section DECLARE");
        assert!(decl < sql.find("BEGIN").unwrap(), "obtenu:\n{sql}");
        assert!(sql.contains("total INTEGER;"), "obtenu:\n{sql}");
        assert!(sql.contains("RETURN total;"));
        assert!(sql.contains("END;"));
    }

    #[test]
    fn sans_type_de_retour_la_fonction_est_void() {
        let sql = sql_of("@function touch() {\n  NULL;\n}");
        assert!(sql.contains("RETURNS VOID"), "obtenu:\n{sql}");
    }

    #[test]
    fn les_drapeaux_et_options_se_rendent() {
        let sql = sql_of("@function slug(t TEXT): TEXT immutable strict {\n  RETURN lower(t);\n}");
        assert!(sql.contains("IMMUTABLE"), "obtenu:\n{sql}");
        assert!(sql.contains("STRICT"));
        assert!(sql.contains("LANGUAGE plpgsql"));
    }

    #[test]
    fn le_langage_est_surchargeable() {
        let sql = sql_of("@function add(a INT, b INT): INT language sql {\n  SELECT a + b;\n}");
        assert!(sql.contains("LANGUAGE sql"), "obtenu:\n{sql}");
        assert!(!sql.contains("LANGUAGE plpgsql"));
    }

    #[test]
    fn setof_et_table_passent() {
        assert!(
            sql_of("@function f(): SETOF users {\n RETURN;\n}").contains("RETURNS SETOF users")
        );
        assert!(
            sql_of("@function f(): TABLE (id UUID, n INT) {\n RETURN;\n}")
                .contains("RETURNS TABLE (id UUID, n INT)")
        );
    }

    // ── @trigger ──────────────────────────────────────────────────────────────

    #[test]
    fn un_trigger_genere_fonction_et_trigger() {
        let sql = sql_of(
            "@trigger touch before update on users for each row {\n\
             \x20   NEW.updated_at = now();\n\
             \x20   RETURN NEW;\n\
             }",
        );
        assert!(sql.contains("CREATE OR REPLACE FUNCTION touch_fn()"));
        assert!(sql.contains("RETURNS TRIGGER"));
        assert!(sql.contains("CREATE TRIGGER touch"));
        assert!(sql.contains("before update ON users"));
        assert!(sql.contains("FOR EACH row"));
        assert!(sql.contains("EXECUTE FUNCTION touch_fn()"));
    }

    #[test]
    fn plusieurs_evenements() {
        let sql =
            sql_of("@trigger t after insert or update on users for each row {\n RETURN NEW;\n}");
        assert!(
            sql.contains("after insert or update ON users"),
            "obtenu:\n{sql}"
        );
    }

    /// Le trigger multi-tables : une fonction et un `CREATE TRIGGER` par table.
    #[test]
    fn multi_tables_genere_une_fonction_par_table() {
        let e = expand(
            "@trigger touch before update on (organizations, users, projects) for each row {\n\
             \x20   NEW.updated_at = now();\n\
             \x20   RETURN NEW;\n\
             }",
        );
        assert!(!has_errors(&e.diagnostics), "{:?}", e.diagnostics);
        for t in ["organizations", "users", "projects"] {
            assert!(
                e.sql.contains(&format!("touch_{t}_fn()")),
                "fonction pour {t} attendue dans:\n{}",
                e.sql
            );
            assert!(
                e.sql.contains(&format!("ON {t}")),
                "trigger sur {t} attendu"
            );
        }
        assert_eq!(e.sql.matches("CREATE OR REPLACE FUNCTION").count(), 3);
        assert_eq!(e.sql.matches("CREATE TRIGGER").count(), 3);
    }

    // ── Passthrough et robustesse ─────────────────────────────────────────────

    #[test]
    fn le_sql_sans_macro_est_recopie_a_lidentique() {
        let src = "CREATE TABLE t (a INT);\nSELECT a FROM t;\n";
        let e = expand(src);
        assert_eq!(e.sql, src);
        assert!(e.is_passthrough());
    }

    #[test]
    fn le_sql_autour_des_macros_est_preserve() {
        let e = expand("CREATE TABLE t (a INT);\n@function f() {\n NULL;\n}\nSELECT 1;");
        assert!(e.sql.contains("CREATE TABLE t (a INT);"));
        assert!(e.sql.contains("SELECT 1;"));
        assert!(e.sql.contains("CREATE OR REPLACE FUNCTION f()"));
    }

    #[test]
    fn un_arobase_dans_une_chaine_nest_pas_une_macro() {
        let src = "SELECT 'a@function b' FROM t;";
        let e = expand(src);
        assert_eq!(e.sql, src, "aucune expansion attendue");
        assert!(e.diagnostics.is_empty());
    }

    #[test]
    fn un_arobase_en_commentaire_nest_pas_une_macro() {
        let src = "-- @function pas une macro\nSELECT 1;";
        let e = expand(src);
        assert_eq!(e.sql, src);
        assert!(e.diagnostics.is_empty());
    }

    #[test]
    fn un_operateur_arobase_nest_pas_une_macro() {
        // `@` est l'opérateur valeur absolue en PostgreSQL.
        let src = "SELECT @ -5;";
        let e = expand(src);
        assert_eq!(e.sql, src);
    }

    #[test]
    fn une_macro_inconnue_est_signalee_avec_la_liste() {
        let e = expand("@machin f() {\n NULL;\n}");
        let d = e
            .diagnostics
            .iter()
            .find(|d| d.severity == Severity::Error)
            .expect("erreur attendue");
        assert!(d.message.contains("inconnue"), "obtenu: {}", d.message);
        assert!(
            d.message.contains("@function"),
            "doit lister: {}",
            d.message
        );
    }

    #[test]
    fn une_erreur_de_macro_ne_bloque_pas_le_reste() {
        let e = expand("@machin f() {\n NULL;\n}\nSELECT 1;");
        assert!(has_errors(&e.diagnostics));
        assert!(
            e.sql.contains("SELECT 1;"),
            "le SQL suivant doit rester analysable"
        );
    }

    #[test]
    fn un_bloc_non_ferme_est_signale() {
        let e = expand("@function f() {\n NULL;");
        assert!(
            e.diagnostics.iter().any(|d| d.message.contains("manquant")),
            "obtenu: {:?}",
            e.diagnostics
        );
    }

    #[test]
    fn un_mot_cle_inconnu_est_signale() {
        let e = expand("@function f(): INT immuable {\n RETURN 1;\n}");
        let d = e
            .diagnostics
            .iter()
            .find(|d| d.severity == Severity::Error)
            .expect("erreur attendue");
        assert!(d.message.contains("immuable"), "obtenu: {}", d.message);
    }

    #[test]
    fn les_accolades_dans_une_chaine_ne_ferment_pas_le_bloc() {
        let sql = sql_of("@function f(): TEXT {\n  RETURN '}';\n}");
        assert!(sql.contains("RETURN '}';"), "obtenu:\n{sql}");
    }

    // ── Source-map ────────────────────────────────────────────────────────────

    #[test]
    fn le_corps_se_retrouve_dans_la_source() {
        let src = "@function f(): INT {\n    RETURN nope;\n}";
        let e = expand(src);
        let needle = "RETURN nope;";
        let gen_at = e.sql.find(needle).expect("corps recopié");
        let src_at = e.map.to_source(gen_at).expect("antécédent");
        assert_eq!(
            &src[src_at..src_at + needle.len()],
            needle,
            "l'antécédent doit désigner le même texte dans le .pgsql"
        );
    }

    #[test]
    fn le_sql_hors_macro_se_retrouve_a_lidentique() {
        let src = "CREATE TABLE t (a INT);\n@function f() {\n NULL;\n}\nSELECT bad FROM t;";
        let e = expand(src);
        let gen_at = e.sql.find("SELECT bad").expect("recopié");
        let src_at = e.map.to_source(gen_at).expect("antécédent");
        assert!(src[src_at..].starts_with("SELECT bad"));
    }

    #[test]
    fn le_boilerplate_na_pas_dantecedent_exact() {
        let src = "@function f(): INT {\n RETURN 1;\n}";
        let e = expand(src);
        let gen_at = e.sql.find("CREATE OR REPLACE").expect("boilerplate");
        assert_eq!(
            e.map.to_source(gen_at),
            None,
            "le boilerplate ne vient pas de la source"
        );
        assert!(
            e.map.nearest_source(gen_at).is_some(),
            "mais doit rester situable"
        );
    }

    // ── Fragments ─────────────────────────────────────────────────────────────

    fn with_fragments(defs: &str) -> MacroSet {
        let mut set = MacroSet::builtin();
        let (defs, errors) = crate::fragment::parse_fragment_file(defs, "f.sql");
        assert!(errors.is_empty(), "{errors:?}");
        for d in defs {
            set.insert_fragment(d).unwrap();
        }
        set
    }

    #[test]
    fn un_fragment_est_rapporte_a_son_invocation() {
        let set = with_fragments("-- fragment: v(t)\nlower(t.nom)");
        let src = "CREATE INDEX i ON t ((@v));\nSELECT 1 FROM t e WHERE @v(e) = 'x';";
        let e = expand_source(src, &set);
        assert!(e.diagnostics.is_empty(), "{:?}", e.diagnostics);
        assert_eq!(
            e.sql,
            "CREATE INDEX i ON t ((lower(nom)));\nSELECT 1 FROM t e WHERE lower(e.nom) = 'x';"
        );
        assert_eq!(e.expanded, 2);
        // Toute position du texte inséré renvoie à l'invocation entière.
        let inserted = e.sql.find("lower(e.nom)").unwrap();
        let call = src.find("@v(e)").unwrap();
        assert_eq!(e.map.to_source(inserted + 3), None);
        assert_eq!(
            e.map.remap(inserted + 3, inserted + 5),
            Some((call, call + "@v(e)".len()))
        );
        // Le texte qui suit retrouve son antécédent exact.
        let after = e.sql.find(" = 'x'").unwrap();
        assert_eq!(e.map.to_source(after), Some(src.find(" = 'x'").unwrap()));
        assert_eq!(e.regions[1].macro_name, "v");
    }

    #[test]
    fn un_fragment_dans_le_corps_d_une_macro() {
        let set = with_fragments("-- fragment: actif(t)\nt.status = 'actif'");
        let sql = expand_source(
            "@function n(): INT {\n  RETURN (SELECT count(*) FROM u WHERE @actif(u));\n}",
            &set,
        )
        .sql;
        assert!(sql.contains("WHERE (u.status = 'actif')"), "{sql}");
    }

    #[test]
    fn decalage_de_base_et_erreur_positionnee() {
        let set = with_fragments("-- fragment: j(a, b)\na.x = b.x");
        let e = expand_source_at("SELECT 1 WHERE @j(p)", 100, &set);
        let d = &e.diagnostics[0];
        assert!(d.message.contains("attend 2"), "{}", d.message);
        assert_eq!(d.span.as_ref().unwrap().start, 100 + 15);
        let ok = expand_source_at("SELECT 1 WHERE @j(p, q)", 100, &set);
        assert_eq!(ok.regions[0].source, 115..123);
    }
}
