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
