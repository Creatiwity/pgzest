//! Identifiants de nœuds déterministes.
//!
//! Chaque parse numérote ses nœuds à partir de 1, dans l'ordre de leur
//! construction : le même texte donne toujours les mêmes `NodeId`, quel que
//! soit ce qui a été parsé avant ou en parallèle. Un `NodeId` est donc unique
//! **dans un parse**, pas entre deux : c'est le couple (fichier, `NodeId`) qui
//! identifie un nœud du projet.
//!
//! Auparavant un compteur atomique global numérotait tous les parses du
//! processus à la suite : les identifiants dépendaient de l'historique (ordre
//! d'ouverture des fichiers dans l'éditeur, tests exécutés avant), ce qui
//! interdit tout cache ou instantané reproductible (PLAN.md, principe 5).
//!
//! Mise en œuvre : une session de numérotation par thread, ouverte par chaque
//! point d'entrée public. Les sessions s'imbriquent : un corps PL/pgSQL parsé
//! pendant le parse de son fichier continue la numérotation du fichier au
//! lieu de la recommencer, sinon deux nœuds du même AST partageraient un
//! identifiant.

use std::cell::Cell;

use pgweave_ast::NodeId;

thread_local! {
    static NEXT: Cell<u32> = const { Cell::new(1) };
    static DEPTH: Cell<u32> = const { Cell::new(0) };
}

/// Garde de session : la numérotation recommence à 1 à l'ouverture de la
/// session la plus externe.
pub(crate) struct Session {
    _not_send: std::marker::PhantomData<*const ()>,
}

/// Ouvre une session de numérotation (ou rejoint la session en cours).
pub(crate) fn session() -> Session {
    DEPTH.with(|d| {
        if d.get() == 0 {
            NEXT.with(|n| n.set(1));
        }
        d.set(d.get() + 1);
    });
    Session {
        _not_send: std::marker::PhantomData,
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        DEPTH.with(|d| d.set(d.get() - 1));
    }
}

/// Identifiant suivant de la session en cours.
pub(crate) fn next_node_id() -> NodeId {
    debug_assert!(
        DEPTH.with(Cell::get) > 0,
        "NodeId demandé hors d'une session de parse : un point d'entrée public \
         n'ouvre pas de session"
    );
    NEXT.with(|n| {
        let id = n.get();
        n.set(id + 1);
        NodeId(id)
    })
}
