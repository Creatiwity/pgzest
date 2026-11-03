//! Requêtes nommées de `queries/`, un module par fichier, générées à la
//! compilation : `queries/menu.sql` → `menu`, `queries/promotions/deals.sql`
//! → `promotions::deals`. Les enums PostgreSQL sont ici, à la racine.

pgweave_macros::queries!("queries");
