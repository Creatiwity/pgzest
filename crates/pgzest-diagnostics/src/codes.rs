//! Codes of the built-in diagnostics.
//!
//! Every code a diagnostic of pgzest carries is a constant of this file
//! (or of a lint rule, a migration risk, a macro), with its documentation:
//! `cargo xtask docs` generates the diagnostics reference of the site from
//! them, and fails on a code written as a literal elsewhere. Plugin rules
//! are prefixed with the plugin name and a `/` (`"my-plugin/my-rule"`).

// ── Analysis ─────────────────────────────────────────────────────────────────

/// SQL that does not parse: the statement is skipped, with the parser's
/// message.
pub const PARSE_ERROR: &str = "parse-error";
/// Column reference that resolves to no column in scope (PostgreSQL
/// `42703`).
pub const COLUMN_NOT_FOUND: &str = "column-not-found";
/// Column reference that several relations in scope provide (`42702`):
/// qualify it.
pub const AMBIGUOUS_COLUMN: &str = "ambiguous-column";
/// Relation that does not exist at this point (`42P01`).
pub const TABLE_NOT_FOUND: &str = "table-not-found";
/// Function or operator with no overload matching the argument types
/// (`42883`).
pub const FUNCTION_NOT_FOUND: &str = "function-not-found";
/// Call that several overloads match equally (`42725`): cast an argument.
pub const AMBIGUOUS_FUNCTION: &str = "ambiguous-function";
/// Types PostgreSQL would refuse: a value assigned to a column of another
/// type, `CASE`/`UNION`/`ARRAY` branches without a common type, an
/// impossible cast, a non-boolean condition, a procedure called as a
/// function… (a comparison without a matching operator is a
/// `function-not-found`, as PostgreSQL reports it).
pub const TYPE_MISMATCH: &str = "type-mismatch";
/// `$n` beyond the parameters of the routine (`42P02`).
pub const PARAMETER_NOT_FOUND: &str = "parameter-not-found";
/// `DEFAULT` outside an `INSERT … VALUES` or an `UPDATE … SET`.
pub const DEFAULT_NOT_ALLOWED: &str = "default-not-allowed";
/// Value written to a generated column, or a generated column used in a
/// generation expression.
pub const GENERATED_COLUMN: &str = "generated-column";
/// Subquery returning more columns than its context takes (`42601`).
pub const SUBQUERY_COLUMNS: &str = "subquery-columns";
/// `VALUES` rows of different lengths.
pub const VALUES_COLUMNS: &str = "values-columns";
/// Branches of a `UNION`, `INTERSECT` or `EXCEPT` with different numbers of
/// columns.
pub const SET_OPERATION_COLUMNS: &str = "set-operation-columns";
/// What the analysis does not cover, stated (information): no conclusion
/// is drawn from it, and nothing is guessed.
pub const NOT_ANALYZED: &str = "not-analyzed";

// ── PL/pgSQL ─────────────────────────────────────────────────────────────────

/// Routine whose body is empty (PL/pgSQL refuses it).
pub const EMPTY_FUNCTION_BODY: &str = "empty-function-body";
/// Trigger function no `CREATE TRIGGER` of the analyzed files attaches:
/// `NEW` and `OLD` have no table to take their type from.
pub const TRIGGER_NO_CONTEXT: &str = "trigger-no-context";
/// Variable used without being declared.
pub const UNDECLARED_VARIABLE: &str = "undeclared-variable";
/// `RAISE` whose `%` placeholders and arguments do not match in number.
pub const RAISE_ARGUMENTS: &str = "raise-arguments";

// ── Parsing ──────────────────────────────────────────────────────────────────

/// Statement the parser could not read: either valid PostgreSQL outside the
/// supported scope, or a syntax error. Level controlled by `[parse] unparsable`.
pub const UNPARSABLE_STATEMENT: &str = "unparsable-statement";
/// Valid statement deliberately not modeled in the AST (GRANT, …).
/// Level controlled by `[parse] unsupported`.
pub const UNSUPPORTED_STATEMENT: &str = "unsupported-statement";

/// Statement read, but with syntax pgzest does not model yet
/// (`DEFERRABLE` on a foreign key…): the analysis ignores it, and SQL
/// generated from the model (migrations) may lack it. The hint links to a
/// prefilled issue, to close the gap.
pub const SYNTAX_NOT_MODELED: &str = "syntax-not-modeled";

/// PL/pgSQL body the parser could not read: the routine is replayed, its
/// body is not analyzed. Level controlled by `[parse] unparsable`.
pub const UNPARSED_FUNCTION_BODY: &str = "unparsed-function-body";

