-- Regles de lint.
--
-- ATTENTION : `pgweave check` ne lance PAS le linter, seulement le type-checker.
-- Ici tu ne verras rien en CLI avec `check`. Deux façons de les voir :
--   - en CLI  : `pgweave lint queries/05-lint.sql`
--   - dans VS Code : le LSP lance check ET lint, donc tout est souligné.

-- L001 : SELECT *
SELECT * FROM users;

-- L002 : table sans clé primaire
CREATE TABLE no_pk (id UUID);

-- L006 : LIKE avec joker en tete (empeche l'usage d'un index)
SELECT email FROM users WHERE email LIKE '%@example.com';
