-- Adversarial data seeding for the nullability soundness oracle
-- (`pg_sanity/soundness.rs`). Created once per query session as temporary
-- functions: they live in that session's `pg_temp` schema only, so the
-- migrations session (and every catalog comparison it drives) never sees
-- them. Everything they insert runs inside a transaction the oracle rolls
-- back.
--
-- `typedpg_sample(typ, variant)` returns an SQL expression of type `typ`
-- (or NULL when no candidate value is accepted by the type's input
-- function and domain constraints). Variant 1 is the adversarial value —
-- a composite with every field NULL, an array holding a NULL element, an
-- empty range, an infinite date, timestamp or interval — and variant 2 a
-- "full" one with every part non-NULL.
--
-- `typedpg_seed(mode)` inserts one row into every user table it can:
-- mode 1 leaves every nullable column NULL, mode 2 fills every column.
-- Mode 3 adds rows mixing the two to the tables with CHECK constraints —
-- one nullable column NULL and the rest filled, or the other way round,
-- under both variants — so constraints tying columns together
-- (`kind <> 'a' OR a_id IS NOT NULL`) get rows on each of their sides.
-- Modes 4 and 5 add rows holding boundary values, one column at a time
-- (see `typedpg_seed_values`): mode 4 the literals of CHECK constraints
-- and their neighbours, enum labels and the query's literals `lits`,
-- mode 5 each type's special values (NaN, infinities, integer extremes,
-- the empty string).
-- Foreign keys take the referenced table's first key, so tables are
-- retried in passes until no pass makes progress (a parent seeded in pass
-- 1 unblocks its children in pass 2). It returns the INSERTs that
-- succeeded (`insert`) and the tables it could not seed (`skip`, with the
-- error of their first attempt).

CREATE FUNCTION pg_temp.typedpg_candidates(typ oid, variant int, depth int)
RETURNS text[]
LANGUAGE plpgsql SET search_path = pg_catalog, pg_temp AS $fn$
DECLARE
    t pg_type;
    out text[] := '{}';
    pairs text[][];
    i int;
    n int;
    elem text;
    fields text[];
    a record;
    f text;
