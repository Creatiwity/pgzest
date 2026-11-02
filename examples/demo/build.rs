// Lit pgweave.toml, rejoue migrations/ et schema/, décrit chaque requête de
// queries/ et écrit le module Rust correspondant dans OUT_DIR. Aucune base
// n'est contactée. Une requête invalide fait échouer la compilation.
fn main() {
    pgweave_build::Builder::new()
        .generate()
        .unwrap_or_else(|e| panic!("pgweave : {e}"));
}
