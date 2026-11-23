//! Random DDL for the catalog oracle: sequences of CREATE / ALTER / DROP
//! over schemas, tables, columns, constraints, indexes, types (enums,
//! composites, ranges), domains, sequences, views, functions, triggers,
//! policies, partitions and inheritance, with renames and SET SCHEMA; and
//! (see [`State::extended`]) PostgreSQL 18/19 table features, the other
//! object kinds a migration manages, roles, grants and default privileges,
//! and transactions with savepoints.
//!
//! The generator tracks the objects it created (only when the statement
//! succeeded) and draws names from them, so most statements apply; the
//! ones that fail still exercise the outcome comparison.

use rand::RngExt;
use rand::rngs::StdRng;

#[derive(Clone, Debug)]
struct Column {
    name: String,
    ty: String,
}

#[derive(Clone, Debug)]
struct Table {
    name: String,
    columns: Vec<Column>,
    /// `PARTITION BY RANGE (column)`: an integer column.
    partition_key: Option<String>,
    has_pk: bool,
    partitions: usize,
}

/// What a statement does to the generator's model when it succeeds.
#[derive(Debug)]
enum Effect {
    None,
    Schema(String),
    Table(Table),
    DropTable(String),
    RenameTable(String, String),
    AddColumn(String, Column),
    DropColumn(String, String),
    RenameColumn(String, String, String),
    ColumnType(String, String, String),
    Pk(String),
    Partition(String),
    NewPartition(String, Table),
    Enum(String, Vec<String>),
    EnumLabel(String, String),
    Composite(String),
    Domain(String),
    DropType(String),
    Sequence(String),
    DropSequence(String),
    View(String),
    DropView(String),
    Function(String),
    DropFunction(String),
    Index(String),
    DropIndex(String),
    RenameIndex(String, String),
    Constraint(String, String),
    Temporal(String, String),
    Detach(String, String),
    RenameSequence(String, String),
    RenameSchema(String, String),
    RenameType(String, String),
    Named(&'static str, String),
    DropNamed(&'static str, String),
    Policy(String, String),
    DropTables(Vec<String>),
}

#[derive(Default)]
pub struct State {
    counter: usize,
    schemas: Vec<String>,
    tables: Vec<Table>,
    enums: Vec<(String, Vec<String>)>,
    composites: Vec<String>,
    domains: Vec<String>,
    sequences: Vec<String>,
    views: Vec<String>,
    functions: Vec<String>,
    indexes: Vec<String>,
    /// `(table, constraint)`.
    constraints: Vec<(String, String)>,
    /// `(table, range type)` of the tables with a temporal key.
    temporals: Vec<(String, String)>,
    /// `(parent, partition)`.
    partitions_of: Vec<(String, String)>,
    /// `(table, policy)`.
    policies: Vec<(String, String)>,
    /// Other objects by kind: procedures, aggregates, publications,
    /// statistics, roles.
    named: Vec<(&'static str, String)>,
    pending: Option<Effect>,
}

const BASE_TYPES: &[&str] = &[
    "int4",
    "int8",
    "int2",
    "text",
    "varchar(20)",
    "numeric(10,2)",
    "numeric",
    "bool",
    "date",
    "timestamptz",
    "timestamp(3)",
    "jsonb",
    "uuid",
    "int4[]",
    "text[]",
    "text COLLATE \"C\"",
    "bytea",
    "interval",
    "float8",
    "char(3)",
];

fn pick<'a, T>(rng: &mut StdRng, items: &'a [T]) -> Option<&'a T> {
    (!items.is_empty()).then(|| &items[rng.random_range(0..items.len())])
}

fn chance(rng: &mut StdRng, percent: u32) -> bool {
    rng.random_range(0..100) < percent
}

impl State {
    fn fresh(&mut self, prefix: &str) -> String {
        self.counter += 1;
        format!("{prefix}{}", self.counter)
    }

    /// A schema-qualified name in a random schema (public most of the
    /// time).
    fn qualified(&mut self, rng: &mut StdRng, prefix: &str) -> String {
        let name = self.fresh(prefix);
        match pick(rng, &self.schemas).filter(|_| chance(rng, 30)) {
            Some(schema) => format!("{schema}.{name}"),
            None => name,
        }
    }

    fn column_type(&self, rng: &mut StdRng) -> String {
        let roll = rng.random_range(0..100);
        if roll < 8
            && let Some((e, _)) = pick(rng, &self.enums)
        {
            return e.clone();
        }
        if roll < 14
            && let Some(d) = pick(rng, &self.domains)
        {
            return d.clone();
        }
        if roll < 17
            && let Some(c) = pick(rng, &self.composites)
        {
            return c.clone();
        }
        (*pick(rng, BASE_TYPES).unwrap()).to_owned()
    }

    fn table(&self, rng: &mut StdRng) -> Option<Table> {
        pick(rng, &self.tables).cloned()
    }

    fn column_def(&mut self, rng: &mut StdRng, column: &Column) -> String {
        let mut def = format!("{} {}", column.name, column.ty);
        let integer = matches!(column.ty.as_str(), "int4" | "int8" | "int2");
        if integer && chance(rng, 10) {
            def.push_str(if chance(rng, 50) {
                " GENERATED ALWAYS AS IDENTITY"
            } else {
                " GENERATED BY DEFAULT AS IDENTITY"
            });
            return def;
        }
        if chance(rng, 30) {
            def.push_str(" NOT NULL");
        }
        if integer && chance(rng, 15) {
            def.push_str(" DEFAULT 0");
        } else if column.ty == "text" && chance(rng, 15) {
            def.push_str(" DEFAULT 'x'");
        } else if column.ty == "timestamptz" && chance(rng, 20) {
            def.push_str(" DEFAULT now()");
        }
        if integer && chance(rng, 10) {
            def.push_str(&format!(" CHECK ({} > 0)", column.name));
        }
        if chance(rng, 5) {
            def.push_str(" UNIQUE");
        }
        def
    }

    /// The next statement, and what it does when it succeeds (applied by
    /// [`Self::applied`]).
    pub fn statement(&mut self, rng: &mut StdRng) -> String {
        let (sql, effect) = loop {
            if let Some(s) = self.try_statement(rng) {
                break s;
            }
        };
        self.pending = Some(effect);
        sql
    }

    /// Record the last statement's outcome.
    pub fn applied(&mut self, _sql: &str, ok: bool) {
        let effect = self.pending.take().unwrap_or(Effect::None);
        if ok {
            self.apply(effect);
        }
    }

