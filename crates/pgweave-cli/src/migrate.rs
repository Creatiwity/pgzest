use std::collections::HashMap;
use std::path::Path;

use pgweave_ast::ddl::CreateTable;
use pgweave_ast::types::DataType;
use pgweave_ast::{DdlStatement, Statement};
use pgweave_codegen::{
    DiffOp, check_migration_safety, diff_schemas, emit_create_table, emit_object_name,
};

// ── Stable content hash (FNV-1a 64-bit) ──────────────────────────────────────

fn fnv1a(s: &str) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000000001b3);
    }
    format!("{:016x}", h)
}

// ── Canonical representation for hashing ─────────────────────────────────────

fn canonical_type(dt: &DataType) -> String {
    match dt {
        DataType::SmallInt => "smallint".into(),
        DataType::Integer => "integer".into(),
        DataType::BigInt => "bigint".into(),
        DataType::Real => "real".into(),
        DataType::DoublePrecision => "double precision".into(),
        DataType::Text => "text".into(),
        DataType::Varchar(n) => n
            .map(|n| format!("varchar({})", n))
            .unwrap_or_else(|| "varchar".into()),
        DataType::Char(n) => n
            .map(|n| format!("char({})", n))
            .unwrap_or_else(|| "char".into()),
        DataType::Boolean => "boolean".into(),
        DataType::Date => "date".into(),
        DataType::Time(p) => match p {
            Some(n) => format!("time({n})"),
            None => "time".into(),
        },
        DataType::TimeTz(p) => match p {
            Some(n) => format!("timetz({n})"),
            None => "timetz".into(),
        },
        DataType::Timestamp(p) => match p {
            Some(n) => format!("timestamp({n})"),
            None => "timestamp".into(),
        },
        DataType::TimestampTz(p) => match p {
            Some(n) => format!("timestamptz({n})"),
            None => "timestamptz".into(),
        },
        DataType::Interval => "interval".into(),
        DataType::Bytea => "bytea".into(),
        DataType::Json => "json".into(),
        DataType::Jsonb => "jsonb".into(),
        DataType::Uuid => "uuid".into(),
        DataType::Numeric { precision, scale } => match (precision, scale) {
            (Some(p), Some(s)) => format!("numeric({},{})", p, s),
            (Some(p), None) => format!("numeric({})", p),
            _ => "numeric".into(),
        },
        DataType::Array(inner) => format!("{}[]", canonical_type(inner)),
        // Casse déjà fixée au parse (principe 8) : `"MyType"` ≠ `mytype`.
        DataType::Custom(s) => s.clone(),
        DataType::Modified { name, modifiers } => format!("{name}({})", modifiers.join(",")),
    }
}

/// Deterministic string representation of a table used for hashing.
/// Column order is preserved (PostgreSQL column positions matter).
fn canonical_table(ct: &CreateTable) -> String {
    let name = emit_object_name(&ct.name).to_lowercase();
    let cols: Vec<String> = ct
        .columns
        .iter()
        .map(|col| {
            let null = if col.nullable { "null" } else { "not null" };
            format!(
                "{}:{} {}",
                col.name.value.to_lowercase(),
                canonical_type(&col.data_type),
                null
            )
        })
        .collect();
    format!("{}({})", name, cols.join(","))
}

fn hash_table(ct: &CreateTable) -> String {
    fnv1a(&canonical_table(ct))
}

// ── I/O helpers ───────────────────────────────────────────────────────────────

/// Parse all `.sql` files in a directory (lexicographic order) into statements.
/// Returns an empty Vec if the directory does not exist.
fn load_and_parse_dir(dir: &Path) -> Result<Vec<Statement>, String> {
    if !dir.exists() {
        return Ok(vec![]);
    }
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .map_err(|e| format!("cannot read {}: {}", dir.display(), e))?
        .filter_map(|e| e.ok())
        .filter(|e| {
            let p = e.path();
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            // *.external.sql files provide type information for the checker
            // but are not owned by the migration system — skip them.
            p.extension()
                .and_then(|x| x.to_str())
                .map(|ext| ext.eq_ignore_ascii_case("sql"))
                .unwrap_or(false)
                && !name.ends_with(".external.sql")
        })
        .collect();
    entries.sort_by_key(|e| e.file_name());

    let mut all = Vec::new();
    for entry in entries {
        let path = entry.path();
        let sql = std::fs::read_to_string(&path)
            .map_err(|e| format!("cannot read {}: {}", path.display(), e))?;
        let stmts =
            pgweave_parser::parse(&sql).map_err(|e| format!("{}: {}", path.display(), e))?;
        all.extend(stmts);
    }
    Ok(all)
}

/// Load snapshot SQL files from a directory and parse them into a flat statement list.
fn load_snapshot_stmts(snapshots_dir: &Path) -> Result<Vec<Statement>, String> {
    load_and_parse_dir(snapshots_dir)
}

/// Build a table_name → &CreateTable map from a statement list.
fn index_tables(stmts: &[Statement]) -> HashMap<String, &CreateTable> {
    let mut map = HashMap::new();
    for stmt in stmts {
        if let Statement::Ddl(DdlStatement::CreateTable(ct)) = stmt {
            map.insert(emit_object_name(&ct.name).to_lowercase(), ct);
        }
    }
    map
}

/// Snapshot filename for a table name (dots → double underscores to be path-safe).
fn snapshot_filename(table_name: &str) -> String {
    format!("{}.sql", table_name.replace('.', "__"))
}

