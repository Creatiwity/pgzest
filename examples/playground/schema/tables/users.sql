CREATE TABLE users (
    id           UUID PRIMARY KEY,
    org_id       UUID NOT NULL REFERENCES organizations(id),
    email        TEXT NOT NULL,
    is_admin     BOOLEAN NOT NULL DEFAULT false,
    last_seen_at TIMESTAMPTZ,
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX users_org_id_idx ON users(org_id);
