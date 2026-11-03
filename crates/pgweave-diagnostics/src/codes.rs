/// Rule ID constants for all built-in diagnostic rules.
///
/// Use these constants instead of raw string literals when emitting or
/// filtering diagnostics. Plugin-specific rules should be prefixed with the
/// plugin name and a `/`, e.g. `"my-plugin/my-rule"`.
pub const PARSE_ERROR: &str = "parse-error";
pub const COLUMN_NOT_FOUND: &str = "column-not-found";
pub const TABLE_NOT_IN_SCOPE: &str = "table-not-in-scope";
pub const TABLE_NOT_FOUND: &str = "table-not-found";
pub const ASSIGN_TO_OLD: &str = "assign-to-old";
pub const TRIGGER_AMBIGUOUS_TABLE: &str = "trigger-ambiguous-table";
pub const FUNCTION_NOT_FOUND: &str = "function-not-found";
pub const EMPTY_FUNCTION_BODY: &str = "empty-function-body";
pub const TRIGGER_NO_CONTEXT: &str = "trigger-no-context";
pub const UNDECLARED_VARIABLE: &str = "undeclared-variable";

/// Statement the parser could not read: either valid PostgreSQL outside the
/// supported scope, or a syntax error. Level controlled by `[parse] unparsable`.
pub const UNPARSABLE_STATEMENT: &str = "unparsable-statement";
/// Valid statement deliberately not modeled in the AST (GRANT, …).
/// Level controlled by `[parse] unsupported`.
pub const UNSUPPORTED_STATEMENT: &str = "unsupported-statement";

/// Unparsed PL/pgSQL function or procedure body: it falls back to an almost
/// blind textual fallback. Level controlled by `[parse] unparsable`.
pub const UNPARSED_FUNCTION_BODY: &str = "unparsed-function-body";
/// Expression the AST does not model. Analysis walks through it without concluding.
pub const UNSUPPORTED_EXPRESSION: &str = "unsupported-expression";

// ── Named queries (docs/named-queries.md) ────────────────────────────────────

/// Migration header `-- pgweave: postgres=… extensions=…` malformed,
/// duplicated or misplaced (PLAN.md, D9).
pub const MIGRATION_HEADER_INVALID: &str = "migration-header-invalid";

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
/// Extension with no pack available for this version (PLAN.md, D9).
pub const EXTENSION_UNAVAILABLE: &str = "extension-unavailable";
/// Routine name without a signature that designates several overloads.
pub const AMBIGUOUS_ROUTINE: &str = "ambiguous-routine";
/// Drop refused by PostgreSQL for lack of `CASCADE` (dependent objects).
pub const DEPENDENT_OBJECTS: &str = "dependent-objects";
/// DDL in a `DO` block that the replay could not apply: condition not
/// evaluable on the catalog, dynamic SQL, loop, unanalyzed body. The
/// catalog may then differ from the database's.
pub const DDL_NOT_REPLAYED: &str = "ddl-not-replayed";
/// Migration header inconsistent with the replayed state (extension announced
/// but absent, or active but not announced) — PLAN.md, D9.
pub const MIGRATION_HEADER_MISMATCH: &str = "migration-header-mismatch";
/// Migrations without a `-- pgweave:` header: analyzed with the configured
/// version (reported once).
pub const MIGRATION_WITHOUT_HEADER: &str = "migration-without-header";
/// Declarative schema the migrations do not reach (D10): a migration
/// remains to be generated (`pgweave migrate create`), or a change cannot
/// be expressed automatically.
pub const SCHEMA_DRIFT: &str = "schema-drift";
/// Query declared to return the rows of another one (`-> other`) whose
/// columns do not match (name, type, nullability) — P6.
pub const QUERY_ROW_MISMATCH: &str = "query-row-mismatch";

/// View columns whose types the query analysis did not establish.
pub const VIEW_NOT_TYPED: &str = "view-not-typed";
