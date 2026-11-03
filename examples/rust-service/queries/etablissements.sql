-- name: load_staging
COPY etablissements_staging (@etablissement_columns) FROM STDIN (FORMAT binary);

-- name: load_staging_csv
COPY etablissements_staging FROM STDIN (FORMAT csv, HEADER true);

-- name: swap_from_staging
INSERT INTO etablissements (@etablissement_columns)
SELECT @etablissement_columns FROM etablissements_staging
ON CONFLICT (siret) DO UPDATE
SET (@etablissement_columns) = ROW(@etablissement_columns(excluded));

-- name: clear_staging
DELETE FROM etablissements_staging;

-- name: count_etablissements
SELECT count(*) AS n FROM etablissements;

-- name: search_etablissements :many
-- Recherche plein texte, servie par l'index `etablissements_search`.
SELECT e.siret, e.nom
FROM etablissements e
WHERE @etablissement_search(e) @@ plainto_tsquery('french', $q)
ORDER BY e.siret;
