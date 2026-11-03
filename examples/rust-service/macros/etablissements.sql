-- Fragments for the SIRENE load (siren pattern): written once, inserted
-- into the schema and into the queries.

-- fragment: etablissement_columns(e)
-- Columns loaded from the file, in COPY order.
e.siret, e.nom, e.effectif, e.ouvert

-- fragment: etablissement_search(e)
-- Full-text index expression. A query that writes it differently silently
-- misses the index: it is written only here.
to_tsvector('french'::regconfig, coalesce(e.nom, ''))
