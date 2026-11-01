//! Système de types confronté à PostgreSQL 18 : chaque attendu a été relevé
//! sur un serveur (`pg_typeof`, messages d'erreur).

use std::sync::OnceLock;

use pgweave_catalog::{Catalog, SearchPath};
use pgweave_sema::coerce::CommonTypeError;
use pgweave_sema::{CoercionContext, Pathway, ResolveError, Ty, TypeSystem, pg};

fn catalog() -> &'static Catalog {
    static CAT: OnceLock<Catalog> = OnceLock::new();
    CAT.get_or_init(|| Catalog::for_postgres(18).expect("pack postgres@18"))
}

fn ts() -> TypeSystem<'static> {
    TypeSystem::new(catalog())
}

fn t(name: &str) -> Ty {
    pg(name)
}

fn func(name: &str, args: &[&str]) -> Result<Option<String>, ResolveError> {
    let args: Vec<Ty> = args.iter().map(|a| t(a)).collect();
    ts().resolve_function(None, name, &args, &SearchPath::default())
        .map(|r| r.returns.map(|q| q.name))
}

fn op(left: Option<&str>, name: &str, right: &str) -> Result<Option<String>, ResolveError> {
    let left = left.map(t);
    ts().resolve_operator(None, name, left.as_ref(), &t(right), &SearchPath::default())
        .map(|r| r.returns.map(|q| q.name))
}

fn ok(name: &str) -> Result<Option<String>, ResolveError> {
    Ok(Some(name.to_string()))
}

#[test]
fn surcharges_de_fonctions() {
    assert_eq!(func("abs", &["int4"]), ok("int4"));
    assert_eq!(func("abs", &["numeric"]), ok("numeric"));
    // Littéral : le type préféré de la catégorie commune l'emporte.
    assert_eq!(func("abs", &["unknown"]), ok("float8"));
    // `text` et `bpchar` sont des chaînes, `text` est préféré.
    assert_eq!(func("length", &["unknown"]), ok("int4"));
    // int4 → float8 et numeric implicites : float8 est préféré.
    assert_eq!(func("round", &["int4"]), ok("float8"));
    assert_eq!(
        func("date_trunc", &["unknown", "timestamptz"]),
        ok("timestamptz")
    );
    assert_eq!(func("generate_series", &["int4", "int4"]), ok("int4"));
    assert_eq!(func("upper", &["unknown"]), ok("text"));
    assert_eq!(func("now", &[]), ok("timestamptz"));
    assert_eq!(func("lower", &["int4"]), Err(ResolveError::NotFound));
    assert_eq!(
        func("date_trunc", &["unknown", "unknown"]),
        Err(ResolveError::Ambiguous)
    );
}

#[test]
fn polymorphisme() {
    assert_eq!(func("array_append", &["_int4", "int4"]), ok("_int4"));
    // `anycompatible` : type commun de int4 et numeric.
    assert_eq!(func("array_append", &["_int4", "numeric"]), ok("_numeric"));
    assert!(matches!(
        func("array_append", &["int4", "int4"]),
        Err(ResolveError::NotFound)
    ));
}

#[test]
fn variadiques() {
    assert_eq!(func("concat", &["unknown", "int4"]), ok("text"));
    assert_eq!(func("format", &["unknown", "int4", "int4"]), ok("text"));
    assert_eq!(
        func("jsonb_extract_path", &["jsonb", "unknown", "unknown"]),
        ok("jsonb")
    );
}

#[test]
fn nom_de_type_appele_comme_une_fonction() {
    // Pas de fonction text(int4) : conversion par le texte.
    assert_eq!(func("text", &["int4"]), ok("text"));
    // int8(int4) existe : c'est elle.
    assert_eq!(func("int8", &["int4"]), ok("int8"));
    // Conversion binaire (`castmethod = 'b'`), sans fonction du même nom.
    assert_eq!(func("text", &["varchar"]), ok("text"));
    assert_eq!(func("varchar", &["text"]), ok("varchar"));
    let r = ts()
        .resolve_function(None, "text", &[t("varchar")], &SearchPath::default())
        .unwrap();
    assert!(matches!(r.target, pgweave_sema::Resolution::Coercion));
}