/// Overwrite `.schema-snapshots/` with the current state.
/// Writes one file per table, removes files for dropped tables.
fn update_snapshots(
    snapshots_dir: &Path,
    current_tables: &HashMap<String, &CreateTable>,
    dropped: &[String],
) -> Result<(), String> {
    std::fs::create_dir_all(snapshots_dir)
        .map_err(|e| format!("cannot create snapshots dir: {}", e))?;

    for (name, ct) in current_tables {
        let path = snapshots_dir.join(snapshot_filename(name));
        let sql = format!("{}\n", emit_create_table(ct));
        std::fs::write(&path, sql)
            .map_err(|e| format!("cannot write snapshot {}: {}", path.display(), e))?;
    }
    for name in dropped {
        let path = snapshots_dir.join(snapshot_filename(name));
        let _ = std::fs::remove_file(&path);
    }
    Ok(())
}

// ── Migration file generation ─────────────────────────────────────────────────

fn format_now() -> String {
    chrono::Utc::now().format("%Y%m%d-%H%M%S").to_string()
}

fn migration_header(
    name: &str,
    timestamp: &str,
    hashes: &[(String, String)], // sorted (table, hash) pairs
) -> String {
    let mut s = format!(
        "-- Migration: {} ({})\n-- Generated by: pgweave migrate create\n",
        name, timestamp
    );
    if !hashes.is_empty() {
        s.push_str("--\n");
        for (table, hash) in hashes {
            s.push_str(&format!("-- pgweave-schema-hash: {}={}\n", table, hash));
        }
    }
    s.push('\n');
    s
}

// ── Public command ────────────────────────────────────────────────────────────

pub fn cmd_migrate_create(
    name: &str,
    schema_dir: &Path,
    migrations_dir: &Path,
    snapshots_dir: &Path,
) -> i32 {
    // 1. Load current schema from schema_dir
    let current_stmts = match load_and_parse_dir(schema_dir) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: {}", e);
            return 1;
        }
    };
    if current_stmts.is_empty() {
        eprintln!("error: no SQL files found in {}", schema_dir.display());
        return 1;
    }

    // 2. Load previous snapshot
    let snapshot_stmts = match load_snapshot_stmts(snapshots_dir) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: {}", e);
            return 1;
        }
    };

    // 3. Compute diff (snapshot → current)
    let diff = diff_schemas(&snapshot_stmts, &current_stmts);
    if diff.operations.is_empty() {
        println!("no schema changes detected — migration not created");
        return 0;
    }

    // 4. Safety analysis
    let safety = check_migration_safety(&diff);

    // 5. Compute table hashes for tables involved in the diff
    let current_tables = index_tables(&current_stmts);
    let mut dropped: Vec<String> = Vec::new();
    let mut affected_tables: Vec<String> = Vec::new();

    for op in &diff.operations {
        match op {
            DiffOp::CreateTable(ct) => {
                let n = emit_object_name(&ct.name).to_lowercase();
                if !affected_tables.contains(&n) {
                    affected_tables.push(n);
                }
            }
            DiffOp::DropTable { name: n } => {
                let n = n.to_lowercase();
                dropped.push(n.clone());
                if !affected_tables.contains(&n) {
                    affected_tables.push(n);
                }
            }
            DiffOp::AddColumn { table: t, .. }
            | DiffOp::DropColumn { table: t, .. }
            | DiffOp::AlterColumnType { table: t, .. } => {
                let n = t.to_lowercase();
                if !affected_tables.contains(&n) {
                    affected_tables.push(n);
                }
            }
            _ => {}
        }
    }

    let mut hashes: Vec<(String, String)> = affected_tables
        .iter()
        .filter_map(|t| current_tables.get(t).map(|ct| (t.clone(), hash_table(ct))))
        .collect();
    hashes.sort_by(|a, b| a.0.cmp(&b.0));

    // 6. Build migration file content
    let timestamp = format_now();
    let mut sql = migration_header(name, &timestamp, &hashes);

    if !safety.is_empty() {
        sql.push_str("-- !! Safety warnings — review carefully before running:\n");
        for d in &safety {
            sql.push_str(&format!("--    [{}] {}\n", d.code, d.message));
            if let Some(h) = &d.hint {
                sql.push_str(&format!("--    hint: {}\n", h));
            }
        }
        sql.push('\n');
    }

    // Dangerous ops (DROP TABLE, DROP COLUMN) are commented out by default
    for op in &diff.operations {
        match op {
            DiffOp::DropTable { name: n } => {
                sql.push_str(&format!(
                    "-- DROP TABLE {}; -- DANGEROUS: uncomment if intentional\n",
                    n
                ));
            }
            DiffOp::DropColumn { table, column } => {
                sql.push_str(&format!(
                    "-- ALTER TABLE {} DROP COLUMN {}; -- DANGEROUS: uncomment if intentional\n",
                    table, column
                ));
            }
            other => {
                sql.push_str(&format!(
                    "{}\n",
                    pgweave_codegen::MigrationDiff {
                        operations: vec![other.clone()],
                    }
                    .to_sql()
                ));
            }
        }
    }

    // 7. Write migration file
    let filename = format!("{}_{}.sql", timestamp, name);
    let migration_path = migrations_dir.join(&filename);

    if let Err(e) = std::fs::create_dir_all(migrations_dir) {
        eprintln!("error: cannot create migrations dir: {}", e);
        return 1;
    }
    if let Err(e) = std::fs::write(&migration_path, &sql) {
        eprintln!("error: cannot write {}: {}", migration_path.display(), e);
        return 1;
    }

    // 8. Update snapshots
    if let Err(e) = update_snapshots(snapshots_dir, &current_tables, &dropped) {
        eprintln!("warning: snapshots not updated: {}", e);
    }

    println!("created: {}", migration_path.display());
    if !safety.is_empty() {
        eprintln!(
            "warning: {} safety issue(s) — see header comments in the migration file",
            safety.len()
        );
    }

    0
}
