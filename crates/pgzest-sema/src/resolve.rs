//! Choice of a function overload (`func_get_detail`,
//! `func_select_candidate`) or of an operator (`oper_select_candidate`).

use pgzest_catalog::model::{ArgMode, Operator, Routine};
use pgzest_catalog::{QName, SearchPath};

use crate::coerce::{CoercionContext, Pathway};
use crate::polymorphic::PolymorphicError;
use crate::types::{CATEGORY_STRING, Ty, TypeSystem, pg};

/// A candidate overload, its argument types expanded to the call
/// (variadic unfolded).
#[derive(Debug, Clone)]
struct Candidate<'c, T> {
    target: &'c T,
    /// Declared types, one per call argument.
    args: Vec<Ty>,
    /// Variadic arguments passed one by one.
    variadic: bool,
    /// Number of arguments filled in by their default value.
    defaults: usize,
    /// Rank of the schema in the search path.
    path_rank: usize,
}

/// What a resolved call designates.
#[derive(Debug, Clone)]
pub enum Resolution<'c> {
    /// A routine.
    Routine(&'c Routine),
    /// An operator.
    Operator(&'c Operator),
    /// `type_name(x)`: a conversion, not a function call.
    Coercion,
}

/// Resolved call: the overload, the expected types for each argument
/// (polymorphism resolved) and the result type.
#[derive(Debug, Clone)]
pub struct ResolvedCall<'c> {
    pub target: Resolution<'c>,
    pub arg_types: Vec<Ty>,
    pub returns: Option<Ty>,
    /// Variadic arguments passed one by one.
    pub variadic: bool,
    /// Arguments filled in by their default value.
    pub defaults: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveError {
    /// No overload fits.
    NotFound,
    /// Several overloads fit without any of them winning.
    Ambiguous,
    /// Overload chosen, but inconsistent polymorphic types.
    Polymorphic(String),
}

impl From<PolymorphicError> for ResolveError {
    fn from(e: PolymorphicError) -> Self {
        ResolveError::Polymorphic(e.0)
    }
}

/// Declared variadic element (`provariadic`) of a `VARIADIC` argument.
fn variadic_element(ts: &TypeSystem, declared: &Ty) -> Option<Ty> {
    if *declared == pg("any") {
        return Some(pg("any"));
    }
    if *declared == pg("anyarray") {
        return Some(pg("anyelement"));
    }
    if *declared == pg("anycompatiblearray") {
        return Some(pg("anycompatible"));
    }
    ts.element(declared)
}

impl<'c> TypeSystem<'c> {
    /// `FuncnameGetCandidates`: overloads visible under this name for
    /// `nargs` arguments, defaults and variadic included. A signature in one
    /// schema hides the same one in a schema further down the path.
    fn routine_candidates(
        &self,
        schema: Option<&str>,
        name: &str,
        nargs: usize,
        names: &[Option<String>],
        path: &SearchPath,
    ) -> Vec<Candidate<'c, Routine>> {
        let schemas: Vec<&str> = match schema {
            Some(s) => vec![s],
            None => path.lookup_order(),
        };
        let mut out: Vec<Candidate<'c, Routine>> = Vec::new();
        for (rank, s) in schemas.iter().enumerate() {
            let Some(overloads) = self.catalog.routines.get(&QName::new(*s, name)) else {
                continue;
            };
            for r in overloads {
                let Some(mut args) = r
                    .args
                    .iter()
                    .filter(|a| matches!(a.mode, ArgMode::In | ArgMode::InOut | ArgMode::Variadic))
                    .map(|a| self.of_ref(&a.ty))
                    .collect::<Option<Vec<Ty>>>()
                else {
                    continue;
                };
                let pronargs = args.len();
                let is_variadic = r.args.iter().any(|a| a.mode == ArgMode::Variadic);
                let ndefaults = r.args.iter().filter(|a| a.has_default).count();
                // Named notation (`MatchNamedCall`): positional ones first,
                // then each name designates a remaining parameter; the
                // parameters not supplied have a default value. No
                // variadic unfolding.
                if names.iter().any(Option::is_some) {
                    let inputs: Vec<_> = r
                        .args
                        .iter()
                        .filter(|a| {
                            matches!(a.mode, ArgMode::In | ArgMode::InOut | ArgMode::Variadic)
                        })
                        .collect();
                    if nargs > pronargs {
                        continue;
                    }
                    let npos = names.iter().take_while(|n| n.is_none()).count();
                    let mut used = vec![false; pronargs];
                    let mut order: Vec<usize> = (0..npos).collect();
                    used[..npos].iter_mut().for_each(|u| *u = true);
                    let mut matched = true;
                    for n in &names[npos..] {
                        let pos = n
                            .as_deref()
                            .and_then(|n| inputs.iter().position(|a| a.name.as_deref() == Some(n)));
                        match pos {
                            Some(i) if !used[i] => {
                                used[i] = true;
                                order.push(i);
                            }
                            _ => {
                                matched = false;
                                break;
                            }
                        }
                    }
                    let first_default = pronargs - ndefaults;
                    if !matched
                        || used
                            .iter()
                            .enumerate()
                            .any(|(i, u)| !u && i < first_default)
                    {
                        continue;
                    }
                    let candidate = Candidate {
                        target: r,
                        args: order.iter().map(|&i| args[i].clone()).collect(),
                        variadic: false,
                        defaults: pronargs - nargs,
                        path_rank: rank,
                    };
                    match out.iter_mut().find(|c| c.args == candidate.args) {
                        Some(prev) if prev.path_rank > rank => *prev = candidate,
                        Some(_) => {}
                        None => out.push(candidate),
                    }
                    continue;
                }
                let variadic = is_variadic && pronargs <= nargs;
                let defaults = if pronargs > nargs {
                    if nargs + ndefaults < pronargs {
                        continue;
                    }
                    pronargs - nargs
                } else {
                    0
                };
                if pronargs != nargs && !variadic && defaults == 0 {
                    continue;
                }
                if variadic {
                    let Some(elem) = variadic_element(self, &args[pronargs - 1]) else {
                        continue;
                    };
                    args.truncate(pronargs - 1);
                    args.resize(nargs, elem);
                } else {
                    args.truncate(nargs);
                }
                let candidate = Candidate {
                    target: r,
                    args,
                    variadic,
                    defaults,
                    path_rank: rank,
                };
                // Same signature already seen: the schema earliest in the
                // path wins; at equal rank, the non-unfolded form.
                match out.iter_mut().find(|c| c.args == candidate.args) {
                    Some(prev)
                        if prev.path_rank > rank || (prev.variadic && !candidate.variadic) =>
                    {
                        *prev = candidate
                    }
                    Some(_) => {}
                    None => out.push(candidate),
                }
            }
        }
        out
    }

    /// `func_match_argtypes`: candidates accepting the arguments through
    /// implicit conversion.
    fn matching<'a, T>(
        &self,
        inputs: &[Ty],
        cands: &'a [Candidate<'c, T>],
    ) -> Vec<&'a Candidate<'c, T>> {
        cands
            .iter()
            .filter(|c| self.can_coerce(inputs, &c.args, CoercionContext::Implicit))
            .collect()
    }

    /// `func_select_candidate`: heuristic tie-break between candidates that
    /// all fit.
    fn select_candidate<'a, T>(
        &self,
        inputs: &[Ty],
        mut cands: Vec<&'a Candidate<'c, T>>,
    ) -> Option<&'a Candidate<'c, T>> {
        let unknown = pg("unknown");
        let base: Vec<Ty> = inputs.iter().map(|t| self.base_type(t)).collect();
        let nunknowns = base.iter().filter(|t| **t == unknown).count();

        // Keeps the candidates that maximize `score`.
        let keep_best = |cands: Vec<&'a Candidate<'c, T>>,
                         score: &dyn Fn(&Candidate<'c, T>) -> usize|
         -> Vec<&'a Candidate<'c, T>> {
            let best = cands.iter().map(|c| score(c)).max().unwrap_or(0);
            cands.into_iter().filter(|c| score(c) == best).collect()
        };

        // 1. Most exact matches.
        cands = keep_best(cands, &|c| {
            base.iter()
                .zip(&c.args)
                .filter(|(i, a)| **i != unknown && i == a)
                .count()
        });
        if cands.len() == 1 {
            return cands.pop();
        }
        // 2. Exact matches or preferred types of the argument's category,
        // where a conversion is needed.
        cands = keep_best(cands, &|c| {
            base.iter()
                .zip(&c.args)
                .filter(|(i, a)| {
                    **i != unknown && (i == a || self.is_preferred(self.category(i), a))
                })
                .count()
        });
        if cands.len() == 1 {
            return cands.pop();
        }
        // 3. Type the literals: the category all candidates share at this
        // position, `S` (string) always winning.
        if nunknowns == 0 {
            return None;
        }
        let mut slots: Vec<Option<(char, bool)>> = vec![None; base.len()];
        let mut resolved = true;
        for (i, input) in base.iter().enumerate() {
            if *input != unknown {
                continue;
            }
            let mut slot: Option<(char, bool)> = None;
            let mut conflict = false;
            for c in &cands {
                let cat = self.category(&c.args[i]);
                let pref = self.is_preferred(cat, &c.args[i]);
                match &mut slot {
                    None => slot = Some((cat, pref)),
                    Some((scat, spref)) if *scat == cat => *spref |= pref,
                    Some(_) if cat == CATEGORY_STRING => slot = Some((cat, pref)),
                    Some(_) => conflict = true,
                }
            }
            if conflict && slot.is_some_and(|(c, _)| c != CATEGORY_STRING) {
                resolved = false;
                break;
            }
            slots[i] = slot;
        }
        if resolved {
            cands.retain(|c| {
                slots.iter().enumerate().all(|(i, slot)| match slot {
                    None => true,
                    Some((cat, pref)) => {
                        let a = &c.args[i];
                        let acat = self.category(a);
                        acat == *cat && (!pref || self.is_preferred(acat, a))
                    }
                })
            });
            if cands.len() == 1 {
                return cands.pop();
            }
        }
        // 4. Last chance: known arguments all of the same type, the
        // literals are assumed to be of that type.
        if nunknowns < base.len() {
            let mut known = base.iter().filter(|t| **t != unknown);
            let first = known.next().cloned()?;
            if known.all(|t| *t == first) {
                let assumed = vec![first; base.len()];
                let fit: Vec<_> = cands
                    .into_iter()
                    .filter(|c| self.can_coerce(&assumed, &c.args, CoercionContext::Implicit))
                    .collect();
                if let [one] = fit.as_slice() {
                    return Some(*one);
                }
            }
        }
        None
    }

    /// Exact, then through implicit conversions, then tie-break.
    fn choose<'a, T>(
        &self,
        inputs: &[Ty],
        cands: &'a [Candidate<'c, T>],
    ) -> Result<&'a Candidate<'c, T>, ResolveError> {
        if let Some(exact) = cands.iter().find(|c| c.args == inputs) {
            return Ok(exact);
        }
        let mut matching = self.matching(inputs, cands);
        match matching.len() {
            0 => Err(ResolveError::NotFound),
            1 => Ok(matching.pop().expect("one candidate")),
            _ => self
                .select_candidate(inputs, matching)
                .ok_or(ResolveError::Ambiguous),
        }
    }

    /// Resolves the call `schema.name(inputs…)` (`func_get_detail`).
    pub fn resolve_function(
        &self,
        schema: Option<&str>,
        name: &str,
        inputs: &[Ty],
        path: &SearchPath,
    ) -> Result<ResolvedCall<'c>, ResolveError> {
        self.resolve_function_named(schema, name, inputs, &vec![None; inputs.len()], path)
    }

    /// Like [`Self::resolve_function`], with the name of each argument
    /// (`None`: positional; `Some`: `name => value`). The returned types
    /// follow the order of the call.
    pub fn resolve_function_named(
        &self,
        schema: Option<&str>,
        name: &str,
        inputs: &[Ty],
        names: &[Option<String>],
        path: &SearchPath,
    ) -> Result<ResolvedCall<'c>, ResolveError> {
        let cands = self.routine_candidates(schema, name, inputs.len(), names, path);
        let named = names.iter().any(Option::is_some);
        let chosen = match cands.iter().find(|c| c.args == inputs) {
            Some(exact) => Some(exact),
            None => {
                // `type_name(x)` without an exact function: a conversion.
                if let [input] = inputs
                    && !named
                    && let Some(target) = self.function_style_cast(schema, name, input, path)
                {
                    self.depend(crate::types::Dependency::Type(target.clone()));
                    return Ok(ResolvedCall {
                        target: Resolution::Coercion,
                        arg_types: vec![target.clone()],
                        returns: Some(target),
                        variadic: false,
                        defaults: 0,
                    });
                }
                None
            }
        };
        let chosen = match chosen {
            Some(c) => c,
            None => self.choose(inputs, &cands)?,
        };
        let r = chosen.target;
        let returns = r.returns.as_ref().and_then(|t| self.of_ref(t));
        let (arg_types, returns) = self.resolve_generic(inputs, &chosen.args, returns.as_ref())?;
        self.depend(crate::types::Dependency::Routine(
            r.name.clone(),
            r.input_types().map(ToString::to_string).collect(),
        ));
        Ok(ResolvedCall {
            target: Resolution::Routine(r),
            arg_types,
            returns,
            variadic: chosen.variadic,
            defaults: chosen.defaults,
        })
    }

    /// `type_name(x)` treated as `x::type_name`: literal, same
    /// representation, or conversion through text (except from a composite
    /// to a string).
    fn function_style_cast(
        &self,
        schema: Option<&str>,
        name: &str,
        input: &Ty,
        path: &SearchPath,
    ) -> Option<Ty> {
        let target = self.catalog.resolve_type(schema, name, path)?.name.clone();
        if *input == pg("unknown") {
            return Some(target);
        }
        match self.pathway(input, &target, CoercionContext::Explicit) {
            Pathway::Relabel => Some(target),
            Pathway::ViaIo
                if !(self.is_complex(input) && self.category(&target) == CATEGORY_STRING) =>
            {
                Some(target)
            }
            _ => None,
        }
    }

    /// Operators visible under this name, prefix (`left` absent) or
    /// binary.
    fn operator_candidates(
        &self,
        schema: Option<&str>,
        name: &str,
        prefix: bool,
        path: &SearchPath,
    ) -> Vec<Candidate<'c, Operator>> {
        let schemas: Vec<&str> = match schema {
            Some(s) => vec![s],
            None => path.lookup_order(),
        };
        let mut out: Vec<Candidate<'c, Operator>> = Vec::new();
        for (rank, s) in schemas.iter().enumerate() {
            let list = self.catalog.operators.get(&QName::new(*s, name));
            for o in list.into_iter().flatten() {
                if o.left.is_some() == prefix {
                    continue;
                }
                let Some(args) = o
                    .left
                    .iter()
                    .chain(std::iter::once(&o.right))
                    .map(|t| self.of_ref(t))
                    .collect::<Option<Vec<Ty>>>()
                else {
                    continue;
                };
                if out.iter().any(|c| c.args == args) {
                    continue;
                }
                out.push(Candidate {
                    target: o,
                    args,
                    variadic: false,
                    defaults: 0,
                    path_rank: rank,
                });
            }
        }
        out
    }

    /// Resolves `left op right`, or `op right` for a prefix operator.
    pub fn resolve_operator(
        &self,
        schema: Option<&str>,
        name: &str,
        left: Option<&Ty>,
        right: &Ty,
        path: &SearchPath,
    ) -> Result<ResolvedCall<'c>, ResolveError> {
        let cands = self.operator_candidates(schema, name, left.is_none(), path);
        let inputs: Vec<Ty> = left.into_iter().chain(Some(right)).cloned().collect();
        let chosen = match self.binary_oper_exact(&inputs, &cands) {
            Some(c) => c,
            None => self.choose(&inputs, &cands)?,
        };
        let o = chosen.target;
        let returns = self.of_ref(&o.result);
        let (arg_types, returns) = self.resolve_generic(&inputs, &chosen.args, returns.as_ref())?;
        self.depend(crate::types::Dependency::Operator(
            o.name.clone(),
            o.left.as_ref().map(ToString::to_string),
            o.right.to_string(),
        ));
        Ok(ResolvedCall {
            target: Resolution::Operator(o),
            arg_types,
            returns,
            variadic: false,
            defaults: 0,
        })
    }

    /// `binary_oper_exact`: exact match, an untyped literal being assumed
    /// of the type of the other operand (then of its base type if it is a
    /// domain).
    fn binary_oper_exact<'a>(
        &self,
        inputs: &[Ty],
        cands: &'a [Candidate<'c, Operator>],
    ) -> Option<&'a Candidate<'c, Operator>> {
        let unknown = pg("unknown");
        let mut args = inputs.to_vec();
        let mut was_unknown = false;
        if let [l, r] = args.as_mut_slice() {
            if *l == unknown && *r != unknown {
                *l = r.clone();
                was_unknown = true;
            } else if *r == unknown && *l != unknown {
                *r = l.clone();
                was_unknown = true;
            }
        }
        if let Some(c) = cands.iter().find(|c| c.args == args) {
            return Some(c);
        }
        if was_unknown {
            let base = self.base_type(&args[0]);
            if base != args[0] {
                let args = vec![base.clone(), base];
                return cands.iter().find(|c| c.args == args);
            }
        }
        None
    }
}
