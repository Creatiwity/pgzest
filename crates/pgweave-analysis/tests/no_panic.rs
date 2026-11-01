//! Aucune entrée ne fait paniquer la chaîne parse → analyse → émission.
//!
//! Fuzzing déterministe, exécuté à chaque `cargo test` : chaque fichier SQL
//! du dépôt (exemples, cas de l'oracle) est tronqué à de nombreuses
//! positions et muté (suppressions, duplications, insertions de fragments
//! qui ouvrent ou ferment des constructions). Le générateur est à graine
//! fixe : un échec se reproduit à l'identique, et le message donne la
//! variante fautive.
//!
//! Un éditeur envoie au LSP un texte à moitié tapé à chaque frappe : une
//! panique y est un plantage de l'outil, pas un diagnostic.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};

use pgweave_analysis::{
    AnalysisOptions, analyze_source_self_contained, analyze_source_with_macros,
};
use pgweave_codegen::emit::emit_statement;
use pgweave_macro::MacroSet;
use pgweave_schema::provider::DdlSchemaProvider;

/// Variantes par fichier, pour chaque famille (troncature, mutation).
/// `PGWEAVE_FUZZ_PER_FILE` l'augmente pour une passe locale approfondie.
const PER_FILE: usize = 120;

fn per_file() -> usize {
    std::env::var("PGWEAVE_FUZZ_PER_FILE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(PER_FILE)
}

/// Fragments insérés : délimiteurs et mots qui ouvrent ou ferment une
/// construction, là où un parser incrémental a le plus de chances de se
/// perdre.
const FRAGMENTS: &[&str] = &[
    "(",
    ")",
    ";",
    "$$",
    "$x$",
    "'",
    "\"",
    "--",
    "/*",
    "*/",
    "[",
    "]",
    ",",
    "::",
    "..",
    ".",
    "\\",
    "BEGIN",
    "END",
    "END IF",
    "LOOP",
    "DECLARE",
    "SELECT",
    "INTO",
    "FROM",
    "WITH",
    "AS",
    "CASE",
    "WHEN",
    "OVER (",
    "LIKE",
    "=>",
    ":=",
    "@function",
    "{",
    "}",
    "é",
    "\u{0}",
];

struct XorShift(u64);

impl XorShift {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

fn seeds() -> Vec<(PathBuf, String)> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut out = Vec::new();
    for dir in ["examples", "crates/pgweave-oracle/cases"] {
        collect(&root.join(dir), &mut out);
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    assert!(
        out.len() >= 10,
        "corpus de graines trop maigre : {}",
        out.len()
    );
    out
}

fn collect(dir: &Path, out: &mut Vec<(PathBuf, String)>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            collect(&p, out);
        } else if p.extension().is_some_and(|x| x == "sql" || x == "pgsql")
            && let Ok(text) = std::fs::read_to_string(&p)
        {
            out.push((p, text));
        }
    }
}

/// Position valide (frontière de caractère) la plus proche avant `i`.
fn floor_boundary(s: &str, mut i: usize) -> usize {
    i = i.min(s.len());
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn variants(text: &str, rng: &mut XorShift) -> Vec<String> {
    let n = per_file();
    let mut out = Vec::new();
    // Troncatures, réparties sur tout le texte.
    for k in 1..=n {
        out.push(text[..floor_boundary(text, text.len() * k / n)].to_string());
    }
    // Mutations.
    for _ in 0..n {
        let a = floor_boundary(text, rng.below(text.len() + 1));
        let b = floor_boundary(text, a + rng.below(24));
        let mut v = String::with_capacity(text.len() + 16);
        match rng.below(3) {
            0 => {
                v.push_str(&text[..a]);
                v.push_str(&text[b..]);
            }
            1 => {
                v.push_str(&text[..b]);
                v.push_str(&text[a..]);
            }
            _ => {
                v.push_str(&text[..a]);
                v.push_str(FRAGMENTS[rng.below(FRAGMENTS.len())]);
                v.push_str(&text[a..]);
            }
        }
        out.push(v);
    }
    out
}

/// Tout ce qu'un client fait d'une source : analyse simple, analyse avec
/// macros, réémission de ce qui a été lu.
fn exercise(sql: &str, macros: &MacroSet) {
    let opts = AnalysisOptions::default();
    let _ = analyze_source_self_contained(sql, &opts);
    let schema = DdlSchemaProvider::from_statements(&[]);
    let _ = analyze_source_with_macros(sql, macros, &schema, &opts);
    if let Ok(stmts) = pgweave_parser::parse(sql) {
        for s in &stmts {
            let _ = emit_statement(s);
        }
    }
}

#[test]
fn aucune_variante_ne_fait_paniquer() {
    let macros = MacroSet::builtin();
    let mut rng = XorShift(0x9e37_79b9_7f4a_7c15);
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));

    let mut failures = Vec::new();
    let mut total = 0usize;
    for (path, text) in seeds() {
        for v in variants(&text, &mut rng) {
            total += 1;
            if let Err(e) = catch_unwind(AssertUnwindSafe(|| exercise(&v, &macros))) {
                let msg = e
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()))
                    .unwrap_or_default();
                failures.push(format!(
                    "{} : {msg}\n--- variante ---\n{v}\n---",
                    path.display()
                ));
            }
        }
    }

    std::panic::set_hook(previous);
    assert!(
        failures.is_empty(),
        "{} panique(s) sur {total} variantes ; premières :\n{}",
        failures.len(),
        failures
            .iter()
            .take(3)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
}
