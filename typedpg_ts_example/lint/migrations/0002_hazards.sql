-- no-transaction
-- One hazard per migration rule. The tables above already existed, so the
-- rows the statements touch are the ones a production database has.
CREATE INDEX posts_author_id_idx ON posts (author_id);

-- The schema rules: the email index is a prefix of the two-column one, and
-- the foreign key's referencing columns have no index.
CREATE INDEX CONCURRENTLY users_email_idx ON users (email);
CREATE INDEX CONCURRENTLY users_email_name_idx ON users (email, name);
ALTER TABLE comments ADD CONSTRAINT comments_post_fk
    FOREIGN KEY (post_id) REFERENCES posts (id);

ALTER TABLE users ALTER COLUMN email SET NOT NULL;
ALTER TABLE users ALTER COLUMN id TYPE bigint;

-- Allowed by its comment; it reports nothing.
-- typedpg-lint: allow(create_index_blocking)
CREATE INDEX users_name_idx ON users (name);