    fn try_statement(&mut self, rng: &mut StdRng) -> Option<(String, Effect)> {
        let roll = rng.random_range(0..150);
        if roll >= 100 {
            return self.extended(rng, roll - 100);
        }
        Some(match roll {
            0..=2 => {
                let name = self.fresh("s");
                (format!("CREATE SCHEMA {name}"), Effect::Schema(name))
            }
            3..=16 => self.create_table(rng),
            17..=19 => {
                let parent = pick(rng, &self.tables)
                    .filter(|t| t.partition_key.is_some())?
                    .clone();
                let name = format!("{}_p{}", parent.name.replace('.', "_"), parent.partitions);
                let lo = parent.partitions * 100;
                let sql = if chance(rng, 15) {
                    format!("CREATE TABLE {name} PARTITION OF {} DEFAULT", parent.name)
                } else {
                    format!(
                        "CREATE TABLE {name} PARTITION OF {} FOR VALUES FROM ({lo}) TO ({})",
                        parent.name,
                        lo + 100
                    )
                };
                let child = Table {
                    name,
                    columns: parent.columns.clone(),
                    partition_key: None,
                    has_pk: parent.has_pk,
                    partitions: 0,
                };
                (sql, Effect::NewPartition(parent.name, child))
            }
            20..=21 => {
                let parent = self.table(rng)?;
                if parent.partition_key.is_some() {
                    return None;
                }
                let name = self.qualified(rng, "c");
                let extra = Column {
                    name: self.fresh("x"),
                    ty: "text".into(),
                };
                let mut columns = parent.columns.clone();
                columns.push(extra.clone());
                (
                    format!(
                        "CREATE TABLE {name} ({} text) INHERITS ({})",
                        extra.name, parent.name
                    ),
                    Effect::Table(Table {
                        name,
                        columns,
                        partition_key: None,
                        has_pk: false,
                        partitions: 0,
                    }),
                )
            }
            22..=25 => {
                let t = self.table(rng)?;
                let column = Column {
                    name: self.fresh("a"),
                    ty: self.column_type(rng),
                };
                let def = self.column_def(rng, &column);
                (
                    format!("ALTER TABLE {} ADD COLUMN {def}", t.name),
                    Effect::AddColumn(t.name, column),
                )
            }
            26..=27 => {
                let t = self.table(rng)?;
                let c = pick(rng, &t.columns)?.clone();
                let cascade = if chance(rng, 30) { " CASCADE" } else { "" };
                (
                    format!("ALTER TABLE {} DROP COLUMN {}{cascade}", t.name, c.name),
                    Effect::DropColumn(t.name, c.name),
                )
            }
            28..=29 => {
                let t = self.table(rng)?;
                let c = pick(rng, &t.columns)?.clone();
                let new = self.fresh("r");
                (
                    format!("ALTER TABLE {} RENAME COLUMN {} TO {new}", t.name, c.name),
                    Effect::RenameColumn(t.name, c.name, new),
                )
            }
            30..=32 => {
                let t = self.table(rng)?;
                let c = pick(rng, &t.columns)?.clone();
                let ty = (*pick(rng, &["int8", "text", "numeric", "varchar(40)", "int4"]).unwrap())
                    .to_owned();
                let using = if chance(rng, 50) {
                    format!(" USING {}::{ty}", c.name)
                } else {
                    String::new()
                };
                (
                    format!(
                        "ALTER TABLE {} ALTER COLUMN {} TYPE {ty}{using}",
                        t.name, c.name
                    ),
                    Effect::ColumnType(t.name, c.name, ty),
                )
            }
            33..=36 => {
                let t = self.table(rng)?;
                let c = pick(rng, &t.columns)?.clone();
                let action = match rng.random_range(0..6) {
                    0 => "SET NOT NULL".to_owned(),
                    1 => "DROP NOT NULL".to_owned(),
                    2 => "DROP DEFAULT".to_owned(),
                    3 => "SET DEFAULT NULL".to_owned(),
                    4 => "ADD GENERATED BY DEFAULT AS IDENTITY".to_owned(),
                    _ => "SET STATISTICS 100".to_owned(),
                };
                (
                    format!("ALTER TABLE {} ALTER COLUMN {} {action}", t.name, c.name),
                    Effect::None,
                )
            }
            37..=41 => {
                let t = self.table(rng)?;
                let c = pick(rng, &t.columns)?.clone();
                let name = self.fresh("k");
                let (body, effect) = match rng.random_range(0..5) {
                    0 if !t.has_pk => (
                        format!("PRIMARY KEY ({})", c.name),
                        Effect::Pk(t.name.clone()),
                    ),
                    1 => (
                        format!("UNIQUE ({})", c.name),
                        Effect::Constraint(t.name.clone(), name.clone()),
                    ),
                    2 => (
                        format!("CHECK ({} IS NOT NULL)", c.name),
                        Effect::Constraint(t.name.clone(), name.clone()),
                    ),
                    3 => {
                        let other = pick(rng, &self.tables).filter(|o| o.has_pk)?.clone();
                        (
                            format!("FOREIGN KEY ({}) REFERENCES {}", c.name, other.name),
                            Effect::Constraint(t.name.clone(), name.clone()),
                        )
                    }
                    _ => (
                        format!("CHECK ({} IS NOT NULL) NOT VALID", c.name),
                        Effect::Constraint(t.name.clone(), name.clone()),
                    ),
                };
                (
                    format!("ALTER TABLE {} ADD CONSTRAINT {name} {body}", t.name),
                    effect,
                )
            }
            42..=43 => {
                let t = self.table(rng)?;
                let new = self.fresh("t");
                (
                    format!("ALTER TABLE {} RENAME TO {new}", t.name),
                    Effect::RenameTable(
                        t.name.clone(),
                        match t.name.split_once('.') {
                            Some((schema, _)) => format!("{schema}.{new}"),
                            None => new,
                        },
                    ),
                )
            }
            44 => {
                let t = self.table(rng)?;
                let schema = pick(rng, &self.schemas)?.clone();
                let bare = t.name.rsplit('.').next().unwrap().to_owned();
                (
                    format!("ALTER TABLE {} SET SCHEMA {schema}", t.name),
                    Effect::RenameTable(t.name, format!("{schema}.{bare}")),
                )
            }
            45..=47 => {
                let t = self.table(rng)?;
                let cascade = if chance(rng, 40) { " CASCADE" } else { "" };
                (
                    format!("DROP TABLE {}{cascade}", t.name),
                    Effect::DropTable(t.name),
                )
            }
            48..=53 => {
                let t = self.table(rng)?;
                let c = pick(rng, &t.columns)?.clone();
                let name = self.fresh("i");
                let unique = if chance(rng, 25) { "UNIQUE " } else { "" };
                let key = match rng.random_range(0..4) {
                    0 if c.ty == "text" => format!("(lower({}))", c.name),
                    1 => format!("({}) WHERE {} IS NOT NULL", c.name, c.name),
                    2 => format!("({} DESC NULLS LAST)", c.name),
                    _ => format!("({})", c.name),
                };
                let schema = t.name.split_once('.').map(|(s, _)| format!("{s}."));
                (
                    format!("CREATE {unique}INDEX {name} ON {} {key}", t.name),
                    Effect::Index(format!("{}{name}", schema.unwrap_or_default())),
                )
            }
            54 => {
                let i = pick(rng, &self.indexes)?.clone();
                (format!("DROP INDEX {i}"), Effect::DropIndex(i))
            }
            55 => {
                let i = pick(rng, &self.indexes)?.clone();
                let new = self.fresh("i");
                let renamed = match i.split_once('.') {
                    Some((s, _)) => format!("{s}.{new}"),
                    None => new.clone(),
                };
                (
                    format!("ALTER INDEX {i} RENAME TO {new}"),
                    Effect::RenameIndex(i, renamed),
                )
            }
            56..=58 => {
                let name = self.qualified(rng, "e");
                let labels: Vec<String> = (0..rng.random_range(1..4))
                    .map(|i| format!("l{i}"))
                    .collect();
                let list = labels
                    .iter()
                    .map(|l| format!("'{l}'"))
                    .collect::<Vec<_>>()
                    .join(", ");
                (
                    format!("CREATE TYPE {name} AS ENUM ({list})"),
                    Effect::Enum(name, labels),
                )
            }
            59..=60 => {
                let (e, labels) = pick(rng, &self.enums)?.clone();
                let new = self.fresh("v");
                let position = match pick(rng, &labels).filter(|_| chance(rng, 60)) {
                    Some(l) if chance(rng, 50) => format!(" BEFORE '{l}'"),
                    Some(l) => format!(" AFTER '{l}'"),
                    None => String::new(),
                };
                (
                    format!("ALTER TYPE {e} ADD VALUE '{new}'{position}"),
                    Effect::EnumLabel(e, new),
                )
            }
            61 => {
                let (e, labels) = pick(rng, &self.enums)?.clone();
                let old = pick(rng, &labels)?.clone();
                let new = self.fresh("w");
                (
                    format!("ALTER TYPE {e} RENAME VALUE '{old}' TO '{new}'"),
                    Effect::None,
                )
            }
            62..=63 => {
                let name = self.qualified(rng, "ct");
                let ty = self.column_type(rng);
                (
                    format!("CREATE TYPE {name} AS (f1 int4, f2 {ty})"),
                    Effect::Composite(name),
                )
            }
            64 => {
                let c = pick(rng, &self.composites)?.clone();
                let action = if chance(rng, 50) {
                    format!("ADD ATTRIBUTE {} text", self.fresh("f"))
                } else {
                    "DROP ATTRIBUTE f1".to_owned()
                };
                (format!("ALTER TYPE {c} {action}"), Effect::None)
            }
            65 => {
                let name = self.qualified(rng, "rg");
                let sub = *pick(rng, &["int4", "numeric", "date", "text"]).unwrap();
                (
                    format!("CREATE TYPE {name} AS RANGE (subtype = {sub})"),
                    Effect::None,
                )
            }
            66..=68 => {
                let name = self.qualified(rng, "d");
                let base = *pick(
                    rng,
                    &[
                        "int4",
                        "text",
                        "numeric(8,2)",
                        "varchar(10)",
                        "jsonb",
                        "int4[]",
                    ],
                )
                .unwrap();
                let mut sql = format!("CREATE DOMAIN {name} AS {base}");
                if base == "text" && chance(rng, 30) {
                    sql.push_str(" COLLATE \"C\"");
                }
                if chance(rng, 30) {
                    sql.push_str(" NOT NULL");
                }
                if chance(rng, 30) {
                    sql.push_str(" DEFAULT NULL");
                }
                if chance(rng, 30) {
                    sql.push_str(" CHECK (VALUE IS NOT NULL)");
                }
                (sql, Effect::Domain(name))
            }
            69 => {
                let d = pick(rng, &self.domains)?.clone();
                let action = *pick(
                    rng,
                    &[
                        "SET NOT NULL",
                        "DROP NOT NULL",
                        "ADD CONSTRAINT dc CHECK (VALUE IS NOT NULL)",
                        "DROP DEFAULT",
                    ],
                )
                .unwrap();
                (format!("ALTER DOMAIN {d} {action}"), Effect::None)
            }
            70..=71 => {
                let ty = match rng.random_range(0..3) {
                    0 => pick(rng, &self.domains).cloned(),
                    1 => pick(rng, &self.enums).map(|e| e.0.clone()),
                    _ => pick(rng, &self.composites).cloned(),
                }?;
                let cascade = if chance(rng, 40) { " CASCADE" } else { "" };
                (format!("DROP TYPE {ty}{cascade}"), Effect::DropType(ty))
            }
            72..=73 => {
                let name = self.qualified(rng, "sq");
                let mut sql = format!("CREATE SEQUENCE {name}");
                if chance(rng, 30) {
                    sql.push_str(" AS int4");
                }
                if chance(rng, 30)
                    && let Some(t) = self.table(rng)
                    && let Some(c) = pick(rng, &t.columns)
                {
                    sql.push_str(&format!(" OWNED BY {}.{}", t.name, c.name));
                }
                (sql, Effect::Sequence(name))
            }
            74 => {
                let s = pick(rng, &self.sequences)?.clone();
                (format!("DROP SEQUENCE {s}"), Effect::DropSequence(s))
            }
            75..=78 => {
                let t = self.table(rng)?;
                let cols: Vec<&str> = t
                    .columns
                    .iter()
                    .filter(|_| chance(rng, 60))
                    .map(|c| c.name.as_str())
                    .collect();
                let list = if cols.is_empty() {
                    "*".to_owned()
                } else {
                    cols.join(", ")
                };
                let name = self.qualified(rng, "v");
                let kind = if chance(rng, 25) {
                    "MATERIALIZED VIEW"
                } else {
                    "VIEW"
                };
                (
                    format!("CREATE {kind} {name} AS SELECT {list} FROM {}", t.name),
                    Effect::View(name),
                )
            }
            79 => {
                let v = pick(rng, &self.views)?.clone();
                (format!("DROP VIEW {v} CASCADE"), Effect::DropView(v))
            }
            80..=82 => {
                let name = self.qualified(rng, "f");
                let sql = match rng.random_range(0..3) {
                    0 => format!(
                        "CREATE FUNCTION {name}(a int4, b text DEFAULT 'x') RETURNS int4 \
                         LANGUAGE sql IMMUTABLE AS 'SELECT a + length(b)'"
                    ),
                    1 => format!(
                        "CREATE FUNCTION {name}() RETURNS trigger LANGUAGE plpgsql AS \
                         $$BEGIN RETURN NEW; END$$"
                    ),
                    _ => format!(
                        "CREATE FUNCTION {name}(VARIADIC xs int4[]) RETURNS SETOF int4 \
                         LANGUAGE sql STABLE STRICT AS 'SELECT unnest(xs)'"
                    ),
                };
                (sql, Effect::Function(name))
            }
            83 => {
                let f = pick(rng, &self.functions)?.clone();
                (
                    format!("DROP FUNCTION {f} CASCADE"),
                    Effect::DropFunction(f),
                )
            }
            84..=85 => {
                let t = self.table(rng)?;
                let f = pick(rng, &self.functions)?.clone();
                let name = self.fresh("tg");
                let timing = *pick(
                    rng,
                    &["BEFORE INSERT", "AFTER UPDATE", "BEFORE INSERT OR UPDATE"],
                )
                .unwrap();
                (
                    format!(
                        "CREATE TRIGGER {name} {timing} ON {} FOR EACH ROW EXECUTE FUNCTION {f}()",
                        t.name
                    ),
                    Effect::None,
                )
            }
            86..=87 => {
                let t = self.table(rng)?;
                let name = self.fresh("pol");
                if chance(rng, 30) {
                    (
                        format!("ALTER TABLE {} ENABLE ROW LEVEL SECURITY", t.name),
                        Effect::None,
                    )
                } else {
                    (
                        format!("CREATE POLICY {name} ON {} USING (true)", t.name),
                        Effect::Policy(t.name, name),
                    )
                }
            }
            88 => {
                let t = self.table(rng)?;
                let c = pick(rng, &t.columns)?.clone();
                (
                    format!("COMMENT ON COLUMN {}.{} IS 'c'", t.name, c.name),
                    Effect::None,
                )
            }
            89 => {
                let ext =
                    *pick(rng, &["citext", "hstore", "pg_trgm", "btree_gist", "ltree"]).unwrap();
                (
                    format!("CREATE EXTENSION IF NOT EXISTS {ext}"),
                    Effect::None,
                )
            }
            90 => {
                let schema = pick(rng, &self.schemas)?.clone();
                (format!("DROP SCHEMA {schema} CASCADE"), Effect::None)
            }
            91..=92 => {
                let t = self.table(rng)?;
                let other = self.table(rng)?;
                if t.name == other.name {
                    return None;
                }
                let sql = if chance(rng, 50) {
                    format!("ALTER TABLE {} INHERIT {}", t.name, other.name)
                } else {
                    format!("ALTER TABLE {} NO INHERIT {}", t.name, other.name)
                };
                (sql, Effect::None)
            }
            93 => {
                let t = self.table(rng)?;
                let action = *pick(
                    rng,
                    &[
                        "SET UNLOGGED",
                        "SET LOGGED",
                        "REPLICA IDENTITY FULL",
                        "SET (fillfactor = 70)",
                        "FORCE ROW LEVEL SECURITY",
                    ],
                )
                .unwrap();
                (format!("ALTER TABLE {} {action}", t.name), Effect::None)
            }
            94 => {
                let name = self.fresh("tt");
                (
                    format!("CREATE TEMP TABLE {name} (a int4 PRIMARY KEY, b text)"),
                    Effect::None,
                )
            }
            95 => {
                let t = self.table(rng)?;
                let c = pick(rng, &t.columns)?.clone();
                let name = self.fresh("g");
                let integer = matches!(c.ty.as_str(), "int4" | "int8" | "int2");
                if !integer {
                    return None;
                }
                (
                    format!(
                        "ALTER TABLE {} ADD COLUMN {name} int8 GENERATED ALWAYS AS ({} * 2) STORED",
                        t.name, c.name
                    ),
                    Effect::AddColumn(
                        t.name.clone(),
                        Column {
                            name,
                            ty: "int8".into(),
                        },
                    ),
                )
            }
            96 => {
                let v = pick(rng, &self.views)?.clone();
                let new = self.fresh("v");
                (
                    format!("ALTER VIEW {v} RENAME TO {new}"),
                    Effect::DropView(v),
                )
            }
            97 => {
                let t = self.table(rng)?;
                let parent = pick(rng, &self.tables)
                    .filter(|p| p.partition_key.is_some() && p.name != t.name)?
                    .clone();
                let lo = 10_000 + parent.partitions * 100;
                (
                    format!(
                        "ALTER TABLE {} ATTACH PARTITION {} FOR VALUES FROM ({lo}) TO ({})",
                        parent.name,
                        t.name,
                        lo + 100
                    ),
                    Effect::Partition(parent.name),
                )
            }
            _ => {
                let t = self.table(rng)?;
                let k = pick(rng, &t.columns)?.clone();
                (
                    format!(
                        "ALTER TABLE {} DROP CONSTRAINT IF EXISTS {}_pkey",
                        t.name, k.name
                    ),
                    Effect::None,
                )
            }
        })
    }

