-- Queries in a .sql file: `typedpg gen` writes users.ts beside it, which
-- sqlfile.test.ts imports.

/* @name insertUser */
INSERT INTO users (name, email, mood, prefs)
VALUES ($name, $email, $mood, $prefs)
RETURNING id;

/* @name userById */
SELECT id, name, email, mood, prefs FROM users WHERE id = $id;

/* @name usersByIds */
SELECT name FROM users WHERE id IN $..ids ORDER BY id;

/* @name insertPosts */
INSERT INTO posts (author_id, title) VALUES $..posts { author, title } RETURNING title;

/* @name countUsers */
SELECT count(*) AS "n!" FROM users;

/* @name renameUser */
UPDATE users SET name = $name WHERE id = $id;
