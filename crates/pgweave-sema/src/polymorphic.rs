//! Types polymorphes : `anyelement` et sa famille, `anycompatible` et la
//! sienne (`check_generic_type_consistency`,
//! `enforce_generic_type_consistency`).

use crate::types::{Ty, TypeSystem, is_polymorphic, pg};

/// Ce que les arguments réels disent des deux familles polymorphes.
#[derive(Default)]
struct Deduced {
    /// Famille `anyelement` : élément, tableau, intervalle, multi-intervalle.
    elem: Option<Ty>,
    array: Option<Ty>,
    range: Option<Ty>,
    multirange: Option<Ty>,
    nonarray: bool,
    is_enum: bool,
    /// Un argument de la famille `anyelement` est un littéral sans type.
    elem_family_unknown: bool,
    /// Famille `anycompatible` : types réels à unifier.
    compatible: Vec<Ty>,
    compatible_nonarray: bool,
    compatible_range: Option<Ty>,
    compatible_range_elem: Option<Ty>,
    /// Un argument de la famille `anycompatible` est présent.
    compatible_family: bool,
}

/// Échec de la résolution polymorphe : le message de PostgreSQL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolymorphicError(pub String);

fn name(t: &Ty) -> &str {
    if t.schema == "pg_catalog" {
        &t.name
    } else {
        ""
    }
}