BEGIN
    SELECT * INTO t FROM pg_type WHERE oid = typ;
    IF NOT FOUND OR depth > 3 THEN
        RETURN out;
    END IF;
    IF t.typtype = 'd' THEN
        -- The base type's candidates; the caller's cast to the domain
        -- applies the domain's constraints.
        RETURN pg_temp.typedpg_candidates(t.typbasetype, variant, depth);
    ELSIF t.typtype = 'e' THEN
        SELECT array_agg(quote_literal(l) ORDER BY o) INTO out
        FROM (
            SELECT enumlabel AS l, enumsortorder AS o FROM pg_enum
            WHERE enumtypid = typ ORDER BY enumsortorder
            OFFSET variant - 1 LIMIT 1
        ) s;
        IF out IS NULL THEN
            SELECT array_agg(quote_literal(enumlabel)) INTO out
            FROM (SELECT enumlabel FROM pg_enum WHERE enumtypid = typ
                  ORDER BY enumsortorder LIMIT 1) s;
        END IF;
        RETURN coalesce(out, '{}');
    ELSIF t.typtype = 'c' THEN
        SELECT count(*) INTO n FROM pg_attribute
        WHERE attrelid = t.typrelid AND attnum > 0 AND NOT attisdropped;
        IF variant = 1 THEN
            -- Every field NULL: '(,,)' has one empty (NULL) field per slot.
            RETURN ARRAY[quote_literal('(' || repeat(',', greatest(n - 1, 0)) || ')')];
        END IF;
        fields := '{}';
        FOR a IN SELECT atttypid FROM pg_attribute
                 WHERE attrelid = t.typrelid AND attnum > 0 AND NOT attisdropped
                 ORDER BY attnum
        LOOP
            f := pg_temp.typedpg_sample(a.atttypid, variant, depth + 1);
            fields := fields || coalesce(f, 'NULL');
        END LOOP;
        IF n = 0 THEN
            RETURN ARRAY['''()'''];
        END IF;
        RETURN ARRAY['ROW(' || array_to_string(fields, ', ') || ')'];
    ELSIF t.typtype = 'r' THEN
        RETURN CASE WHEN variant = 1 THEN ARRAY['''empty'''] ELSE ARRAY['''(,)'''] END;
    ELSIF t.typtype = 'm' THEN
        RETURN CASE WHEN variant = 1 THEN ARRAY['''{}'''] ELSE ARRAY['''{"(,)"}'''] END;
    ELSIF t.typcategory = 'A' AND t.typelem <> 0 THEN
        elem := pg_temp.typedpg_sample(t.typelem, variant, depth + 1);
        IF variant = 1 THEN
            out := ARRAY['''{NULL}'''];
        END IF;
        IF elem IS NOT NULL THEN
            out := out || ('ARRAY[' || elem || ']');
        END IF;
        RETURN out || '''{}'''::text;
    END IF;

    -- Base types: per-category literals first, then a generic pool.
    -- Each pair is (variant 1, variant 2).
    pairs := CASE t.typcategory
        WHEN 'B' THEN ARRAY[['false', 'true']]
        WHEN 'S' THEN ARRAY[['a', 'b']]
        WHEN 'N' THEN ARRAY[['1', '2']]
        WHEN 'D' THEN ARRAY[['infinity', '2001-01-01'], ['2000-01-01', '2001-01-01'],
                            ['01:00', '02:00']]
        WHEN 'T' THEN ARRAY[['infinity', '2 days'], ['1 day', '2 days']]
        WHEN 'I' THEN ARRAY[['127.0.0.1', '127.0.0.2']]
        WHEN 'G' THEN ARRAY[['(0,0)', '(1,1)'], ['((0,0),(1,1))', '((0,0),(2,2))'],
                            ['{1,1,1}', '{1,2,1}'], ['<(0,0),1>', '<(0,0),2>']]
        WHEN 'V' THEN ARRAY[['1', '0']]
        -- jsonb (and other 'U' types, which reject it): an object with
        -- the keys a JSON constraint may require; adversarially, an array
        -- holding them as strings (`? 'a'` is TRUE of it too).
        WHEN 'U' THEN ARRAY[['["a", "k", "n"]', '{"a": "a", "k": 1, "n": {"x": 1}}']]
        ELSE ARRAY[['1', '2']]
    END || ARRAY[
        ['1', '2'], ['a', 'b'], ['false', 'true'],
        ['00000000-0000-0000-0000-000000000001', '00000000-0000-0000-0000-000000000002'],
        ['08:00:2b:01:02:03', '08:00:2b:01:02:04'], ['0/1', '0/2'],
        ['(0,1)', '(0,2)'], ['1:1:', '1:2:'],
        ['{}', '{}'], ['()', '()']
    ];
    FOR i IN 1 .. array_length(pairs, 1) LOOP
        out := out || quote_literal(pairs[i][variant]);
    END LOOP;
    RETURN out;
END
$fn$;

CREATE FUNCTION pg_temp.typedpg_sample(typ oid, variant int, depth int DEFAULT 0)
RETURNS text
LANGUAGE plpgsql SET search_path = pg_catalog, pg_temp AS $fn$
DECLARE
    tname text := format_type(typ, NULL);
    c text;
BEGIN
    FOREACH c IN ARRAY pg_temp.typedpg_candidates(typ, variant, depth) LOOP
        BEGIN
            EXECUTE format('SELECT (%s)::%s', c, tname);
            RETURN format('(%s)::%s', c, tname);
        EXCEPTION WHEN others THEN
            NULL;
        END;
    END LOOP;
    RETURN NULL;
END
$fn$;

-- The text form of a variant-1 sample of `typ`, for binding a non-null
-- parameter in text format.
CREATE FUNCTION pg_temp.typedpg_param_text(typ oid, variant int DEFAULT 1)
RETURNS text
LANGUAGE plpgsql SET search_path = pg_catalog, pg_temp AS $fn$
DECLARE
    e text := pg_temp.typedpg_sample(typ, variant);
    v text;
BEGIN
    IF e IS NULL THEN
        RETURN NULL;
    END IF;
    EXECUTE format('SELECT (%s)::text', e) INTO v;
    RETURN v;
END
$fn$;

