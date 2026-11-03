-- Fragments de la démo : des morceaux de SQL écrits une fois, insérés par
-- `@nom` dans les requêtes. Survolez un `@dish_card` dans queries/menu.sql.

-- fragment: dish_card(d)
-- La fiche d'un plat, telle que l'affichent toutes les listes. Avec un
-- alias : `@dish_card(x)` → `x.id, x.name, …` ; sans : colonnes nues.
d.id, d.name, d.kind, d.price, d.vegetarian

-- fragment: affordable(d)
-- Plat à moins de 10 € (prix en centimes).
d.price < 1000
