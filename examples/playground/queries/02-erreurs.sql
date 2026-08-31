-- Ce fichier doit montrer des soulignements. Survole-les.

-- 1. Colonne inexistante -> error. Le hint liste les colonnes disponibles.
SELECT nope FROM users;

-- 2. Table inexistante -> error.
SELECT * FROM nonexistent_table;