impl TypeSystem<'_> {
    /// Collecte et vérifie ; `Err` porte la raison de l'incohérence.
    fn deduce(&self, actual: &[Ty], declared: &[Ty]) -> Result<Deduced, PolymorphicError> {
        let unknown = pg("unknown");
        let mut d = Deduced::default();
        let conflict = |what: &str| {
            PolymorphicError(format!("arguments declared \"{what}\" are not all alike"))
        };
        for (a, decl) in actual.iter().zip(declared) {
            if !is_polymorphic(decl) {
                continue;
            }
            let kind = name(decl);
            if !kind.starts_with("anycompatible") && *a == unknown {
                d.elem_family_unknown = true;
                continue;
            }
            match kind {
                "anyelement" | "anynonarray" | "anyenum" => {
                    d.nonarray |= kind == "anynonarray";
                    d.is_enum |= kind == "anyenum";
                    if d.elem.as_ref().is_some_and(|e| e != a) {
                        return Err(conflict("anyelement"));
                    }
                    d.elem = Some(a.clone());
                }
                "anyarray" => {
                    let a = self.base_type(a);
                    if d.array.as_ref().is_some_and(|x| *x != a) {
                        return Err(conflict("anyarray"));
                    }
                    d.array = Some(a);
                }
                "anyrange" => {
                    let a = self.base_type(a);
                    if d.range.as_ref().is_some_and(|x| *x != a) {
                        return Err(conflict("anyrange"));
                    }
                    d.range = Some(a);
                }
                "anymultirange" => {
                    let a = self.base_type(a);
                    if d.multirange.as_ref().is_some_and(|x| *x != a) {
                        return Err(conflict("anymultirange"));
                    }
                    d.multirange = Some(a);
                }
                _ => {
                    d.compatible_family = true;
                    if *a == unknown {
                        continue;
                    }
                    match kind {
                        "anycompatible" | "anycompatiblenonarray" => {
                            d.compatible_nonarray |= kind == "anycompatiblenonarray";
                            d.compatible.push(a.clone());
                        }
                        "anycompatiblearray" => {
                            let e = self.element(&self.base_type(a)).ok_or_else(|| {
                                PolymorphicError(format!(
                                    "argument declared anycompatiblearray is not an array but type {a}"
                                ))
                            })?;
                            d.compatible.push(e);
                        }
                        "anycompatiblerange" | "anycompatiblemultirange" => {
                            let mut r = self.base_type(a);
                            if kind == "anycompatiblemultirange" {
                                r = self.multirange_range(&r).ok_or_else(|| {
                                    PolymorphicError(format!(
                                        "argument declared anycompatiblemultirange is not a multirange type but type {a}"
                                    ))
                                })?;
                            }
                            if d.compatible_range.as_ref().is_some_and(|x| *x != r) {
                                return Err(conflict("anycompatiblerange"));
                            }
                            let sub = self.range_subtype(&r).ok_or_else(|| {
                                PolymorphicError(format!(
                                    "argument declared anycompatiblerange is not a range type but type {a}"
                                ))
                            })?;
                            d.compatible.push(sub.clone());
                            d.compatible_range = Some(r);
                            d.compatible_range_elem = Some(sub);
                        }
                        _ => unreachable!("type polymorphe inconnu : {kind}"),
                    }
                }
            }
        }

        // L'élément déduit du tableau, du multi-intervalle, de l'intervalle.
        let mut agree = |from: Ty, what: &str| -> Result<(), PolymorphicError> {
            match &d.elem {
                Some(e) if *e != from => Err(PolymorphicError(format!(
                    "argument declared {what} is not consistent with argument declared anyelement"
                ))),
                Some(_) => Ok(()),
                None => {
                    d.elem = Some(from);
                    Ok(())
                }
            }
        };
        if let Some(arr) = d.array.clone() {
            let e = self.element(&arr).ok_or_else(|| {
                PolymorphicError(format!(
                    "argument declared anyarray is not an array but type {arr}"
                ))
            })?;
            agree(e, "anyarray")?;
        }
        if let Some(mr) = d.multirange.clone() {
            let r = self.multirange_range(&mr).ok_or_else(|| {
                PolymorphicError(format!(
                    "argument declared anymultirange is not a multirange type but type {mr}"
                ))
            })?;
            match &d.range {
                Some(x) if *x != r => {
                    return Err(PolymorphicError(
                        "argument declared anymultirange is not consistent with argument declared anyrange".into(),
                    ));
                }
                _ => d.range = Some(r),
            }
        }
        if let Some(r) = d.range.clone() {
            let sub = self.range_subtype(&r).ok_or_else(|| {
                PolymorphicError(format!(
                    "argument declared anyrange is not a range type but type {r}"
                ))
            })?;
            agree(sub, "anyrange")?;
        }
        if let Some(e) = &d.elem {
            if d.nonarray && self.is_array_or_array_domain(e) {
                return Err(PolymorphicError(format!(
                    "type matched to anynonarray is an array type: {e}"
                )));
            }
            if d.is_enum && !self.is_enum(e) {
                return Err(PolymorphicError(format!(
                    "type matched to anyenum is not an enum type: {e}"
                )));
            }
        }
        Ok(d)
    }

    /// Type commun de la famille `anycompatible` (`text` si tout est
    /// littéral), vérifié contre ses contraintes.
    fn compatible_common(&self, d: &Deduced) -> Result<Option<Ty>, PolymorphicError> {
        if !d.compatible_family {
            return Ok(None);
        }
        if d.compatible.is_empty() {
            return Ok(Some(pg("text")));
        }
        let common = self.common_type(&d.compatible).map_err(|_| {
            PolymorphicError(
                "arguments declared \"anycompatible\" cannot be cast to a common type".into(),
            )
        })?;
        if d.compatible_nonarray && self.is_array_or_array_domain(&common) {
            return Err(PolymorphicError(format!(
                "type matched to anycompatiblenonarray is an array type: {common}"
            )));
        }
        if let Some(sub) = &d.compatible_range_elem
            && *sub != common
        {
            return Err(PolymorphicError(format!(
                "anycompatiblerange type {} does not match anycompatible type {common}",
                d.compatible_range
                    .as_ref()
                    .expect("intervalle déduit avec son sous-type")
            )));
        }
        Ok(Some(common))
    }

    /// `check_generic_type_consistency` : les arguments réels sont-ils
    /// compatibles avec les types polymorphes déclarés ?
    pub fn generic_consistent(&self, actual: &[Ty], declared: &[Ty]) -> bool {
        self.deduce(actual, declared)
            .and_then(|d| self.compatible_common(&d))
            .is_ok()
    }

    /// `enforce_generic_type_consistency` : remplace les types polymorphes
    /// des arguments et du résultat par les types réels.
    pub fn resolve_generic(
        &self,
        actual: &[Ty],
        declared: &[Ty],
        returns: Option<&Ty>,
    ) -> Result<(Vec<Ty>, Option<Ty>), PolymorphicError> {
        let any_poly = declared.iter().any(is_polymorphic) || returns.is_some_and(is_polymorphic);
        if !any_poly {
            return Ok((declared.to_vec(), returns.cloned()));
        }
        let d = self.deduce(actual, declared)?;
        let common = self.compatible_common(&d)?;
        let elem_family_used = declared
            .iter()
            .chain(returns)
            .any(|t| is_polymorphic(t) && !name(t).starts_with("anycompatible"));
        if elem_family_used && d.elem.is_none() && d.elem_family_unknown {
            return Err(PolymorphicError(
                "could not determine polymorphic type because input has type unknown".into(),
            ));
        }

        let subst = |t: &Ty| -> Result<Ty, PolymorphicError> {
            if !is_polymorphic(t) {
                return Ok(t.clone());
            }
            let missing = || {
                PolymorphicError(format!(
                    "could not determine polymorphic type {t} because input has no matching argument"
                ))
            };
            let no_array =
                |e: &Ty| PolymorphicError(format!("could not find array type for data type {e}"));
            match name(t) {
                "anyelement" | "anynonarray" | "anyenum" => d.elem.clone().ok_or_else(missing),
                "anyarray" => match &d.array {
                    Some(a) => Ok(a.clone()),
                    None => {
                        let e = d.elem.as_ref().ok_or_else(missing)?;
                        self.array_of(e).ok_or_else(|| no_array(e))
                    }
                },
                "anyrange" => d.range.clone().ok_or_else(missing),
                "anymultirange" => match &d.multirange {
                    Some(m) => Ok(m.clone()),
                    None => {
                        let r = d.range.as_ref().ok_or_else(missing)?;
                        self.range_multirange(r).ok_or_else(|| {
                            PolymorphicError(format!(
                                "could not find multirange type for data type {r}"
                            ))
                        })
                    }
                },
                "anycompatible" | "anycompatiblenonarray" => common.clone().ok_or_else(missing),
                "anycompatiblearray" => {
                    let c = common.as_ref().ok_or_else(missing)?;
                    self.array_of(c).ok_or_else(|| no_array(c))
                }
                "anycompatiblerange" => d.compatible_range.clone().ok_or_else(missing),
                "anycompatiblemultirange" => {
                    let r = d.compatible_range.as_ref().ok_or_else(missing)?;
                    self.range_multirange(r).ok_or_else(|| {
                        PolymorphicError(format!(
                            "could not find multirange type for data type {r}"
                        ))
                    })
                }
                _ => Err(missing()),
            }
        };
        let args = declared.iter().map(subst).collect::<Result<Vec<_>, _>>()?;
        let ret = returns.map(subst).transpose()?;
        Ok((args, ret))
    }
}
