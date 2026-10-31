//! Directives en commentaire, à la ESLint, pour débrayer les checks localement.
//!
//! Syntaxe reconnue, en commentaire ligne (`--`) ou bloc (`/* */`) :
//!
//! ```sql
//! -- pgweave-disable-next-line                        -- toutes les règles
//! -- pgweave-disable-next-line column-not-found       -- une règle
//! -- pgweave-disable-next-line column-not-found, L001 -- plusieurs
//! -- pgweave-enable-next-line L001                    -- réactive ponctuellement
//! -- pgweave-disable                                  -- début de bloc
//! -- pgweave-enable                                   -- fin de bloc
//! -- pgweave-disable-file                             -- tout le fichier
//! ```
//!
//! Un `-- pgweave-disable` sans `-- pgweave-enable` porte jusqu'à la fin du fichier.
//!
//! Une raison peut être accolée après `--` et est ignorée par le parseur de
//! directives, ce qui permet de documenter la suppression :
//! `-- pgweave-disable-next-line column-not-found -- colonne créée par la migration 42`
//!
//! ## Limite
//!
//! La suppression est positionnelle : elle a besoin du `span` du diagnostic.
//! Un diagnostic sans span (certaines règles ne les remplissent pas encore) ne
//! peut être supprimé que par `pgweave-disable-file` ou un bloc couvrant tout le
//! fichier. Voir [`Suppressions::is_suppressed`].

use std::collections::HashMap;

use crate::Diagnostic;

// ── Modèle ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirectiveKind {
    DisableNextLine,
    EnableNextLine,
    Disable,
    Enable,
    DisableFile,
}

