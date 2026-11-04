//! Scope of a routine: its parameters and, in PL/pgSQL, the variables of
//! its blocks, visible to the queries of the body.
//!
//! Name resolution reproduced from PostgreSQL:
//!
//! - PL/pgSQL (`pl_comp.c`, `plpgsql_ns_lookup`, `resolve_column_ref`):
//!   blocks from innermost to outermost; `a.b` is the variable `b`
//!   of the block labelled `a`, or the field `b` of the row variable `a`; with
//!   `plpgsql.variable_conflict = error` (the default), a name that designates
//!   both a column and a variable is an error;
//! - SQL (`functions.c`, `sql_fn_post_column_ref`): a parameter is only
//!   looked up if no column has that name; `f.p` designates the
//!   parameter `p` of function `f`.

use pgzest_ast::expr::ColumnRef;

use crate::scope::Column;
use crate::types::Ty;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Language {
    Sql,
    Plpgsql,
}

/// Type of a variable.
#[derive(Debug, Clone, PartialEq)]
pub enum VarTy {
    /// Established scalar type (`PLPGSQL_NSTYPE_VAR`).
    Scalar(Ty),
    /// Row of an established composite type: its fields are its attributes.
    Row(Ty),
    /// `RECORD`: structure fixed by the last assignment, `None` as long
    /// as none is known.
    Record(Option<Vec<Column>>),
    /// Type static analysis cannot establish (polymorphic argument,
    /// record assigned differently depending on the path…), with the reason.
    Unknown(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Var {
    pub name: String,
    pub ty: VarTy,
}

/// A block and its declarations.
#[derive(Debug, Clone, Default)]
pub struct Frame {
    pub label: Option<String>,
    pub vars: Vec<Var>,
}

/// Scope of a routine being analyzed.
#[derive(Debug, Clone)]
pub struct Routine {
    pub language: Language,
    /// Visible blocks, innermost last. The first one carries the
    /// parameters, under the routine's name.
    pub frames: Vec<Frame>,
    /// Types of the parameters `$1`, `$2`…
    pub positional: Vec<VarTy>,
}

/// What a resolved reference designates in the routine.
#[derive(Debug, Clone, PartialEq)]
pub enum VarRef {
    /// The whole variable.
    Whole(Var),
    /// A field of a row or record variable.
    Field(Var, String),
}

impl Routine {
    pub fn new(language: Language, name: &str) -> Self {
        Routine {
            language,
            frames: vec![Frame {
                label: Some(name.to_string()),
                vars: Vec::new(),
            }],
            positional: Vec::new(),
        }
    }

    /// Variable `name` declared in the current (innermost) block.
    pub fn declare(&mut self, name: &str, ty: VarTy) {
        let frame = self.frames.last_mut().expect("routine block");
        frame.vars.retain(|v| v.name != name);
        frame.vars.push(Var {
            name: name.to_string(),
            ty,
        });
    }

    /// Changes the type of a visible variable (structure of a record after
    /// assignment).
    pub fn assign(&mut self, name: &str, ty: VarTy) {
        for f in self.frames.iter_mut().rev() {
            if let Some(v) = f.vars.iter_mut().find(|v| v.name == name) {
                v.ty = ty;
                return;
            }
        }
    }

    /// Variable visible under this name, from the innermost block
    /// outwards.
    pub fn var(&self, name: &str) -> Option<&Var> {
        self.frames
            .iter()
            .rev()
            .find_map(|f| f.vars.iter().rev().find(|v| v.name == name))
    }

    /// Variable `name` of the block labelled `label`.
    fn labelled(&self, label: &str, name: &str) -> Option<&Var> {
        self.frames
            .iter()
            .rev()
            .filter(|f| f.label.as_deref() == Some(label))
            .find_map(|f| f.vars.iter().rev().find(|v| v.name == name))
    }

    /// Resolution of a reference `a`, `a.b` or `a.b.c` among the variables
    /// (`plpgsql_ns_lookup`: in each block, a variable named `a` before
    /// the label `a`).
    pub fn lookup(&self, c: &ColumnRef) -> Option<VarRef> {
        let parts: Vec<&str> = [c.schema.as_ref(), c.table.as_ref(), Some(&c.column)]
            .into_iter()
            .flatten()
            .map(|i| i.value.as_str())
            .collect();
        match parts.as_slice() {
            [a] => self.var(a).cloned().map(VarRef::Whole),
            [a, b] => match self.var(a) {
                Some(v) if is_row(v) => Some(VarRef::Field(v.clone(), (*b).to_string())),
                _ => self.labelled(a, b).cloned().map(VarRef::Whole),
            },
            [a, b, c] => self
                .labelled(a, b)
                .filter(|v| is_row(v))
                .cloned()
                .map(|v| VarRef::Field(v, (*c).to_string())),
            _ => None,
        }
    }
}

/// Variable whose fields are accessible through `v.field`
/// (`PLPGSQL_NSTYPE_REC`): row, record, or type not established (which may
/// be one).
fn is_row(v: &Var) -> bool {
    !matches!(v.ty, VarTy::Scalar(_))
}
