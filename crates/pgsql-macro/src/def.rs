//! Définition déclarative d'une macro, et chargement depuis un fichier.
//!
//! Format volontairement minimal — cinq directives, un jeu fermé de types de
//! trou. Assez pour exprimer `@function` et `@trigger`, donc assez pour la
//! plupart des boilerplates SQL, sans devenir un langage de gabarits.
//!
//! ```text
//! macro function
//! header {name:ident}({params:parens})[: {ret:type}]
//! default ret = VOID
//! option language = plpgsql -> LANGUAGE {}
//! flag immutable -> IMMUTABLE
//! flag security_definer -> SECURITY DEFINER
//! template
//! CREATE OR REPLACE FUNCTION {name}({params})
//! RETURNS {ret}
//! {options}
//! AS $$
//! BEGIN
//! {body}
//! END;
//! $$;
//! end
//! ```
//!
//! Directives :
//!
//! - `macro <nom>` — le nom invoqué, ici `@function`.
//! - `header <motif>` — ce qui suit `@nom` jusqu'à l'accolade ouvrante. Trous
//!   `{nom:type}`, groupes optionnels entre `[ ]`, le reste est littéral.
//! - `default <trou> = <texte>` — valeur si le trou est absent.
//! - `flag <mot> -> <texte>` — mot-clé de présence, rendu dans `{options}`.
//! - `option <mot> = <défaut> -> <texte avec {}>` — mot-clé à valeur, rendu
//!   dans `{options}` ; `{}` reçoit la valeur.
//! - `template` … `end` — le gabarit. `{trou}` interpole, `{body}` insère le
//!   corps, `{options}` les drapeaux et options rendus.
//!
//! Types de trou disponibles : voir [`SlotKind`].

use std::collections::HashMap;

use crate::header::{HeaderPattern, SlotKind};

/// Un drapeau de présence : `immutable` → `IMMUTABLE`.
#[derive(Debug, Clone)]
pub struct Flag {
    pub keyword: String,
    pub emits: String,
}

/// Un mot-clé à valeur : `language sql` → `LANGUAGE sql`.
#[derive(Debug, Clone)]
pub struct Opt {
    pub keyword: String,
    /// Valeur utilisée si le mot-clé est absent. `None` = ne rien rendre.
    pub default: Option<String>,
    /// Gabarit de rendu, `{}` recevant la valeur.
    pub emits: String,
}

/// Une macro : comment la reconnaître, et en quoi la traduire.
#[derive(Debug, Clone)]
pub struct MacroDef {
    pub name: String,
    pub header: HeaderPattern,
    pub defaults: HashMap<String, String>,
    pub flags: Vec<Flag>,
    pub options: Vec<Opt>,
    pub template: String,
    /// Chemin d'origine, pour situer une erreur de définition.
    pub origin: String,
}

impl MacroDef {
    /// Analyse une définition de macro.
    pub fn parse(text: &str, origin: &str) -> Result<Self, String> {
        let mut name = None;
        let mut header = None;
        let mut defaults = HashMap::new();
        let mut flags = Vec::new();
        let mut options = Vec::new();
        let mut template: Option<String> = None;

        let mut lines = text.lines().enumerate().peekable();
        while let Some((lineno, raw)) = lines.next() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let err = |msg: &str| format!("{origin}:{}: {msg}", lineno + 1);

            if let Some(rest) = line.strip_prefix("macro ") {
                name = Some(rest.trim().to_string());
            } else if let Some(rest) = line.strip_prefix("header ") {
                header = Some(HeaderPattern::parse(rest.trim()).map_err(|e| err(&e))?);
            } else if let Some(rest) = line.strip_prefix("default ") {
                let (k, v) = rest
                    .split_once('=')
                    .ok_or_else(|| err("`default` attend `<trou> = <texte>`"))?;
                defaults.insert(k.trim().to_string(), v.trim().to_string());
            } else if let Some(rest) = line.strip_prefix("flag ") {
                let (kw, emits) = rest
                    .split_once("->")
                    .ok_or_else(|| err("`flag` attend `<mot> -> <texte>`"))?;
                flags.push(Flag {
                    keyword: kw.trim().to_lowercase(),
                    emits: emits.trim().to_string(),
                });
            } else if let Some(rest) = line.strip_prefix("option ") {
                let (lhs, emits) = rest
                    .split_once("->")
                    .ok_or_else(|| err("`option` attend `<mot>[= <défaut>] -> <texte>`"))?;
                let (kw, default) = match lhs.split_once('=') {
                    Some((k, d)) => (k.trim(), Some(d.trim().to_string())),
                    None => (lhs.trim(), None),
                };
                options.push(Opt {
                    keyword: kw.to_lowercase(),
                    default,
                    emits: emits.trim().to_string(),
                });
            } else if line == "template" {
                let mut body = String::new();
                let mut closed = false;
                for (_, tl) in lines.by_ref() {
                    if tl.trim() == "end" {
                        closed = true;
                        break;
                    }
                    body.push_str(tl);
                    body.push('\n');
                }
                if !closed {
                    return Err(err("`template` sans `end`"));
                }
                template = Some(body);
            } else {
                return Err(err(&format!("directive inconnue : {line:?}")));
            }
        }

