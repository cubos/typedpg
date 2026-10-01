-- Arrays of domains: PostgreSQL describes them by their own array type,
-- whose element is the domain (a domain column is described by its base).
CREATE DOMAIN positive_int AS INT CHECK (VALUE > 0);

CREATE TABLE domain_arrays (
    id INT PRIMARY KEY,
    nums positive_int[] NOT NULL,
    prefs user_preferences[]
);