-- Whether a value of `typ` can never be NULL (a NOT NULL domain, possibly
-- nested).
CREATE FUNCTION pg_temp.typedpg_type_not_null(typ oid)
RETURNS bool
LANGUAGE sql STABLE SET search_path = pg_catalog, pg_temp AS $fn$
    WITH RECURSIVE d(oid, not_null, base) AS (
        SELECT oid, typnotnull, typbasetype FROM pg_type WHERE oid = typ
        UNION ALL
        SELECT t.oid, t.typnotnull, t.typbasetype
        FROM pg_type t JOIN d ON t.oid = d.base
    )
    SELECT coalesce(bool_or(not_null), false) FROM d
$fn$;

-- The INSERT that seeds one row of `rel`, or NULL when a NOT NULL column
-- has no usable value. `null_nullable` leaves nullable columns NULL;
-- `use_defaults` leaves columns with a default to it (NOT NULL ones always,
-- nullable ones only under `null_nullable`).
CREATE FUNCTION pg_temp.typedpg_insert_sql(rel oid, variant int, null_nullable bool, use_defaults bool)
RETURNS text
LANGUAGE plpgsql SET search_path = pg_catalog, pg_temp AS $fn$
DECLARE
    a record;
    fk record;
    cols text[] := '{}';
    vals text[] := '{}';
    must_fill bool;
    v text;
    pos int;
BEGIN
    FOR a IN SELECT attnum, attname, atttypid, attnotnull, atthasdef, attidentity, attgenerated
             FROM pg_attribute
             WHERE attrelid = rel AND attnum > 0 AND NOT attisdropped
             ORDER BY attnum
    LOOP
        CONTINUE WHEN a.attgenerated <> '' OR a.attidentity = 'a';
        must_fill := a.attnotnull OR pg_temp.typedpg_type_not_null(a.atttypid);
        IF use_defaults AND (a.atthasdef OR a.attidentity = 'd')
           AND (must_fill OR null_nullable) THEN
            CONTINUE;
        END IF;
        SELECT c.conkey, c.confkey, c.confrelid INTO fk
        FROM pg_constraint c
        WHERE c.conrelid = rel AND c.contype = 'f' AND a.attnum = ANY (c.conkey)
        ORDER BY c.oid LIMIT 1;
        IF FOUND THEN
            IF NOT must_fill AND (null_nullable OR cardinality(fk.conkey) > 1) THEN
                v := 'NULL';
            ELSE
                pos := array_position(fk.conkey, a.attnum);
                -- A foreign key references the table's own rows (the RI
                -- check reads ONLY it), but a partitioned table's partitions'.
                SELECT format('(SELECT %I FROM %s%s ORDER BY %s LIMIT 1)',
                              (SELECT attname FROM pg_attribute
                               WHERE attrelid = fk.confrelid AND attnum = fk.confkey[pos]),
                              CASE WHEN (SELECT relkind FROM pg_class WHERE oid = fk.confrelid) = 'p'
                                   THEN '' ELSE 'ONLY ' END,
                              fk.confrelid::regclass,
                              string_agg(quote_ident(r.attname), ', ' ORDER BY k.ord))
                INTO v
                FROM unnest(fk.confkey) WITH ORDINALITY AS k(attnum, ord)
                JOIN pg_attribute r ON r.attrelid = fk.confrelid AND r.attnum = k.attnum;
            END IF;
        ELSIF NOT must_fill AND null_nullable THEN
            v := 'NULL';
        ELSE
            v := pg_temp.typedpg_sample(a.atttypid, variant);
            IF v IS NULL THEN
                IF must_fill THEN
                    RETURN NULL;
                END IF;
                v := 'NULL';
            END IF;
        END IF;
        cols := cols || quote_ident(a.attname);
        vals := vals || v;
    END LOOP;
    IF cardinality(cols) = 0 THEN
        RETURN format('INSERT INTO %s DEFAULT VALUES', rel::regclass);
    END IF;
    RETURN format('INSERT INTO %s (%s) VALUES (%s)', rel::regclass,
                  array_to_string(cols, ', '), array_to_string(vals, ', '));
END
$fn$;

