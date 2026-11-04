//! Queries written in the Rust code (`sql!`): the string holds only the
//! query; the name and cardinality are parameters of the macro. Editing
//! the string reruns the analysis, in the editor as well as at compile
//! time.

pgzest_macros::sql! {
    /// A restaurant's dishes, from cheapest to most expensive. Same row as
    /// the dish card of `queries/menu.sql`: the struct is reused, and the
    /// column compatibility checked at compile time.
    pub dishes_by_price: many -> crate::queries::menu::DishCardRow,
    r#"SELECT @dish_card(d)
       FROM dishes d
       WHERE d.restaurant_id = $restaurant
       ORDER BY d.price, d.name"#
}

pgzest_macros::sql! {
    /// Restaurants open since a date (inferred cardinality: `many`).
    pub opened_since,
    r#"SELECT slug, name, opened_on
       FROM restaurants
       WHERE opened_on >= $since
       ORDER BY opened_on"#
}
