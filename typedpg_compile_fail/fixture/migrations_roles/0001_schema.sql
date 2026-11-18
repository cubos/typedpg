-- The schema of the `roles` database: every statement of it is checked
-- against `fixture_app` (the config's `roles`), unless a call site names
-- another role.
CREATE ROLE fixture_app;
CREATE ROLE fixture_admin;
CREATE ROLE fixture_auditor;

CREATE TABLE machines (
    id     INT PRIMARY KEY,
    name   TEXT NOT NULL,
    serial TEXT
);

CREATE TABLE parts (
    id    INT PRIMARY KEY,
    label TEXT NOT NULL
);

CREATE TABLE audits (
    id INT PRIMARY KEY
);

CREATE SCHEMA restricted;
CREATE TABLE restricted.secrets (
    id INT PRIMARY KEY
);

-- `fixture_app` reads every machine, and a part only through its label.
GRANT SELECT ON machines TO fixture_app;
GRANT SELECT (label) ON parts TO fixture_app;

-- `fixture_admin` may also write machines.
GRANT SELECT, INSERT, UPDATE, DELETE ON machines TO fixture_admin;

-- `fixture_auditor` gets nothing, and `restricted` grants no USAGE.
