-- One hazard for each migration rule: the tables above already exist, so
-- the rows these statements touch are the ones a production database has.
CREATE INDEX users_name_idx ON users (name);

ALTER TABLE posts ADD CONSTRAINT posts_author_fk
    FOREIGN KEY (author_id) REFERENCES users (id);
