//! Requêtes écrites dans le code Rust (`sql!`) : la chaîne ne contient que
//! la requête, le nom et la cardinalité sont des paramètres de la macro.
//! Modifier la chaîne relance l'analyse, dans l'éditeur comme à la
//! compilation.

pgweave_macros::sql! {
    /// Les plats d'un restaurant, du moins cher au plus cher. Même ligne que
    /// la fiche de plat de `queries/menu.sql` : la struct est reprise, et la
    /// compatibilité des colonnes vérifiée à la compilation.
    pub dishes_by_price: many -> crate::queries::menu::DishCardRow,
    r#"SELECT @dish_card(d)
       FROM dishes d
       WHERE d.restaurant_id = $restaurant
       ORDER BY d.price, d.name"#
}

pgweave_macros::sql! {
    /// Restaurants ouverts depuis une date (cardinalité inférée : `many`).
    pub opened_since,
    r#"SELECT slug, name, opened_on
       FROM restaurants
       WHERE opened_on >= $since
       ORDER BY opened_on"#
}
