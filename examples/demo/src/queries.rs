//! Named queries from `queries/`, one module per file, generated at compile
//! time: `queries/menu.sql` → `menu`, `queries/promotions/deals.sql`
//! → `promotions::deals`. PostgreSQL enums live here, at the root.

pgzest_macros::queries!("queries");
