-- Imported as a .sql file (main.ts): a loader or bundler plugin compiles
-- it; tsc reads its types from users.d.sql.ts (`emit: "dts"`), or from the
-- content mapper (tsconfig.mapper.json).

/* @name insertUser */
INSERT INTO users (name, prefs) VALUES ($name, $prefs) RETURNING id;

/* @name userById */
SELECT id, name, email, prefs FROM users WHERE id = $id;

/* @name countUsers */
SELECT count(*) AS "n!" FROM users;
