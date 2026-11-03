-- Macros. Compile with:  pgweave build queries/06-macros.pgsql
--
-- This file is .pgsql: macros live here, and `pgweave build` turns it into
-- standard .sql, readable by psql and pg_dump.

-- @function: the function from schema/functions/user_count.sql, in 5 lines
-- instead of 11. The DECLARE is written next to where it is used and hoisted
-- at compile time into a section before BEGIN.
@function user_count(target_org UUID): INTEGER {
    DECLARE total INTEGER;
    SELECT count(*) INTO total FROM users WHERE org_id = target_org;
    RETURN total;
}

-- Attributes: everything CREATE FUNCTION accepts can still be expressed.
@function slugify(txt TEXT): TEXT immutable strict {
    RETURN lower(txt);
}

-- language sql, without BEGIN/END.
@function add(a INT, b INT): INT language sql {
    SELECT a + b;
}

-- @trigger: the function AND the CREATE TRIGGER in one go.
@trigger touch_one before update on users for each row {
    NEW.last_seen_at = now();
    RETURN NEW;
}

-- Multi-table: one function per table is generated, so NEW and OLD can be
-- resolved.
--
-- That is the real gain. With a shared function, the checker cannot
-- resolve NEW and settles for a `trigger-ambiguous-table` — a useless false
-- positive (that is what you see on examples/schema.sql). Here it really
-- checks the column on EACH table.
--
-- Try: replace `updated_at` with `last_seen_at` below. The column exists on
-- `users` but not on `organizations`, and the checker will say so
-- precisely — where the old form could not assert anything.
@trigger touch_all before update on (users, organizations) for each row {
    NEW.updated_at = now();
    RETURN NEW;
}

-- @audited comes from macros/audited.pgmacro — a project macro, not a bundled one.
@audited audit_demo {
    id   UUID PRIMARY KEY,
    note TEXT
}
