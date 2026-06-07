-- Supabase auth schema stubs for pgsql-tools.
--
-- HOW TO USE:
--   Copy this file into the directory referenced by pgsql-tools.schemaFile
--   (e.g. your "schema/" folder). The checker will then resolve auth.uid(),
--   auth.users, etc. without a live database connection.
--
--   With the CLI:
--     pgsql-lsp --schema path/to/schema/ your_queries.sql
--
-- Add stubs here for any other Supabase internals your code references.

SET search_path = auth, public;

-- ── auth.users ───────────────────────────────────────────────────────────────

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

-- ── Core auth helpers ────────────────────────────────────────────────────────

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