    /// The statement kinds added on top of the original mix: PostgreSQL
    /// 18/19 table features (constraint enforceability and inheritability,
    /// virtual generated columns, temporal keys, access methods,
    /// partition detach), and the other object kinds a migration manages
    /// (sequences, schemas, routines, aggregates, operators and classes,
    /// casts, publications, statistics, policies, rules, event triggers,
    /// roles, grants and default privileges, comments, collations, text
    /// search, transactions and savepoints).
    fn extended(&mut self, rng: &mut StdRng, roll: u32) -> Option<(String, Effect)> {
        Some(match roll {
            0..=2 => {
                let (table, name) = pick(rng, &self.constraints)?.clone();
                let action = *pick(
                    rng,
                    &[
                        "NOT ENFORCED",
                        "ENFORCED",
                        "DEFERRABLE",
                        "NOT DEFERRABLE",
                        "DEFERRABLE INITIALLY DEFERRED",
                        "INHERIT",
                        "NO INHERIT",
                        "NOT VALID",
                    ],
                )
                .unwrap();
                (
                    format!("ALTER TABLE {table} ALTER CONSTRAINT {name} {action}"),
                    Effect::None,
                )
            }
            3 => {
                let t = self.table(rng)?;
                let c = pick(rng, &t.columns)?.clone();
                let name = self.fresh("k");
                let body = match rng.random_range(0..4) {
                    0 => format!("CHECK ({} IS NOT NULL) NOT ENFORCED", c.name),
                    1 => {
                        let other = pick(rng, &self.tables).filter(|o| o.has_pk)?.clone();
                        format!(
                            "FOREIGN KEY ({}) REFERENCES {} NOT ENFORCED",
                            c.name, other.name
                        )
                    }
                    2 => format!("NOT NULL {} NOT VALID", c.name),
                    _ => format!("NOT NULL {} NO INHERIT", c.name),
                };
                (
                    format!("ALTER TABLE {} ADD CONSTRAINT {name} {body}", t.name),
                    Effect::Constraint(t.name, name),
                )
            }
            4 => {
                let t = self.table(rng)?;
                let c = pick(rng, &t.columns)
                    .filter(|c| matches!(c.ty.as_str(), "int4" | "int8" | "int2"))?
                    .clone();
                let name = self.fresh("gv");
                let extra = match rng.random_range(0..4) {
                    0 => format!(" CHECK ({name} > 0)"),
                    1 => " NOT NULL".to_owned(),
                    _ => String::new(),
                };
                (
                    format!(
                        "ALTER TABLE {} ADD COLUMN {name} int8 GENERATED ALWAYS AS ({} * 2) \
                         VIRTUAL{extra}",
                        t.name, c.name
                    ),
                    Effect::AddColumn(
                        t.name,
                        Column {
                            name,
                            ty: "int8".into(),
                        },
                    ),
                )
            }
            5 => {
                // A temporal key needs btree_gist for its scalar part.
                let name = self.qualified(rng, "tp");
                let range = *pick(rng, &["daterange", "tstzrange", "int4range"]).unwrap();
                let key = if chance(rng, 50) {
                    "PRIMARY KEY"
                } else {
                    "UNIQUE"
                };
                (
                    format!(
                        "CREATE EXTENSION IF NOT EXISTS btree_gist; CREATE TABLE {name} (id int4, \
                         valid_at {range}, {key} (id, valid_at WITHOUT OVERLAPS))"
                    ),
                    Effect::Temporal(name, range.to_owned()),
                )
            }
            6 => {
                let (parent, range) = pick(rng, &self.temporals)?.clone();
                let name = self.qualified(rng, "tf");
                let action = *pick(
                    rng,
                    &[
                        "",
                        " ON DELETE NO ACTION",
                        " ON UPDATE RESTRICT",
                        " ON DELETE CASCADE",
                    ],
                )
                .unwrap();
                (
                    format!(
                        "CREATE TABLE {name} (id int4, valid_at {range}, p_id int4, FOREIGN KEY \
                         (p_id, PERIOD valid_at) REFERENCES {parent} (id, PERIOD valid_at){action})"
                    ),
                    Effect::None,
                )
            }
            7 => {
                let t = self.table(rng)?;
                let am = *pick(rng, &["heap", "DEFAULT", "btree"]).unwrap();
                (
                    format!("ALTER TABLE {} SET ACCESS METHOD {am}", t.name),
                    Effect::None,
                )
            }
            8 => {
                let (parent, child) = pick(rng, &self.partitions_of)?.clone();
                let mode = *pick(rng, &["", " FINALIZE"]).unwrap();
                (
                    format!("ALTER TABLE {parent} DETACH PARTITION {child}{mode}"),
                    Effect::Detach(parent, child),
                )
            }
            9..=10 => {
                let s = pick(rng, &self.sequences)?.clone();
                let action = match rng.random_range(0..10) {
                    0 => "INCREMENT BY 5".to_owned(),
                    1 => "MINVALUE 10 MAXVALUE 1000 RESTART WITH 10".to_owned(),
                    2 => "AS int2".to_owned(),
                    3 => "CYCLE CACHE 10".to_owned(),
                    4 => "OWNED BY NONE".to_owned(),
                    5 => "START WITH 0".to_owned(),
                    6 => "INCREMENT BY -1".to_owned(),
                    7 => {
                        let t = self.table(rng)?;
                        let c = pick(rng, &t.columns)?.clone();
                        format!("OWNED BY {}.{}", t.name, c.name)
                    }
                    8 => {
                        let new = self.fresh("sq");
                        return Some((
                            format!("ALTER SEQUENCE {s} RENAME TO {new}"),
                            Effect::RenameSequence(
                                s.clone(),
                                match s.split_once('.') {
                                    Some((schema, _)) => format!("{schema}.{new}"),
                                    None => new,
                                },
                            ),
                        ));
                    }
                    _ => "SET UNLOGGED".to_owned(),
                };
                (format!("ALTER SEQUENCE {s} {action}"), Effect::None)
            }
            11 => {
                let schema = pick(rng, &self.schemas)?.clone();
                let new = self.fresh("s");
                (
                    format!("ALTER SCHEMA {schema} RENAME TO {new}"),
                    Effect::RenameSchema(schema, new),
                )
            }
            12 => {
                let name = self.qualified(rng, "pr");
                let sql = match rng.random_range(0..4) {
                    0 => format!("CREATE PROCEDURE {name}(a int4) LANGUAGE sql AS 'SELECT a'"),
                    1 => format!(
                        "CREATE PROCEDURE {name}(INOUT a int4) LANGUAGE plpgsql AS $$BEGIN a := a \
                         + 1; END$$"
                    ),
                    2 => {
                        format!("CREATE PROCEDURE {name}() LANGUAGE sql BEGIN ATOMIC SELECT 1; END")
                    }
                    _ => format!("CREATE PROCEDURE {name}(a int4) IMMUTABLE LANGUAGE sql AS ''"),
                };
                (sql, Effect::Named("procedure", name))
            }
            13 => {
                let p = self.named("procedure", rng)?;
                let action = match rng.random_range(0..4) {
                    0 => format!("DROP PROCEDURE {p}"),
                    1 => format!("ALTER PROCEDURE {p} SECURITY DEFINER"),
                    2 => format!("ALTER PROCEDURE {p} SET search_path = public"),
                    _ => format!("DROP FUNCTION {p}"),
                };
                let effect = if action.starts_with("DROP PROCEDURE") {
                    Effect::DropNamed("procedure", p)
                } else {
                    Effect::None
                };
                (action, effect)
            }
            14 => {
                let name = self.qualified(rng, "ag");
                let sql = match rng.random_range(0..4) {
                    0 => format!("CREATE AGGREGATE {name}(int4) (sfunc = int4pl, stype = int4)"),
                    1 => format!(
                        "CREATE AGGREGATE {name}(int4) (sfunc = int4pl, stype = int4, \
                         initcond = '0', combinefunc = int4pl, parallel = safe)"
                    ),
                    2 => format!(
                        "CREATE AGGREGATE {name}(int8) (sfunc = int8larger, stype = int8, \
                         sortop = >)"
                    ),
                    _ => format!("CREATE AGGREGATE {name}(text) (sfunc = int4pl, stype = int4)"),
                };
                (sql, Effect::Named("aggregate", name))
            }
            15 => {
                let a = self.named("aggregate", rng)?;
                (format!("DROP AGGREGATE IF EXISTS {a}(int4)"), Effect::None)
            }
            16 => {
                let op = *pick(rng, &["===", "<<~", "##", "@@@"]).unwrap();
                let sql = match rng.random_range(0..3) {
                    0 => format!(
                        "CREATE OPERATOR {op} (leftarg = int4, rightarg = int4, function = \
                         int4eq, commutator = {op}, restrict = eqsel, join = eqjoinsel)"
                    ),
                    1 => format!("CREATE OPERATOR {op} (rightarg = int4, function = int4um)"),
                    _ => format!("DROP OPERATOR IF EXISTS {op} (int4, int4)"),
                };
                (sql, Effect::None)
            }
            17 => {
                let name = self.fresh("oc");
                let sql = if chance(rng, 50) {
                    format!(
                        "CREATE OPERATOR CLASS {name} FOR TYPE int4 USING btree AS OPERATOR 1 <, \
                         OPERATOR 2 <=, OPERATOR 3 =, OPERATOR 4 >=, OPERATOR 5 >, FUNCTION 1 \
                         btint4cmp(int4, int4)"
                    )
                } else {
                    format!(
                        "CREATE OPERATOR FAMILY {name} USING hash; ALTER OPERATOR FAMILY {name} \
                         USING hash ADD OPERATOR 1 = (int4, int4), FUNCTION 1 hashint4(int4)"
                    )
                };
                (sql, Effect::None)
            }
            18 => {
                let (e, _) = pick(rng, &self.enums)?.clone();
                let sql = match rng.random_range(0..3) {
                    0 => format!("CREATE CAST ({e} AS int4) WITH INOUT AS ASSIGNMENT"),
                    1 => format!("CREATE CAST (text AS {e}) WITH INOUT AS IMPLICIT"),
                    _ => format!("DROP CAST IF EXISTS ({e} AS int4)"),
                };
                (sql, Effect::None)
            }
            19..=20 => {
                let t = self.table(rng)?;
                let name = self.fresh("pub");
                let c = pick(rng, &t.columns)?.clone();
                let target = match rng.random_range(0..4) {
                    0 => format!("TABLE {}", t.name),
                    1 => format!("TABLE {} (id, {})", t.name, c.name),
                    2 => format!("TABLE {} WHERE ({} IS NOT NULL)", t.name, c.name),
                    _ => match pick(rng, &self.schemas) {
                        Some(s) => format!("TABLES IN SCHEMA {s}"),
                        None => format!("TABLE {}", t.name),
                    },
                };
                let with = *pick(
                    rng,
                    &[
                        "",
                        " WITH (publish = 'insert')",
                        " WITH (publish_via_partition_root)",
                        " WITH (publish_generated_columns = stored)",
                    ],
                )
                .unwrap();
                (
                    format!("CREATE PUBLICATION {name} FOR {target}{with}"),
                    Effect::Named("publication", name),
                )
            }
            21 => {
                let p = self.named("publication", rng)?;
                let t = self.table(rng)?;
                let action = match rng.random_range(0..5) {
                    0 => format!("ADD TABLE {}", t.name),
                    1 => format!("DROP TABLE {}", t.name),
                    2 => format!("SET TABLE {}", t.name),
                    3 => "SET (publish = 'update, delete')".to_owned(),
                    _ => {
                        return Some((
                            format!("DROP PUBLICATION {p}"),
                            Effect::DropNamed("publication", p),
                        ));
                    }
                };
                (format!("ALTER PUBLICATION {p} {action}"), Effect::None)
            }
            22 => {
                let t = self.table(rng)?;
                let a = pick(rng, &t.columns)?.clone();
                let b = pick(rng, &t.columns)?.clone();
                let name = self.qualified(rng, "st");
                let kinds = *pick(rng, &["", " (ndistinct)", " (dependencies, mcv)"]).unwrap();
                let target = if chance(rng, 30) {
                    format!("({} IS NULL), {}", a.name, b.name)
                } else {
                    format!("{}, {}", a.name, b.name)
                };
                (
                    format!(
                        "CREATE STATISTICS {name}{kinds} ON {target} FROM {}",
                        t.name
                    ),
                    Effect::Named("statistics", name),
                )
            }
            23 => {
                let s = self.named("statistics", rng)?;
                let sql = match rng.random_range(0..3) {
                    0 => format!("ALTER STATISTICS {s} SET STATISTICS 50"),
                    1 => {
                        let new = self.fresh("st");
                        return Some((
                            format!("ALTER STATISTICS {s} RENAME TO {new}"),
                            Effect::DropNamed("statistics", s),
                        ));
                    }
                    _ => {
                        return Some((
                            format!("DROP STATISTICS {s}"),
                            Effect::DropNamed("statistics", s),
                        ));
                    }
                };
                (sql, Effect::None)
            }
            24 => {
                let (table, name) = pick(rng, &self.policies)?.clone();
                let sql = match rng.random_range(0..4) {
                    0 => format!("ALTER POLICY {name} ON {table} USING (false)"),
                    1 => format!("ALTER POLICY {name} ON {table} TO PUBLIC"),
                    2 => format!(
                        "ALTER POLICY {name} ON {table} RENAME TO {}",
                        self.fresh("pol")
                    ),
                    _ => format!("DROP POLICY {name} ON {table}"),
                };
                (sql, Effect::None)
            }
            25 => {
                let t = self.table(rng)?;
                let name = self.fresh("pol");
                let command = *pick(
                    rng,
                    &[
                        "FOR SELECT USING (true)",
                        "FOR INSERT WITH CHECK (true)",
                        "AS RESTRICTIVE FOR UPDATE USING (true) WITH CHECK (true)",
                        "FOR DELETE USING (id IS NOT NULL)",
                    ],
                )
                .unwrap();
                (
                    format!("CREATE POLICY {name} ON {} {command}", t.name),
                    Effect::Policy(t.name, name),
                )
            }
            26 => {
                let t = self.table(rng)?;
                let name = self.fresh("ru");
                let sql = match rng.random_range(0..3) {
                    0 => format!(
                        "CREATE RULE {name} AS ON INSERT TO {} DO ALSO NOTHING",
                        t.name
                    ),
                    1 => format!(
                        "CREATE RULE {name} AS ON UPDATE TO {} WHERE old.id IS NULL DO INSTEAD \
                         NOTHING",
                        t.name
                    ),
                    _ => format!("DROP RULE IF EXISTS {name} ON {}", t.name),
                };
                (sql, Effect::None)
            }
            27 => {
                let name = self.fresh("et");
                let event = *pick(
                    rng,
                    &[
                        "ddl_command_start",
                        "ddl_command_end",
                        "sql_drop",
                        "table_rewrite",
                    ],
                )
                .unwrap();
                (
                    format!(
                        "CREATE FUNCTION {name}_f() RETURNS event_trigger LANGUAGE plpgsql AS \
                         $$BEGIN END$$; CREATE EVENT TRIGGER {name} ON {event} EXECUTE FUNCTION \
                         {name}_f(); ALTER EVENT TRIGGER {name} DISABLE"
                    ),
                    Effect::None,
                )
            }
            28 => {
                // Roles live in the cluster: a name unique to this run.
                let name = format!("cor{}_{}", self.fresh("r"), rng.random_range(0..u32::MAX));
                let options = *pick(rng, &["", " LOGIN", " NOINHERIT", " CREATEROLE"]).unwrap();
                (
                    format!("CREATE ROLE {name}{options}"),
                    Effect::Named("role", name),
                )
            }
            29..=31 => {
                let grantee = if chance(rng, 30) {
                    "PUBLIC".to_owned()
                } else {
                    self.named("role", rng)?
                };
                let verb_grant = chance(rng, 70);
                let object = match rng.random_range(0..7) {
                    0 => {
                        let t = self.table(rng)?;
                        format!("SELECT, INSERT ON {}", t.name)
                    }
                    1 => {
                        let t = self.table(rng)?;
                        let c = pick(rng, &t.columns)?.clone();
                        format!("UPDATE ({}) ON {}", c.name, t.name)
                    }
                    2 => format!("USAGE ON SEQUENCE {}", pick(rng, &self.sequences)?),
                    3 => format!("USAGE ON SCHEMA {}", pick(rng, &self.schemas)?),
                    4 => format!("USAGE ON TYPE {}", pick(rng, &self.enums)?.0),
                    5 => "EXECUTE ON ALL FUNCTIONS IN SCHEMA public".to_owned(),
                    _ => "ALL ON ALL TABLES IN SCHEMA public".to_owned(),
                };
                let sql = if verb_grant {
                    let option = if chance(rng, 20) {
                        " WITH GRANT OPTION"
                    } else {
                        ""
                    };
                    format!("GRANT {object} TO {grantee}{option}")
                } else {
                    format!("REVOKE {object} FROM {grantee}")
                };
                (sql, Effect::None)
            }
            32..=33 => {
                let grantee = if chance(rng, 40) {
                    "PUBLIC".to_owned()
                } else {
                    self.named("role", rng)?
                };
                let in_schema = match pick(rng, &self.schemas) {
                    Some(s) if chance(rng, 40) => format!(" IN SCHEMA {s}"),
                    _ => String::new(),
                };
                let what = *pick(
                    rng,
                    &[
                        "SELECT ON TABLES",
                        "USAGE ON SEQUENCES",
                        "EXECUTE ON FUNCTIONS",
                        "EXECUTE ON ROUTINES",
                        "USAGE ON TYPES",
                        "USAGE ON SCHEMAS",
                        "SELECT ON LARGE OBJECTS",
                        "ALL ON TABLES",
                    ],
                )
                .unwrap();
                let sql = if chance(rng, 60) {
                    format!("ALTER DEFAULT PRIVILEGES{in_schema} GRANT {what} TO {grantee}")
                } else {
                    format!("ALTER DEFAULT PRIVILEGES{in_schema} REVOKE {what} FROM {grantee}")
                };
                (sql, Effect::None)
            }
            34 => {
                let target = match rng.random_range(0..8) {
                    0 => format!("TABLE {}", self.table(rng)?.name),
                    1 => format!("TYPE {}", pick(rng, &self.enums)?.0),
                    2 => format!("DOMAIN {}", pick(rng, &self.domains)?),
                    3 => format!("SCHEMA {}", pick(rng, &self.schemas)?),
                    4 => format!("SEQUENCE {}", pick(rng, &self.sequences)?),
                    5 => format!("INDEX {}", pick(rng, &self.indexes)?),
                    6 => {
                        let (table, name) = pick(rng, &self.constraints)?.clone();
                        format!("CONSTRAINT {name} ON {table}")
                    }
                    _ => format!("VIEW {}", pick(rng, &self.views)?),
                };
                let text = if chance(rng, 20) { "NULL" } else { "'c'" };
                (format!("COMMENT ON {target} IS {text}"), Effect::None)
            }
            35..=36 => {
                // A transaction block inside the migration: the savepoint's
                // work is rolled back, the rest commits.
                let t = self.table(rng)?;
                let kept = Column {
                    name: self.fresh("a"),
                    ty: "int4".into(),
                };
                let lost = self.fresh("a");
                let sql = match rng.random_range(0..3) {
                    0 => format!(
                        "BEGIN; ALTER TABLE {t} ADD COLUMN {k} int4; SAVEPOINT s1; ALTER TABLE \
                         {t} ADD COLUMN {lost} int4; ROLLBACK TO SAVEPOINT s1; RELEASE s1; COMMIT",
                        t = t.name,
                        k = kept.name
                    ),
                    1 => format!(
                        "ALTER TABLE {t} ADD COLUMN {k} int4; COMMIT; BEGIN; ALTER TABLE {t} ADD \
                         COLUMN {lost} int4; ROLLBACK",
                        t = t.name,
                        k = kept.name
                    ),
                    _ => format!(
                        "BEGIN ISOLATION LEVEL SERIALIZABLE; ALTER TABLE {t} ADD COLUMN {k} int4; \
                         SET TRANSACTION READ ONLY; COMMIT",
                        t = t.name,
                        k = kept.name
                    ),
                };
                (sql, Effect::AddColumn(t.name, kept))
            }
            37 => {
                let d = pick(rng, &self.domains)?.clone();
                let action = match rng.random_range(0..7) {
                    0 => "ADD CONSTRAINT dk CHECK (VALUE IS NOT NULL) NOT VALID".to_owned(),
                    1 => "VALIDATE CONSTRAINT dk".to_owned(),
                    2 => "RENAME CONSTRAINT dk TO dk2".to_owned(),
                    3 => "DROP CONSTRAINT IF EXISTS dk CASCADE".to_owned(),
                    4 => "OWNER TO CURRENT_USER".to_owned(),
                    5 => match pick(rng, &self.schemas) {
                        Some(s) => {
                            let bare = d.rsplit('.').next().unwrap().to_owned();
                            return Some((
                                format!("ALTER DOMAIN {d} SET SCHEMA {s}"),
                                Effect::RenameType(d.clone(), format!("{s}.{bare}")),
                            ));
                        }
                        None => "SET DEFAULT NULL".to_owned(),
                    },
                    _ => {
                        let new = self.fresh("d");
                        let renamed = match d.split_once('.') {
                            Some((s, _)) => format!("{s}.{new}"),
                            None => new.clone(),
                        };
                        return Some((
                            format!("ALTER DOMAIN {d} RENAME TO {new}"),
                            Effect::RenameType(d.clone(), renamed),
                        ));
                    }
                };
                (format!("ALTER DOMAIN {d} {action}"), Effect::None)
            }
            38 => {
                let c = pick(rng, &self.composites)?.clone();
                let action = match rng.random_range(0..4) {
                    0 => "ALTER ATTRIBUTE f1 TYPE int8".to_owned(),
                    1 => "ALTER ATTRIBUTE f1 TYPE int8 CASCADE".to_owned(),
                    2 => format!("RENAME ATTRIBUTE f1 TO {}", self.fresh("f")),
                    _ => format!("ADD ATTRIBUTE {} text CASCADE", self.fresh("f")),
                };
                (format!("ALTER TYPE {c} {action}"), Effect::None)
            }
            39 => {
                let (e, _) = pick(rng, &self.enums)?.clone();
                let new = self.fresh("e");
                let renamed = match e.split_once('.') {
                    Some((s, _)) => format!("{s}.{new}"),
                    None => new.clone(),
                };
                (
                    format!("ALTER TYPE {e} RENAME TO {new}"),
                    Effect::RenameType(e, renamed),
                )
            }
            40 => {
                let f = pick(rng, &self.functions)?.clone();
                let action = *pick(
                    rng,
                    &[
                        "IMMUTABLE",
                        "STABLE STRICT",
                        "SET search_path = public",
                        "RESET ALL",
                        "COST 10",
                        "PARALLEL SAFE",
                        "LEAKPROOF",
                        "SECURITY DEFINER",
                        "ROWS 5",
                    ],
                )
                .unwrap();
                (format!("ALTER FUNCTION {f} {action}"), Effect::None)
            }
            41 => {
                let t = self.table(rng)?;
                let c = pick(rng, &t.columns)?.clone();
                let action = *pick(
                    rng,
                    &[
                        "SET STORAGE EXTERNAL",
                        "SET STORAGE MAIN",
                        "SET COMPRESSION pglz",
                        "SET COMPRESSION default",
                        "SET (n_distinct = -1)",
                        "RESET (n_distinct)",
                        "SET STATISTICS -1",
                    ],
                )
                .unwrap();
                (
                    format!("ALTER TABLE {} ALTER COLUMN {} {action}", t.name, c.name),
                    Effect::None,
                )
            }
            42..=43 => {
                let t = self.table(rng)?;
                let c = pick(rng, &t.columns)?.clone();
                let name = self.fresh("i");
                let key = match rng.random_range(0..6) {
                    0 => format!("({}) INCLUDE (id)", c.name),
                    1 if c.ty == "text" => format!("({} text_pattern_ops)", c.name),
                    2 => format!("({}) NULLS NOT DISTINCT", c.name),
                    3 if c.ty == "jsonb" => format!("USING gin ({})", c.name),
                    4 => format!("USING brin ({})", c.name),
                    5 => format!("USING hash ({})", c.name),
                    _ => format!("({}) WITH (fillfactor = 50)", c.name),
                };
                let unique = if key.contains("NULLS") { "UNIQUE " } else { "" };
                let schema = t.name.split_once('.').map(|(s, _)| format!("{s}."));
                (
                    format!("CREATE {unique}INDEX {name} ON {} {key}", t.name),
                    Effect::Index(format!("{}{name}", schema.unwrap_or_default())),
                )
            }
            44 => {
                let i = pick(rng, &self.indexes)?.clone();
                let action = *pick(
                    rng,
                    &[
                        "SET (fillfactor = 60)",
                        "RESET (fillfactor)",
                        "ALTER COLUMN 1 SET STATISTICS 10",
                    ],
                )
                .unwrap();
                (format!("ALTER INDEX {i} {action}"), Effect::None)
            }
            45 => {
                let t = self.table(rng)?;
                let c = pick(rng, &t.columns)?.clone();
                let index = self.fresh("i");
                let constraint = self.fresh("k");
                let kind = if chance(rng, 50) {
                    "PRIMARY KEY"
                } else {
                    "UNIQUE"
                };
                (
                    format!(
                        "CREATE UNIQUE INDEX {index} ON {t} ({c}); ALTER TABLE {t} ADD \
                         CONSTRAINT {constraint} {kind} USING INDEX {index}",
                        t = t.name,
                        c = c.name
                    ),
                    Effect::Constraint(t.name, constraint),
                )
            }
            46 => {
                let c = pick(rng, &self.composites)?.clone();
                let name = self.qualified(rng, "ty");
                if chance(rng, 50) {
                    (
                        format!("CREATE TABLE {name} OF {c} (f1 PRIMARY KEY)"),
                        Effect::None,
                    )
                } else {
                    let t = self.table(rng)?;
                    (
                        format!(
                            "ALTER TABLE {} {}",
                            t.name,
                            if chance(rng, 50) {
                                format!("OF {c}")
                            } else {
                                "NOT OF".to_owned()
                            }
                        ),
                        Effect::None,
                    )
                }
            }
            47 => {
                let t = self.table(rng)?;
                let name = self.qualified(rng, "lk");
                let including = *pick(
                    rng,
                    &[
                        "INCLUDING ALL",
                        "INCLUDING DEFAULTS INCLUDING CONSTRAINTS",
                        "INCLUDING INDEXES EXCLUDING CONSTRAINTS",
                        "INCLUDING IDENTITY INCLUDING GENERATED",
                        "",
                    ],
                )
                .unwrap();
                (
                    format!("CREATE TABLE {name} (LIKE {} {including})", t.name),
                    Effect::None,
                )
            }
            48 => {
                let name = self.fresh("co");
                let sql = match rng.random_range(0..5) {
                    0 => format!("CREATE COLLATION {name} (provider = icu, locale = 'und')"),
                    1 => {
                        format!("CREATE COLLATION {name} (provider = builtin, locale = 'C.UTF-8')")
                    }
                    2 => format!(
                        "CREATE COLLATION {name} (provider = icu, locale = 'und-u-ks-level2', \
                         deterministic = false)"
                    ),
                    3 => format!("CREATE COLLATION {name} FROM \"C\""),
                    _ => format!(
                        "CREATE TEXT SEARCH CONFIGURATION {name} (COPY = english); ALTER TEXT \
                         SEARCH CONFIGURATION {name} DROP MAPPING FOR email"
                    ),
                };
                (sql, Effect::None)
            }
            _ => {
                let t = self.table(rng)?;
                let other = self.table(rng)?;
                let behavior = *pick(rng, &["", " CASCADE", " RESTRICT"]).unwrap();
                if t.name == other.name {
                    return None;
                }
                (
                    format!("DROP TABLE {}, {}{behavior}", t.name, other.name),
                    Effect::DropTables(vec![t.name, other.name]),
                )
            }
        })
    }

