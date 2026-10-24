-- The types `types` maps to are checked where a .sql file's query uses
-- them: those that don't fit fail in src/queries.ts (../expected.tsc).

/* @name mapped */
SELECT 1::user_id AS id, ROW('a', 1)::short_address AS short, ROW('b', 2)::address AS addr,
       now() AS at, $prefs::prefs AS prefs;
