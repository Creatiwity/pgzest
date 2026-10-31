-- Les directives, à la ESLint. Compare avec 02-erreurs.sql.

-- Rien n'est signalé sur la ligne suivante :
-- pgweave-disable-next-line
SELECT nope FROM users;

-- Seule column-not-found est tue ; les autres règles restent actives :
-- pgweave-disable-next-line column-not-found -- colonne ajoutée par la migration 42
SELECT also_missing FROM users;

-- Une justification après un second `--` est ignorée par le parseur de
-- directives, donc documente librement.

-- Bloc : tout est tu entre disable et enable.
-- pgweave-disable
SELECT missing_a FROM users;
SELECT missing_b FROM users;
-- pgweave-enable

-- Hors du bloc, le check reprend → celle-ci doit être soulignée :
SELECT missing_c FROM users;
