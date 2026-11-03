-- Demo fragments: pieces of SQL written once, inserted with `@name` in
-- queries. Hover a `@dish_card` in queries/menu.sql.

-- fragment: dish_card(d)
-- A dish's card, as every list displays it. With an alias:
-- `@dish_card(x)` → `x.id, x.name, …`; without: bare columns.
d.id, d.name, d.kind, d.price, d.vegetarian

-- fragment: affordable(d)
-- Dish under €10 (price in cents).
d.price < 1000