impl DirectiveKind {
    fn parse(token: &str) -> Option<Self> {
        match token {
            "pgweave-disable-next-line" => Some(Self::DisableNextLine),
            "pgweave-enable-next-line" => Some(Self::EnableNextLine),
            "pgweave-disable" => Some(Self::Disable),
            "pgweave-enable" => Some(Self::Enable),
            "pgweave-disable-file" => Some(Self::DisableFile),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Directive {
    pub kind: DirectiveKind,
    /// Règles visées. Vide = toutes les règles.
    pub rules: Vec<String>,
    /// Ligne (0-indexée) où la directive apparaît.
    pub line: usize,
}

/// Une liste de règles vide vaut « toutes les règles ».
fn rules_match(rules: &[String], code: &str) -> bool {
    rules.is_empty() || rules.iter().any(|r| r == code)
}

impl Directive {
    /// Cette directive vise-t-elle `code` ?
    pub fn targets(&self, code: &str) -> bool {
        rules_match(&self.rules, code)
    }
}

// ── Scan ──────────────────────────────────────────────────────────────────────

/// Ensemble des suppressions actives pour une source donnée.
#[derive(Debug, Clone, Default)]
pub struct Suppressions {
    /// Décalages cumulés de début de ligne, pour mapper offset octet → ligne.
    line_starts: Vec<usize>,
    /// `pgweave-disable-file` présent (règles visées ; vide = toutes).
    file_disabled: Option<Vec<String>>,
    /// Par ligne : règles désactivées via `disable-next-line`.
    next_line: HashMap<usize, Vec<String>>,
    /// Par ligne : règles réactivées via `enable-next-line`.
    next_line_enabled: HashMap<usize, Vec<String>>,
    /// Plages `[début, fin)` en lignes issues des blocs disable/enable.
    blocks: Vec<(usize, usize, Vec<String>)>,
    /// Toutes les directives trouvées, dans l'ordre — utile pour diagnostiquer
    /// une directive inutile ou mal orthographiée.
    pub directives: Vec<Directive>,
}

impl Suppressions {
    /// Scanne la source et collecte les directives.
    pub fn scan(source: &str) -> Self {
        let line_starts = compute_line_starts(source);
        let mut out = Self {
            line_starts,
            ..Default::default()
        };

        let mut open_block: Option<(usize, Vec<String>)> = None;
        let total_lines = source.lines().count();

        for (lineno, line) in source.lines().enumerate() {
            for (kind, rules) in directives_in_line(line) {
                out.directives.push(Directive {
                    kind,
                    rules: rules.clone(),
                    line: lineno,
                });
                match kind {
                    DirectiveKind::DisableFile => {
                        // Le plus permissif gagne : « toutes les règles » écrase une liste.
                        match (&mut out.file_disabled, rules.is_empty()) {
                            (_, true) => out.file_disabled = Some(Vec::new()),
                            (Some(existing), false) if !existing.is_empty() => {
                                existing.extend(rules)
                            }
                            (Some(_), false) => {}
                            (None, false) => out.file_disabled = Some(rules),
                        }
                    }
                    DirectiveKind::DisableNextLine => {
                        out.next_line.entry(lineno + 1).or_default().extend(rules);
                    }
                    DirectiveKind::EnableNextLine => {
                        out.next_line_enabled
                            .entry(lineno + 1)
                            .or_default()
                            .extend(rules);
                    }
                    DirectiveKind::Disable => {
                        if open_block.is_none() {
                            open_block = Some((lineno, rules));
                        }
                    }
                    DirectiveKind::Enable => {
                        if let Some((start, block_rules)) = open_block.take() {
                            out.blocks.push((start, lineno, block_rules));
                        }
                    }
                }
            }
        }

        // Un bloc laissé ouvert court jusqu'à la fin du fichier.
        if let Some((start, rules)) = open_block {
            out.blocks.push((start, total_lines + 1, rules));
        }

        out
    }

    /// Ligne (0-indexée) contenant l'offset octet donné.
    fn line_of(&self, offset: usize) -> usize {
        match self.line_starts.binary_search(&offset) {
            Ok(i) => i,
            Err(i) => i.saturating_sub(1),
        }
    }

    /// Le diagnostic `code` situé à `offset` est-il supprimé ?
    ///
    /// `offset` à `None` (diagnostic sans span) : seules les suppressions
    /// couvrant tout le fichier s'appliquent, faute de position à comparer.
    pub fn is_suppressed(&self, code: &str, offset: Option<usize>) -> bool {
        if let Some(rules) = &self.file_disabled
            && rules_match(rules, code)
        {
            return true;
        }

        let Some(offset) = offset else {
            return false;
        };
        let line = self.line_of(offset);

        // `enable-next-line` est plus spécifique que le bloc : il gagne.
        if let Some(rules) = self.next_line_enabled.get(&line)
            && rules_match(rules, code)
        {
            return false;
        }

        if let Some(rules) = self.next_line.get(&line)
            && rules_match(rules, code)
        {
            return true;
        }

        self.blocks
            .iter()
            .any(|(start, end, rules)| line > *start && line < *end && rules_match(rules, code))
    }

    /// Retire de `diags` tout ce qui est couvert par une directive.
    pub fn apply(&self, diags: &mut Vec<Diagnostic>) {
        diags.retain(|d| !self.is_suppressed(d.code, d.span.as_ref().map(|s| s.start)));
    }

    /// Vrai si la source ne contient aucune directive — permet d'éviter le
    /// travail de filtrage dans le cas courant.
    pub fn is_empty(&self) -> bool {
        self.directives.is_empty()
    }
}

// ── Parsing d'une ligne ───────────────────────────────────────────────────────

fn compute_line_starts(source: &str) -> Vec<usize> {
    let mut starts = vec![0usize];
    for (i, b) in source.bytes().enumerate() {
        if b == b'\n' {
            starts.push(i + 1);
        }
    }
    starts
}

/// Extrait les directives d'une ligne. Une ligne peut en porter plusieurs si
/// elle contient plusieurs commentaires.
fn directives_in_line(line: &str) -> Vec<(DirectiveKind, Vec<String>)> {
    let mut found = Vec::new();

    for comment in comment_bodies(line) {
        let body = comment.trim();
        let mut parts = body.split_whitespace();
        let Some(head) = parts.next() else { continue };
        let Some(kind) = DirectiveKind::parse(head) else {
            continue;
        };

        // Le reste = liste de règles, séparées par virgule et/ou espace.
        // Un `--` termine la liste : ce qui suit est une justification libre.
        let rest: Vec<&str> = parts.take_while(|t| *t != "--").collect();
        let rules: Vec<String> = rest
            .join(" ")
            .split(',')
            .map(|r| r.trim())
            .filter(|r| !r.is_empty())
            .map(|r| r.to_string())
            .collect();

        found.push((kind, rules));
    }

    found
}

/// Corps des commentaires présents dans une ligne (`-- …` et `/* … */`).
fn comment_bodies(line: &str) -> Vec<&str> {
    let mut out = Vec::new();

    if let Some(i) = line.find("--") {
        out.push(&line[i + 2..]);
    }

    let mut rest = line;
    while let Some(open) = rest.find("/*") {
        let after = &rest[open + 2..];
        match after.find("*/") {
            Some(close) => {
                out.push(&after[..close]);
                rest = &after[close + 2..];
            }
            None => {
                out.push(after);
                break;
            }
        }
    }

    out
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Diagnostic, Span};

    fn diag(code: &'static str, start: usize) -> Diagnostic {
        Diagnostic::error(code, "boom").with_span(Span {
            start,
            end: start + 1,
        })
    }

    /// Offset du début de la ligne `n` (0-indexée).
    fn line_offset(src: &str, n: usize) -> usize {
        compute_line_starts(src)[n]
    }

    #[test]
    fn disable_next_line_toutes_regles() {
        let src = "-- pgweave-disable-next-line\nSELECT bad;\nSELECT other;";
        let s = Suppressions::scan(src);
        assert!(s.is_suppressed("column-not-found", Some(line_offset(src, 1))));
        assert!(!s.is_suppressed("column-not-found", Some(line_offset(src, 2))));
    }

    #[test]
    fn disable_next_line_regle_ciblee() {
        let src = "-- pgweave-disable-next-line column-not-found\nSELECT bad;";
        let s = Suppressions::scan(src);
        let off = Some(line_offset(src, 1));
        assert!(s.is_suppressed("column-not-found", off));
        assert!(
            !s.is_suppressed("table-not-found", off),
            "autre règle intacte"
        );
    }

    #[test]
    fn plusieurs_regles_separees_par_virgule() {
        let src = "-- pgweave-disable-next-line column-not-found, L001\nSELECT bad;";
        let s = Suppressions::scan(src);
        let off = Some(line_offset(src, 1));
        assert!(s.is_suppressed("column-not-found", off));
        assert!(s.is_suppressed("L001", off));
        assert!(!s.is_suppressed("L002", off));
    }

    #[test]
    fn justification_apres_double_tiret_ignoree() {
        let src = "-- pgweave-disable-next-line L001 -- ajouté par la migration 42\nSELECT *;";
        let s = Suppressions::scan(src);
        let off = Some(line_offset(src, 1));
        assert!(s.is_suppressed("L001", off));
        assert!(
            !s.is_suppressed("migration", off),
            "la justification ne doit pas devenir une règle"
        );
    }

    #[test]
    fn bloc_disable_enable() {
        let src = "-- pgweave-disable\nSELECT a;\nSELECT b;\n-- pgweave-enable\nSELECT c;";
        let s = Suppressions::scan(src);
        assert!(s.is_suppressed("x", Some(line_offset(src, 1))));
        assert!(s.is_suppressed("x", Some(line_offset(src, 2))));
        assert!(
            !s.is_suppressed("x", Some(line_offset(src, 4))),
            "après enable, plus de suppression"
        );
    }

    #[test]
    fn bloc_non_ferme_court_jusqua_la_fin() {
        let src = "SELECT a;\n-- pgweave-disable\nSELECT b;\nSELECT c;";
        let s = Suppressions::scan(src);
        assert!(!s.is_suppressed("x", Some(line_offset(src, 0))));
        assert!(s.is_suppressed("x", Some(line_offset(src, 2))));
        assert!(s.is_suppressed("x", Some(line_offset(src, 3))));
    }

    #[test]
    fn disable_file_couvre_tout_meme_sans_span() {
        let src = "-- pgweave-disable-file\nSELECT a;";
        let s = Suppressions::scan(src);
        assert!(s.is_suppressed("x", Some(line_offset(src, 1))));
        assert!(
            s.is_suppressed("x", None),
            "un diagnostic sans span doit être couvert par disable-file"
        );
    }

    #[test]
    fn sans_span_non_supprime_par_disable_next_line() {
        let src = "-- pgweave-disable-next-line\nSELECT a;";
        let s = Suppressions::scan(src);
        assert!(
            !s.is_suppressed("x", None),
            "limite assumée : pas de position, pas de suppression ciblée"
        );
    }

    #[test]
    fn enable_next_line_gagne_sur_le_bloc() {
        let src = "-- pgweave-disable\nSELECT a;\n-- pgweave-enable-next-line L001\nSELECT b;";
        let s = Suppressions::scan(src);
        assert!(s.is_suppressed("L001", Some(line_offset(src, 1))));
        assert!(
            !s.is_suppressed("L001", Some(line_offset(src, 3))),
            "enable-next-line est plus spécifique que le bloc"
        );
    }

    #[test]
    fn commentaire_bloc_reconnu() {
        let src = "/* pgweave-disable-next-line L001 */\nSELECT *;";
        let s = Suppressions::scan(src);
        assert!(s.is_suppressed("L001", Some(line_offset(src, 1))));
    }

    #[test]
    fn apply_filtre_la_liste() {
        let src = "-- pgweave-disable-next-line L001\nSELECT *;\nSELECT *;";
        let s = Suppressions::scan(src);
        let mut diags = vec![
            diag("L001", line_offset(src, 1)),
            diag("L001", line_offset(src, 2)),
        ];
        s.apply(&mut diags);
        assert_eq!(diags.len(), 1, "seul le second doit survivre");
    }

    #[test]
    fn source_sans_directive_est_vide() {
        let s = Suppressions::scan("SELECT 1;\n-- juste un commentaire\n");
        assert!(s.is_empty());
        assert!(!s.is_suppressed("x", Some(0)));
    }

    #[test]
    fn directive_mal_orthographiee_ignoree() {
        let s = Suppressions::scan("-- pgweave-disbale-next-line\nSELECT a;");
        assert!(s.is_empty(), "une faute de frappe ne doit rien supprimer");
    }
}
