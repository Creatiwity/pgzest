//! Conversions de types (`parse_coerce.c`).

use pgweave_catalog::model::{CastContext, CastMethod};

use crate::types::{CATEGORY_STRING, Ty, TypeSystem, is_polymorphic, pg};

/// Contexte d'une conversion (`CoercionContext`) : ce qui est permis
/// grandit d'implicite (expression) à explicite (`CAST`, `::`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum CoercionContext {
    /// Argument d'opérateur ou de fonction, opérande d'une expression.
    Implicit,
    /// Valeur affectée à une colonne (`INSERT`, `UPDATE`).
    Assignment,
    /// `CAST(x AS t)`, `x::t`.
    Explicit,
}

impl CoercionContext {
    fn allows(self, cast: CastContext) -> bool {
        match cast {
            CastContext::Implicit => true,
            CastContext::Assignment => self >= CoercionContext::Assignment,
            CastContext::Explicit => self == CoercionContext::Explicit,
        }
    }
}

/// Chemin de conversion (`CoercionPathType`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pathway {
    /// Aucune conversion possible dans ce contexte.
    None,
    /// Même représentation (types identiques, domaine et sa base,
    /// conversion binaire de `pg_cast`).
    Relabel,
    /// Entrée de `pg_cast` mise en œuvre par une fonction.
    Cast,
    /// Tableau converti élément par élément.
    ArrayCoerce,
    /// Par la représentation texte (sortie puis entrée).
    ViaIo,
}

/// Échec de [`TypeSystem::common_type`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommonTypeError {
    /// Deux types de catégories différentes (`text` et `integer`).
    CategoryMismatch(Ty, Ty),
    /// Un type ne se convertit pas implicitement vers le type retenu.
    NotCoercible { from: Ty, to: Ty },
}

impl TypeSystem<'_> {
    /// `find_coercion_pathway` : comment convertir `source` en `target`.
    pub fn pathway(&self, source: &Ty, target: &Ty, ctx: CoercionContext) -> Pathway {
        if source == target {
            return Pathway::Relabel;
        }
        // Un domaine se convertit vers et depuis son type de base.
        let source = self.base_type(source);
        let target = self.base_type(target);
        if source == target {
            return Pathway::Relabel;
        }
        if let Some(cast) = self.cast(&source, &target) {
            if !ctx.allows(cast.context) {
                return Pathway::None;
            }
            return match cast.method {
                CastMethod::Function => Pathway::Cast,
                CastMethod::Binary => Pathway::Relabel,
                CastMethod::InOut => Pathway::ViaIo,
            };
        }
        // Sans entrée de `pg_cast` : tableau vers tableau si les éléments
        // se convertissent (jamais vers `oidvector` ni `int2vector`)…
        if target != pg("oidvector")
            && target != pg("int2vector")
            && let (Some(te), Some(se)) = (self.element(&target), self.element(&source))
            && self.pathway(&se, &te, ctx) != Pathway::None
        {
            return Pathway::ArrayCoerce;
        }
        // … sinon conversion par le texte : vers une chaîne en affectation,
        // depuis une chaîne en explicite.
        if (ctx >= CoercionContext::Assignment && self.category(&target) == CATEGORY_STRING)
            || (ctx == CoercionContext::Explicit && self.category(&source) == CATEGORY_STRING)
        {
            return Pathway::ViaIo;
        }
        Pathway::None
    }

    /// `can_coerce_type` : chaque entrée se convertit-elle vers sa cible ?
    /// Les cibles polymorphes sont vérifiées ensemble, en dernier.
    pub fn can_coerce(&self, inputs: &[Ty], targets: &[Ty], ctx: CoercionContext) -> bool {
        let unknown = pg("unknown");
        let mut generic = false;
        for (input, target) in inputs.iter().zip(targets) {
            if input == target || *target == pg("any") {
                continue;
            }
            if is_polymorphic(target) {
                generic = true;
                continue;
            }
            // Un littéral sans type devient ce que la cible demande.
            if *input == unknown {
                continue;
            }
            if self.pathway(input, target, ctx) != Pathway::None {
                continue;
            }
            // `record` et types composites se convertissent l'un vers l'autre.
            if (*input == pg("record") && self.is_complex(target))
                || (*target == pg("record") && self.is_complex(input))
            {
                continue;
            }
            if *target == pg("_record")
                && self
                    .element(&self.base_type(input))
                    .is_some_and(|e| self.is_complex(&e))
            {
                continue;
            }
            return false;
        }
        !generic || self.generic_consistent(inputs, targets)
    }

    /// `select_common_type` : type commun des branches d'un `UNION`, d'un
    /// `CASE`, des arguments de `COALESCE`, des lignes de `VALUES`…
    pub fn common_type(&self, inputs: &[Ty]) -> Result<Ty, CommonTypeError> {
        let unknown = pg("unknown");
        let Some(first) = inputs.first() else {
            return Ok(pg("text"));
        };
        // Seule façon d'obtenir un domaine : tous identiques.
        if inputs.iter().all(|t| t == first) && *first != unknown {
            return Ok(first.clone());
        }
        let mut ptype = self.base_type(first);
        let mut pcategory = self.category(&ptype);
        let mut ppreferred = self.is_preferred(pcategory, &ptype);
        for t in &inputs[1..] {
            let ntype = self.base_type(t);
            if ntype == unknown || ntype == ptype {
                continue;
            }
            let ncategory = self.category(&ntype);
            let npreferred = self.is_preferred(ncategory, &ntype);
            if ptype == unknown {
                // Jusqu'ici des littéraux : on prend ce qui vient.
                (ptype, pcategory, ppreferred) = (ntype, ncategory, npreferred);
            } else if ncategory != pcategory {
                return Err(CommonTypeError::CategoryMismatch(ptype, ntype));
            } else if !ppreferred
                && self.can_coerce(
                    std::slice::from_ref(&ptype),
                    std::slice::from_ref(&ntype),
                    CoercionContext::Implicit,
                )
                && !self.can_coerce(
                    std::slice::from_ref(&ntype),
                    std::slice::from_ref(&ptype),
                    CoercionContext::Implicit,
                )
            {
                // Le nouveau type reçoit l'ancien implicitement, pas
                // l'inverse ; un type préféré n'est jamais quitté.
                (ptype, pcategory, ppreferred) = (ntype, ncategory, npreferred);
            }
        }
        // Que des littéraux : `text`.
        if ptype == unknown {
            ptype = pg("text");
        }
        // `verify_common_type` : tout doit pouvoir y être converti.
        for t in inputs {
            if !self.can_coerce(
                std::slice::from_ref(t),
                std::slice::from_ref(&ptype),
                CoercionContext::Implicit,
            ) {
                return Err(CommonTypeError::NotCoercible {
                    from: t.clone(),
                    to: ptype,
                });
            }
        }
        Ok(ptype)
    }
}
