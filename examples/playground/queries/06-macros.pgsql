-- Les macros. Compile avec :  pgsql build queries/06-macros.pgsql
--
-- Ce fichier est en .pgsql : les macros y vivent, et `pgsql build` en produit
-- du .sql standard, lisible par psql et pg_dump.

-- @function : la fonction de schema/functions/user_count.sql, en 5 lignes
-- au lieu de 11. Le DECLARE est écrit près de son usage et remonté à la
-- compilation dans une section avant BEGIN.
@function user_count(target_org UUID): INTEGER {
    DECLARE total INTEGER;
    SELECT count(*) INTO total FROM users WHERE org_id = target_org;
    RETURN total;
}

-- Attributs : tout ce que CREATE FUNCTION accepte reste exprimable.
@function slugify(txt TEXT): TEXT immutable strict {
    RETURN lower(txt);
}

-- language sql, sans BEGIN/END.
@function add(a INT, b INT): INT language sql {
    SELECT a + b;
}

-- @trigger : la fonction ET le CREATE TRIGGER d'un coup.
@trigger touch_one before update on users for each row {
    NEW.last_seen_at = now();
    RETURN NEW;
}

-- Multi-tables : une fonction par table est générée, donc NEW et OLD sont
-- résolubles.
--
-- C'est le vrai gain. Avec une fonction partagée, le checker ne sait pas
-- résoudre NEW et se contente d'un `trigger-ambiguous-table` — un faux positif
-- inutile (c'est ce qu'on observe sur examples/schema.sql). Ici il vérifie
-- vraiment la colonne sur CHAQUE table.
--
-- Essaie : remplace `updated_at` par `last_seen_at` ci-dessous. La colonne
-- existe sur `users` mais pas sur `organizations`, et le checker le dira
-- précisément — là où l'ancienne forme ne pouvait rien affirmer.
@trigger touch_all before update on (users, organizations) for each row {
    NEW.updated_at = now();
    RETURN NEW;
}

-- @audited vient de macros/audited.pgmacro — une macro du projet, pas fournie.
@audited audit_demo {
    id   UUID PRIMARY KEY,
    note TEXT
}
