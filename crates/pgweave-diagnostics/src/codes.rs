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

// ── Requêtes nommées (docs/named-queries.md) ─────────────────────────────────

/// En-tête `-- name:` mal formé (nom, cardinalité, position).
pub const QUERY_HEADER_INVALID: &str = "query-header-invalid";
/// SQL placé hors d'une requête nommée dans un fichier de requêtes.
pub const QUERY_WITHOUT_NAME: &str = "query-without-name";
/// En-tête sans SQL.
pub const QUERY_EMPTY: &str = "query-empty";
/// Plus d'un statement sous un même en-tête.
pub const QUERY_MULTIPLE_STATEMENTS: &str = "query-multiple-statements";
/// Deux requêtes de même nom dans un fichier.
pub const QUERY_DUPLICATE_NAME: &str = "query-duplicate-name";
/// `$nom` et `$1` dans la même requête.
pub const QUERY_MIXED_PARAMETERS: &str = "query-mixed-parameters";
/// Paramètres positionnels avec un trou (`$1`, `$3` sans `$2`).
pub const QUERY_PARAMETER_GAP: &str = "query-parameter-gap";
/// Fichier de `[queries] directory` qui n'est pas un `.sql`.
pub const QUERY_FILE_EXTENSION: &str = "query-file-extension";
/// Paramètre mal formé (`$0`, `$pageId`).
pub const QUERY_PARAMETER_INVALID: &str = "query-parameter-invalid";