    fn named(&self, kind: &str, rng: &mut StdRng) -> Option<String> {
        let names: Vec<&String> = self
            .named
            .iter()
            .filter(|(k, _)| *k == kind)
            .map(|(_, n)| n)
            .collect();
        pick(rng, &names).map(|n| (*n).clone())
    }

    fn create_table(&mut self, rng: &mut StdRng) -> (String, Effect) {
        let name = self.qualified(rng, "t");
        let n = rng.random_range(1..6);
        let mut columns = Vec::new();
        let mut defs = Vec::new();
        let partitioned = chance(rng, 15);
        let mut has_pk = false;
        let id = Column {
            name: "id".into(),
            ty: if chance(rng, 50) { "int4" } else { "int8" }.into(),
        };
        if partitioned {
            defs.push(format!("{} {} NOT NULL", id.name, id.ty));
        } else if chance(rng, 60) {
            let serial = if chance(rng, 30) {
                "serial"
            } else {
                id.ty.as_str()
            };
            defs.push(format!("{} {serial} PRIMARY KEY", id.name));
            has_pk = true;
        } else {
            defs.push(format!("{} {}", id.name, id.ty));
        }
        columns.push(id);
        for _ in 0..n {
            let column = Column {
                name: self.fresh("a"),
                ty: self.column_type(rng),
            };
            defs.push(self.column_def(rng, &column));
            columns.push(column);
        }
        if chance(rng, 15)
            && let Some(other) = pick(rng, &self.tables).filter(|t| t.has_pk)
        {
            defs.push(format!(
                "ref int8 REFERENCES {} ON DELETE CASCADE",
                other.name
            ));
            columns.push(Column {
                name: "ref".into(),
                ty: "int8".into(),
            });
        }
        let suffix = if partitioned {
            " PARTITION BY RANGE (id)"
        } else {
            ""
        };
        (
            format!("CREATE TABLE {name} ({}){suffix}", defs.join(", ")),
            Effect::Table(Table {
                name,
                columns,
                partition_key: partitioned.then(|| "id".into()),
                has_pk,
                partitions: 0,
            }),
        )
    }

