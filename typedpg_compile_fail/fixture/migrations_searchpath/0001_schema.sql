-- The `searchpath` database: two roles whose `search_path` settings make
-- `items` resolve to tables with different `x` column types.
CREATE ROLE sp_alt;
CREATE ROLE sp_public;

CREATE SCHEMA alternate;
CREATE TABLE public.items (id INT PRIMARY KEY, x INT NOT NULL);
CREATE TABLE alternate.items (id INT PRIMARY KEY, x TEXT NOT NULL);

GRANT USAGE ON SCHEMA alternate TO sp_alt;
GRANT SELECT ON public.items TO sp_alt, sp_public;
GRANT SELECT ON alternate.items TO sp_alt;

ALTER ROLE sp_alt SET search_path = alternate, public;
ALTER ROLE sp_public SET search_path = public;