// ── Named queries ────────────────────────────────────────────────────────────

/// Malformed `-- name:` header (name, cardinality, position).
pub const QUERY_HEADER_INVALID: &str = "query-header-invalid";
/// SQL placed outside a named query in a query file.
pub const QUERY_WITHOUT_NAME: &str = "query-without-name";
/// Header without SQL.
pub const QUERY_EMPTY: &str = "query-empty";
/// More than one statement under the same header.
pub const QUERY_MULTIPLE_STATEMENTS: &str = "query-multiple-statements";
/// Two queries with the same name in a file.
pub const QUERY_DUPLICATE_NAME: &str = "query-duplicate-name";
/// `$name` and `$1` in the same query.
pub const QUERY_MIXED_PARAMETERS: &str = "query-mixed-parameters";
/// Positional parameters with a gap (`$1`, `$3` without `$2`).
pub const QUERY_PARAMETER_GAP: &str = "query-parameter-gap";
/// File in `[queries] directory` that is not a `.sql`.
pub const QUERY_FILE_EXTENSION: &str = "query-file-extension";
/// Malformed parameter (`$0`, `$pageId`).
pub const QUERY_PARAMETER_INVALID: &str = "query-parameter-invalid";
/// Query the describer does not cover (other than SELECT, INSERT,
/// UPDATE, DELETE, CALL).
pub const QUERY_NOT_DESCRIBABLE: &str = "query-not-describable";
/// Parameter whose type no usage determines.
pub const QUERY_PARAMETER_UNTYPED: &str = "query-parameter-untyped";
/// Declared cardinality incompatible with the query's.
pub const QUERY_CARDINALITY_MISMATCH: &str = "query-cardinality-mismatch";

// ── Catalog (DDL replay) ─────────────────────────────────────────────────────

/// Referenced type not found in the catalog (neither built-in, nor from an
/// active extension, nor declared by the project).
pub const TYPE_NOT_FOUND: &str = "type-not-found";
/// Altered or dropped object that does not exist at this point of the replay.
pub const OBJECT_NOT_FOUND: &str = "object-not-found";
/// Object created while it already exists (without `IF NOT EXISTS` / `OR REPLACE`).
pub const OBJECT_ALREADY_EXISTS: &str = "object-already-exists";
/// Nonexistent schema.
pub const SCHEMA_NOT_FOUND: &str = "schema-not-found";
/// Extension whose catalog pack neither pgzest nor the project
/// (`[postgres] packs`) provides for this version.
pub const EXTENSION_UNAVAILABLE: &str = "extension-unavailable";
/// Project extension pack (`[postgres] packs`) that cannot be used: unreadable,
/// generated for another PostgreSQL version, or duplicated.
pub const INVALID_PACK: &str = "invalid-pack";
/// Routine name without a signature that designates several overloads.
pub const AMBIGUOUS_ROUTINE: &str = "ambiguous-routine";
/// Drop refused by PostgreSQL for lack of `CASCADE` (dependent objects).
pub const DEPENDENT_OBJECTS: &str = "dependent-objects";
/// DDL in a `DO` block that the replay could not apply: condition not
/// evaluable on the catalog, dynamic SQL, loop, unanalyzed body. The
/// catalog may then differ from the database's.
pub const DDL_NOT_REPLAYED: &str = "ddl-not-replayed";
/// Migration header `-- pgzest: postgres=… extensions=…` malformed,
/// duplicated or misplaced.
pub const MIGRATION_HEADER_INVALID: &str = "migration-header-invalid";
/// Migration header inconsistent with the replayed state: an extension
/// announced but absent or active but not announced, routines generated at
/// another `log=` level.
pub const MIGRATION_HEADER_MISMATCH: &str = "migration-header-mismatch";
/// Migrations without a `-- pgzest:` header: analyzed with the configured
/// version (reported once).
pub const MIGRATION_WITHOUT_HEADER: &str = "migration-without-header";
/// Declarative schema the migrations do not reach: a migration remains to
/// be generated (`pgzest migrate create`), or a change cannot
/// be expressed automatically.
pub const SCHEMA_DRIFT: &str = "schema-drift";
/// Query declared to return the rows of another one (`-> other`) whose
/// columns do not match (name, type, nullability).
pub const QUERY_ROW_MISMATCH: &str = "query-row-mismatch";

/// View columns whose types the query analysis did not establish.
pub const VIEW_NOT_TYPED: &str = "view-not-typed";
