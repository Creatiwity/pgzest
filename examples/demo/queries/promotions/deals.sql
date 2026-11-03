-- Promotions : un sous-répertoire est un module (`queries::promotions::deals`).
-- La fiche reprise vient d'un autre fichier : `-> menu::dish_card`, chemin
-- depuis la racine de `queries/`.

-- name: affordable_dishes :many -> menu::dish_card
-- `@affordable(d)` est un prédicat : inséré entre parenthèses.
SELECT @dish_card(d)
FROM dishes d
WHERE d.restaurant_id = $restaurant AND @affordable(d)
ORDER BY d.price, d.name;
