//! Deterministic node identifiers.
//!
//! Each parse numbers its nodes starting from 1, in the order of their
//! construction: the same text always yields the same `NodeId`s, whatever
//! was parsed before or in parallel. A `NodeId` is therefore unique
//! **within a parse**, not across two: the pair (file, `NodeId`) is what
//! identifies a project node.
//!
//! Previously a global atomic counter numbered all the process's parses
//! in sequence: identifiers depended on history (order in which files
//! were opened in the editor, tests run before), which precluded any
//! cache or reproducible snapshot (PLAN.md, principle 5).
//!
//! Implementation: one numbering session per thread, opened by each
//! public entry point. Sessions nest: a PL/pgSQL body parsed
//! during the parse of its file continues the file's numbering instead
//! of restarting it, otherwise two nodes of the same AST would share an
//! identifier.

use std::cell::Cell;

use pgzest_ast::NodeId;

thread_local! {
    static NEXT: Cell<u32> = const { Cell::new(1) };
    static DEPTH: Cell<u32> = const { Cell::new(0) };
}

/// Session guard: numbering restarts at 1 when the outermost session
/// is opened.
pub(crate) struct Session {
    _not_send: std::marker::PhantomData<*const ()>,
}

/// Opens a numbering session (or joins the current one).
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

/// Next identifier of the current session.
pub(crate) fn next_node_id() -> NodeId {
    debug_assert!(
        DEPTH.with(Cell::get) > 0,
        "NodeId requested outside a parse session: a public entry point \
         does not open a session"
    );
    NEXT.with(|n| {
        let id = n.get();
        n.set(id + 1);
        NodeId(id)
    })
}
