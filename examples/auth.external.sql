-- Supabase + PostGIS external stubs for pgweave.
--
-- Files named *.external.sql are loaded by the checker and LSP for type
-- resolution but are intentionally excluded from migration diff generation
-- (they describe external schemas not owned by this codebase).
--
-- HOW TO USE:
--   Copy this file into the directory referenced by pgweave.schemaFile
--   (e.g. your "schema/" folder). The checker will then resolve auth.uid(),
--   auth.users, PostGIS types, etc. without a live database connection.
--
--   With the CLI:
--     pgweave-lsp --schema path/to/schema/ your_queries.sql

-- ── PostGIS ───────────────────────────────────────────────────────────────────

CREATE EXTENSION IF NOT EXISTS postgis WITH SCHEMA extensions;

-- Minimal type stubs so the checker recognises geometry/geography columns.
-- The real types are registered by the extension at runtime.
CREATE TABLE extensions.spatial_ref_sys (
    srid      integer NOT NULL PRIMARY KEY,
    auth_name varchar(256),
    auth_srid integer,
    srtext    varchar(2048),
    proj4text varchar(2048)
);

-- ── Supabase auth schema ──────────────────────────────────────────────────────

SET search_path = auth, public;

CREATE TABLE auth.users (
    id                   uuid        NOT NULL PRIMARY KEY,
    email                text,
    phone                text,
    role                 text,
    raw_user_meta_data   jsonb,
    raw_app_meta_data    jsonb,
    created_at           timestamptz,
    updated_at           timestamptz,
    last_sign_in_at      timestamptz,
    is_anonymous         boolean     NOT NULL DEFAULT false
);

-- Returns the UUID of the currently authenticated user (NULL if anonymous).
CREATE FUNCTION auth.uid() RETURNS uuid
    LANGUAGE sql STABLE
AS $$ SELECT NULL::uuid $$;

-- Returns the role claim from the current JWT ('authenticated', 'anon', etc.).
CREATE FUNCTION auth.role() RETURNS text
    LANGUAGE sql STABLE
AS $$ SELECT NULL::text $$;

-- Returns the email claim from the current JWT.
CREATE FUNCTION auth.email() RETURNS text
    LANGUAGE sql STABLE
AS $$ SELECT NULL::text $$;

-- Returns the JWT claims as a JSON object.
CREATE FUNCTION auth.jwt() RETURNS jsonb
    LANGUAGE sql STABLE
AS $$ SELECT NULL::jsonb $$;
