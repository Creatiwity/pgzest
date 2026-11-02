-- name: load_staging
COPY etablissements_staging (siret, nom, effectif, ouvert) FROM STDIN (FORMAT binary);

-- name: load_staging_csv
COPY etablissements_staging FROM STDIN (FORMAT csv, HEADER true);

-- name: swap_from_staging
INSERT INTO etablissements
SELECT siret, nom, effectif, ouvert FROM etablissements_staging
ON CONFLICT (siret) DO UPDATE
SET nom = excluded.nom, effectif = excluded.effectif, ouvert = excluded.ouvert;

-- name: clear_staging
DELETE FROM etablissements_staging;

-- name: count_etablissements
SELECT count(*) AS n FROM etablissements;
