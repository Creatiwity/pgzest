//! Service d'exemple : les requêtes de `queries/` compilées en fonctions
//! Rust typées par `pgweave-build` (voir `build.rs`).

pub mod queries {
    include!(concat!(env!("OUT_DIR"), "/pgweave_queries.rs"));
}
