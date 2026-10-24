-- Each mistake is reported by `typedpg gen` (pinned by ../expected.stderr);
-- broken_uses.ts pins how tsc reports a use of a query with an error.
SELECT 0;

/* @name typo */
SELECT id, nmae FROM users;

/* @name */
SELECT 1;

/* @name pgtypedParams @param ids -> (...) */
SELECT id FROM users WHERE id = ANY($ids);

/* @name typo */
SELECT 2;

/* @name twoStatements */
SELECT 3; SELECT 4;

/* @name default */
SELECT 5;

/* @name positional */
SELECT id FROM users WHERE id = $1;

/* @name empty */
