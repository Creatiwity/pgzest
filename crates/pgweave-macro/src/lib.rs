//! Macros de surface : une syntaxe compacte qui se désucre en SQL standard.
//!
//! Objectif : supprimer le boilerplate des fonctions et des triggers, sans rien
//! deviner. Toute l'information est portée par la syntaxe.
//!
//! ```text
//! @function user_count(target_org UUID): INTEGER {
//!     DECLARE total INTEGER;
//!     SELECT count(*) INTO total FROM users WHERE org_id = target_org;
//!     RETURN total;
//! }
//! ```
//!
//! # Les macros sont des données
//!
//! Aucune macro n'est codée en dur : `@function` et `@trigger` sont écrites
//! dans le même format déclaratif que celui offert à l'utilisateur (voir
//! [`def`] et `builtins/`). C'est volontaire — un mécanisme d'extension dont
//! les macros fournies ne se servent pas est un mécanisme non testé.
//!
//! # Positions
//!
//! Le corps des macros est recopié verbatim, et [`SourceMap`] conserve la
//! correspondance offset généré → offset source. Un diagnostic portant sur le
//! corps d'une fonction pointe donc la bonne ligne du `.pgsql`, pas une
//! position dans le SQL généré.

pub mod def;
pub mod expand;
pub mod header;

pub use def::{MacroDef, MacroSet};
pub use expand::{Expansion, expand_source};
/// Réexport : la correspondance de positions vit dans `pgweave-ast`, aux côtés
/// de `Span`, pour que le parser y accède sans dépendre de ce crate.
pub use pgweave_ast::SourceMap;
