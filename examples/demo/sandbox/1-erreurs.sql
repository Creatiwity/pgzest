-- Zone d'essai : ce fichier n'est ni une migration ni une requête compilée
-- en Rust. L'éditeur le vérifie contre le schéma du projet ; chaque
-- requête ci-dessous contient une erreur volontaire, soulignée en rouge.
-- Corrigez-les une à une pour voir le diagnostic disparaître.

-- name: mauvais_type
-- `name` est un texte : PostgreSQL refuserait `text = integer`.
SELECT id FROM restaurants WHERE name = 42;

-- name: colonne_inconnue
SELECT id, titre FROM dishes;

-- name: table_inconnue
SELECT * FROM plats;

-- name: enum_inconnu
-- 'dinner' n'est pas une valeur de dish_kind… mais c'est une erreur
-- d'exécution (le littéral est converti à l'exécution), pas d'analyse :
-- seul le type est vérifié ici. Remplacez 42 par 'main' :
SELECT name FROM dishes WHERE kind = 42;

-- name: parametre_sans_type
-- Rien ne fixe le type de $x : pas de fonction Rust possible.
SELECT $x;

-- name: cardinalite_contredite :none
-- Déclarée sans ligne renvoyée, mais c'est un SELECT.
SELECT id FROM orders;

-- name: colonne_generee_interdite
-- `id` est GENERATED ALWAYS AS IDENTITY.
INSERT INTO restaurants (id, slug, name) VALUES (1, 'x', 'X');

-- name: default_hors_contexte
SELECT default FROM orders;

-- name: ambigu
-- `id` existe dans les deux tables : laquelle ?
SELECT id, name FROM restaurants JOIN dishes ON dishes.restaurant_id = restaurants.id;
