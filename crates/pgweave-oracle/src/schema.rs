//! Migration oracle (PLAN.md P7): a migration sequence applied to an empty
//! database must produce exactly the declarative schema applied to another
//! one. Both databases are compared through what PostgreSQL says about
//! their objects (`format_type`, `pg_get_*def`), independently of pgweave's
//! model.
//!
//! Not compared: column position (`ADD COLUMN` appends at the end of the
//! table, the diff reports it without being able to reproduce it) and the
//! name of NOT NULL constraints (not modeled by pgweave).

use std::collections::BTreeSet;

use tokio_postgres::{Client, Config};

use crate::pg::{connect_config, connect_url, db_error_message};

/// Objects of a database, excluding the system catalog and extension
/// members: one normalized line per compared property.
const INTROSPECTION: &[&str] = &[
    // Schemas.
    "SELECT 'schema ' || nspname FROM pg_namespace
     WHERE nspname NOT LIKE 'pg\\_%' AND nspname <> 'information_schema'",
    // Extensions.
    "SELECT 'extension ' || e.extname || ' ' || e.extversion || ' ' || n.nspname
     FROM pg_extension e JOIN pg_namespace n ON n.oid = e.extnamespace
     WHERE e.extname <> 'plpgsql'",
    // Types: enums, domains, composites.
    "SELECT 'enum ' || t.oid::regtype || ' ' || string_agg(e.enumlabel, ',' ORDER BY e.enumsortorder)
     FROM pg_type t JOIN pg_enum e ON e.enumtypid = t.oid
     WHERE NOT EXISTS (SELECT 1 FROM pg_depend d WHERE d.classid = 'pg_type'::regclass AND d.objid = t.oid AND d.deptype = 'e')
     GROUP BY t.oid",
    "SELECT 'domain ' || t.oid::regtype || ' ' || format_type(t.typbasetype, t.typtypmod)
            || ' notnull=' || t.typnotnull || ' default=' || coalesce(t.typdefault, '')
            || ' checks=' || coalesce((SELECT string_agg(c.conname || ':' || pg_get_constraintdef(c.oid), ';' ORDER BY c.conname)
                                       FROM pg_constraint c WHERE c.contypid = t.oid), '')
     FROM pg_type t JOIN pg_namespace n ON n.oid = t.typnamespace
     WHERE t.typtype = 'd' AND n.nspname NOT LIKE 'pg\\_%' AND n.nspname <> 'information_schema'
       AND NOT EXISTS (SELECT 1 FROM pg_depend d WHERE d.classid = 'pg_type'::regclass AND d.objid = t.oid AND d.deptype = 'e')",
    "SELECT 'composite ' || t.oid::regtype || ' ' || string_agg(a.attname || ' ' || format_type(a.atttypid, a.atttypmod), ',' ORDER BY a.attnum)
     FROM pg_type t JOIN pg_class c ON c.oid = t.typrelid JOIN pg_attribute a ON a.attrelid = c.oid AND a.attnum > 0 AND NOT a.attisdropped
     JOIN pg_namespace n ON n.oid = t.typnamespace
     WHERE t.typtype = 'c' AND c.relkind = 'c' AND n.nspname NOT LIKE 'pg\\_%' AND n.nspname <> 'information_schema'
       AND NOT EXISTS (SELECT 1 FROM pg_depend d WHERE d.classid = 'pg_type'::regclass AND d.objid = t.oid AND d.deptype = 'e')
     GROUP BY t.oid",
    // Relations, columns, view definitions.
    "SELECT 'relation ' || c.oid::regclass || ' kind=' || c.relkind::text || ' persistence=' || c.relpersistence::text
            || ' rls=' || c.relrowsecurity || ' force_rls=' || c.relforcerowsecurity
     FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
     WHERE c.relkind IN ('r', 'p', 'v', 'm', 'f') AND n.nspname NOT LIKE 'pg\\_%' AND n.nspname <> 'information_schema'
       AND NOT EXISTS (SELECT 1 FROM pg_depend d WHERE d.classid = 'pg_class'::regclass AND d.objid = c.oid AND d.deptype = 'e')",
    "SELECT 'column ' || c.oid::regclass || '.' || quote_ident(a.attname) || ' ' || format_type(a.atttypid, a.atttypmod)
            || ' notnull=' || a.attnotnull || ' default=' || coalesce(pg_get_expr(d.adbin, d.adrelid), '')
            || ' identity=' || a.attidentity::text || ' generated=' || a.attgenerated::text
     FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
     JOIN pg_attribute a ON a.attrelid = c.oid AND a.attnum > 0 AND NOT a.attisdropped
     LEFT JOIN pg_attrdef d ON d.adrelid = c.oid AND d.adnum = a.attnum
     WHERE c.relkind IN ('r', 'p', 'v', 'm', 'f') AND n.nspname NOT LIKE 'pg\\_%' AND n.nspname <> 'information_schema'
       AND NOT EXISTS (SELECT 1 FROM pg_depend x WHERE x.classid = 'pg_class'::regclass AND x.objid = c.oid AND x.deptype = 'e')",
    "SELECT 'view ' || c.oid::regclass || ' ' || pg_get_viewdef(c.oid)
     FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
     WHERE c.relkind IN ('v', 'm') AND n.nspname NOT LIKE 'pg\\_%' AND n.nspname <> 'information_schema'
       AND NOT EXISTS (SELECT 1 FROM pg_depend d WHERE d.classid = 'pg_class'::regclass AND d.objid = c.oid AND d.deptype = 'e')",
    // Constraints. A NOT NULL constraint (named since PostgreSQL 18) is
    // compared without its name: pgweave does not model that name, which a
    // column rename leaves unchanged (documented limit, PLAN.md P7).
    "SELECT 'constraint ' || c.conrelid::regclass || ' '
            || CASE WHEN c.contype = 'n' THEN '' ELSE quote_ident(c.conname) || ' ' END
            || pg_get_constraintdef(c.oid) || ' valid=' || c.convalidated
     FROM pg_constraint c JOIN pg_namespace n ON n.oid = c.connamespace
     WHERE c.conrelid <> 0 AND n.nspname NOT LIKE 'pg\\_%' AND n.nspname <> 'information_schema'
       AND NOT EXISTS (SELECT 1 FROM pg_depend d WHERE d.classid = 'pg_class'::regclass AND d.objid = c.conrelid AND d.deptype = 'e')",
    // Indexes outside constraints.
    "SELECT 'index ' || pg_get_indexdef(i.indexrelid)
     FROM pg_index i JOIN pg_class c ON c.oid = i.indexrelid JOIN pg_namespace n ON n.oid = c.relnamespace
     WHERE n.nspname NOT LIKE 'pg\\_%' AND n.nspname <> 'information_schema'
       AND NOT EXISTS (SELECT 1 FROM pg_constraint k WHERE k.conindid = i.indexrelid AND k.conrelid = i.indrelid)
       AND NOT EXISTS (SELECT 1 FROM pg_depend d WHERE d.classid = 'pg_class'::regclass AND d.objid = i.indrelid AND d.deptype = 'e')",
    // Sequences.
    "SELECT 'sequence ' || quote_ident(schemaname) || '.' || quote_ident(sequencename) || ' ' || data_type
            || ' start=' || start_value || ' min=' || min_value || ' max=' || max_value
            || ' increment=' || increment_by || ' cycle=' || cycle || ' cache=' || cache_size
     FROM pg_sequences s
     WHERE NOT EXISTS (SELECT 1 FROM pg_depend d WHERE d.classid = 'pg_class'::regclass
                         AND d.objid = (quote_ident(s.schemaname) || '.' || quote_ident(s.sequencename))::regclass
                         AND d.deptype = 'e')",
    // Routines (aggregates have no pg_get_functiondef).
    "SELECT 'routine ' || p.oid::regprocedure || ' ' || pg_get_functiondef(p.oid)
     FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace
     WHERE p.prokind IN ('f', 'p', 'w') AND n.nspname NOT LIKE 'pg\\_%' AND n.nspname <> 'information_schema'
       AND NOT EXISTS (SELECT 1 FROM pg_depend d WHERE d.classid = 'pg_proc'::regclass AND d.objid = p.oid AND d.deptype = 'e')",
    // Triggers, policies.
    "SELECT 'trigger ' || pg_get_triggerdef(t.oid)
     FROM pg_trigger t JOIN pg_class c ON c.oid = t.tgrelid JOIN pg_namespace n ON n.oid = c.relnamespace
     WHERE NOT t.tgisinternal AND n.nspname NOT LIKE 'pg\\_%' AND n.nspname <> 'information_schema'",
    "SELECT 'policy ' || quote_ident(schemaname) || '.' || quote_ident(tablename) || ' ' || quote_ident(policyname)
            || ' ' || permissive || ' ' || array_to_string(roles, ',') || ' ' || cmd
            || ' using=' || coalesce(qual, '') || ' check=' || coalesce(with_check, '')
     FROM pg_policies",
    // Comments.
    "SELECT 'comment ' || o.type || ' ' || o.identity || ' ' || d.description
     FROM pg_description d, LATERAL pg_identify_object(d.classoid, d.objoid, d.objsubid) o
     WHERE coalesce(o.schema, '') NOT LIKE 'pg\\_%' AND coalesce(o.schema, '') <> 'information_schema'
       AND o.type NOT IN ('extension')
       AND NOT (o.type = 'schema' AND o.identity = 'public')
       AND NOT EXISTS (SELECT 1 FROM pg_depend x WHERE x.objid = d.objoid AND x.classid = d.classoid AND x.deptype = 'e')",
];

