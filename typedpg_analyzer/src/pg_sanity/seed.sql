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
CREATE FUNCTION pg_temp.typedpg_param_text(typ oid)
RETURNS text
LANGUAGE plpgsql SET search_path = pg_catalog, pg_temp AS $fn$
DECLARE
    e text := pg_temp.typedpg_sample(typ, 1);
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
                SELECT format('(SELECT %I FROM %s ORDER BY %s LIMIT 1)',
                              (SELECT attname FROM pg_attribute
                               WHERE attrelid = fk.confrelid AND attnum = fk.confkey[pos]),
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
-- such rows fit.
CREATE FUNCTION pg_temp.typedpg_pattern_sql(rel oid, variant int, nulls text[], salt int)
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
        IF a.attname = ANY (nulls) AND NOT a.attnotnull
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
                SELECT format('(SELECT %I FROM %s ORDER BY %s LIMIT 1)',
                              (SELECT attname FROM pg_attribute
                               WHERE attrelid = fk.confrelid AND attnum = fk.confkey[pos]),
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

CREATE FUNCTION pg_temp.typedpg_seed(mode int)
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
