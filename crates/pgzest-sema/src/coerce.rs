//! Type conversions (`parse_coerce.c`).

use pgzest_catalog::model::{CastContext, CastMethod};

use crate::types::{CATEGORY_STRING, Ty, TypeSystem, is_polymorphic, pg};

/// Context of a conversion (`CoercionContext`): what is allowed grows
/// from implicit (expression) to explicit (`CAST`, `::`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum CoercionContext {
    /// Operator or function argument, operand of an expression.
    Implicit,
    /// Value assigned to a column (`INSERT`, `UPDATE`).
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

/// Conversion pathway (`CoercionPathType`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pathway {
    /// No conversion possible in this context.
    None,
    /// Same representation (identical types, domain and its base,
    /// binary conversion from `pg_cast`).
    Relabel,
    /// `pg_cast` entry implemented by a function.
    Cast,
    /// Array converted element by element.
    ArrayCoerce,
    /// Through the text representation (output then input).
    ViaIo,
}

/// Failure of [`TypeSystem::common_type`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommonTypeError {
    /// Two types of different categories (`text` and `integer`).
    CategoryMismatch(Ty, Ty),
    /// A type does not convert implicitly to the chosen type.
    NotCoercible { from: Ty, to: Ty },
}

impl TypeSystem<'_> {
    /// `find_coercion_pathway`: how to convert `source` into `target`.
    pub fn pathway(&self, source: &Ty, target: &Ty, ctx: CoercionContext) -> Pathway {
        if source == target {
            return Pathway::Relabel;
        }
        // A domain converts to and from its base type.
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
        // Without a `pg_cast` entry: array to array if the elements
        // convert (never to `oidvector` nor `int2vector`)…
        if target != pg("oidvector")
            && target != pg("int2vector")
            && let (Some(te), Some(se)) = (self.element(&target), self.element(&source))
            && self.pathway(&se, &te, ctx) != Pathway::None
        {
            return Pathway::ArrayCoerce;
        }
        // … otherwise conversion through text: to a string in assignment,
        // from a string in explicit context.
        if (ctx >= CoercionContext::Assignment && self.category(&target) == CATEGORY_STRING)
            || (ctx == CoercionContext::Explicit && self.category(&source) == CATEGORY_STRING)
        {
            return Pathway::ViaIo;
        }
        Pathway::None
    }

    /// `can_coerce_type`: does each input convert to its target?
    /// Polymorphic targets are checked together, last.
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
            // An untyped literal becomes what the target asks for.
            if *input == unknown {
                continue;
            }
            if self.pathway(input, target, ctx) != Pathway::None {
                continue;
            }
            // `record` and composite types convert to each other.
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

    /// `select_common_type`: common type of the branches of a `UNION`, of a
    /// `CASE`, of the arguments of `COALESCE`, of the rows of `VALUES`…
    pub fn common_type(&self, inputs: &[Ty]) -> Result<Ty, CommonTypeError> {
        let unknown = pg("unknown");
        let Some(first) = inputs.first() else {
            return Ok(pg("text"));
        };
        // The only way to get a domain: all identical.
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
                // Only literals so far: take what comes.
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
                // The new type accepts the old one implicitly, not the reverse;
                // a preferred type is never left.
                (ptype, pcategory, ppreferred) = (ntype, ncategory, npreferred);
            }
        }
        // Only literals: `text`.
        if ptype == unknown {
            ptype = pg("text");
        }
        // `verify_common_type`: everything must be convertible to it.
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
