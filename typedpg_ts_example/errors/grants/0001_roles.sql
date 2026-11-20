-- The role the errors fixture's queries are checked against (`roles` in
-- typedpg.config.json). An analysis-only migration: `typedpg gen` reads it
-- to build the schema, the migration runner never applies it.
CREATE ROLE error_app;
GRANT pg_read_all_data TO error_app;