/// Normalized description of a database's objects.
pub async fn introspect(client: &Client) -> Result<BTreeSet<String>, String> {
    let mut out = BTreeSet::new();
    for q in INTROSPECTION {
        for row in client
            .query(*q, &[])
            .await
            .map_err(|e| format!("introspection: {}\n{q}", db_error_message(&e)))?
        {
            let line: Option<String> = row.get(0);
            out.extend(line);
        }
    }
    Ok(out)
}

/// Empty database, dropped after use.
struct Scratch {
    admin: Client,
    name: String,
}

impl Scratch {
    async fn create(admin_url: &str, label: &str) -> Result<Self, String> {
        let admin = connect_url(admin_url).await?;
        let name = format!(
            "pgweave_migrate_{}_{}",
            label.replace(|c: char| !c.is_ascii_alphanumeric(), "_"),
            std::process::id()
        );
        admin
            .batch_execute(&format!("DROP DATABASE IF EXISTS \"{name}\" WITH (FORCE)"))
            .await
            .map_err(|e| db_error_message(&e))?;
        admin
            .batch_execute(&format!("CREATE DATABASE \"{name}\""))
            .await
            .map_err(|e| db_error_message(&e))?;
        Ok(Scratch { admin, name })
    }

    async fn client(&self, admin_url: &str) -> Result<Client, String> {
        let mut config: Config = admin_url.parse().map_err(|e| format!("URL: {e}"))?;
        config.dbname(&self.name);
        connect_config(&config).await
    }

