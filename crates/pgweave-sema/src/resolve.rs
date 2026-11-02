//! Choix d'une surcharge de fonction (`func_get_detail`,
//! `func_select_candidate`) ou d'opérateur (`oper_select_candidate`).

use pgweave_catalog::model::{ArgMode, Operator, Routine};
use pgweave_catalog::{QName, SearchPath};

use crate::coerce::{CoercionContext, Pathway};
use crate::polymorphic::PolymorphicError;
use crate::types::{CATEGORY_STRING, Ty, TypeSystem, pg};

/// Une surcharge candidate, ses types d'arguments étendus à l'appel
/// (variadique déplié).
#[derive(Debug, Clone)]
struct Candidate<'c, T> {
    target: &'c T,
    /// Types déclarés, un par argument de l'appel.
    args: Vec<Ty>,
    /// Arguments variadiques passés un par un.
    variadic: bool,
    /// Nombre d'arguments complétés par leur valeur par défaut.
    defaults: usize,
    /// Rang du schéma dans le chemin de recherche.
    path_rank: usize,
}

/// Ce que désigne un appel résolu.
#[derive(Debug, Clone)]
pub enum Resolution<'c> {
    /// Une routine.
    Routine(&'c Routine),
    /// Un opérateur.
    Operator(&'c Operator),
    /// `nom_de_type(x)` : conversion, pas un appel de fonction.
    Coercion,
}

/// Appel résolu : la surcharge, les types attendus pour chaque argument
/// (polymorphisme résolu) et le type du résultat.
#[derive(Debug, Clone)]
pub struct ResolvedCall<'c> {
    pub target: Resolution<'c>,
    pub arg_types: Vec<Ty>,
    pub returns: Option<Ty>,
    /// Arguments variadiques passés un par un.
    pub variadic: bool,
    /// Arguments complétés par leur valeur par défaut.
    pub defaults: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveError {
    /// Aucune surcharge ne convient.
    NotFound,
    /// Plusieurs surcharges conviennent sans qu'aucune ne l'emporte.
    Ambiguous,
    /// Surcharge choisie, mais types polymorphes incohérents.
    Polymorphic(String),
}

impl From<PolymorphicError> for ResolveError {
    fn from(e: PolymorphicError) -> Self {
        ResolveError::Polymorphic(e.0)
    }
}

/// Élément variadique déclaré (`provariadic`) d'un argument `VARIADIC`.
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
    /// `FuncnameGetCandidates` : surcharges visibles sous ce nom pour
    /// `nargs` arguments, défauts et variadique compris. Une signature d'un
    /// schéma masque la même dans un schéma plus loin du chemin.
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
                // Notation nommée (`MatchNamedCall`) : positionnels d'abord,
                // puis chaque nom désigne un paramètre restant ; les
                // paramètres non fournis ont une valeur par défaut. Pas de
                // dépliage variadique.
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
                // Même signature déjà vue : le schéma le plus tôt dans le
                // chemin l'emporte ; à rang égal, la forme non dépliée.
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

    /// `func_match_argtypes` : candidats acceptant les arguments par
    /// conversion implicite.
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

    /// `func_select_candidate` : départage heuristique de candidats qui
    /// conviennent tous.
    fn select_candidate<'a, T>(
        &self,
        inputs: &[Ty],
        mut cands: Vec<&'a Candidate<'c, T>>,
    ) -> Option<&'a Candidate<'c, T>> {
        let unknown = pg("unknown");
        let base: Vec<Ty> = inputs.iter().map(|t| self.base_type(t)).collect();
        let nunknowns = base.iter().filter(|t| **t == unknown).count();

        // Garde les candidats qui maximisent `score`.
        let keep_best = |cands: Vec<&'a Candidate<'c, T>>,
                         score: &dyn Fn(&Candidate<'c, T>) -> usize|
         -> Vec<&'a Candidate<'c, T>> {
            let best = cands.iter().map(|c| score(c)).max().unwrap_or(0);
            cands.into_iter().filter(|c| score(c) == best).collect()
        };

        // 1. Le plus de correspondances exactes.
        cands = keep_best(cands, &|c| {
            base.iter()
                .zip(&c.args)
                .filter(|(i, a)| **i != unknown && i == a)
                .count()
        });
        if cands.len() == 1 {
            return cands.pop();
        }
        // 2. Correspondances exactes ou types préférés de la catégorie de
        // l'argument, là où une conversion est nécessaire.
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
        // 3. Typer les littéraux : la catégorie que tous les candidats
        // partagent à cette position, `S` (chaîne) l'emportant toujours.
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
        // 4. Dernière chance : arguments connus tous du même type, les
        // littéraux sont supposés de ce type.
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

    /// Exacte, puis par conversions implicites, puis départage.
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
            1 => Ok(matching.pop().expect("un candidat")),
            _ => self
                .select_candidate(inputs, matching)
                .ok_or(ResolveError::Ambiguous),
        }
    }

    /// Résout l'appel `schema.name(inputs…)` (`func_get_detail`).
    pub fn resolve_function(
        &self,
        schema: Option<&str>,
        name: &str,
        inputs: &[Ty],
        path: &SearchPath,
    ) -> Result<ResolvedCall<'c>, ResolveError> {
        self.resolve_function_named(schema, name, inputs, &vec![None; inputs.len()], path)
    }

    /// Comme [`Self::resolve_function`], avec le nom de chaque argument
    /// (`None` : positionnel ; `Some` : `nom => valeur`). Les types renvoyés
    /// suivent l'ordre de l'appel.
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
                // `nom_de_type(x)` sans fonction exacte : une conversion.
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

    /// `nom_de_type(x)` traité comme `x::nom_de_type` : littéral, même
    /// représentation, ou conversion par le texte (sauf d'un composite vers
    /// une chaîne).
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

    /// Opérateurs visibles sous ce nom, préfixes (`left` absent) ou
    /// binaires.
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

    /// Résout `left op right`, ou `op right` pour un opérateur préfixe.
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

    /// `binary_oper_exact` : correspondance exacte, un littéral sans type
    /// étant supposé du type de l'autre opérande (puis de son type de base
    /// s'il s'agit d'un domaine).
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
