fn main() {
    let schema_sql = std::fs::read_to_string("examples/schema.sql").expect("read schema");
    // Parse what we can (CREATE EXTENSION will fail, that's OK)
    let stmts = pgweave_parser::parse(&schema_sql).unwrap_or_default();
    let schema = pgweave_catalog::ProjectCatalog::from_statements(&stmts);
    let result = pgweave_semantic::analyze(&stmts, &schema);

    println!("=== pgweave-semantic demo ===");
    println!(
        "Tables in symbol table: {:?}",
        result.symbols.table_names().collect::<Vec<_>>()
    );
    println!(
        "Functions: {:?}",
        ["set_updated_at", "current_user_org_ids", "project_summary"]
            .iter()
            .map(|f| (f, result.symbols.get_function(f).map(|s| &s.data_type)))
            .collect::<Vec<_>>()
    );
    println!("SELECTs analyzed: {}", result.select_columns.len());
    for (node_id, cols) in &result.select_columns {
        println!("  SELECT #{}: {} columns", node_id.0, cols.len());
        for col in cols {
            println!("    {:?}: {:?}", col.name, col.data_type);
        }
    }
}