        Ok(Self {
            name: name.ok_or_else(|| format!("{origin}: directive `macro <nom>` manquante"))?,
            header: header.ok_or_else(|| format!("{origin}: directive `header` manquante"))?,
            defaults,
            flags,
            options,
            template: template
                .ok_or_else(|| format!("{origin}: bloc `template` … `end` manquant"))?,
            origin: origin.to_string(),
        })
    }

    /// Le trou `body` est-il attendu, c'est-à-dire la macro a-t-elle un bloc ?
    pub fn has_body(&self) -> bool {
        self.template.contains("{body}")
    }

    /// Rend `{options}` : options (avec leurs défauts) puis drapeaux présents.
    pub(crate) fn render_options(&self, present: &Keywords) -> String {
        let mut out: Vec<String> = Vec::new();

        for opt in &self.options {
            let value = present
                .values
                .get(&opt.keyword)
                .cloned()
                .or_else(|| opt.default.clone());
            if let Some(v) = value {
                out.push(opt.emits.replace("{}", &v));
            }
        }
        for flag in &self.flags {
            if present.flags.contains(&flag.keyword) {
                out.push(flag.emits.clone());
            }
        }
        out.join(" ")
    }
}

/// Mots-clés relevés dans l'en-tête d'une invocation.
#[derive(Debug, Default, Clone)]
pub struct Keywords {
    pub flags: Vec<String>,
    pub values: HashMap<String, String>,
}

impl Keywords {
    /// Découpe une suite de mots-clés en drapeaux et couples clé/valeur.
    ///
    /// Un mot déclaré comme `option` consomme le mot suivant comme valeur ;
    /// tout autre mot est un drapeau.
    pub fn parse(text: &str, def: &MacroDef) -> Result<Self, String> {
        let mut out = Self::default();
        let words: Vec<&str> = text.split_whitespace().collect();
        let mut i = 0;
        while i < words.len() {
            let w = words[i].to_lowercase();
            if let Some(opt) = def.options.iter().find(|o| o.keyword == w) {
                let v = words
                    .get(i + 1)
                    .ok_or_else(|| format!("`{}` attend une valeur", opt.keyword))?;
                out.values.insert(w, (*v).to_string());
                i += 2;
            } else if def.flags.iter().any(|f| f.keyword == w) {
                out.flags.push(w);
                i += 1;
            } else {
                let known: Vec<&str> = def
                    .flags
                    .iter()
                    .map(|f| f.keyword.as_str())
                    .chain(def.options.iter().map(|o| o.keyword.as_str()))
                    .collect();
                return Err(format!(
                    "mot-clé inconnu `{w}` pour @{} ; attendus : {}",
                    def.name,
                    known.join(", ")
                ));
            }
        }
        Ok(out)
    }
}

/// L'ensemble des macros reconnues : celles fournies, plus celles du projet.
#[derive(Debug, Clone, Default)]
pub struct MacroSet {
    defs: HashMap<String, MacroDef>,
}

impl MacroSet {
    /// Les macros fournies avec l'outil.
    ///
    /// Définies dans le même format que celles de l'utilisateur, et embarquées
    /// à la compilation. Un mécanisme d'extension dont les macros fournies ne
    /// se servent pas est un mécanisme non testé.
    pub fn builtin() -> Self {
        let mut set = Self::default();
        for (origin, text) in Self::builtin_sources() {
            let def = MacroDef::parse(text, origin)
                .unwrap_or_else(|e| panic!("macro fournie invalide : {e}"));
            set.defs.insert(def.name.clone(), def);
        }
        set
    }

    pub(crate) fn builtin_sources() -> &'static [(&'static str, &'static str)] {
        &[
            (
                "<builtin>/function.pgmacro",
                include_str!("builtins/function.pgmacro"),
            ),
            (
                "<builtin>/procedure.pgmacro",
                include_str!("builtins/procedure.pgmacro"),
            ),
            (
                "<builtin>/trigger.pgmacro",
                include_str!("builtins/trigger.pgmacro"),
            ),
        ]
    }

    /// Ajoute une définition. Une macro de l'utilisateur portant le nom d'une
    /// macro fournie la remplace — c'est le seul moyen d'ajuster une macro
    /// fournie sans forker l'outil.
    pub fn insert(&mut self, def: MacroDef) {
        self.defs.insert(def.name.clone(), def);
    }

    /// Charge tous les `*.pgmacro` d'un répertoire, récursivement.
    pub fn load_dir(&mut self, dir: &std::path::Path) -> Vec<String> {
        let mut errors = Vec::new();
        let mut files = Vec::new();
        collect_pgmacro(dir, &mut files);
        files.sort();
        for path in files {
            match std::fs::read_to_string(&path) {
                Ok(text) => match MacroDef::parse(&text, &path.display().to_string()) {
                    Ok(def) => self.insert(def),
                    Err(e) => errors.push(e),
                },
                Err(e) => errors.push(format!("{}: {e}", path.display())),
            }
        }
        errors
    }

    pub fn get(&self, name: &str) -> Option<&MacroDef> {
        self.defs.get(name)
    }

    /// Noms connus, triés — pour un message d'erreur ou `pgsql macros`.
    pub fn names(&self) -> Vec<&str> {
        let mut v: Vec<&str> = self.defs.keys().map(String::as_str).collect();
        v.sort_unstable();
        v
    }

    pub fn len(&self) -> usize {
        self.defs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.defs.is_empty()
    }

    /// Les trous déclarés par une macro, pour documentation.
    pub fn slots_of(&self, name: &str) -> Option<Vec<(&str, SlotKind)>> {
        self.defs.get(name).map(|d| d.header.slots())
    }
}

