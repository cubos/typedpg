-- no-transaction
CREATE INDEX CONCURRENTLY things_label ON things (label);
CREATE INDEX CONCURRENTLY things_big ON things (big);