    async fn drop(self) -> Result<(), String> {
        self.admin
            .batch_execute(&format!(
                "DROP DATABASE IF EXISTS \"{}\" WITH (FORCE)",
                self.name
            ))
            .await
            .map_err(|e| db_error_message(&e))
    }
}

async fn applied(
    admin_url: &str,
    label: &str,
    scripts: &[(String, String)],
) -> Result<BTreeSet<String>, String> {
    let db = Scratch::create(admin_url, label).await?;
    let result = async {
        let client = db.client(admin_url).await?;
        for (name, sql) in scripts {
            client
                .batch_execute(sql)
                .await
                .map_err(|e| format!("{name} rejected by PostgreSQL: {}", db_error_message(&e)))?;
        }
        introspect(&client).await
    }
    .await;
    db.drop().await?;
    result
}

/// Gap between the state reached by `migrations` (each `(name, SQL)`,
/// applied in order) and `schema` applied at once: lines specific to each
/// side. Empty if both databases are identical.
pub async fn compare(
    admin_url: &str,
    label: &str,
    migrations: &[(String, String)],
    schema: &str,
) -> Result<SchemaComparison, String> {
    let from_migrations = applied(admin_url, &format!("{label}_m"), migrations).await?;
    let declared = applied(
        admin_url,
        &format!("{label}_s"),
        &[("declarative schema".into(), schema.to_string())],
    )
    .await?;
    Ok(SchemaComparison {
        only_migrations: from_migrations.difference(&declared).cloned().collect(),
        only_schema: declared.difference(&from_migrations).cloned().collect(),
        objects: declared.len(),
    })
}

#[derive(Debug)]
pub struct SchemaComparison {
    pub only_migrations: Vec<String>,
    pub only_schema: Vec<String>,
    /// Compared lines (size of the declarative schema).
    pub objects: usize,
}

impl SchemaComparison {
    pub fn is_identical(&self) -> bool {
        self.only_migrations.is_empty() && self.only_schema.is_empty()
    }
}