-- The INSERT of one row of `rel` with the nullable columns in `nulls`
-- NULL and every other column filled with variant `variant`'s sample
-- (foreign keys taking the referenced table's first key). A column of a
-- unique index of integer or text type takes `salt` instead, so several
-- such rows fit. Columns `over_atts`, when given, take the expressions
-- `over_vals` (in the same order) instead of anything else.
CREATE FUNCTION pg_temp.typedpg_pattern_sql(rel oid, variant int, nulls text[], salt int,
                                            over_atts int[] DEFAULT '{}', over_vals text[] DEFAULT '{}')
RETURNS text
LANGUAGE plpgsql SET search_path = pg_catalog, pg_temp AS $fn$
DECLARE
    a record;
    fk record;
    cols text[] := '{}';
    vals text[] := '{}';
    v text;
    pos int;
    is_unique bool;
BEGIN
    FOR a IN SELECT attnum, attname, atttypid, attnotnull, attidentity, attgenerated
             FROM pg_attribute
             WHERE attrelid = rel AND attnum > 0 AND NOT attisdropped
             ORDER BY attnum
    LOOP
        CONTINUE WHEN a.attgenerated <> '' OR a.attidentity = 'a';
        IF a.attnum = ANY (over_atts) THEN
            v := over_vals[array_position(over_atts, a.attnum::int)];
        ELSIF a.attname = ANY (nulls) AND NOT a.attnotnull
           AND NOT pg_temp.typedpg_type_not_null(a.atttypid) THEN
            v := 'NULL';
        ELSE
            SELECT c.conkey, c.confkey, c.confrelid INTO fk
            FROM pg_constraint c
            WHERE c.conrelid = rel AND c.contype = 'f' AND a.attnum = ANY (c.conkey)
            ORDER BY c.oid LIMIT 1;
            SELECT EXISTS (
                SELECT 1 FROM pg_index i
                WHERE i.indrelid = rel AND i.indisunique AND a.attnum = ANY (i.indkey)
            ) INTO is_unique;
            IF FOUND AND fk.conkey IS NOT NULL THEN
                pos := array_position(fk.conkey, a.attnum);
                -- A foreign key references the table's own rows (the RI
                -- check reads ONLY it), but a partitioned table's partitions'.
                SELECT format('(SELECT %I FROM %s%s ORDER BY %s LIMIT 1)',
                              (SELECT attname FROM pg_attribute
                               WHERE attrelid = fk.confrelid AND attnum = fk.confkey[pos]),
                              CASE WHEN (SELECT relkind FROM pg_class WHERE oid = fk.confrelid) = 'p'
                                   THEN '' ELSE 'ONLY ' END,
                              fk.confrelid::regclass,
                              string_agg(quote_ident(r.attname), ', ' ORDER BY k.ord))
                INTO v
                FROM unnest(fk.confkey) WITH ORDINALITY AS k(attnum, ord)
                JOIN pg_attribute r ON r.attrelid = fk.confrelid AND r.attnum = k.attnum;
            ELSIF is_unique AND a.atttypid IN ('int2'::regtype, 'int4'::regtype, 'int8'::regtype,
                                                'numeric'::regtype) THEN
                v := salt::text;
            ELSIF is_unique AND a.atttypid IN ('text'::regtype, 'varchar'::regtype) THEN
                v := quote_literal('u' || salt);
            ELSE
                v := pg_temp.typedpg_sample(a.atttypid, variant);
                IF v IS NULL THEN
                    RETURN NULL;
                END IF;
            END IF;
        END IF;
        cols := cols || quote_ident(a.attname);
        vals := vals || v;
    END LOOP;
    IF cardinality(cols) = 0 THEN
        RETURN NULL;
    END IF;
    RETURN format('INSERT INTO %s (%s) VALUES (%s)', rel::regclass,
                  array_to_string(cols, ', '), array_to_string(vals, ', '));
END
$fn$;

