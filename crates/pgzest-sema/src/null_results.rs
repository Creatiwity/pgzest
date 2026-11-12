//! Built-in routines and operators that return NULL for non-null
//! arguments.
//!
//! `STRICT` (`proisstrict`) only says that a NULL argument gives a NULL
//! result; nothing in the catalog says a routine never returns NULL by
//! itself. The rule "strict, non-null arguments, non-null result" is
//! therefore applied to `pg_catalog` routines only, except those listed
//! here: each one returns NULL for some non-null arguments in PostgreSQL 18
//! (missing JSON key, empty array, no regular expression match, unknown
//! name, no previous row…). A routine of the project or of an extension
//! carries no such guarantee: its result is not established.

use pgzest_catalog::model::{Operator, Routine, RoutineKind, TypeRef};

use crate::scope::Nullability;

/// Result of a routine called with non-null arguments, if it is strict;
/// `Nullable` whether it is strict or not for those that return NULL by
/// themselves.
pub(crate) fn routine_result(r: &Routine) -> Nullability {
    if r.name.schema != "pg_catalog" {
        Nullability::Unknown
    } else if routine_may_return_null(r) {
        Nullability::Nullable
    } else {
        Nullability::NonNull
    }
}

/// Result of an operator applied to non-null operands. The catalog does not
/// hold the function behind an operator, so a project or extension operator
/// (`hstore -> text`) is not established either.
pub(crate) fn operator_result(o: &Operator) -> Nullability {
    if o.name.schema != "pg_catalog" {
        Nullability::Unknown
    } else if operator_may_return_null(o) {
        Nullability::Nullable
    } else {
        Nullability::NonNull
    }
}

fn routine_may_return_null(r: &Routine) -> bool {
    let name = r.name.name.as_str();
    let inputs: Vec<String> = r.input_types().map(TypeRef::to_string).collect();
    let inputs: Vec<&str> = inputs.iter().map(String::as_str).collect();
    if r.kind == RoutineKind::Window {
        // No previous, next or n-th row in the frame.
        return matches!(
            name,
            "lag" | "lead" | "first_value" | "last_value" | "nth_value"
        );
    }
    match name {
        // Missing key, index or path, JSON `null`.
        "json_extract_path"
        | "json_extract_path_text"
        | "jsonb_extract_path"
        | "jsonb_extract_path_text"
        | "json_object_field"
        | "json_object_field_text"
        | "jsonb_object_field"
        | "jsonb_object_field_text"
        | "json_array_element"
        | "json_array_element_text"
        | "jsonb_array_element"
        | "jsonb_array_element_text"
        | "jsonb_path_query_first"
        | "jsonb_path_query_first_tz"
        | "jsonb_path_exists"
        | "jsonb_path_exists_tz"
        | "jsonb_path_match"
        | "jsonb_path_match_tz" => true,
        // Element not found; empty array or missing dimension.
        "array_position" | "array_length" | "array_lower" | "array_upper" | "array_dims"
        | "array_ndims" => true,
        // No match.
        "regexp_match" | "regexp_substr" => true,
        // `substring(text FROM pattern)`, `substring(text SIMILAR pattern
        // ESCAPE e)`: no match. The positional forms take integers.
        "substring" => inputs.iter().skip(1).all(|t| *t == "pg_catalog.text"),
        // Empty or unbounded range.
        "lower" | "upper" => inputs
            .first()
            .is_some_and(|t| matches!(*t, "pg_catalog.anyrange" | "pg_catalog.anymultirange")),
        // `current_setting(name, missing_ok)`: unknown setting.
        "current_setting" => inputs.len() == 2,
        // Unknown object.
        "obj_description"
        | "col_description"
        | "shobj_description"
        | "pg_get_viewdef"
        | "pg_get_functiondef"
        | "pg_get_indexdef"
        | "pg_get_constraintdef"
        | "pg_get_triggerdef"
        | "pg_get_ruledef"
        | "pg_get_expr"
        | "pg_relation_size"
        | "pg_total_relation_size"
        | "pg_table_size"
        | "pg_indexes_size"
        | "pg_relation_filenode"
        | "pg_relation_filepath"
        | "pg_xact_commit_timestamp"
        | "pg_sequence_last_value"
        | "txid_status"
        | "pg_xact_status" => true,
        _ => name.starts_with("to_reg"),
    }
}

fn operator_may_return_null(o: &Operator) -> bool {
    let left = o.left.as_ref().map(TypeRef::to_string);
    let json = matches!(
        left.as_deref(),
        Some("pg_catalog.json" | "pg_catalog.jsonb")
    );
    match o.name.name.as_str() {
        // Missing key, index or path, JSON `null`.
        "->" | "->>" | "#>" | "#>>" => json,
        // `jsonb @? jsonpath`, `jsonb @@ jsonpath`: unknown result (strict
        // path error, non-boolean predicate).
        "@?" | "@@" => json && o.right.to_string() == "pg_catalog.jsonpath",
        _ => false,
    }
}
