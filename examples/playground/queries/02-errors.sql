-- This file should show underlines. Hover them.

-- 1. Nonexistent column -> error. The hint lists the available columns.
SELECT nope FROM users;

-- 2. Nonexistent table -> error.
SELECT * FROM nonexistent_table;
