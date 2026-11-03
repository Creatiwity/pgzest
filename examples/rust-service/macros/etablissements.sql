-- Fragments du chargement SIRENE (motif siren) : écrits une fois, insérés
-- dans le schéma et dans les requêtes.

-- fragment: etablissement_columns(e)
-- Colonnes chargées depuis le fichier, dans l'ordre du COPY.
e.siret, e.nom, e.effectif, e.ouvert

-- fragment: etablissement_search(e)
-- Expression de l'index plein texte. Une requête qui l'écrit autrement ne
-- l'utilise pas, sans erreur : elle n'est écrite qu'ici.
to_tsvector('french'::regconfig, coalesce(e.nom, ''))
