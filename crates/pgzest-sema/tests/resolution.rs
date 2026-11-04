//! Type system checked against PostgreSQL 18: each expectation was taken
//! from a server (`pg_typeof`, error messages).

use std::sync::OnceLock;

use pgzest_catalog::{Catalog, SearchPath};
use pgzest_sema::coerce::CommonTypeError;
use pgzest_sema::{CoercionContext, Pathway, ResolveError, Ty, TypeSystem, pg};

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
fn function_overloads() {
    assert_eq!(func("abs", &["int4"]), ok("int4"));
    assert_eq!(func("abs", &["numeric"]), ok("numeric"));
    // Literal: the preferred type of the common category wins.
    assert_eq!(func("abs", &["unknown"]), ok("float8"));
    // `text` and `bpchar` are strings, `text` is preferred.
    assert_eq!(func("length", &["unknown"]), ok("int4"));
    // int4 → float8 and numeric are implicit: float8 is preferred.
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
fn polymorphism() {
    assert_eq!(func("array_append", &["_int4", "int4"]), ok("_int4"));
    // `anycompatible`: common type of int4 and numeric.
    assert_eq!(func("array_append", &["_int4", "numeric"]), ok("_numeric"));
    assert!(matches!(
        func("array_append", &["int4", "int4"]),
        Err(ResolveError::NotFound)
    ));
}

#[test]
fn variadics() {
    assert_eq!(func("concat", &["unknown", "int4"]), ok("text"));
    assert_eq!(func("format", &["unknown", "int4", "int4"]), ok("text"));
    assert_eq!(
        func("jsonb_extract_path", &["jsonb", "unknown", "unknown"]),
        ok("jsonb")
    );
}

#[test]
fn type_name_called_as_a_function() {
    // No text(int4) function: conversion through text.
    assert_eq!(func("text", &["int4"]), ok("text"));
    // int8(int4) exists: that is the one.
    assert_eq!(func("int8", &["int4"]), ok("int8"));
    // Binary conversion (`castmethod = 'b'`), without a function of the same name.
    assert_eq!(func("text", &["varchar"]), ok("text"));
    assert_eq!(func("varchar", &["text"]), ok("varchar"));
    let r = ts()
        .resolve_function(None, "text", &[t("varchar")], &SearchPath::default())
        .unwrap();
    assert!(matches!(r.target, pgzest_sema::Resolution::Coercion));
}

#[test]
fn operators() {
    assert_eq!(op(Some("int4"), "+", "numeric"), ok("numeric"));
    // Literal: assumed to be of the type of the other operand.
    assert_eq!(op(Some("int4"), "+", "unknown"), ok("int4"));
    assert_eq!(op(Some("timestamptz"), "-", "unknown"), ok("interval"));
    assert_eq!(op(Some("unknown"), "||", "unknown"), ok("text"));
    // text || anynonarray, the string category breaking the tie.
    assert_eq!(op(Some("unknown"), "||", "int4"), ok("text"));
    assert_eq!(op(Some("_int4"), "||", "int4"), ok("_int4"));
    assert_eq!(op(Some("timestamptz"), "-", "timestamptz"), ok("interval"));
    assert_eq!(op(Some("date"), "+", "int4"), ok("date"));
    assert_eq!(op(Some("jsonb"), "->", "unknown"), ok("jsonb"));
    assert_eq!(op(Some("unknown"), "=", "unknown"), ok("bool"));
    assert_eq!(op(None, "-", "int4"), ok("int4"));
}

#[test]
fn conversion_pathways() {
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
    // To a string: through text, in assignment only.
    assert_eq!(
        path("int4", "text", CoercionContext::Implicit),
        Pathway::None
    );
    assert_eq!(
        path("int4", "text", CoercionContext::Assignment),
        Pathway::ViaIo
    );
    // From a string: in explicit context only.
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
fn common_type() {
    let ts = ts();
    let common = |types: &[&str]| {
        ts.common_type(&types.iter().map(|x| t(x)).collect::<Vec<_>>())
            .map(|q| q.name)
    };
    assert_eq!(common(&["int4", "numeric"]), Ok("numeric".into()));
    assert_eq!(common(&["int4", "int8"]), Ok("int8".into()));
    assert_eq!(common(&["unknown", "int4"]), Ok("int4".into()));
    assert_eq!(common(&["unknown", "unknown"]), Ok("text".into()));
    // A preferred type is not left; numeric joins float8.
    assert_eq!(common(&["float8", "numeric"]), Ok("float8".into()));
    assert_eq!(common(&["numeric", "float8"]), Ok("float8".into()));
    // Implicit conversions both ways: the first one stays. (A
    // `CASE` gives `text` for `THEN varchar ELSE text`: PostgreSQL puts
    // the `ELSE` first.)
    assert_eq!(common(&["varchar", "text"]), Ok("varchar".into()));
    assert_eq!(common(&["text", "varchar"]), Ok("text".into()));
    assert_eq!(common(&["date", "timestamptz"]), Ok("timestamptz".into()));
    assert!(matches!(
        common(&["int4", "text"]),
        Err(CommonTypeError::CategoryMismatch(..))
    ));
}

#[test]
fn named_arguments() {
    let ts = ts();
    let named = |args: &[(&str, Option<&str>)]| {
        let tys: Vec<Ty> = args.iter().map(|(t_, _)| t(t_)).collect();
        let names: Vec<Option<String>> = args.iter().map(|(_, n)| n.map(str::to_string)).collect();
        ts.resolve_function_named(None, "make_interval", &tys, &names, &SearchPath::default())
            .map(|r| r.returns.map(|q| q.name))
    };
    // The other parameters have a default value.
    assert_eq!(named(&[("int4", Some("days"))]), ok("interval"));
    assert_eq!(
        named(&[("int4", None), ("int4", Some("hours"))]),
        ok("interval")
    );
    assert_eq!(
        named(&[("int4", Some("nope"))]),
        Err(ResolveError::NotFound)
    );
    // A parameter already supplied by position cannot be renamed.
    assert_eq!(
        named(&[("int4", None), ("int4", Some("years"))]),
        Err(ResolveError::NotFound)
    );
}
