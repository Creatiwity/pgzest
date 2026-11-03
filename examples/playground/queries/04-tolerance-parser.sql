-- Parser tolerance, set by [parse] in pgweave.toml.

-- Valid SQL but outside the AST's scope → unsupported-statement.
-- With `unsupported = "ignore"` (default), nothing is reported.
GRANT SELECT ON users TO readonly;

-- Plainly invalid SQL → unparsable-statement.
-- With `unparsable = "warn"` (default), a warning, not an error:
-- the tool stays adoptable on an existing codebase.
-- Set `unparsable = "error"` in the config to harden.
SELCT * FRM users;

-- The rest of the file is still analyzed normally — an unreadable
-- statement does not interrupt the analysis. This one should be underlined:
SELECT still_checked FROM users;
