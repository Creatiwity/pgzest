-- pgweave — Schéma d'exemple
--
-- Cas couverts pour le toolchain :
--   - DDL multi-table avec FK, CHECK, index
--   - Trigger functions PL/pgSQL (corps $$...$$)
--   - Transitions d'état avec mutation de NEW
--   - RLS avec sous-requêtes cross-table (USING / WITH CHECK)
--   - Helper SECURITY DEFINER référencé dans les policies
--   - Procédure stockée avec DECLARE, validation métier, RAISE
--   - Fonction retournant une TABLE (typage du résultat)
--   - auth.uid() : fonction externe non définie dans ce DDL (cas SchemaProvider inconnu)

-- ---------------------------------------------------------------------------
-- Extensions
-- ---------------------------------------------------------------------------

CREATE EXTENSION IF NOT EXISTS "uuid-ossp";

-- ---------------------------------------------------------------------------
-- Tables
-- ---------------------------------------------------------------------------

CREATE TABLE organizations (
  id          UUID        PRIMARY KEY DEFAULT uuid_generate_v4(),
  name        TEXT        NOT NULL,
  slug        TEXT        NOT NULL UNIQUE,
  created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
  updated_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE users (
  id           UUID        PRIMARY KEY DEFAULT uuid_generate_v4(),
  email        TEXT        NOT NULL UNIQUE,
  display_name TEXT        NOT NULL,
  created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
  updated_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE memberships (
  user_id   UUID  NOT NULL REFERENCES users(id)         ON DELETE CASCADE,
  org_id    UUID  NOT NULL REFERENCES organizations(id)  ON DELETE CASCADE,
  role      TEXT  NOT NULL CHECK (role IN ('owner', 'admin', 'member')),
  joined_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  PRIMARY KEY (user_id, org_id)
);

CREATE TABLE projects (
  id          UUID        PRIMARY KEY DEFAULT uuid_generate_v4(),
  org_id      UUID        NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
  name        TEXT        NOT NULL,
  description TEXT,
  archived_at TIMESTAMPTZ,
  created_by  UUID        NOT NULL REFERENCES users(id),
  created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
  updated_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE tasks (
  id           UUID     PRIMARY KEY DEFAULT uuid_generate_v4(),
  project_id   UUID     NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  title        TEXT     NOT NULL,
  body         TEXT,
  status       TEXT     NOT NULL DEFAULT 'open'
                        CHECK (status IN ('open', 'in_progress', 'done', 'cancelled')),
  priority     INTEGER  NOT NULL DEFAULT 0 CHECK (priority BETWEEN 0 AND 3),
  assigned_to  UUID     REFERENCES users(id) ON DELETE SET NULL,
  completed_at TIMESTAMPTZ,
  created_by   UUID     NOT NULL REFERENCES users(id),
  created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
  updated_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- ---------------------------------------------------------------------------
-- Index
-- ---------------------------------------------------------------------------

CREATE INDEX tasks_project_id_idx   ON tasks(project_id);
CREATE INDEX tasks_assigned_to_idx  ON tasks(assigned_to);
CREATE INDEX projects_org_id_idx    ON projects(org_id);
CREATE INDEX memberships_org_id_idx ON memberships(org_id);

-- ---------------------------------------------------------------------------
-- Trigger : updated_at automatique
-- ---------------------------------------------------------------------------

CREATE OR REPLACE FUNCTION set_updated_at()
RETURNS TRIGGER
LANGUAGE plpgsql AS $$
BEGIN
  NEW.updated_at = now();
  RETURN NEW;
END;
$$;

CREATE TRIGGER organizations_updated_at
  BEFORE UPDATE ON organizations
  FOR EACH ROW EXECUTE FUNCTION set_updated_at();

CREATE TRIGGER users_updated_at
  BEFORE UPDATE ON users
  FOR EACH ROW EXECUTE FUNCTION set_updated_at();

CREATE TRIGGER projects_updated_at
  BEFORE UPDATE ON projects
  FOR EACH ROW EXECUTE FUNCTION set_updated_at();

CREATE TRIGGER tasks_updated_at
  BEFORE UPDATE ON tasks
  FOR EACH ROW EXECUTE FUNCTION set_updated_at();

-- ---------------------------------------------------------------------------
-- Trigger : transitions de statut des tasks
--
-- Règles métier :
--   - Une task annulée ne peut pas être rouverte.
--   - Passer à 'done' fixe completed_at ; en sortir le remet à NULL.
-- ---------------------------------------------------------------------------

CREATE OR REPLACE FUNCTION enforce_task_status_transition()
RETURNS TRIGGER
LANGUAGE plpgsql AS $$
BEGIN
  IF OLD.status = 'cancelled' AND NEW.status != 'cancelled' THEN
    RAISE EXCEPTION 'Cannot reopen a cancelled task (id=%)', OLD.id;
  END IF;

  IF NEW.status = 'done' AND OLD.status != 'done' THEN
    NEW.completed_at = now();
  END IF;

  IF OLD.status = 'done' AND NEW.status != 'done' THEN
    NEW.completed_at = NULL;
  END IF;

  RETURN NEW;
END;
$$;

CREATE TRIGGER tasks_status_transition
  BEFORE UPDATE OF status ON tasks
  FOR EACH ROW EXECUTE FUNCTION enforce_task_status_transition();

-- ---------------------------------------------------------------------------
-- RLS
-- ---------------------------------------------------------------------------

ALTER TABLE organizations ENABLE ROW LEVEL SECURITY;
ALTER TABLE memberships   ENABLE ROW LEVEL SECURITY;
ALTER TABLE projects      ENABLE ROW LEVEL SECURITY;
ALTER TABLE tasks         ENABLE ROW LEVEL SECURITY;

-- Helper : orgs accessibles à l'utilisateur courant.
-- SECURITY DEFINER pour bypasser le RLS sur memberships lors de l'évaluation
-- des autres policies (évite la récursion).
CREATE OR REPLACE FUNCTION current_user_org_ids()
RETURNS SETOF UUID
LANGUAGE sql STABLE SECURITY DEFINER AS $$
  SELECT org_id FROM memberships WHERE user_id = auth.uid()
$$;

-- organizations : lecture réservée aux membres
CREATE POLICY orgs_select ON organizations
  FOR SELECT
  USING (id IN (SELECT current_user_org_ids()));

-- memberships : un membre voit les autres membres de ses orgs
CREATE POLICY memberships_select ON memberships
  FOR SELECT
  USING (org_id IN (SELECT current_user_org_ids()));

-- projects : lecture pour tous les membres ; écriture pour owner/admin
CREATE POLICY projects_select ON projects
  FOR SELECT
  USING (org_id IN (SELECT current_user_org_ids()));

CREATE POLICY projects_insert ON projects
  FOR INSERT
  WITH CHECK (
    org_id IN (
      SELECT org_id FROM memberships
      WHERE user_id = auth.uid()
        AND role IN ('owner', 'admin')
    )
  );

CREATE POLICY projects_update ON projects
  FOR UPDATE
  USING (
    org_id IN (
      SELECT org_id FROM memberships
      WHERE user_id = auth.uid()
        AND role IN ('owner', 'admin')
    )
  );

-- tasks : lecture pour tous les membres de l'org
--         modification pour l'assigné ou les owner/admin
CREATE POLICY tasks_select ON tasks
  FOR SELECT
  USING (
    project_id IN (
      SELECT id FROM projects
      WHERE org_id IN (SELECT current_user_org_ids())
    )
  );

CREATE POLICY tasks_insert ON tasks
  FOR INSERT
  WITH CHECK (
    project_id IN (
      SELECT id FROM projects
      WHERE org_id IN (SELECT current_user_org_ids())
    )
  );

CREATE POLICY tasks_update ON tasks
  FOR UPDATE
  USING (
    assigned_to = auth.uid()
    OR project_id IN (
      SELECT p.id
      FROM projects p
      JOIN memberships m ON m.org_id = p.org_id
      WHERE m.user_id = auth.uid()
        AND m.role IN ('owner', 'admin')
    )
  );

-- ---------------------------------------------------------------------------
-- Procédure : assigner une task
--
-- Vérifie que la task existe, que l'utilisateur cible est membre de l'org,
-- et que la task est dans un état 'open' avant de la passer en 'in_progress'.
-- ---------------------------------------------------------------------------

CREATE OR REPLACE PROCEDURE assign_task(
  p_task_id UUID,
  p_user_id UUID
)
LANGUAGE plpgsql AS $$
DECLARE
  v_org_id UUID;
BEGIN
  SELECT p.org_id
  INTO   v_org_id
  FROM   tasks t
  JOIN   projects p ON p.id = t.project_id
  WHERE  t.id = p_task_id;

  IF NOT FOUND THEN
    RAISE EXCEPTION 'Task not found: %', p_task_id;
  END IF;

  IF NOT EXISTS (
    SELECT 1 FROM memberships
    WHERE user_id = p_user_id
      AND org_id  = v_org_id
  ) THEN
    RAISE EXCEPTION 'User % is not a member of the organization', p_user_id;
  END IF;

  UPDATE tasks
  SET    assigned_to = p_user_id,
         status      = 'in_progress'
  WHERE  id     = p_task_id
    AND  status = 'open';

  IF NOT FOUND THEN
    RAISE EXCEPTION 'Task % is not in open status', p_task_id;
  END IF;
END;
$$;

-- ---------------------------------------------------------------------------
-- Fonction : résumé d'un projet
--
-- Retourne une ligne avec les compteurs par statut et le taux d'avancement.
-- Cas intéressant pour la génération de typages : TABLE avec colonnes nommées.
-- ---------------------------------------------------------------------------

CREATE OR REPLACE FUNCTION project_summary(p_project_id UUID)
RETURNS TABLE (
  total_tasks       BIGINT,
  open_tasks        BIGINT,
  in_progress_tasks BIGINT,
  done_tasks        BIGINT,
  completion_rate   NUMERIC
)
LANGUAGE sql STABLE AS $$
  SELECT
    COUNT(*)                                             AS total_tasks,
    COUNT(*) FILTER (WHERE status = 'open')             AS open_tasks,
    COUNT(*) FILTER (WHERE status = 'in_progress')      AS in_progress_tasks,
    COUNT(*) FILTER (WHERE status = 'done')             AS done_tasks,
    ROUND(
      COUNT(*) FILTER (WHERE status = 'done')::NUMERIC
      / NULLIF(COUNT(*), 0) * 100,
      1
    )                                                   AS completion_rate
  FROM tasks
  WHERE project_id = p_project_id;
$$;
