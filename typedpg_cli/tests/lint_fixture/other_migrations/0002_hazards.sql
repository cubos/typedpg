-- The second database's migration rules run too: `things` already has rows
-- from 0001, so a plain CREATE INDEX blocks writes on it.
CREATE INDEX things_name_idx ON things (name);
