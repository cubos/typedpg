-- Each query is unbounded; deleteAllowed carries its allow comment between
-- the @name block and the statement, where the scanner must find it.

/* @name deleteAll */
DELETE FROM users;

/* @name deleteAllowed */
-- typedpg-lint: allow(unbounded_write)
DELETE FROM users;

/* @name renameAll */
UPDATE users SET name = name;
