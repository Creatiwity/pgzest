-- Lint rules.
--
-- WARNING: `pgweave check` does NOT run the linter, only the type-checker.
-- Here you will see nothing in the CLI with `check`. Two ways to see them:
--   - in the CLI: `pgweave lint queries/05-lint.sql`
--   - in VS Code: the LSP runs check AND lint, so everything is underlined.

-- L001: SELECT *
SELECT * FROM users;

-- L002: table without a primary key
CREATE TABLE no_pk (id UUID);

-- L006: LIKE with a leading wildcard (prevents index use)
SELECT email FROM users WHERE email LIKE '%@example.com';
