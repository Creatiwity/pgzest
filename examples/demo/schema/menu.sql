-- Vue déclarative (rejouée après les migrations) : ses colonnes sont typées
-- par l'analyse, comme le ferait PostgreSQL.
CREATE VIEW menu AS
SELECT r.slug AS restaurant,
       d.id,
       d.name,
       d.kind,
       d.price,
       d.vegetarian
FROM dishes d
JOIN restaurants r ON r.id = d.restaurant_id;
