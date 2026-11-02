//! Portée d'une routine : ses paramètres et, en PL/pgSQL, les variables de
//! ses blocs, visibles des requêtes du corps.
//!
//! Résolution des noms reproduite de PostgreSQL :
//!
//! - PL/pgSQL (`pl_comp.c`, `plpgsql_ns_lookup`, `resolve_column_ref`) :
//!   blocs du plus intérieur au plus extérieur ; `a.b` est la variable `b`
//!   du bloc étiqueté `a`, ou le champ `b` de la variable ligne `a` ; avec
//!   `plpgsql.variable_conflict = error` (le défaut), un nom qui désigne à
//!   la fois une colonne et une variable est une erreur ;
//! - SQL (`functions.c`, `sql_fn_post_column_ref`) : un paramètre n'est
//!   cherché que si aucune colonne ne porte ce nom ; `f.p` désigne le
//!   paramètre `p` de la fonction `f`.

use pgweave_ast::expr::ColumnRef;

use crate::scope::Column;
use crate::types::Ty;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Language {
    Sql,
    Plpgsql,
}

/// Type d'une variable.
#[derive(Debug, Clone, PartialEq)]
pub enum VarTy {
    /// Type scalaire établi (`PLPGSQL_NSTYPE_VAR`).
    Scalar(Ty),
    /// Ligne d'un type composite établi : ses champs sont ses attributs.
    Row(Ty),
    /// `RECORD` : structure fixée par la dernière affectation, `None` tant
    /// qu'aucune n'est connue.
    Record(Option<Vec<Column>>),
    /// Type que l'analyse statique ne peut établir (argument polymorphe,
    /// record affecté différemment selon le chemin…), avec la raison.
    Unknown(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Var {
    pub name: String,
    pub ty: VarTy,
}

/// Un bloc et ses déclarations.
#[derive(Debug, Clone, Default)]
pub struct Frame {
    pub label: Option<String>,
    pub vars: Vec<Var>,
}

/// Portée d'une routine en cours d'analyse.
#[derive(Debug, Clone)]
pub struct Routine {
    pub language: Language,
    /// Blocs visibles, le plus intérieur en dernier. Le premier porte les
    /// paramètres, sous le nom de la routine.
    pub frames: Vec<Frame>,
    /// Types des paramètres `$1`, `$2`…
    pub positional: Vec<VarTy>,
}

/// Ce que désigne une référence résolue dans la routine.
#[derive(Debug, Clone, PartialEq)]
pub enum VarRef {
    /// La variable entière.
    Whole(Var),
    /// Un champ d'une variable ligne ou record.
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

    /// Variable `name` déclarée dans le bloc courant (le plus intérieur).
    pub fn declare(&mut self, name: &str, ty: VarTy) {
        let frame = self.frames.last_mut().expect("bloc de la routine");
        frame.vars.retain(|v| v.name != name);
        frame.vars.push(Var {
            name: name.to_string(),
            ty,
        });
    }

    /// Change le type d'une variable visible (structure d'un record après
    /// affectation).
    pub fn assign(&mut self, name: &str, ty: VarTy) {
        for f in self.frames.iter_mut().rev() {
            if let Some(v) = f.vars.iter_mut().find(|v| v.name == name) {
                v.ty = ty;
                return;
            }
        }
    }

    /// Variable visible sous ce nom, du bloc le plus intérieur vers
    /// l'extérieur.
    pub fn var(&self, name: &str) -> Option<&Var> {
        self.frames
            .iter()
            .rev()
            .find_map(|f| f.vars.iter().rev().find(|v| v.name == name))
    }

    /// Variable `name` du bloc étiqueté `label`.
    fn labelled(&self, label: &str, name: &str) -> Option<&Var> {
        self.frames
            .iter()
            .rev()
            .filter(|f| f.label.as_deref() == Some(label))
            .find_map(|f| f.vars.iter().rev().find(|v| v.name == name))
    }

    /// Résolution d'une référence `a`, `a.b` ou `a.b.c` parmi les variables
    /// (`plpgsql_ns_lookup` : à chaque bloc, une variable du nom `a` avant
    /// l'étiquette `a`).
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

/// Variable dont les champs sont accessibles par `v.champ`
/// (`PLPGSQL_NSTYPE_REC`) : ligne, record, ou type non établi (qui peut en
/// être une).
fn is_row(v: &Var) -> bool {
    !matches!(v.ty, VarTy::Scalar(_))
}
