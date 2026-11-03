-- The public menu. Its columns are typed by the analysis, as PostgreSQL
-- would type them.
CREATE VIEW menu AS
SELECT r.slug AS restaurant,
       d.id,
       d.name,
       d.kind,
       d.price,
       d.vegetarian
FROM dishes d
JOIN restaurants r ON r.id = d.restaurant_id;