fn collect_pgmacro(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in rd.filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.is_dir() {
            collect_pgmacro(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("pgmacro") {
            out.push(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIMPLE: &str = "\
macro greet
header {name:ident}
template
SELECT 'hello {name}';
end
";

    #[test]
    fn une_definition_minimale_se_lit() {
        let d = MacroDef::parse(SIMPLE, "t.pgmacro").expect("parse");
        assert_eq!(d.name, "greet");
        assert!(d.template.contains("hello {name}"));
        assert!(!d.has_body(), "pas de {{body}} dans le gabarit");
    }

    #[test]
    fn les_macros_fournies_sont_valides() {
        // Garde-fou : les macros embarquées doivent parser, sinon `builtin()`
        // paniquerait au démarrage.
        let set = MacroSet::builtin();
        assert!(set.len() >= 3, "au moins function, procedure, trigger");
        assert!(set.get("function").is_some());
        assert!(set.get("trigger").is_some());
    }

    #[test]
    fn une_directive_inconnue_est_signalee() {
        let e = MacroDef::parse("macro x\nheader {a:ident}\nblah\n", "t").unwrap_err();
        assert!(e.contains("directive inconnue"), "obtenu: {e}");
    }

    #[test]
    fn un_template_non_ferme_est_signale() {
        let e = MacroDef::parse("macro x\nheader {a:ident}\ntemplate\nfoo\n", "t").unwrap_err();
        assert!(e.contains("sans `end`"), "obtenu: {e}");
    }

    #[test]
    fn les_directives_obligatoires_sont_verifiees() {
        assert!(
            MacroDef::parse("header {a:ident}\ntemplate\nx\nend\n", "t")
                .unwrap_err()
                .contains("`macro <nom>` manquante")
        );
        assert!(
            MacroDef::parse("macro x\ntemplate\nx\nend\n", "t")
                .unwrap_err()
                .contains("`header` manquante")
        );
        assert!(
            MacroDef::parse("macro x\nheader {a:ident}\n", "t")
                .unwrap_err()
                .contains("`template`")
        );
    }

    #[test]
    fn drapeaux_et_options_se_rendent() {
        let def = MacroDef::parse(
            "\
macro f
header {name:ident}
option language = plpgsql -> LANGUAGE {}
flag immutable -> IMMUTABLE
template
{options}
end
",
            "t",
        )
        .unwrap();

        // Rien de fourni : seul le défaut de l'option sort.
        let none = Keywords::parse("", &def).unwrap();
        assert_eq!(def.render_options(&none), "LANGUAGE plpgsql");

        // Drapeau présent + option surchargée.
        let some = Keywords::parse("immutable language sql", &def).unwrap();
        assert_eq!(def.render_options(&some), "LANGUAGE sql IMMUTABLE");
    }

    #[test]
    fn un_mot_cle_inconnu_est_refuse_avec_la_liste() {
        let def = MacroDef::parse(
            "macro f\nheader {n:ident}\nflag immutable -> IMMUTABLE\ntemplate\nx\nend\n",
            "t",
        )
        .unwrap();
        let e = Keywords::parse("immuable", &def).unwrap_err();
        assert!(e.contains("inconnu"), "obtenu: {e}");
        assert!(e.contains("immutable"), "doit lister les attendus: {e}");
    }

    #[test]
    fn une_option_sans_valeur_est_refusee() {
        let def = MacroDef::parse(
            "macro f\nheader {n:ident}\noption language -> LANGUAGE {}\ntemplate\nx\nend\n",
            "t",
        )
        .unwrap();
        assert!(
            Keywords::parse("language", &def)
                .unwrap_err()
                .contains("attend une valeur")
        );
    }

    #[test]
    fn une_macro_utilisateur_remplace_une_macro_fournie() {
        let mut set = MacroSet::builtin();
        let custom = MacroDef::parse(
            "macro function\nheader {n:ident}\ntemplate\n-- remplacée\nend\n",
            "mine.pgmacro",
        )
        .unwrap();
        set.insert(custom);
        assert_eq!(set.get("function").unwrap().origin, "mine.pgmacro");
    }
}