-- Mode 3 of `typedpg_seed`: up to 8 mixed rows per table with a CHECK
-- constraint (one without is covered by modes 1 and 2's extremes).
CREATE FUNCTION pg_temp.typedpg_seed_mixed()
RETURNS TABLE (kind text, detail text)
LANGUAGE plpgsql SET search_path = pg_catalog, pg_temp AS $fn$
DECLARE
    r oid;
    nullable text[];
    c text;
    variant int;
    nset text[];
    stmt text;
    salt int := 1000;
    added int;
    k int;
BEGIN
    FOR r IN SELECT cl.oid FROM pg_class cl JOIN pg_namespace n ON n.oid = cl.relnamespace
             WHERE cl.relkind IN ('r', 'p') AND NOT cl.relispartition
               AND cl.relpersistence <> 't'
               AND n.nspname NOT IN ('pg_catalog', 'information_schema')
               AND n.nspname NOT LIKE 'pg\_toast%'
               AND EXISTS (SELECT 1 FROM pg_constraint k
                           WHERE k.conrelid = cl.oid AND k.contype = 'c')
             ORDER BY cl.oid
    LOOP
        SELECT coalesce(array_agg(attname ORDER BY attnum), '{}') INTO nullable
        FROM pg_attribute
        WHERE attrelid = r AND attnum > 0 AND NOT attisdropped AND NOT attnotnull
          AND attgenerated = '' AND attidentity = ''
          AND NOT pg_temp.typedpg_type_not_null(atttypid);
        CONTINUE WHEN cardinality(nullable) = 0;
        added := 0;
        <<patterns>>
        FOR variant IN 1 .. 2 LOOP
            FOREACH c IN ARRAY nullable LOOP
                FOR k IN 1 .. 2 LOOP
                    nset := CASE k WHEN 1 THEN array_remove(nullable, c) ELSE ARRAY[c] END;
                    salt := salt + 1;
                    stmt := pg_temp.typedpg_pattern_sql(r, variant, nset, salt);
                    CONTINUE WHEN stmt IS NULL;
                    BEGIN
                        EXECUTE stmt;
                        added := added + 1;
                        kind := 'insert';
                        detail := stmt;
                        RETURN NEXT;
                    EXCEPTION WHEN others THEN
                        NULL;
                    END;
                    EXIT patterns WHEN added >= 8;
                END LOOP;
            END LOOP;
        END LOOP;
    END LOOP;
END
$fn$;

-- The literals in the CHECK constraints on column `att` of `rel` and in
-- those of the domains its type `typ` is (string constants unquoted,
-- numbers as written).
CREATE FUNCTION pg_temp.typedpg_check_literals(rel oid, att int, typ oid)
RETURNS text[]
LANGUAGE sql STABLE SET search_path = pg_catalog, pg_temp AS $fn$
    WITH RECURSIVE d(oid, base) AS (
        SELECT oid, typbasetype FROM pg_type WHERE oid = typ
        UNION ALL
        SELECT t.oid, t.typbasetype FROM pg_type t JOIN d ON t.oid = d.base
    ), defs AS (
        SELECT pg_get_constraintdef(c.oid) AS def FROM pg_constraint c
        WHERE c.contype = 'c'
          AND ((c.conrelid = rel AND att = ANY (c.conkey))
               OR c.contypid IN (SELECT oid FROM d WHERE base <> 0))
    )
    SELECT coalesce(array_agg(DISTINCT lit), '{}') FROM (
        SELECT coalesce(replace(m[1], '''''', ''''), m[2]) AS lit
        FROM defs,
             regexp_matches(def, '''((?:[^'']|'''')*)''|(?<![[:alnum:]_.$])(-?[0-9]+(?:\.[0-9]+)?(?:[eE][-+]?[0-9]+)?)', 'g') AS m
    ) s
    WHERE lit IS NOT NULL
$fn$;

-- Expressions for the value `lit` and its neighbours in the base type
-- `base`: b - 1, b, b + 1 of a number (and b ± 0.001 of a non-integer
-- one); b ± one day and one microsecond of a date / time; b ± one
-- microsecond of an interval. An expression `lit` can't be read as fails
-- at evaluation and is dropped by the caller.
CREATE FUNCTION pg_temp.typedpg_near(lit text, base oid)
RETURNS text[]
LANGUAGE plpgsql STABLE SET search_path = pg_catalog, pg_temp AS $fn$
DECLARE
    t pg_type;
    tname text := format_type(base, NULL);
    out text[] := ARRAY[quote_literal(lit)];
BEGIN
    SELECT * INTO t FROM pg_type WHERE oid = base;
    IF t.typcategory = 'N' AND lit ~ '^-?[0-9]+(\.[0-9]+)?([eE][-+]?[0-9]+)?$' THEN
        out := out || format('(%L::numeric - 1)', lit) || format('(%L::numeric + 1)', lit);
        IF t.typname IN ('numeric', 'float4', 'float8') THEN
            out := out || format('(%L::numeric - 0.001)', lit) || format('(%L::numeric + 0.001)', lit);
        END IF;
    ELSIF t.typcategory = 'D' THEN
        out := out
            || format('(%L::%s - interval ''1 day'')', lit, tname)
            || format('(%L::%s + interval ''1 day'')', lit, tname)
            || format('(%L::%s - interval ''1 microsecond'')', lit, tname)
            || format('(%L::%s + interval ''1 microsecond'')', lit, tname);
    ELSIF t.typcategory = 'T' THEN
        out := out
            || format('(%L::interval - interval ''1 microsecond'')', lit)
            || format('(%L::interval + interval ''1 microsecond'')', lit);
    END IF;
    RETURN out;
END
$fn$;

-- The special values of the base type `base`: an integer's extremes, a
-- number's NaN and infinities, a date's or timestamp's infinities, the
-- empty string.
CREATE FUNCTION pg_temp.typedpg_extremes(base oid)
RETURNS text[]
LANGUAGE sql STABLE SET search_path = pg_catalog, pg_temp AS $fn$
    SELECT coalesce(array_agg(quote_literal(v)), '{}') FROM unnest(
        CASE (SELECT typname FROM pg_type WHERE oid = base AND typnamespace = 'pg_catalog'::regnamespace)
            WHEN 'int2' THEN '{-32768,32767,0,-1}'::text[]
            WHEN 'int4' THEN '{-2147483648,2147483647,0,-1}'
            WHEN 'int8' THEN '{-9223372036854775808,9223372036854775807,0,-1}'
            WHEN 'numeric' THEN '{NaN,Infinity,-Infinity,0,-1,0.5}'
            WHEN 'float4' THEN '{NaN,Infinity,-Infinity,-0,0}'
            WHEN 'float8' THEN '{NaN,Infinity,-Infinity,-0,0}'
            WHEN 'date' THEN '{infinity,-infinity,1970-01-01}'
            WHEN 'timestamp' THEN '{infinity,-infinity,1970-01-01}'
            WHEN 'timestamptz' THEN '{infinity,-infinity,1970-01-01}'
            WHEN 'interval' THEN '{infinity,-infinity,0}'
            WHEN 'text' THEN '{"", " "}'
            WHEN 'varchar' THEN '{"", " "}'
            WHEN 'bpchar' THEN '{"", " "}'
            ELSE '{}'
        END) AS v
$fn$;

-- Modes 4 and 5 of `typedpg_seed`: rows of every table that hold the
-- values a refinement may depend on in one column, the other columns
-- filled as in mode 2. Mode 4 (`bounds`) takes, per column, the literals
-- of its CHECK constraints and its domains' and their neighbours, every
-- label of an enum, and the query's own literals `lits` (with their
-- neighbours); mode 5 (`extremes`) the type's special values. A value
-- the column's type or domain rejects is dropped, a row a constraint
-- rejects skipped. Columns take turns (each one's first value, then each
-- one's second, …), up to `per_table` rows per table; the other columns
-- keep the values of the first row that went in.
CREATE FUNCTION pg_temp.typedpg_seed_values(mode int, lits text[])
RETURNS TABLE (kind text, detail text)
LANGUAGE plpgsql SET search_path = pg_catalog, pg_temp AS $fn$
DECLARE
    per_table int := CASE WHEN mode = 4 THEN 32 ELSE 16 END;
    per_column int := 12;
    r oid;
    a record;
    base oid;
    tname text;
    exprs text[];
    own int;
    e text;
    j int;
    canon text;
    seen text[];
    cand_att int[];
    cand_rank int[];
    cand_val text[];
    i int;
    stmt text;
    salt int := 2000;
    added int;
    l text;
    base_atts int[];
    base_vals text[];
    base_ok bool;
    retry int[];
    queue int[];
    q int;
    checks text[];
    probe_att int[];
    probe_val text[];
    atts int[];
    vals text[];
    k int;
BEGIN
    FOR r IN SELECT cl.oid FROM pg_class cl JOIN pg_namespace n ON n.oid = cl.relnamespace
             WHERE cl.relkind IN ('r', 'p') AND NOT cl.relispartition
               AND cl.relpersistence <> 't'
               AND n.nspname NOT IN ('pg_catalog', 'information_schema')
               AND n.nspname NOT LIKE 'pg\_toast%'
             ORDER BY cl.oid
    LOOP
        cand_att := '{}';
        cand_rank := '{}';
        cand_val := '{}';
        probe_att := '{}';
        probe_val := '{}';
        FOR a IN SELECT attnum, atttypid FROM pg_attribute
                 WHERE attrelid = r AND attnum > 0 AND NOT attisdropped
                   AND attgenerated = '' AND attidentity <> 'a'
                   AND NOT EXISTS (SELECT 1 FROM pg_constraint c
                                   WHERE c.conrelid = r AND c.contype = 'f'
                                     AND attnum = ANY (c.conkey))
                 ORDER BY attnum
        LOOP
            base := NULL;
            WITH RECURSIVE d(oid, b) AS (
                SELECT oid, typbasetype FROM pg_type WHERE oid = a.atttypid
                UNION ALL
                SELECT t.oid, t.typbasetype FROM pg_type t JOIN d ON t.oid = d.b
            )
            SELECT oid INTO base FROM d WHERE b = 0;
            CONTINUE WHEN base IS NULL;
            tname := format_type(a.atttypid, NULL);
            exprs := '{}';
            checks := pg_temp.typedpg_check_literals(r, a.attnum, a.atttypid);
            FOREACH l IN ARRAY checks LOOP
                probe_att := probe_att || a.attnum::int;
                probe_val := probe_val || format('%L::%s', l, tname);
            END LOOP;
            IF mode = 4 THEN
                SELECT coalesce(array_agg(quote_literal(enumlabel) ORDER BY enumsortorder), '{}')
                INTO exprs FROM pg_enum WHERE enumtypid = base;
                FOREACH l IN ARRAY checks LOOP
                    exprs := exprs || pg_temp.typedpg_near(l, base);
                END LOOP;
                -- The column's own values come before the query's.
                own := cardinality(exprs);
                FOREACH l IN ARRAY lits LOOP
                    exprs := exprs || pg_temp.typedpg_near(l, base);
                END LOOP;
            ELSE
                exprs := pg_temp.typedpg_extremes(base);
                own := cardinality(exprs);
            END IF;
            seen := '{}';
            FOR j IN 1 .. coalesce(cardinality(exprs), 0) LOOP
                EXIT WHEN cardinality(seen) >= per_column;
                e := exprs[j];
                BEGIN
                    EXECUTE format('SELECT ((%s)::%s)::text', e, tname) INTO canon;
                EXCEPTION WHEN others THEN
                    CONTINUE;
                END;
                CONTINUE WHEN canon IS NULL OR canon = ANY (seen);
                seen := seen || canon;
                cand_att := cand_att || a.attnum::int;
                cand_rank := cand_rank || (cardinality(seen) + CASE WHEN j > own THEN 1000 ELSE 0 END);
                cand_val := cand_val || format('%L::%s', canon, tname);
            END LOOP;
        END LOOP;
        CONTINUE WHEN cardinality(cand_att) = 0;
        added := 0;
        base_atts := '{}';
        base_vals := '{}';
        base_ok := false;
        retry := '{}';
        SELECT array_agg(s ORDER BY cand_rank[s], cand_att[s]) INTO queue
        FROM generate_subscripts(cand_att, 1) AS s;
        -- The mode 2 values of the other columns may be what a CHECK
        -- rejects (`d >= '2020-01-01'`): find a base the next rows vary one
        -- column of — the mode 2 row itself, or it with one column taking
        -- a literal of its CHECKs — by inserting it and rolling it back.
        FOR j IN 0 .. cardinality(probe_att) LOOP
            IF j = 0 THEN
                atts := '{}';
                vals := '{}';
            ELSE
                atts := ARRAY[probe_att[j]];
                vals := ARRAY[probe_val[j]];
            END IF;
            stmt := pg_temp.typedpg_pattern_sql(r, 2, '{}', salt, atts, vals);
            CONTINUE WHEN stmt IS NULL;
            BEGIN
                EXECUTE stmt;
                RAISE EXCEPTION 'probe' USING ERRCODE = 'TPGOK';
            EXCEPTION
                WHEN SQLSTATE 'TPGOK' THEN
                    base_ok := true;
                    base_atts := atts;
                    base_vals := vals;
                WHEN others THEN
                    NULL;
            END;
            EXIT WHEN base_ok;
        END LOOP;
        -- Otherwise the first row that goes in becomes the base, and the
        -- candidates tried before it are tried again right away.
        q := 0;
        WHILE q < cardinality(queue) AND added < per_table LOOP
            q := q + 1;
            i := queue[q];
            atts := base_atts;
            vals := base_vals;
            k := array_position(atts, cand_att[i]);
            IF k IS NULL THEN
                atts := atts || cand_att[i];
                vals := vals || cand_val[i];
            ELSIF vals[k] = cand_val[i] THEN
                CONTINUE;
            ELSE
                vals[k] := cand_val[i];
            END IF;
            salt := salt + 1;
            stmt := pg_temp.typedpg_pattern_sql(r, 2, '{}', salt, atts, vals);
            CONTINUE WHEN stmt IS NULL;
            BEGIN
                EXECUTE stmt;
                added := added + 1;
                kind := 'insert';
                detail := stmt;
                RETURN NEXT;
                IF NOT base_ok THEN
                    base_ok := true;
                    base_atts := atts;
                    base_vals := vals;
                    queue := queue[1:q] || retry || queue[q + 1:];
                END IF;
            EXCEPTION WHEN others THEN
                IF NOT base_ok THEN
                    retry := retry || i;
                END IF;
            END;
        END LOOP;
    END LOOP;
END
$fn$;

CREATE FUNCTION pg_temp.typedpg_seed(mode int, lits text[] DEFAULT '{}')
RETURNS TABLE (kind text, detail text)
LANGUAGE plpgsql SET search_path = pg_catalog, pg_temp AS $fn$
DECLARE
    pending oid[];
    still oid[];
    r oid;
    progress bool;
    errs jsonb := '{}';
    attempts bool[][];
    i int;
    stmt text;
    done bool;
BEGIN
    IF mode = 3 THEN
        RETURN QUERY SELECT * FROM pg_temp.typedpg_seed_mixed();
        RETURN;
    END IF;
    IF mode IN (4, 5) THEN
        RETURN QUERY SELECT * FROM pg_temp.typedpg_seed_values(mode, lits);
        RETURN;
    END IF;
    SELECT coalesce(array_agg(c.oid ORDER BY c.oid), '{}') INTO pending
    FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
    WHERE c.relkind IN ('r', 'p') AND NOT c.relispartition
      AND c.relpersistence <> 't'
      AND n.nspname NOT IN ('pg_catalog', 'information_schema')
      AND n.nspname NOT LIKE 'pg\_toast%';
    -- (null_nullable, use_defaults), in order of preference.
    attempts := CASE WHEN mode = 1
        THEN ARRAY[[true, true], [false, true], [false, false]]
        ELSE ARRAY[[false, true], [false, false]]
    END;
    LOOP
        progress := false;
        still := '{}';
        FOREACH r IN ARRAY pending LOOP
            done := false;
            FOR i IN 1 .. array_length(attempts, 1) LOOP
                stmt := pg_temp.typedpg_insert_sql(r, mode, attempts[i][1], attempts[i][2]);
                IF stmt IS NULL THEN
                    errs := errs || jsonb_build_object(r::text, coalesce(errs ->> r::text,
                        'a NOT NULL column has a type with no sample value'));
                    CONTINUE;
                END IF;
                BEGIN
                    EXECUTE stmt;
                    done := true;
                EXCEPTION WHEN others THEN
                    errs := errs || jsonb_build_object(r::text, coalesce(errs ->> r::text, SQLERRM));
                END;
                EXIT WHEN done;
            END LOOP;
            IF done THEN
                kind := 'insert';
                detail := stmt;
                RETURN NEXT;
                progress := true;
                errs := errs - r::text;
            ELSE
                still := still || r;
            END IF;
        END LOOP;
        EXIT WHEN NOT progress OR cardinality(still) = 0;
        pending := still;
    END LOOP;
    FOREACH r IN ARRAY still LOOP
        kind := 'skip';
        detail := format('%s: %s', r::regclass, errs ->> r::text);
        RETURN NEXT;
    END LOOP;
END
$fn$;
