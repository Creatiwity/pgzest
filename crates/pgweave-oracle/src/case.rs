use std::path::Path;

use pgweave_query::{NamedQuery, parse_query_file};

/// A corpus case: a schema and the queries to describe against it.
#[derive(Debug, Clone)]
pub struct Case {
    pub name: String,
    pub schema: String,
    pub queries: Vec<NamedQuery>,
}

/// Loads every case of `dir`, sorted by name.
///
/// A malformed case (missing file, invalid query format) is an error: a
/// half-loaded corpus would give a misleading report.
pub fn load_cases(dir: &Path) -> Result<Vec<Case>, String> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .map_err(|e| format!("{}: {e}", dir.display()))?
        .filter_map(Result::ok)
        .filter(|e| e.path().is_dir())
        .collect();
    entries.sort_by_key(|e| e.file_name());

    let mut cases = Vec::new();
    for entry in entries {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        let read = |file: &str| {
            std::fs::read_to_string(path.join(file))
                .map_err(|e| format!("{}/{file}: {e}", path.display()))
        };
        let schema = read("schema.sql")?;
        let queries_src = read("queries.sql")?;
        let parsed = parse_query_file(&queries_src);
        if !parsed.diagnostics.is_empty() {
            let msgs: Vec<String> = parsed
                .diagnostics
                .iter()
                .map(|d| format!("{}: {}", d.code, d.message))
                .collect();
            return Err(format!(
                "{}/queries.sql is invalid:\n  {}",
                path.display(),
                msgs.join("\n  ")
            ));
        }
        cases.push(Case {
            name,
            schema,
            queries: parsed.queries,
        });
    }
    Ok(cases)
}