#[test]
fn operateurs() {
    assert_eq!(op(Some("int4"), "+", "numeric"), ok("numeric"));
    // Littéral : supposé du type de l'autre opérande.
    assert_eq!(op(Some("int4"), "+", "unknown"), ok("int4"));
    assert_eq!(op(Some("timestamptz"), "-", "unknown"), ok("interval"));
    assert_eq!(op(Some("unknown"), "||", "unknown"), ok("text"));
    // text || anynonarray, la catégorie chaîne départageant.
    assert_eq!(op(Some("unknown"), "||", "int4"), ok("text"));
    assert_eq!(op(Some("_int4"), "||", "int4"), ok("_int4"));
    assert_eq!(op(Some("timestamptz"), "-", "timestamptz"), ok("interval"));
    assert_eq!(op(Some("date"), "+", "int4"), ok("date"));
    assert_eq!(op(Some("jsonb"), "->", "unknown"), ok("jsonb"));
    assert_eq!(op(Some("unknown"), "=", "unknown"), ok("bool"));
    assert_eq!(op(None, "-", "int4"), ok("int4"));
}

#[test]
fn chemins_de_conversion() {
    let ts = ts();
    let path = |s: &str, d: &str, c| ts.pathway(&t(s), &t(d), c);
    assert_eq!(
        path("int4", "int8", CoercionContext::Implicit),
        Pathway::Cast
    );
    assert_eq!(
        path("int8", "int4", CoercionContext::Implicit),
        Pathway::None
    );
    assert_eq!(
        path("int8", "int4", CoercionContext::Assignment),
        Pathway::Cast
    );
    // Vers une chaîne : par le texte, en affectation seulement.
    assert_eq!(
        path("int4", "text", CoercionContext::Implicit),
        Pathway::None
    );
    assert_eq!(
        path("int4", "text", CoercionContext::Assignment),
        Pathway::ViaIo
    );
    // Depuis une chaîne : en explicite seulement.
    assert_eq!(
        path("text", "uuid", CoercionContext::Assignment),
        Pathway::None
    );
    assert_eq!(
        path("text", "uuid", CoercionContext::Explicit),
        Pathway::ViaIo
    );
    assert_eq!(
        path("_int4", "_int8", CoercionContext::Implicit),
        Pathway::ArrayCoerce
    );
}

#[test]
fn type_commun() {
    let ts = ts();
    let common = |types: &[&str]| {
        ts.common_type(&types.iter().map(|x| t(x)).collect::<Vec<_>>())
            .map(|q| q.name)
    };
    assert_eq!(common(&["int4", "numeric"]), Ok("numeric".into()));
    assert_eq!(common(&["int4", "int8"]), Ok("int8".into()));
    assert_eq!(common(&["unknown", "int4"]), Ok("int4".into()));
    assert_eq!(common(&["unknown", "unknown"]), Ok("text".into()));
    // Un type préféré n'est pas quitté ; numeric rejoint float8.
    assert_eq!(common(&["float8", "numeric"]), Ok("float8".into()));
    assert_eq!(common(&["numeric", "float8"]), Ok("float8".into()));
    // Conversions implicites dans les deux sens : le premier reste. (Un
    // `CASE` donne `text` pour `THEN varchar ELSE text` : PostgreSQL place
    // le `ELSE` en tête.)
    assert_eq!(common(&["varchar", "text"]), Ok("varchar".into()));
    assert_eq!(common(&["text", "varchar"]), Ok("text".into()));
    assert_eq!(common(&["date", "timestamptz"]), Ok("timestamptz".into()));
    assert!(matches!(
        common(&["int4", "text"]),
        Err(CommonTypeError::CategoryMismatch(..))
    ));
}

#[test]
fn arguments_nommes() {
    let ts = ts();
    let named = |args: &[(&str, Option<&str>)]| {
        let tys: Vec<Ty> = args.iter().map(|(t_, _)| t(t_)).collect();
        let names: Vec<Option<String>> = args.iter().map(|(_, n)| n.map(str::to_string)).collect();
        ts.resolve_function_named(None, "make_interval", &tys, &names, &SearchPath::default())
            .map(|r| r.returns.map(|q| q.name))
    };
    // Les autres paramètres ont une valeur par défaut.
    assert_eq!(named(&[("int4", Some("days"))]), ok("interval"));
    assert_eq!(
        named(&[("int4", None), ("int4", Some("hours"))]),
        ok("interval")
    );
    assert_eq!(
        named(&[("int4", Some("nope"))]),
        Err(ResolveError::NotFound)
    );
    // Un paramètre déjà fourni par position ne peut pas être renommé.
    assert_eq!(
        named(&[("int4", None), ("int4", Some("years"))]),
        Err(ResolveError::NotFound)
    );
}
