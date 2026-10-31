-- Tolérance du parser, réglée par [parse] dans pgweave.toml.

-- SQL valide mais hors périmètre de l'AST → unsupported-statement.
-- Avec `unsupported = "ignore"` (défaut), rien n'est signalé.
GRANT SELECT ON users TO readonly;

-- SQL franchement invalide → unparsable-statement.
-- Avec `unparsable = "warn"` (défaut), un warning, pas une erreur :
-- l'outil reste adoptable sur une base existante.
-- Mets `unparsable = "error"` dans la config pour durcir.
SELCT * FRM users;

-- Le reste du fichier continue d'être analysé normalement — un statement
-- illisible n'interrompt pas l'analyse. Celle-ci doit être soulignée :
SELECT still_checked FROM users;
