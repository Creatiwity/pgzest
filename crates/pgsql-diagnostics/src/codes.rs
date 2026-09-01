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

/// Statement que le parser n'a pas su lire : soit du PostgreSQL valide hors
/// périmètre, soit une faute de syntaxe. Niveau piloté par `[parse] unparsable`.
pub const UNPARSABLE_STATEMENT: &str = "unparsable-statement";
/// Statement valide mais volontairement pas modélisé dans l'AST (GRANT, …).
/// Niveau piloté par `[parse] unsupported`.
pub const UNSUPPORTED_STATEMENT: &str = "unsupported-statement";

/// Corps de fonction ou de procédure PL/pgSQL non parsé : il retombe sur un
/// repli textuel presque aveugle. Niveau piloté par `[parse] unparsable`.
pub const UNPARSED_FUNCTION_BODY: &str = "unparsed-function-body";
/// Expression que l'AST ne modélise pas. L'analyse la traverse sans conclure.
pub const UNSUPPORTED_EXPRESSION: &str = "unsupported-expression";
