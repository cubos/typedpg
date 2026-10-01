-- $..spread targets: enum, jsonb-domain and scalar-domain columns, scalar
-- and array, NOT NULL and nullable (tests/spread.rs).
CREATE TABLE spread_targets (
    id INT PRIMARY KEY,
    status post_status NOT NULL,
    maybe_status post_status,
    statuses post_status[],
    pref user_preferences NOT NULL,
    maybe_pref user_preferences,
    prefs user_preferences[],
    num positive_int,
    nums positive_int[]
);
