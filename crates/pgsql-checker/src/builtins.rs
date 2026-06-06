/// Returns true if `name` (case-insensitive) is a function or operator
/// built-in to PostgreSQL or SQL standard that does not need to be declared
/// in the user schema.
pub fn is_builtin_function(name: &str) -> bool {
    let lower = name.to_lowercase();
    BUILTIN_FUNCTIONS.contains(&lower.as_str())
}

/// Set of built-in function/aggregate/operator names for PostgreSQL.
/// Non-exhaustive but covers the most common cases.
static BUILTIN_FUNCTIONS: &[&str] = &[
    // Aggregates
    "count", "sum", "avg", "min", "max",
    "array_agg", "string_agg", "json_agg", "jsonb_agg",
    "bool_and", "bool_or", "every",
    "bit_and", "bit_or",
    "corr", "covar_pop", "covar_samp", "stddev", "stddev_pop", "stddev_samp",
    "variance", "var_pop", "var_samp",
    "percentile_cont", "percentile_disc",
    "mode", "rank", "dense_rank", "percent_rank", "cume_dist", "ntile",
    "row_number", "lag", "lead", "first_value", "last_value", "nth_value",
    // Date/time functions
    "now", "current_timestamp", "current_date", "current_time",
    "clock_timestamp", "statement_timestamp", "transaction_timestamp",
    "age", "date_part", "date_trunc", "extract", "make_date", "make_time",
    "make_timestamp", "make_timestamptz", "make_interval",
    "to_timestamp", "to_date", "timezone",
    "date_bin", "justify_days", "justify_hours", "justify_interval",
    "localtime", "localtimestamp",
    // String functions
    "length", "char_length", "bit_length", "octet_length",
    "lower", "upper", "initcap",
    "ltrim", "rtrim", "btrim", "trim",
    "lpad", "rpad",
    "substr", "substring", "left", "right",
    "position", "strpos", "locate",
    "replace", "overlay", "translate",
    "split_part", "regexp_split_to_array", "regexp_split_to_table",
    "regexp_match", "regexp_matches", "regexp_replace",
    "concat", "concat_ws", "format",
    "quote_ident", "quote_literal", "quote_nullable",
    "encode", "decode",
    "to_char", "to_number",
    "chr", "ascii", "repeat", "reverse",
    "starts_with", "ends_with",
    "pg_catalog.lower", "pg_catalog.upper",
    // Math functions
    "abs", "ceil", "ceiling", "floor", "round", "trunc", "sign",
    "sqrt", "cbrt", "exp", "ln", "log", "log10",
    "mod", "div",
    "power", "pow",
    "pi", "degrees", "radians",
    "sin", "cos", "tan", "asin", "acos", "atan", "atan2",
    "sinh", "cosh", "tanh", "asinh", "acosh", "atanh",
    "greatest", "least",
    "random", "setseed",
    "width_bucket",
    // Type / conversion functions
    "cast", "nullif", "coalesce", "nvl",
    "oid", "regclass", "regtype", "regproc",
    "pg_typeof", "format_type",
    "array", "array_length", "array_dims", "array_lower", "array_upper",
    "array_append", "array_prepend", "array_cat", "array_remove", "array_replace",
    "array_position", "array_positions", "unnest", "array_to_string", "string_to_array",
    "cardinality",
    // JSON functions
    "json_build_object", "json_build_array", "json_object", "json_array",
    "jsonb_build_object", "jsonb_build_array",
    "json_extract_path", "json_extract_path_text",
    "jsonb_extract_path", "jsonb_extract_path_text",
    "json_array_length", "jsonb_array_length",
    "json_each", "json_each_text", "jsonb_each", "jsonb_each_text",
    "json_object_keys", "jsonb_object_keys",
    "json_populate_record", "jsonb_populate_record",
    "json_to_record", "jsonb_to_record",
    "json_typeof", "jsonb_typeof",
    "json_strip_nulls", "jsonb_strip_nulls",
    "jsonb_set", "jsonb_insert", "jsonb_delete", "jsonb_path_query",
    "row_to_json", "to_json", "to_jsonb",
    // UUID
    "uuid_generate_v4", "uuid_generate_v1", "uuid_generate_v1mc",
    "uuid_ns_dns", "uuid_ns_url", "uuid_ns_oid", "uuid_ns_x500",
    "gen_random_uuid",
    // Control / conditional
    "case",
    // System functions
    "current_user", "session_user", "current_schema", "current_database",
    "current_catalog", "current_schemas",
    "pg_backend_pid", "pg_postmaster_start_time", "pg_conf_load_time",
    "version", "pg_version", "pg_version_num",
    "inet_client_addr", "inet_client_port", "inet_server_addr", "inet_server_port",
    "pg_is_in_recovery", "pg_is_in_backup",
    "pg_relation_size", "pg_total_relation_size", "pg_table_size", "pg_indexes_size",
    "pg_database_size", "pg_tablespace_size",
    "pg_column_size", "pg_size_pretty",
    "pg_sleep", "pg_sleep_for", "pg_sleep_until",
    "pg_notify", "pg_listening_channels",
    "pg_cancel_backend", "pg_terminate_backend",
    "txid_current", "txid_current_snapshot",
    "nextval", "currval", "setval", "lastval",
    // Network functions
    "host", "hostmask", "netmask", "network", "broadcast", "abbrev",
    "inet", "cidr",
    "set_masklen", "masklen", "family",
    // Full-text search functions
    "to_tsvector", "to_tsquery", "plainto_tsquery", "phraseto_tsquery",
    "ts_rank", "ts_rank_cd", "ts_headline",
    "tsvector_to_array", "array_to_tsvector",
    // Misc / crypto
    "md5", "sha256", "sha512", "hmac", "digest",
    "pg_read_file", "pg_read_binary_file",
    "gen_random_bytes",
    // Type name keywords sometimes parsed as function calls
    "bool", "int", "int2", "int4", "int8", "float4", "float8",
    "numeric", "text", "varchar", "char", "bpchar",
    "date", "time", "timestamp", "timestamptz", "interval",
    "boolean", "bigint", "smallint", "integer", "real",
    "row", "record",
    // EXISTS is sometimes parsed as a function call
    "exists",
    "not_exists",
];
