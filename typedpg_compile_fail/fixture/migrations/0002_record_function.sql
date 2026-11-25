-- A function returning `record` with no OUT parameters: its fields are
-- known only to a caller giving a column definition list.
CREATE FUNCTION untyped_pair() RETURNS record LANGUAGE sql AS 'SELECT 1, ''a''::text';