    /// `ALTER SCHEMA old RENAME TO new`: every name qualified with `old`.
    fn rename_schema(&mut self, old: &str, new: &str) {
        let prefix = format!("{old}.");
        let fix = |name: &mut String| {
            if let Some(rest) = name.strip_prefix(&prefix) {
                *name = format!("{new}.{rest}");
            }
        };
        for s in &mut self.schemas {
            if s == old {
                *s = new.to_owned();
            }
        }
        for t in &mut self.tables {
            fix(&mut t.name);
        }
        for e in &mut self.enums {
            fix(&mut e.0);
        }
        for name in self
            .composites
            .iter_mut()
            .chain(&mut self.domains)
            .chain(&mut self.sequences)
            .chain(&mut self.views)
            .chain(&mut self.functions)
            .chain(&mut self.indexes)
        {
            fix(name);
        }
        for (a, b) in self
            .constraints
            .iter_mut()
            .chain(&mut self.temporals)
            .chain(&mut self.partitions_of)
            .chain(&mut self.policies)
        {
            fix(a);
            fix(b);
        }
        for (_, name) in &mut self.named {
            fix(name);
        }
    }

    fn apply(&mut self, effect: Effect) {
        let table = |tables: &mut Vec<Table>, name: &str| -> Option<usize> {
            tables.iter().position(|t| t.name == name)
        };
        match effect {
            Effect::None => {}
            Effect::Schema(s) => self.schemas.push(s),
            Effect::Table(t) => self.tables.push(t),
            Effect::DropTable(name) => self.tables.retain(|t| t.name != name),
            Effect::RenameTable(old, new) => {
                if let Some(i) = table(&mut self.tables, &old) {
                    self.tables[i].name = new;
                }
            }
            Effect::AddColumn(t, c) => {
                if let Some(i) = table(&mut self.tables, &t) {
                    self.tables[i].columns.push(c);
                }
            }
            Effect::DropColumn(t, c) => {
                if let Some(i) = table(&mut self.tables, &t) {
                    self.tables[i].columns.retain(|col| col.name != c);
                }
            }
            Effect::RenameColumn(t, old, new) => {
                if let Some(i) = table(&mut self.tables, &t) {
                    for col in &mut self.tables[i].columns {
                        if col.name == old {
                            col.name = new.clone();
                        }
                    }
                }
            }
            Effect::ColumnType(t, c, ty) => {
                if let Some(i) = table(&mut self.tables, &t) {
                    for col in &mut self.tables[i].columns {
                        if col.name == c {
                            col.ty = ty.clone();
                        }
                    }
                }
            }
            Effect::Pk(t) => {
                if let Some(i) = table(&mut self.tables, &t) {
                    self.tables[i].has_pk = true;
                }
            }
            Effect::Partition(t) => {
                if let Some(i) = table(&mut self.tables, &t) {
                    self.tables[i].partitions += 1;
                }
            }
            Effect::NewPartition(parent, child) => {
                if let Some(i) = table(&mut self.tables, &parent) {
                    self.tables[i].partitions += 1;
                }
                self.partitions_of.push((parent, child.name.clone()));
                self.tables.push(child);
            }
            Effect::Constraint(t, c) => self.constraints.push((t, c)),
            Effect::Temporal(name, range) => {
                self.temporals.push((name.clone(), range.clone()));
                self.tables.push(Table {
                    name,
                    columns: vec![
                        Column {
                            name: "id".into(),
                            ty: "int4".into(),
                        },
                        Column {
                            name: "valid_at".into(),
                            ty: range,
                        },
                    ],
                    partition_key: None,
                    has_pk: false,
                    partitions: 0,
                });
            }
            Effect::Detach(parent, child) => self
                .partitions_of
                .retain(|(p, c)| !(*p == parent && *c == child)),
            Effect::RenameSequence(old, new) => {
                for s in &mut self.sequences {
                    if *s == old {
                        s.clone_from(&new);
                    }
                }
            }
            Effect::RenameSchema(old, new) => self.rename_schema(&old, &new),
            Effect::RenameType(old, new) => {
                for e in &mut self.enums {
                    if e.0 == old {
                        e.0.clone_from(&new);
                    }
                }
                for name in self.composites.iter_mut().chain(&mut self.domains) {
                    if *name == old {
                        name.clone_from(&new);
                    }
                }
            }
            Effect::Named(kind, name) => self.named.push((kind, name)),
            Effect::DropNamed(kind, name) => {
                self.named.retain(|(k, n)| !(*k == kind && *n == name));
            }
            Effect::Policy(t, p) => self.policies.push((t, p)),
            Effect::DropTables(names) => self.tables.retain(|t| !names.contains(&t.name)),
            Effect::Enum(e, labels) => self.enums.push((e, labels)),
            Effect::EnumLabel(e, l) => {
                if let Some(entry) = self.enums.iter_mut().find(|(n, _)| *n == e) {
                    entry.1.push(l);
                }
            }
            Effect::Composite(c) => self.composites.push(c),
            Effect::Domain(d) => self.domains.push(d),
            Effect::DropType(t) => {
                self.enums.retain(|(e, _)| *e != t);
                self.composites.retain(|c| *c != t);
                self.domains.retain(|d| *d != t);
            }
            Effect::Sequence(s) => self.sequences.push(s),
            Effect::DropSequence(s) => self.sequences.retain(|x| *x != s),
            Effect::View(v) => self.views.push(v),
            Effect::DropView(v) => self.views.retain(|x| *x != v),
            Effect::Function(f) => self.functions.push(f),
            Effect::DropFunction(f) => self.functions.retain(|x| *x != f),
            Effect::Index(i) => self.indexes.push(i),
            Effect::DropIndex(i) => self.indexes.retain(|x| *x != i),
            Effect::RenameIndex(old, new) => {
                for i in &mut self.indexes {
                    if *i == old {
                        *i = new.clone();
                    }
                }
            }
        }
    }
}
