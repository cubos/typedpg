//! The lint oracle's catalog-and-query generator: a random schema per case
//! (enums, domains with CHECKs, keys of every kind — composite, nullable,
//! `NULLS NOT DISTINCT`, deferrable, partial and expression unique indexes —
//! `NOT VALID` and `NOT ENFORCED` checks, generated columns, foreign keys,
//! partitioned and inherited tables, views, row security), and queries over
//! it in the shapes real code uses: joins of every kind (`USING`,
//! `NATURAL`, `LATERAL`), CTEs (recursive, materialized, data-modifying),
//! set operations, `DISTINCT [ON]`, grouping sets, window functions and
//! `FILTER`, correlated subqueries, `CASE`, `COALESCE` / `NULLIF` /
//! `GREATEST`, `IN` / `ANY` / `ALL` over lists, arrays and subqueries,
//! casts and collations; `UPDATE … FROM`, `DELETE … USING`,
//! `INSERT … SELECT`, `ON CONFLICT`, `MERGE` and `RETURNING old/new`.
//!
//! Everything is deterministic per seed: [`case`] builds case `index` of a
//! run seeded with `seed` alone, so one case reproduces without the rest.

use std::fmt::Write as _;

/// A splitmix64 generator: deterministic per seed, no dependency.
pub struct Rng(pub u64);

impl Rng {
    pub fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
    pub fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
    pub fn chance(&mut self, percent: usize) -> bool {
        self.below(100) < percent
    }
    pub fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }
}

/// A generated case: the schema's statements and the queries over it.
pub struct GenCase {
    pub ddl: Vec<String>,
    pub queries: Vec<String>,
}

/// Case `index` of the run seeded with `seed`: a schema and `queries`
/// queries over it.
pub fn case(seed: u64, index: u64, queries: usize) -> GenCase {
    let rng = Rng(seed ^ index.wrapping_mul(0xa076_1d64_78bd_642f) ^ 0x1d8e_4e27_c47d_124f);
    let mut g = Gen {
        rng,
        schema: Schema::default(),
        alias: 0,
        no_params: false,
    };
    g.schema();
    let queries = (0..queries).map(|_| g.statement()).collect();
    GenCase {
        ddl: g.schema.ddl.clone(),
        queries,
    }
}

// ── types ──────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Base {
    Int,
    Big,
    Num,
    Text,
    Bool,
    Date,
    Ts,
    Enum(usize),
}

/// What compares with what without a cast.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Class {
    Number,
    Text,
    Bool,
    Date,
    Ts,
    Enum(usize),
}

impl Base {
    fn class(self) -> Class {
        match self {
            Base::Int | Base::Big | Base::Num => Class::Number,
            Base::Text => Class::Text,
            Base::Bool => Class::Bool,
            Base::Date => Class::Date,
            Base::Ts => Class::Ts,
            Base::Enum(k) => Class::Enum(k),
        }
    }
    fn sql(self) -> String {
        match self {
            Base::Int => "integer".into(),
            Base::Big => "bigint".into(),
            Base::Num => "numeric".into(),
            Base::Text => "text".into(),
            Base::Bool => "boolean".into(),
            Base::Date => "date".into(),
            Base::Ts => "timestamptz".into(),
            Base::Enum(k) => format!("e{k}"),
        }
    }
    fn orderable(self) -> bool {
        self != Base::Bool
    }
}

/// A column's type: its base, and the name it is declared with (a domain's).
#[derive(Clone, PartialEq, Eq, Debug)]
struct Ty {
    base: Base,
    name: String,
    /// The domain's literals, for a domain with a CHECK.
    domain: Option<usize>,
}

impl Ty {
    fn of(base: Base) -> Ty {
        Ty {
            base,
            name: base.sql(),
            domain: None,
        }
    }
}

#[derive(Clone, Debug)]
struct Col {
    name: String,
    ty: Ty,
    nullable: bool,
}

const ENUM_LABELS: &[&[&str]] = &[&["red", "green", "blue"], &["low", "mid", "high", "top"]];

/// Domains: name, base, definition tail, literals that are (mostly) valid.
const DOMAINS: &[(&str, Base, &str, &[&str])] = &[
    ("d_pos", Base::Int, "CHECK (VALUE > 0)", &["1", "2", "5"]),
    (
        "d_code",
        Base::Text,
        "CHECK (VALUE IN ('x', 'y', 'z'))",
        &["'x'", "'y'", "'w'"],
    ),
    (
        "d_small",
        Base::Int,
        "NOT NULL CHECK (VALUE BETWEEN 0 AND 10)",
        &["0", "3", "10"],
    ),
    (
        "d_pct",
        Base::Num,
        "CHECK (VALUE >= 0 AND VALUE <= 100)",
        &["0", "50", "100.5"],
    ),
    ("d_name", Base::Text, "", &["'a'", "'b'"]),
];

// ── schema ─────────────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
struct Rel {
    name: String,
    cols: Vec<Col>,
    /// `ONLY` applies (an inheritance parent).
    parent: bool,
    /// The DML generators may write it, and how.
    write: Option<Write>,
}

#[derive(Clone, Debug)]
struct Write {
    /// The columns an INSERT or UPDATE may set.
    settable: Vec<usize>,
    /// `ON CONFLICT` targets: `(a, b)`, `(lower(x))`, `(c) WHERE p`,
    /// `ON CONSTRAINT k`.
    conflict: Vec<String>,
    /// A plain table (not a view).
    table: bool,
}

#[derive(Default)]
struct Schema {
    ddl: Vec<String>,
    enums: usize,
    domains: Vec<usize>,
    rels: Vec<Rel>,
    /// (table, column, referenced table, referenced column).
    fks: Vec<(String, String, String, String)>,
    /// (table, parent column, key column): a self-reference.
    trees: Vec<(String, String, String)>,
    /// Partitioned tables (no table inherits from one).
    partitioned: Vec<String>,
}

/// Shared column names, so `USING` and `NATURAL` find common columns.
const NAMES: &[(&str, Base)] = &[
    ("code", Base::Text),
    ("kind", Base::Text),
    ("qty", Base::Int),
    ("name", Base::Text),
    ("flag", Base::Bool),
    ("amount", Base::Num),
    ("created", Base::Date),
    ("seen", Base::Ts),
    ("note", Base::Text),
    ("score", Base::Big),
    ("lvl", Base::Int),
];

struct Gen {
    rng: Rng,
    schema: Schema,
    alias: usize,
    /// Writing DDL: literals instead of parameters.
    no_params: bool,
}

impl Gen {
    fn fresh(&mut self, prefix: &str) -> String {
        self.alias += 1;
        format!("{prefix}{}", self.alias)
    }

    fn schema(&mut self) {
        let enums = self.rng.below(3);
        for (k, labels) in ENUM_LABELS.iter().enumerate().take(enums) {
            let labels: Vec<String> = labels.iter().map(|l| format!("'{l}'")).collect();
            self.schema
                .ddl
                .push(format!("CREATE TYPE e{k} AS ENUM ({})", labels.join(", ")));
        }
        self.schema.enums = enums;
        for (i, (name, base, tail, _)) in DOMAINS.iter().enumerate() {
            if self.rng.chance(45) {
                self.schema.domains.push(i);
                self.schema
                    .ddl
                    .push(format!("CREATE DOMAIN {name} AS {} {tail}", base.sql()));
            }
        }
        let tables = 2 + self.rng.below(4);
        for i in 0..tables {
            match self.rng.below(10) {
                0 => self.partitioned(i),
                1 if !self.schema.rels.is_empty() => self.inherited(i),
                _ => self.table(i),
            }
        }
        for i in 0..self.rng.below(3) {
            self.view(i);
        }
    }

    fn column_type(&mut self, default: Base) -> Ty {
        if self.rng.chance(12) && !self.schema.domains.is_empty() {
            let d = *self.rng.pick(&self.schema.domains);
            let (name, base, _, _) = DOMAINS[d];
            return Ty {
                base,
                name: name.into(),
                domain: Some(d),
            };
        }
        if self.rng.chance(12) && self.schema.enums > 0 {
            return Ty::of(Base::Enum(self.rng.below(self.schema.enums)));
        }
        if self.rng.chance(80) {
            Ty::of(default)
        } else {
            Ty::of(
                *self
                    .rng
                    .pick(&[Base::Int, Base::Text, Base::Big, Base::Num]),
            )
        }
    }

    /// A CHECK over `cols` (indices into it), or None.
    fn check_for(&mut self, cols: &[Col]) -> Option<String> {
        let c = self.rng.pick(cols).clone();
        Some(match c.ty.base {
            Base::Int | Base::Big | Base::Num => match self.rng.below(3) {
                0 => format!("{} >= 0", c.name),
                1 => format!("{} BETWEEN -5 AND 1000", c.name),
                _ => {
                    let other = cols
                        .iter()
                        .find(|o| o.name != c.name && o.ty.base.class() == Class::Number)?;
                    format!("{} <= {} OR {} IS NULL", c.name, other.name, other.name)
                }
            },
            Base::Text => match self.rng.below(3) {
                0 => format!("{} IN ('a', 'b', 'x')", c.name),
                1 => format!("{} <> ''", c.name),
                _ => format!("length({}) < 50", c.name),
            },
            Base::Bool => {
                let other = cols.iter().find(|o| o.name != c.name && o.nullable)?;
                format!("NOT {} OR {} IS NOT NULL", c.name, other.name)
            }
            Base::Enum(k) => format!("{} <> '{}'", c.name, ENUM_LABELS[k][0]),
            _ => {
                let nullable: Vec<&Col> = cols.iter().filter(|o| o.nullable).collect();
                if nullable.len() < 2 {
                    return None;
                }
                format!(
                    "num_nonnulls({}, {}) >= 1",
                    nullable[0].name, nullable[1].name
                )
            }
        })
    }

    fn plain_columns(&mut self, n: usize) -> Vec<Col> {
        let mut cols: Vec<Col> = Vec::new();
        for _ in 0..n {
            let (name, base) = *self.rng.pick(NAMES);
            if cols.iter().any(|c| c.name == name) {
                continue;
            }
            let ty = self.column_type(base);
            let nullable = ty
                .domain
                .is_none_or(|d| !DOMAINS[d].2.starts_with("NOT NULL"))
                && self.rng.chance(55);
            cols.push(Col {
                name: name.into(),
                ty,
                nullable,
            });
        }
        cols
    }

    fn column_ddl(c: &Col) -> String {
        format!(
            "{} {}{}",
            c.name,
            c.ty.name,
            if c.nullable { "" } else { " NOT NULL" }
        )
    }

    fn table(&mut self, i: usize) {
        let name = format!("t{i}");
        let mut defs: Vec<String> = Vec::new();
        let mut cols: Vec<Col> = Vec::new();
        let mut settable: Vec<usize> = Vec::new();
        let mut conflict: Vec<String> = Vec::new();
        let mut uniques: Vec<String> = Vec::new();
        let mut post: Vec<String> = Vec::new();
        // The key.
        let key_kind = self.rng.below(10);
        let id_base = if self.rng.chance(50) {
            Base::Int
        } else {
            Base::Big
        };
        let has_id = key_kind < 8;
        if has_id {
            let identity = self.rng.chance(30);
            let always = identity && self.rng.chance(50);
            let mut def = format!("id {}", id_base.sql());
            if identity {
                def.push_str(if always {
                    " GENERATED ALWAYS AS IDENTITY"
                } else {
                    " GENERATED BY DEFAULT AS IDENTITY"
                });
            }
            if key_kind < 6 {
                def.push_str(" PRIMARY KEY");
                conflict.push("(id)".into());
            } else if key_kind == 6 {
                def.push_str(" NOT NULL UNIQUE");
                conflict.push("(id)".into());
            }
            defs.push(def);
            if !always {
                settable.push(cols.len());
            }
            cols.push(Col {
                name: "id".into(),
                ty: Ty::of(id_base),
                nullable: key_kind == 7,
            });
        }
        // Foreign keys to earlier tables with a single-column key.
        let targets: Vec<(String, Ty)> = self
            .schema
            .rels
            .iter()
            .filter(|r| {
                r.write
                    .as_ref()
                    .is_some_and(|w| w.table && w.conflict.contains(&"(id)".to_owned()))
            })
            .filter_map(|r| {
                let id = r.cols.iter().find(|c| c.name == "id")?;
                Some((r.name.clone(), id.ty.clone()))
            })
            .collect();
        for _ in 0..self.rng.below(3) {
            if targets.is_empty() {
                break;
            }
            let (target, ty) = self.rng.pick(&targets).clone();
            let col = format!("{target}_id");
            if cols.iter().any(|c| c.name == col) {
                continue;
            }
            let nullable = self.rng.chance(50);
            let mut def = format!(
                "{col} {}{} REFERENCES {target} (id)",
                ty.name,
                if nullable { "" } else { " NOT NULL" }
            );
            match self.rng.below(6) {
                0 => def.push_str(" ON DELETE CASCADE"),
                1 => def.push_str(" DEFERRABLE INITIALLY DEFERRED"),
                2 => def.push_str(" ON DELETE SET NULL"),
                _ => {}
            }
            defs.push(def);
            settable.push(cols.len());
            cols.push(Col {
                name: col.clone(),
                ty,
                nullable,
            });
            self.schema
                .fks
                .push((name.clone(), col, target, "id".into()));
        }
        // A self-reference.
        if has_id && key_kind < 6 && self.rng.chance(20) {
            defs.push(format!("parent {} REFERENCES {name}", id_base.sql()));
            settable.push(cols.len());
            cols.push(Col {
                name: "parent".into(),
                ty: Ty::of(id_base),
                nullable: true,
            });
            self.schema
                .trees
                .push((name.clone(), "parent".into(), "id".into()));
        }
        let n = 2 + self.rng.below(5);
        for c in self.plain_columns(n) {
            let mut def = Self::column_ddl(&c);
            if self.rng.chance(15) {
                def.push_str(&format!(" DEFAULT {}", self.literal(&c.ty)));
            }
            defs.push(def);
            settable.push(cols.len());
            cols.push(c);
        }
        // A generated column.
        if self.rng.chance(25)
            && let Some(src) = cols
                .iter()
                .find(|c| matches!(c.ty.base, Base::Int | Base::Num) && c.ty.domain.is_none())
                .cloned()
        {
            let stored = if self.rng.chance(50) {
                " STORED"
            } else {
                " VIRTUAL"
            };
            defs.push(format!(
                "gen {} GENERATED ALWAYS AS ({} * 2){stored}",
                src.ty.base.sql(),
                src.name
            ));
            cols.push(Col {
                name: "gen".into(),
                ty: Ty::of(src.ty.base),
                nullable: src.nullable,
            });
        }
        // Composite and nullable keys.
        let plain: Vec<Col> = cols
            .iter()
            .filter(|c| c.name != "id" && c.name != "gen" && c.ty.base != Base::Bool)
            .cloned()
            .collect();
        if key_kind >= 8 && plain.len() >= 2 {
            let (a, b) = (&plain[0].name, &plain[1].name);
            if plain[0].nullable || plain[1].nullable {
                uniques.push(format!("UNIQUE ({a}, {b})"));
            } else {
                uniques.push(format!("PRIMARY KEY ({a}, {b})"));
            }
            conflict.push(format!("({a}, {b})"));
        }
        if !plain.is_empty() && self.rng.chance(40) {
            let c = self.rng.pick(&plain).name.clone();
            match self.rng.below(4) {
                0 => {
                    uniques.push(format!("UNIQUE NULLS NOT DISTINCT ({c})"));
                    conflict.push(format!("({c})"));
                }
                1 => {
                    // An arbiter ExecCheckIndexConstraints refuses.
                    uniques.push(format!("CONSTRAINT {name}_{c}_d UNIQUE ({c}) DEFERRABLE"));
                    conflict.push(format!("ON CONSTRAINT {name}_{c}_d"));
                }
                2 => {
                    uniques.push(format!("CONSTRAINT {name}_{c}_k UNIQUE ({c})"));
                    conflict.push(format!("ON CONSTRAINT {name}_{c}_k"));
                }
                _ => {
                    uniques.push(format!("UNIQUE ({c})"));
                    conflict.push(format!("({c})"));
                }
            }
        }
        // Checks: plain, NOT VALID, NOT ENFORCED.
        for _ in 0..self.rng.below(3) {
            let Some(check) = self.check_for(&cols.clone()) else {
                continue;
            };
            match self.rng.below(5) {
                0 => post.push(format!(
                    "ALTER TABLE {name} ADD CONSTRAINT {} CHECK ({check}) NOT VALID",
                    self.fresh(&format!("{name}_nv"))
                )),
                1 => defs.push(format!("CHECK ({check}) NOT ENFORCED")),
                _ => defs.push(format!("CHECK ({check})")),
            }
        }
        // Partial and expression unique indexes.
        if !plain.is_empty() && self.rng.chance(35) {
            let c = self.rng.pick(&plain).clone();
            let flag = cols.iter().find(|c| c.ty.base == Base::Bool).cloned();
            match (self.rng.below(3), flag) {
                (0, Some(flag)) => {
                    post.push(format!(
                        "CREATE UNIQUE INDEX ON {name} ({}) WHERE {}",
                        c.name, flag.name
                    ));
                    conflict.push(format!("({}) WHERE {}", c.name, flag.name));
                }
                (1, _) if c.ty.base == Base::Text && c.ty.domain.is_none() => {
                    post.push(format!("CREATE UNIQUE INDEX ON {name} (lower({}))", c.name));
                    conflict.push(format!("(lower({}))", c.name));
                }
                _ => post.push(format!(
                    "CREATE UNIQUE INDEX ON {name} ({}) WHERE {} IS NOT NULL",
                    c.name, c.name
                )),
            }
        }
        defs.extend(uniques);
        self.schema.ddl.push(format!(
            "CREATE TABLE {name} (\n    {}\n)",
            defs.join(",\n    ")
        ));
        self.schema.ddl.extend(post);
        // Row security.
        if self.rng.chance(12) {
            let pred = self.rls_predicate(&cols);
            self.schema
                .ddl
                .push(format!("ALTER TABLE {name} ENABLE ROW LEVEL SECURITY"));
            if self.rng.chance(50) {
                self.schema
                    .ddl
                    .push(format!("ALTER TABLE {name} FORCE ROW LEVEL SECURITY"));
            }
            self.schema
                .ddl
                .push(format!("CREATE POLICY {name}_p ON {name} USING ({pred})"));
        }
        self.schema.rels.push(Rel {
            name,
            cols,
            parent: false,
            write: Some(Write {
                settable,
                conflict,
                table: true,
            }),
        });
    }

    fn rls_predicate(&mut self, cols: &[Col]) -> String {
        match cols.iter().find(|c| c.ty.base.class() == Class::Number) {
            Some(c) => format!("{} IS NULL OR {} > 0", c.name, c.name),
            None => "true".into(),
        }
    }

    fn partitioned(&mut self, i: usize) {
        let name = format!("t{i}");
        let by_list = self.rng.chance(50);
        self.schema.partitioned.push(name.clone());
        let mut cols = vec![
            Col {
                name: "id".into(),
                ty: Ty::of(Base::Int),
                nullable: false,
            },
            Col {
                name: "kind".into(),
                ty: Ty::of(Base::Text),
                nullable: false,
            },
        ];
        for c in self.plain_columns(3) {
            if c.name != "kind" {
                cols.push(c);
            }
        }
        let defs: Vec<String> = cols.iter().map(Self::column_ddl).collect();
        let key = if by_list { "kind" } else { "id" };
        let pk = self.rng.chance(70);
        self.schema.ddl.push(format!(
            "CREATE TABLE {name} (\n    {}{}\n) PARTITION BY {} ({key})",
            defs.join(",\n    "),
            if pk {
                ",\n    PRIMARY KEY (id, kind)"
            } else {
                ""
            },
            if by_list { "LIST" } else { "RANGE" }
        ));
        let parts: Vec<String> = if by_list {
            vec![
                "FOR VALUES IN ('a')".into(),
                "FOR VALUES IN ('b', 'x')".into(),
                "DEFAULT".into(),
            ]
        } else {
            vec![
                "FOR VALUES FROM (MINVALUE) TO (5)".into(),
                "FOR VALUES FROM (5) TO (MAXVALUE)".into(),
            ]
        };
        let n = if self.rng.chance(30) {
            parts.len() - 1
        } else {
            parts.len()
        };
        let settable: Vec<usize> = (0..cols.len()).collect();
        for (j, bound) in parts.iter().take(n).enumerate() {
            let part = format!("{name}_{j}");
            self.schema
                .ddl
                .push(format!("CREATE TABLE {part} PARTITION OF {name} {bound}"));
            self.schema.rels.push(Rel {
                name: part,
                cols: cols.clone(),
                parent: false,
                write: Some(Write {
                    settable: settable.clone(),
                    conflict: if pk {
                        vec!["(id, kind)".into()]
                    } else {
                        vec![]
                    },
                    table: true,
                }),
            });
        }
        self.schema.rels.push(Rel {
            name,
            cols,
            parent: false,
            write: Some(Write {
                settable,
                conflict: if pk {
                    vec!["(id, kind)".into()]
                } else {
                    vec![]
                },
                table: true,
            }),
        });
    }

    fn inherited(&mut self, i: usize) {
        let parents: Vec<Rel> = self
            .schema
            .rels
            .iter()
            .filter(|r| {
                r.write.as_ref().is_some_and(|w| w.table)
                    && !r.name.contains('_')
                    && !self.schema.partitioned.contains(&r.name)
                    && !r.cols.iter().any(|c| c.name == "extra")
            })
            .cloned()
            .collect();
        if parents.is_empty() {
            return self.table(i);
        }
        let parent = self.rng.pick(&parents).clone();
        let name = format!("t{i}");
        let mut cols = parent.cols.clone();
        let extra = Col {
            name: "extra".into(),
            ty: Ty::of(Base::Text),
            nullable: true,
        };
        self.schema.ddl.push(format!(
            "CREATE TABLE {name} (extra text{}) INHERITS ({})",
            if self.rng.chance(40) {
                ", CHECK (extra <> '')"
            } else {
                ""
            },
            parent.name
        ));
        cols.push(extra);
        let mut settable = parent.write.as_ref().unwrap().settable.clone();
        settable.push(cols.len() - 1);
        if let Some(p) = self.schema.rels.iter_mut().find(|r| r.name == parent.name) {
            p.parent = true;
        }
        self.schema.rels.push(Rel {
            name,
            cols,
            parent: false,
            write: Some(Write {
                settable,
                conflict: vec![],
                table: true,
            }),
        });
    }

    fn view(&mut self, i: usize) {
        // A view stores no parameters.
        self.no_params = true;
        self.view_body(i);
        self.no_params = false;
    }

    fn view_body(&mut self, i: usize) {
        let name = format!("v{i}");
        let invoker = if self.rng.chance(40) {
            " WITH (security_invoker = true)"
        } else {
            ""
        };
        let tables: Vec<Rel> = self
            .schema
            .rels
            .iter()
            .filter(|r| r.write.as_ref().is_some_and(|w| w.table))
            .cloned()
            .collect();
        let base = self.rng.pick(&tables).clone();
        let a = self.fresh("b");
        let (sql, cols, write) = match self.rng.below(4) {
            // A simple, updatable view.
            0 | 1 => {
                let keep: Vec<usize> = (0..base.cols.len())
                    .filter(|_| self.rng.chance(75))
                    .collect();
                let keep = if keep.is_empty() { vec![0] } else { keep };
                let scope = Scope::of(&a, &base.cols);
                let cond = if self.rng.chance(60) {
                    format!(" WHERE {}", self.pred(&scope, 2))
                } else {
                    String::new()
                };
                let list: Vec<String> = keep
                    .iter()
                    .map(|&k| format!("{a}.{}", base.cols[k].name))
                    .collect();
                let cols: Vec<Col> = keep.iter().map(|&k| base.cols[k].clone()).collect();
                let settable: Vec<usize> = keep
                    .iter()
                    .enumerate()
                    .filter(|(_, k)| base.write.as_ref().unwrap().settable.contains(k))
                    .map(|(j, _)| j)
                    .collect();
                (
                    format!("SELECT {} FROM {} {a}{cond}", list.join(", "), base.name),
                    cols,
                    Some(Write {
                        settable,
                        conflict: vec![],
                        table: false,
                    }),
                )
            }
            _ => {
                let sel = self.select(&Scope::default(), None, 2, true);
                (sel.sql, sel.cols, None)
            }
        };
        self.schema
            .ddl
            .push(format!("CREATE VIEW {name}{invoker} AS {sql}"));
        self.schema.rels.push(Rel {
            name,
            cols,
            parent: false,
            write,
        });
    }
}

// ── scopes and expressions ─────────────────────────────────────────────────

/// The relations a query level sees: its own FROM items, and (for
/// correlation) the levels around it.
#[derive(Clone, Default)]
struct Scope {
    items: Vec<(String, Vec<Col>)>,
    outer: Vec<(String, Vec<Col>)>,
}

impl Scope {
    fn of(alias: &str, cols: &[Col]) -> Scope {
        Scope {
            items: vec![(alias.to_owned(), cols.to_vec())],
            outer: Vec::new(),
        }
    }
    fn local(&self) -> Scope {
        Scope {
            items: self.items.clone(),
            outer: Vec::new(),
        }
    }
    fn all(&self) -> impl Iterator<Item = (&String, &Col, bool)> {
        self.items
            .iter()
            .flat_map(|(a, cols)| cols.iter().map(move |c| (a, c, false)))
            .chain(
                self.outer
                    .iter()
                    .flat_map(|(a, cols)| cols.iter().map(move |c| (a, c, true))),
            )
    }
}

/// A SELECT and its output columns.
struct Sel {
    sql: String,
    cols: Vec<Col>,
}

impl Gen {
    fn literal(&mut self, ty: &Ty) -> String {
        if let Some(d) = ty.domain
            && self.rng.chance(70)
        {
            return self.rng.pick(DOMAINS[d].3).to_string();
        }
        self.base_literal(ty.base, true)
    }

    /// A literal of `base`; `typed` spells the type out where an unknown
    /// literal would not resolve to it.
    fn base_literal(&mut self, base: Base, typed: bool) -> String {
        match base {
            Base::Int | Base::Big => self
                .rng
                .pick(&["0", "1", "2", "-1", "5", "100", "3"])
                .to_string(),
            Base::Num => self.rng.pick(&["0", "1.5", "-2", "100", "0.5"]).to_string(),
            Base::Text => self
                .rng
                .pick(&["'a'", "'b'", "''", "'x'", "'y'", "'red'"])
                .to_string(),
            Base::Bool => self.rng.pick(&["true", "false"]).to_string(),
            Base::Date => {
                let v = self
                    .rng
                    .pick(&["'2020-01-01'", "'1999-12-31'", "'infinity'"])
                    .to_string();
                if typed { format!("{v}::date") } else { v }
            }
            Base::Ts => {
                let v = self
                    .rng
                    .pick(&["'2020-01-01 00:00:00+00'", "'2021-06-01 12:00:00+00'"])
                    .to_string();
                if typed {
                    format!("{v}::timestamptz")
                } else {
                    v
                }
            }
            Base::Enum(k) => {
                let v = format!("'{}'", self.rng.pick(ENUM_LABELS[k]));
                if typed { format!("{v}::e{k}") } else { v }
            }
        }
    }

    fn param(&mut self, base: Base) -> String {
        if self.no_params {
            return self.base_literal(base, true);
        }
        let prefix = match base {
            Base::Int => "i".to_owned(),
            Base::Big => "g".to_owned(),
            Base::Num => "n".to_owned(),
            Base::Text => "s".to_owned(),
            Base::Bool => "b".to_owned(),
            Base::Date => "d".to_owned(),
            Base::Ts => "ts".to_owned(),
            Base::Enum(k) => format!("e{k}_"),
        };
        format!("${prefix}{}::{}", 1 + self.rng.below(2), base.sql())
    }

    fn value(&mut self, ty: &Ty) -> String {
        match self.rng.below(10) {
            0 | 1 => self.param(ty.base),
            2 => format!("NULL::{}", ty.base.sql()),
            _ => self.literal(ty),
        }
    }

    /// A column of `class` in scope (an outer one with `outer` percent).
    fn column_of(&mut self, sc: &Scope, class: Class, outer: usize) -> Option<(String, Ty)> {
        let cands: Vec<(String, Ty, bool)> = sc
            .all()
            .filter(|(_, c, _)| c.ty.base.class() == class)
            .map(|(a, c, o)| (format!("{a}.{}", c.name), c.ty.clone(), o))
            .collect();
        let want_outer = self.rng.chance(outer);
        let preferred: Vec<&(String, Ty, bool)> =
            cands.iter().filter(|c| c.2 == want_outer).collect();
        let pool: Vec<&(String, Ty, bool)> = if preferred.is_empty() {
            cands.iter().collect()
        } else {
            preferred
        };
        if pool.is_empty() {
            return None;
        }
        let (c, t, _) = *self.rng.pick(&pool);
        Some((c.clone(), t.clone()))
    }

    /// Any column in scope.
    fn any_column(&mut self, sc: &Scope) -> Option<(String, Ty)> {
        let cands: Vec<(String, Ty)> = sc
            .all()
            .filter(|(_, _, o)| !o)
            .map(|(a, c, _)| (format!("{a}.{}", c.name), c.ty.clone()))
            .collect();
        if cands.is_empty() {
            None
        } else {
            Some(self.rng.pick(&cands).clone())
        }
    }

    /// A random type in scope (a column's), or a plain one.
    fn some_type(&mut self, sc: &Scope) -> Ty {
        match self.any_column(sc) {
            Some((_, t)) if self.rng.chance(85) => t,
            _ => Ty::of(
                *self
                    .rng
                    .pick(&[Base::Int, Base::Text, Base::Num, Base::Bool]),
            ),
        }
    }

    /// An expression of `ty`'s base type.
    fn expr(&mut self, sc: &Scope, ty: &Ty, depth: u32) -> String {
        let base = ty.base;
        let class = base.class();
        let leaf = depth >= 3;
        let n = if leaf { 6 } else { 22 };
        match self.rng.below(n) {
            0..=3 => self
                .column_of(sc, class, 15)
                .map(|(c, _)| c)
                .unwrap_or_else(|| self.value(ty)),
            4 | 5 => self.value(ty),
            6 => {
                let a = self.expr(sc, ty, depth + 1);
                let b = self.expr(sc, ty, depth + 1);
                if self.rng.chance(50) {
                    format!("COALESCE({a}, {b})")
                } else {
                    format!("COALESCE({a}, {b}, {})", self.literal(ty))
                }
            }
            7 => format!(
                "NULLIF({}, {})",
                self.expr(sc, ty, depth + 1),
                self.value(ty)
            ),
            8 if base.orderable() => format!(
                "{}({}, {})",
                self.rng.pick(&["GREATEST", "LEAST"]),
                self.expr(sc, ty, depth + 1),
                self.expr(sc, ty, depth + 1)
            ),
            9 | 10 => {
                let mut s = String::from("CASE");
                for _ in 0..1 + self.rng.below(2) {
                    let p = self.pred(sc, depth + 1);
                    let _ = write!(s, " WHEN {p} THEN {}", self.expr(sc, ty, depth + 1));
                }
                if self.rng.chance(60) {
                    let _ = write!(s, " ELSE {}", self.expr(sc, ty, depth + 1));
                }
                s.push_str(" END");
                s
            }
            11 => {
                let probe = self.some_type(sc);
                let subject = self.expr(sc, &probe, depth + 1);
                let mut s = format!("CASE {subject}");
                for _ in 0..1 + self.rng.below(3) {
                    let w = if self.rng.chance(10) {
                        "NULL".to_owned()
                    } else {
                        self.base_literal(probe.base, false)
                    };
                    let _ = write!(s, " WHEN {w} THEN {}", self.expr(sc, ty, depth + 1));
                }
                if self.rng.chance(50) {
                    let _ = write!(s, " ELSE {}", self.expr(sc, ty, depth + 1));
                }
                s.push_str(" END");
                s
            }
            12 | 13 => self.scalar_subquery(sc, ty, depth),
            14 => match base {
                Base::Int | Base::Big | Base::Num => {
                    let e = self.expr(sc, ty, depth + 1);
                    match self.rng.below(4) {
                        0 => format!("{e} + {}", self.base_literal(base, false)),
                        1 => format!("({e}) * 2"),
                        2 => format!("abs({e})"),
                        _ => format!("{e} - {}", self.expr(sc, ty, depth + 1)),
                    }
                }
                Base::Text => {
                    let e = self.expr(sc, ty, depth + 1);
                    match self.rng.below(4) {
                        0 => format!("{e} || 'x'"),
                        1 => format!("lower({e})"),
                        2 => format!("({e} COLLATE \"C\")"),
                        _ => format!("upper({e})"),
                    }
                }
                Base::Date => format!("{} + 1", self.expr(sc, ty, depth + 1)),
                Base::Bool => format!("({})", self.pred(sc, depth + 1)),
                _ => self.value(ty),
            },
            15 | 16 => self.cast_to(sc, ty, depth),
            17 if base == Base::Bool => format!("({})", self.pred(sc, depth + 1)),
            // A parenthesized operand: the rules' spans and fixes must keep
            // the parentheses with what they quote or remove.
            19 => format!("({})", self.expr(sc, ty, depth + 1)),
            // An integer division used as a non-integer value
            // (`integer_division`).
            20 if base == Base::Num => {
                let e = self.expr(sc, &Ty::of(Base::Int), depth + 1);
                match self.rng.below(3) {
                    0 => format!("{e} / 2 * 1.5"),
                    1 => format!("({e} / 3)::numeric"),
                    _ => format!("{e} / 4 + 0.5"),
                }
            }
            // What `simplifiable_case` reports.
            18 if base == Base::Bool => format!(
                "CASE WHEN {} THEN {} ELSE {} END",
                self.pred(sc, depth + 1),
                self.rng.pick(&["true", "false"]),
                self.rng.pick(&["false", "true", "NULL"])
            ),
            _ => self
                .column_of(sc, class, 5)
                .map(|(c, _)| c)
                .unwrap_or_else(|| self.value(ty)),
        }
    }

    /// An expression of `ty` through a cast: an identity cast (what
    /// `redundant_cast` reports), a binary or I/O one, to a domain, or
    /// from text to an enum.
    fn cast_to(&mut self, sc: &Scope, ty: &Ty, depth: u32) -> String {
        let base = ty.base;
        match base {
            Base::Text => {
                let from = self.some_type(sc);
                let e = self.expr(sc, &from, depth + 1);
                if self.rng.chance(50) {
                    format!("({e})::text")
                } else {
                    format!("CAST({e} AS varchar)")
                }
            }
            Base::Int | Base::Big | Base::Num => {
                let from = Ty::of(*self.rng.pick(&[Base::Int, Base::Big, Base::Num]));
                let e = self.expr(sc, &from, depth + 1);
                let target = if ty.domain.is_some() && self.rng.chance(50) {
                    ty.name.clone()
                } else {
                    base.sql()
                };
                format!("({e})::{target}")
            }
            Base::Enum(k) => {
                if self.rng.chance(50) {
                    format!(
                        "({})::e{k}",
                        self.expr(sc, &Ty::of(Base::Enum(k)), depth + 1)
                    )
                } else {
                    format!("'{}'::text::e{k}", self.rng.pick(ENUM_LABELS[k]))
                }
            }
            Base::Date => format!("({})::date", self.expr(sc, &Ty::of(Base::Ts), depth + 1)),
            _ => format!("({})::{}", self.expr(sc, ty, depth + 1), base.sql()),
        }
    }

    /// A table (or view) to read in a subquery, aliased fresh.
    fn some_rel(&mut self) -> (Rel, String) {
        let rel = self.rng.pick(&self.schema.rels).clone();
        let alias = self.fresh("s");
        (rel, alias)
    }

    /// The correlation of a subquery over `rel` (alias `alias`) with the
    /// scope around it: ` WHERE …`, or nothing.
    fn correlation(&mut self, sc: &Scope, rel: &Rel, alias: &str, depth: u32) -> String {
        let inner = Scope {
            items: vec![(alias.to_owned(), rel.cols.clone())],
            outer: {
                let mut o = sc.items.clone();
                o.extend(sc.outer.iter().cloned());
                o
            },
        };
        let mut conds = Vec::new();
        if self.rng.chance(70) {
            // An equality with an outer column, through a foreign key when
            // there is one.
            let fk = self
                .schema
                .fks
                .iter()
                .filter(|(t, _, r, _)| t == &rel.name || r == &rel.name)
                .cloned()
                .collect::<Vec<_>>();
            let mut done = false;
            if !fk.is_empty() && self.rng.chance(60) {
                let (t, c, r, rc) = self.rng.pick(&fk).clone();
                let (mine, theirs, other) = if t == rel.name {
                    (c, rc, r)
                } else {
                    (rc, c, t)
                };
                if let Some((a, _)) = sc.items.iter().chain(sc.outer.iter()).find(|(_, cols)| {
                    cols.iter().any(|col| col.name == theirs)
                        && self
                            .schema
                            .rels
                            .iter()
                            .any(|r| r.name == other && r.cols.len() == cols.len())
                }) {
                    conds.push(format!("{alias}.{mine} = {a}.{theirs}"));
                    done = true;
                }
            }
            if !done {
                let col = self.rng.pick(&rel.cols).clone();
                if let Some((o, _)) = self.column_of(
                    &Scope {
                        items: vec![],
                        outer: inner.outer.clone(),
                    },
                    col.ty.base.class(),
                    100,
                ) {
                    let op = *self.rng.pick(&["=", "=", "<", "IS NOT DISTINCT FROM"]);
                    conds.push(format!("{alias}.{} {op} {o}", col.name));
                }
            }
        }
        if self.rng.chance(35) {
            conds.push(self.pred(&inner, depth + 1));
        }
        if conds.is_empty() {
            String::new()
        } else {
            format!(" WHERE {}", conds.join(" AND "))
        }
    }

    fn scalar_subquery(&mut self, sc: &Scope, ty: &Ty, depth: u32) -> String {
        let (rel, alias) = self.some_rel();
        let corr = self.correlation(sc, &rel, &alias, depth);
        let inner = Scope {
            items: vec![(alias.clone(), rel.cols.clone())],
            outer: Vec::new(),
        };
        let from = format!(
            "{}{} {alias}",
            if rel.parent && self.rng.chance(30) {
                "ONLY "
            } else {
                ""
            },
            rel.name
        );
        match self.rng.below(5) {
            0 | 1 if matches!(ty.base, Base::Big) => {
                let filter = if self.rng.chance(30) {
                    format!(" FILTER (WHERE {})", self.pred(&inner, depth + 2))
                } else {
                    String::new()
                };
                format!("(SELECT count(*){filter} FROM {from}{corr})")
            }
            0 | 1 if ty.base.orderable() => {
                let e = self.expr(&inner, ty, depth + 2);
                format!(
                    "(SELECT {}({e}) FROM {from}{corr})",
                    self.rng.pick(&["max", "min"])
                )
            }
            2 => {
                let e = self.expr(&inner, ty, depth + 2);
                let order = self
                    .any_column(&inner)
                    .map(|(c, _)| format!(" ORDER BY {c}"))
                    .unwrap_or_default();
                format!("(SELECT {e} FROM {from}{corr}{order} LIMIT 1)")
            }
            _ => {
                let e = self.expr(&inner, ty, depth + 2);
                format!("(SELECT {e} FROM {from}{corr})")
            }
        }
    }

    /// A condition.
    fn pred(&mut self, sc: &Scope, depth: u32) -> String {
        let leaf = depth >= 3;
        let ty = self.some_type(sc);
        let base = ty.base;
        let n = if leaf { 8 } else { 30 };
        match self.rng.below(n) {
            0..=2 => {
                let a = self.expr(sc, &ty, depth + 1);
                let b = if self.rng.chance(50) {
                    self.value(&ty)
                } else {
                    self.expr(sc, &ty, depth + 1)
                };
                let op = if base.orderable() {
                    *self.rng.pick(&["=", "<>", "<", ">", "<=", ">="])
                } else {
                    *self.rng.pick(&["=", "<>"])
                };
                format!("{a} {op} {b}")
            }
            3 | 4 => {
                let a = self.expr(sc, &ty, depth + 1);
                format!(
                    "{a} IS {}NULL",
                    if self.rng.chance(50) { "NOT " } else { "" }
                )
            }
            5 => {
                let a = self.expr(sc, &ty, depth + 1);
                let b = if self.rng.chance(30) {
                    "NULL".to_owned()
                } else {
                    self.expr(sc, &ty, depth + 1)
                };
                format!(
                    "{a} IS {}DISTINCT FROM {b}",
                    if self.rng.chance(50) { "NOT " } else { "" }
                )
            }
            6 => {
                let a = self.expr(sc, &ty, depth + 1);
                let n = 1 + self.rng.below(3);
                let items: Vec<String> = (0..n)
                    .map(|_| {
                        if self.rng.chance(12) {
                            "NULL".to_owned()
                        } else {
                            self.literal(&ty)
                        }
                    })
                    .collect();
                format!(
                    "{a} {}IN ({})",
                    if self.rng.chance(30) { "NOT " } else { "" },
                    items.join(", ")
                )
            }
            7 => {
                if let Some((c, t)) = self.column_of(sc, Class::Bool, 0) {
                    match self.rng.below(4) {
                        0 => c,
                        1 => format!("NOT {c}"),
                        2 => format!(
                            "{c} IS {}",
                            self.rng
                                .pick(&["TRUE", "NOT TRUE", "FALSE", "NOT FALSE", "UNKNOWN"])
                        ),
                        _ => format!("{c} = {}", self.value(&t)),
                    }
                } else {
                    format!("{} IS NOT NULL", self.expr(sc, &ty, depth + 1))
                }
            }
            8 | 9 => {
                let a = self.pred(sc, depth + 1);
                let b = self.pred(sc, depth + 1);
                format!("({a} {} {b})", self.rng.pick(&["AND", "OR"]))
            }
            10 => format!("NOT ({})", self.pred(sc, depth + 1)),
            11 | 12 => {
                // [NOT] EXISTS, correlated.
                let (rel, alias) = self.some_rel();
                let corr = self.correlation(sc, &rel, &alias, depth);
                let what = self.rng.pick(&["1", "*", "NULL"]).to_string();
                let tail = if self.rng.chance(20) { " LIMIT 1" } else { "" };
                format!(
                    "{}EXISTS (SELECT {what} FROM {} {alias}{corr}{tail})",
                    if self.rng.chance(30) { "NOT " } else { "" },
                    rel.name
                )
            }
            13 | 14 => {
                // [NOT] IN (subquery).
                let a = self.expr(sc, &ty, depth + 1);
                let sub = self.column_subquery(sc, &ty, depth);
                format!(
                    "{a} {}IN ({sub})",
                    if self.rng.chance(40) { "NOT " } else { "" }
                )
            }
            15 => {
                // op ANY / ALL over an array.
                let a = self.expr(sc, &ty, depth + 1);
                let n = 1 + self.rng.below(3);
                let items: Vec<String> = (0..n)
                    .map(|_| {
                        if self.rng.chance(15) {
                            "NULL".to_owned()
                        } else {
                            self.expr(sc, &ty, 3)
                        }
                    })
                    .collect();
                let op = if base.orderable() {
                    *self.rng.pick(&["=", "<>", "<", ">="])
                } else {
                    "="
                };
                format!(
                    "{a} {op} {}(ARRAY[{}]::{}[])",
                    self.rng.pick(&["ANY", "ALL"]),
                    items.join(", "),
                    base.sql()
                )
            }
            16 => {
                // op ANY / ALL over a subquery.
                let a = self.expr(sc, &ty, depth + 1);
                let sub = self.column_subquery(sc, &ty, depth);
                let op = if base.orderable() {
                    *self.rng.pick(&["=", "<>", "<", ">="])
                } else {
                    "="
                };
                format!(
                    "{a} {op} {} ({sub})",
                    self.rng.pick(&["ANY", "ALL", "SOME"])
                )
            }
            17 if base.orderable() => {
                let a = self.expr(sc, &ty, depth + 1);
                format!(
                    "{a} {}BETWEEN {}{} AND {}",
                    if self.rng.chance(30) { "NOT " } else { "" },
                    if self.rng.chance(15) {
                        "SYMMETRIC "
                    } else {
                        ""
                    },
                    self.value(&ty),
                    self.literal(&ty)
                )
            }
            18 if base == Base::Text => {
                let a = self.expr(sc, &ty, depth + 1);
                let pat = match self.rng.below(10) {
                    0..=2 => self.param(Base::Text),
                    // A pattern built from a parameter (`like_unescaped_param`).
                    3 => format!("'%' || {} || '%'", self.param(Base::Text)),
                    4 => format!("concat({}, '%')", self.param(Base::Text)),
                    _ => self.rng.pick(&["'a%'", "'%'", "'x'", "'%b_'"]).to_string(),
                };
                format!(
                    "{a} {}{} {pat}",
                    if self.rng.chance(30) { "NOT " } else { "" },
                    self.rng.pick(&["LIKE", "ILIKE"])
                )
            }
            19 => {
                let p = self.pred(sc, depth + 1);
                format!(
                    "CASE WHEN {p} THEN {} ELSE {} END",
                    self.pred(sc, depth + 1),
                    self.pred(sc, depth + 1)
                )
            }
            20 => {
                // A row comparison.
                let t2 = self.some_type(sc);
                let a = self.expr(sc, &ty, depth + 1);
                let b = self.expr(sc, &t2, depth + 1);
                format!(
                    "({a}, {b}) {} ({}, {})",
                    if ty.base.orderable() && t2.base.orderable() {
                        *self.rng.pick(&["=", "<>", "<"])
                    } else {
                        "="
                    },
                    self.value(&ty),
                    self.value(&t2)
                )
            }
            21 => self
                .rng
                .pick(&["true", "false", "1 = 1", "NULL::boolean"])
                .to_string(),
            22 => {
                // A comparison with a scalar subquery.
                let a = self.expr(sc, &ty, depth + 1);
                let s = self.scalar_subquery(sc, &ty, depth + 1);
                format!(
                    "{a} {} {s}",
                    if base.orderable() {
                        *self.rng.pick(&["=", "<", ">="])
                    } else {
                        "="
                    }
                )
            }
            23 => {
                // A comparison between two columns of the same class.
                let class = base.class();
                match (self.column_of(sc, class, 10), self.column_of(sc, class, 10)) {
                    (Some((a, _)), Some((b, _))) => format!("{a} = {b}"),
                    _ => format!("{} IS NULL", self.expr(sc, &ty, depth + 1)),
                }
            }
            24 => {
                let a = self.expr(sc, &ty, depth + 1);
                format!("{a} = {a}")
            }
            // A floating-point equality (`float_equality`).
            25 if base.class() == Class::Number => {
                let a = self.expr(sc, &ty, depth + 1);
                let value = match self.rng.below(3) {
                    0 if !self.no_params => "$f1::float8",
                    1 => "1",
                    _ => "0.1",
                };
                format!("({a})::float8 {} {value}", self.rng.pick(&["=", "<>"]))
            }
            // A date or timestamp compared with a timestamptz
            // (`timestamp_timezone_mix`).
            26 if matches!(base, Base::Date | Base::Ts) => {
                let a = self.expr(sc, &ty, depth + 1);
                let (lhs, rhs) = if base == Base::Date {
                    (a, self.expr(sc, &Ty::of(Base::Ts), depth + 1))
                } else {
                    (format!("({a})::timestamp"), "now()".to_owned())
                };
                format!("{lhs} {} {rhs}", self.rng.pick(&["<", ">=", "="]))
            }
            _ => {
                let a = self.expr(sc, &ty, depth + 1);
                let b = self.value(&ty);
                format!(
                    "{a} {} {b}",
                    if base.orderable() {
                        *self.rng.pick(&["=", ">", "<="])
                    } else {
                        "="
                    }
                )
            }
        }
    }

    /// A one-column subquery of `ty`, maybe correlated, maybe with NULLs.
    fn column_subquery(&mut self, sc: &Scope, ty: &Ty, depth: u32) -> String {
        if self.rng.chance(15) {
            let n = 1 + self.rng.below(3);
            let rows: Vec<String> = (0..n)
                .map(|_| {
                    if self.rng.chance(20) {
                        format!("(NULL::{})", ty.base.sql())
                    } else {
                        format!("({})", self.literal(ty))
                    }
                })
                .collect();
            return format!("VALUES {}", rows.join(", "));
        }
        let (rel, alias) = self.some_rel();
        let corr = self.correlation(sc, &rel, &alias, depth);
        let inner = Scope::of(&alias, &rel.cols);
        let e = self.expr(&inner, ty, depth + 2);
        let distinct = if self.rng.chance(15) { "DISTINCT " } else { "" };
        format!("SELECT {distinct}{e} FROM {} {alias}{corr}", rel.name)
    }

    /// An aggregate over `sc`'s own items, and its type.
    fn aggregate(&mut self, sc: &Scope, want: Option<&Ty>) -> Option<(String, Ty)> {
        let local = sc.local();
        let filter = |g: &mut Gen| {
            if g.rng.chance(25) {
                format!(" FILTER (WHERE {})", g.pred(&local, 2))
            } else {
                String::new()
            }
        };
        let choice = match want.map(|t| t.base) {
            None => self.rng.below(8),
            Some(Base::Big) => *self.rng.pick(&[0, 1, 2, 3]),
            Some(Base::Num) => *self.rng.pick(&[3, 4, 5]),
            Some(Base::Bool) => 6,
            Some(Base::Text) => *self.rng.pick(&[5, 7]),
            Some(b) if b.orderable() => 5,
            Some(_) => return None,
        };
        Some(match choice {
            0 => (format!("count(*){}", filter(self)), Ty::of(Base::Big)),
            1 => {
                let t = self.some_type(&local);
                let e = self.expr(&local, &t, 2);
                (format!("count({e}){}", filter(self)), Ty::of(Base::Big))
            }
            2 => {
                let t = self.some_type(&local);
                let e = self.expr(&local, &t, 2);
                (
                    format!("count(DISTINCT {e}){}", filter(self)),
                    Ty::of(Base::Big),
                )
            }
            3 => {
                let (b, out) = match want.map(|t| t.base) {
                    Some(Base::Big) => (Base::Int, Base::Big),
                    _ => (Base::Num, Base::Num),
                };
                let e = self.expr(&local, &Ty::of(b), 2);
                (format!("sum({e}){}", filter(self)), Ty::of(out))
            }
            4 => {
                let e = self.expr(&local, &Ty::of(Base::Num), 2);
                (format!("avg({e}){}", filter(self)), Ty::of(Base::Num))
            }
            5 => {
                let t = match want {
                    Some(t) => Ty::of(t.base),
                    None => {
                        let t = self.some_type(&local);
                        if t.base.orderable() {
                            Ty::of(t.base)
                        } else {
                            Ty::of(Base::Int)
                        }
                    }
                };
                let e = self.expr(&local, &t, 2);
                (
                    format!("{}({e}){}", self.rng.pick(&["max", "min"]), filter(self)),
                    t,
                )
            }
            6 => {
                let p = self.pred(&local, 2);
                (
                    format!(
                        "{}({p}){}",
                        self.rng.pick(&["bool_and", "bool_or", "every"]),
                        filter(self)
                    ),
                    Ty::of(Base::Bool),
                )
            }
            _ => {
                let e = self.expr(&local, &Ty::of(Base::Text), 2);
                // Ordered by the value itself: ties are equal strings, so
                // the result is the same whatever order a plan reads rows in.
                let order = format!(" ORDER BY {e}");
                (
                    format!("string_agg({e}, ','{order}){}", filter(self)),
                    Ty::of(Base::Text),
                )
            }
        })
    }

    /// A window function call, and its type.
    fn window(&mut self, sc: &Scope) -> (String, Ty) {
        let local = sc.local();
        let mut over = Vec::new();
        if self.rng.chance(50)
            && let Some((c, _)) = self.any_column(&local)
        {
            over.push(format!("PARTITION BY {c}"));
        }
        let ordered = self.rng.chance(70);
        if ordered && let Some((c, _)) = self.any_column(&local) {
            over.push(format!(
                "ORDER BY {c}{}",
                if self.rng.chance(30) {
                    " DESC NULLS LAST"
                } else {
                    ""
                }
            ));
        }
        // Only functions whose value doesn't depend on the order of peers
        // (`row_number`, `lag`, a ROWS frame do): a plan may read ties in
        // any order, and the oracle compares the results of two plans.
        let over = over.join(" ");
        match self.rng.below(6) {
            0 => (format!("dense_rank() OVER ({over})"), Ty::of(Base::Big)),
            1 => (format!("rank() OVER ({over})"), Ty::of(Base::Big)),
            2 => {
                let (c, t) = self
                    .any_column(&local)
                    .unwrap_or_else(|| ("1".into(), Ty::of(Base::Int)));
                let (c, t) = if t.base.orderable() {
                    (c, t)
                } else {
                    ("1".to_owned(), Ty::of(Base::Int))
                };
                (
                    format!("{}({c}) OVER ({over})", self.rng.pick(&["max", "min"])),
                    Ty::of(t.base),
                )
            }
            3 => {
                let e = self.expr(&local, &Ty::of(Base::Num), 2);
                (format!("sum({e}) OVER ({over})"), Ty::of(Base::Num))
            }
            4 => {
                let p = self.pred(&local, 2);
                (
                    format!("count(*) FILTER (WHERE {p}) OVER ({over})"),
                    Ty::of(Base::Big),
                )
            }
            _ => {
                let t = self.some_type(&local);
                let e = self.expr(&local, &t, 2);
                (format!("count({e}) OVER ({over})"), Ty::of(Base::Big))
            }
        }
    }
}

// ── FROM, SELECT ───────────────────────────────────────────────────────────

impl Gen {
    /// A FROM item that is not a join: (text, alias, columns). `left` is
    /// what a LATERAL item may reference; `ctes` the CTEs in scope.
    fn source_item(
        &mut self,
        outer: &Scope,
        left: &Scope,
        ctes: &[Rel],
        lateral_ok: bool,
        depth: u32,
    ) -> (String, String, Vec<Col>) {
        let alias = self.fresh("a");
        let roll = self.rng.below(20);
        match roll {
            0..=1 if !ctes.is_empty() => {
                let rel = self.rng.pick(ctes).clone();
                (format!("{} {alias}", rel.name), alias, rel.cols)
            }
            2 if depth < 2 => {
                let sel = self.select(outer, None, depth + 1, true);
                (format!("({}) {alias}", sel.sql), alias, sel.cols)
            }
            3 | 4 if lateral_ok && depth < 2 && !left.items.is_empty() => {
                let mut sc = left.clone();
                sc.outer.extend(outer.items.iter().cloned());
                sc.outer.extend(outer.outer.iter().cloned());
                let inner_outer = Scope {
                    items: sc.items.clone(),
                    outer: sc.outer.clone(),
                };
                let sel = self.select(&inner_outer, None, depth + 1, true);
                (format!("LATERAL ({}) {alias}", sel.sql), alias, sel.cols)
            }
            5 => {
                let col = Col {
                    name: "n".into(),
                    ty: Ty::of(Base::Int),
                    nullable: false,
                };
                let hi = match (lateral_ok, self.column_of(left, Class::Number, 0)) {
                    (true, Some((c, _))) if self.rng.chance(50) => {
                        format!("LEAST(({c})::integer, 3)")
                    }
                    _ => self.rng.pick(&["1", "3", "0"]).to_string(),
                };
                (
                    format!("generate_series(1, {hi}) AS {alias}(n)"),
                    alias,
                    vec![col],
                )
            }
            6 => {
                let ty = self.some_type(left);
                let n = 1 + self.rng.below(3);
                let items: Vec<String> = (0..n)
                    .map(|_| {
                        if self.rng.chance(20) {
                            "NULL".into()
                        } else if lateral_ok && self.rng.chance(50) {
                            self.column_of(left, ty.base.class(), 0)
                                .map(|(c, _)| c)
                                .unwrap_or_else(|| self.literal(&ty))
                        } else {
                            self.literal(&ty)
                        }
                    })
                    .collect();
                let ord = self.rng.chance(30);
                let mut cols = vec![Col {
                    name: "v".into(),
                    ty: Ty::of(ty.base),
                    nullable: true,
                }];
                if ord {
                    cols.push(Col {
                        name: "ord".into(),
                        ty: Ty::of(Base::Big),
                        nullable: false,
                    });
                }
                (
                    format!(
                        "unnest(ARRAY[{}]::{}[]){} AS {alias}({})",
                        items.join(", "),
                        ty.base.sql(),
                        if ord { " WITH ORDINALITY" } else { "" },
                        if ord { "v, ord" } else { "v" }
                    ),
                    alias,
                    cols,
                )
            }
            7 => {
                let n = 1 + self.rng.below(3);
                let t1 = Ty::of(Base::Int);
                let t2 = Ty::of(Base::Text);
                let rows: Vec<String> = (0..n)
                    .map(|_| {
                        let a = if self.rng.chance(20) {
                            "NULL::integer".into()
                        } else {
                            self.literal(&t1)
                        };
                        let b = if self.rng.chance(20) {
                            "NULL::text".into()
                        } else {
                            self.literal(&t2)
                        };
                        format!("({a}, {b})")
                    })
                    .collect();
                (
                    format!("(VALUES {}) AS {alias}(x, y)", rows.join(", ")),
                    alias,
                    vec![
                        Col {
                            name: "x".into(),
                            ty: t1,
                            nullable: true,
                        },
                        Col {
                            name: "y".into(),
                            ty: t2,
                            nullable: true,
                        },
                    ],
                )
            }
            _ => {
                let rel = self.rng.pick(&self.schema.rels).clone();
                let only = if rel.parent && self.rng.chance(40) {
                    "ONLY "
                } else {
                    ""
                };
                (format!("{only}{} {alias}", rel.name), alias, rel.cols)
            }
        }
    }

    /// A FROM clause: an item and a chain of joins.
    fn join_tree(&mut self, outer: &Scope, ctes: &[Rel], depth: u32) -> (String, Scope) {
        let empty = Scope::default();
        let (text, alias, cols) = self.source_item(outer, &empty, ctes, false, depth);
        let mut sc = Scope {
            items: vec![(alias, cols)],
            outer: outer
                .items
                .iter()
                .chain(outer.outer.iter())
                .cloned()
                .collect(),
        };
        let mut text = text;
        let joins = self.rng.below(if depth == 0 { 4 } else { 3 });
        for _ in 0..joins {
            let kind = *self.rng.pick(&[
                "JOIN",
                "JOIN",
                "LEFT JOIN",
                "LEFT JOIN",
                "LEFT JOIN",
                "RIGHT JOIN",
                "FULL JOIN",
                "CROSS JOIN",
                ",",
                "NATURAL JOIN",
                "NATURAL LEFT JOIN",
            ]);
            let lateral_ok = matches!(kind, "JOIN" | "LEFT JOIN" | "CROSS JOIN" | ",");
            let left = sc.local();
            let (item, alias, cols) = self.source_item(outer, &left, ctes, lateral_ok, depth);
            let mut joined = sc.clone();
            joined.items.push((alias.clone(), cols.clone()));
            if kind == "," || kind == "CROSS JOIN" {
                let _ = write!(
                    text,
                    "{} {item}",
                    if kind == "," { "," } else { " CROSS JOIN" }
                );
                sc = joined;
                if kind == "," {
                    // A later JOIN ... ON could not see the items before the comma.
                    break;
                }
                continue;
            }
            // Common names: once on the left, once on the right, same class.
            let common: Vec<String> = cols
                .iter()
                .filter(|c| {
                    let on_left: Vec<&Col> = left
                        .items
                        .iter()
                        .flat_map(|(_, cs)| cs.iter())
                        .filter(|l| l.name == c.name)
                        .collect();
                    on_left.len() == 1 && on_left[0].ty.base.class() == c.ty.base.class()
                })
                .map(|c| c.name.clone())
                .collect();
            let all_common_ok = cols.iter().all(|c| {
                let on_left: Vec<&Col> = left
                    .items
                    .iter()
                    .flat_map(|(_, cs)| cs.iter())
                    .filter(|l| l.name == c.name)
                    .collect();
                on_left.is_empty()
                    || (on_left.len() == 1 && on_left[0].ty.base.class() == c.ty.base.class())
            }) && cols
                .iter()
                .filter(|c| cols.iter().filter(|d| d.name == c.name).count() > 1)
                .count()
                == 0;
            if kind.starts_with("NATURAL") {
                if all_common_ok && !item.starts_with("LATERAL") {
                    let _ = write!(text, " {kind} {item}");
                } else {
                    let _ = write!(text, " CROSS JOIN {item}");
                }
                sc = joined;
                continue;
            }
            let cond = if !common.is_empty() && self.rng.chance(20) {
                let n = 1 + self.rng.below(common.len().min(2));
                format!(" USING ({})", common[..n].join(", "))
            } else {
                let mut conds = Vec::new();
                // A foreign key, or an equality between same-class columns.
                let right = Scope::of(&alias, &cols);
                let fk_pairs: Vec<String> =
                    self.schema
                        .fks
                        .iter()
                        .flat_map(|(t, c, r, rc)| {
                            let mut out = Vec::new();
                            for (la, lcols) in &left.items {
                                let l_is = |name: &str, col: &str| {
                                    lcols.iter().any(|x| x.name == col)
                                        && self.schema.rels.iter().any(|rel| {
                                            rel.name == name && rel.cols.len() == lcols.len()
                                        })
                                };
                                let r_is = |name: &str, col: &str| {
                                    cols.iter().any(|x| x.name == col)
                                        && self.schema.rels.iter().any(|rel| {
                                            rel.name == name && rel.cols.len() == cols.len()
                                        })
                                };
                                if l_is(t, c) && r_is(r, rc) {
                                    out.push(format!("{alias}.{rc} = {la}.{c}"));
                                }
                                if l_is(r, rc) && r_is(t, c) {
                                    out.push(format!("{alias}.{c} = {la}.{rc}"));
                                }
                            }
                            out
                        })
                        .collect();
                if !fk_pairs.is_empty() && self.rng.chance(75) {
                    conds.push(self.rng.pick(&fk_pairs).clone());
                } else if let Some(c) = cols.get(self.rng.below(cols.len())).cloned()
                    && let Some((l, _)) = self.column_of(&left, c.ty.base.class(), 0)
                {
                    conds.push(format!("{alias}.{} = {l}", c.name));
                }
                if conds.is_empty() || self.rng.chance(25) {
                    let mut both = left.clone();
                    both.items.extend(right.items);
                    conds.push(self.pred(&both, 2));
                }
                format!(" ON {}", conds.join(" AND "))
            };
            let _ = write!(text, " {kind} {item}{cond}");
            sc = joined;
        }
        (text, sc)
    }

    /// A SELECT; `want` fixes its output types (a set operation's arm, an
    /// `INSERT … SELECT`); `named` gives every output a distinct name (a
    /// FROM item's, a CTE's, a view's).
    fn select(&mut self, outer: &Scope, want: Option<&[Ty]>, depth: u32, named: bool) -> Sel {
        if depth < 2 && want.is_none() && self.rng.chance(12) {
            return self.set_operation(outer, depth, named);
        }
        let mut with = String::new();
        let mut ctes: Vec<Rel> = Vec::new();
        if depth < 2 && self.rng.chance(if depth == 0 { 20 } else { 8 }) {
            let (w, rels) = self.with_clause(outer, depth, false);
            with = w;
            ctes = rels;
        }
        let (from, sc) = self.join_tree(outer, &ctes, depth);
        let grouped = self.rng.chance(22);
        let mut items: Vec<String> = Vec::new();
        let mut cols: Vec<Col> = Vec::new();
        let mut keys: Vec<String> = Vec::new();
        let mut distinct = String::new();
        let mut distinct_on: Vec<String> = Vec::new();
        let local = sc.local();
        let push =
            |items: &mut Vec<String>, cols: &mut Vec<Col>, text: String, ty: Ty, g: &mut Gen| {
                let name = if named || g.rng.chance(40) {
                    let n = format!("c{}", cols.len());
                    items.push(format!("{text} AS {n}"));
                    n
                } else {
                    items.push(text);
                    format!("?{}", cols.len())
                };
                cols.push(Col {
                    name,
                    ty,
                    nullable: true,
                });
            };
        if grouped {
            for _ in 0..1 + self.rng.below(2) {
                if let Some((c, _)) = self.any_column(&local)
                    && !keys.contains(&c)
                {
                    keys.push(c);
                }
            }
            let wanted: Vec<Option<Ty>> = match want {
                Some(w) => w.iter().cloned().map(Some).collect(),
                None => (0..1 + self.rng.below(3)).map(|_| None).collect(),
            };
            for w in wanted {
                let key = keys
                    .iter()
                    .find(|k| {
                        w.as_ref().is_none_or(|w| {
                            local.all().any(|(a, c, _)| {
                                &format!("{a}.{}", c.name) == *k
                                    && c.ty.base.class() == w.base.class()
                                    && c.ty.base == w.base
                            })
                        })
                    })
                    .cloned();
                match (key, self.rng.chance(45)) {
                    (Some(k), true) => {
                        let ty = local
                            .all()
                            .find(|(a, c, _)| format!("{a}.{}", c.name) == k)
                            .map(|(_, c, _)| c.ty.clone())
                            .unwrap();
                        push(&mut items, &mut cols, k, ty, self);
                    }
                    _ => match self.aggregate(&sc, w.as_ref()) {
                        Some((text, ty)) => push(&mut items, &mut cols, text, ty, self),
                        None => {
                            let w = w.unwrap();
                            let v = self.value(&w);
                            push(&mut items, &mut cols, v, w, self);
                        }
                    },
                }
            }
        } else {
            match self.rng.below(10) {
                0 => distinct = "DISTINCT ".into(),
                1 | 2 => {
                    for _ in 0..1 + self.rng.below(2) {
                        if let Some((c, _)) = self.any_column(&local) {
                            distinct_on.push(c);
                        }
                    }
                    if !distinct_on.is_empty() {
                        distinct = format!("DISTINCT ON ({}) ", distinct_on.join(", "));
                    }
                }
                _ => {}
            }
            let wanted: Vec<Option<Ty>> = match want {
                Some(w) => w.iter().cloned().map(Some).collect(),
                None => (0..1 + self.rng.below(3)).map(|_| None).collect(),
            };
            for w in wanted {
                let (text, ty) = match w {
                    Some(t) => (self.expr(&sc, &t, 1), t),
                    None => match self.rng.below(12) {
                        0 if depth == 0 || named => self.window(&sc),
                        1..=4 => match self.any_column(&local) {
                            Some(c) => c,
                            None => (self.value(&Ty::of(Base::Int)), Ty::of(Base::Int)),
                        },
                        5 if !named && depth == 0 => {
                            if let Some((a, _)) = local.items.first() {
                                items.push(format!("{a}.*"));
                                cols.extend(local.items[0].1.iter().cloned());
                            }
                            continue;
                        }
                        _ => {
                            let t = self.some_type(&sc);
                            (self.expr(&sc, &t, 1), Ty::of(t.base))
                        }
                    },
                };
                push(&mut items, &mut cols, text, ty, self);
            }
        }
        if items.is_empty() {
            push(&mut items, &mut cols, "1".into(), Ty::of(Base::Int), self);
        }
        let mut sql = format!("{with}SELECT {distinct}{} FROM {from}", items.join(", "));
        if self.rng.chance(70) {
            let _ = write!(sql, " WHERE {}", self.pred(&sc, 0));
        }
        let mut grouping_sets = false;
        if grouped && !keys.is_empty() {
            let k = keys.join(", ");
            match self.rng.below(8) {
                0 => {
                    let _ = write!(sql, " GROUP BY ROLLUP ({k})");
                    grouping_sets = true;
                }
                1 => {
                    let _ = write!(sql, " GROUP BY CUBE ({k})");
                    grouping_sets = true;
                }
                2 => {
                    let sets: Vec<String> = keys.iter().map(|k| format!("({k})")).collect();
                    let _ = write!(sql, " GROUP BY GROUPING SETS ({}, ())", sets.join(", "));
                    grouping_sets = true;
                }
                3 => {
                    let _ = write!(sql, " GROUP BY DISTINCT {k}, {k}");
                }
                _ => {
                    let _ = write!(sql, " GROUP BY {k}");
                }
            }
            if self.rng.chance(30)
                && let Some((agg, ty)) = self.aggregate(&sc, None)
            {
                let cmp = if ty.base.orderable() {
                    format!("{agg} > {}", self.literal(&ty))
                } else {
                    format!("{agg} IS NOT NULL")
                };
                let _ = write!(sql, " HAVING {cmp}");
            }
        } else if grouped {
            // No key: an aggregate over the whole input.
        }
        let _ = grouping_sets;
        // ORDER BY: DISTINCT ON's keys first; otherwise outputs and columns.
        let mut order: Vec<String> = distinct_on.clone();
        let ordered = !distinct_on.is_empty() && self.rng.chance(70) || self.rng.chance(35);
        if ordered || !distinct_on.is_empty() && self.rng.chance(50) {
            for _ in 0..1 + self.rng.below(2) {
                let key = if grouped || !distinct.is_empty() || self.rng.chance(40) {
                    format!("{}", 1 + self.rng.below(cols.len()))
                } else {
                    match self.any_column(&local) {
                        Some((c, _)) => self.maybe_parens(&c),
                        None => "1".into(),
                    }
                };
                let dir = *self
                    .rng
                    .pick(&["", "", " DESC", " NULLS FIRST", " DESC NULLS LAST"]);
                order.push(format!("{key}{dir}"));
            }
        }
        if !distinct_on.is_empty() && !order.is_empty() && self.rng.chance(30) {
            // DISTINCT ON without ORDER BY.
            order.clear();
        }
        if !order.is_empty() && (distinct.is_empty() || !distinct_on.is_empty()) {
            // Plain DISTINCT needs ORDER BY keys in the list: ordinals are.
            let _ = write!(sql, " ORDER BY {}", order.join(", "));
        } else if !order.is_empty() {
            let _ = write!(sql, " ORDER BY 1");
            order = vec!["1".into()];
        }
        match self.rng.below(12) {
            0 | 1 => {
                let _ = write!(
                    sql,
                    " LIMIT {}",
                    if self.no_params {
                        "2"
                    } else {
                        self.rng.pick(&["1", "2", "5", "$lim::bigint"])
                    }
                );
            }
            2 => {
                let _ = write!(sql, " LIMIT 3 OFFSET 1");
            }
            3 if !order.is_empty() => {
                let _ = write!(
                    sql,
                    " FETCH FIRST {} ROWS WITH TIES",
                    self.rng.pick(&["1", "2"])
                );
            }
            4 if depth == 0 && !grouped && distinct.is_empty() && self.rng.chance(30) => {
                let _ = write!(sql, " FOR UPDATE");
            }
            _ => {}
        }
        Sel { sql, cols }
    }

    /// `a UNION b`, `INTERSECT`, `EXCEPT` (`ALL` or not), maybe ordered.
    fn set_operation(&mut self, outer: &Scope, depth: u32, named: bool) -> Sel {
        let first = self.select(outer, None, depth + 1, true);
        let types: Vec<Ty> = first.cols.iter().map(|c| Ty::of(c.ty.base)).collect();
        let second = self.select(outer, Some(&types), depth + 1, true);
        let op = *self.rng.pick(&[
            "UNION",
            "UNION ALL",
            "UNION ALL",
            "INTERSECT",
            "EXCEPT",
            "EXCEPT ALL",
        ]);
        let mut sql = format!("({}) {op} ({})", first.sql, second.sql);
        if self.rng.chance(30) {
            let _ = write!(sql, " ORDER BY {}", 1 + self.rng.below(types.len()));
            if self.rng.chance(40) {
                sql.push_str(" LIMIT 2");
            }
        }
        let cols = first
            .cols
            .iter()
            .enumerate()
            .map(|(i, c)| Col {
                name: if named {
                    c.name.clone()
                } else {
                    format!("c{i}")
                },
                ty: Ty::of(c.ty.base),
                nullable: true,
            })
            .collect();
        Sel { sql, cols }
    }

    /// `WITH …`: plain, MATERIALIZED / NOT MATERIALIZED, recursive, and
    /// (`top`) data-modifying CTEs. The text ends with a space.
    fn with_clause(&mut self, outer: &Scope, depth: u32, top: bool) -> (String, Vec<Rel>) {
        let mut parts = Vec::new();
        let mut rels: Vec<Rel> = Vec::new();
        let mut recursive = false;
        for _ in 0..1 + self.rng.below(2) {
            let name = self.fresh("w");
            match self.rng.below(10) {
                0 | 1 => {
                    recursive = true;
                    let trees = self.schema.trees.clone();
                    if !trees.is_empty() && self.rng.chance(60) {
                        let (t, p, k) = self.rng.pick(&trees).clone();
                        parts.push(format!(
                            "{name}(id, parent, depth) AS (SELECT r.{k}, r.{p}, 1 FROM {t} r WHERE r.{p} IS NULL \
                             UNION SELECT c.{k}, c.{p}, {name}.depth + 1 FROM {t} c JOIN {name} ON c.{p} = {name}.id \
                             WHERE {name}.depth < 4)"
                        ));
                        let ty = self
                            .schema
                            .rels
                            .iter()
                            .find(|r| r.name == t)
                            .and_then(|r| r.cols.iter().find(|c| c.name == k))
                            .map(|c| c.ty.clone())
                            .unwrap_or(Ty::of(Base::Int));
                        rels.push(Rel {
                            name,
                            cols: vec![
                                Col {
                                    name: "id".into(),
                                    ty: ty.clone(),
                                    nullable: false,
                                },
                                Col {
                                    name: "parent".into(),
                                    ty,
                                    nullable: true,
                                },
                                Col {
                                    name: "depth".into(),
                                    ty: Ty::of(Base::Int),
                                    nullable: false,
                                },
                            ],
                            parent: false,
                            write: None,
                        });
                    } else {
                        parts.push(format!(
                            "{name}(n) AS (SELECT 1 UNION{} SELECT n + 1 FROM {name} WHERE n < {})",
                            if self.rng.chance(50) { " ALL" } else { "" },
                            self.rng.pick(&["3", "5"])
                        ));
                        rels.push(Rel {
                            name,
                            cols: vec![Col {
                                name: "n".into(),
                                ty: Ty::of(Base::Int),
                                nullable: false,
                            }],
                            parent: false,
                            write: None,
                        });
                    }
                }
                2 if top => {
                    if let Some((sql, cols)) = self.dml_returning() {
                        parts.push(format!("{name} AS ({sql})"));
                        rels.push(Rel {
                            name,
                            cols,
                            parent: false,
                            write: None,
                        });
                    }
                }
                _ => {
                    let sel = self.select(outer, None, depth + 1, true);
                    let mat = *self
                        .rng
                        .pick(&["", "", "MATERIALIZED ", "NOT MATERIALIZED "]);
                    parts.push(format!("{name} AS {mat}({})", sel.sql));
                    rels.push(Rel {
                        name,
                        cols: sel.cols,
                        parent: false,
                        write: None,
                    });
                }
            }
        }
        if parts.is_empty() {
            return (String::new(), Vec::new());
        }
        (
            format!(
                "WITH {}{} ",
                if recursive { "RECURSIVE " } else { "" },
                parts.join(", ")
            ),
            rels,
        )
    }
}

// ── DML ────────────────────────────────────────────────────────────────────

impl Gen {
    fn writable(&mut self, tables_only: bool) -> Option<Rel> {
        let rels: Vec<Rel> = self
            .schema
            .rels
            .iter()
            .filter(|r| {
                r.write
                    .as_ref()
                    .is_some_and(|w| !w.settable.is_empty() && (w.table || !tables_only))
            })
            .cloned()
            .collect();
        if rels.is_empty() {
            None
        } else {
            Some(self.rng.pick(&rels).clone())
        }
    }

    /// A `RETURNING` list over the target `alias` (`old`/`new` allowed).
    fn returning(&mut self, alias: &str, rel: &Rel, old_new: bool) -> (String, Vec<Col>) {
        match self.rng.below(6) {
            0 => (" RETURNING *".into(), rel.cols.clone()),
            1 if old_new => {
                let c = self.rng.pick(&rel.cols).clone();
                (
                    format!(" RETURNING old.{0} AS o, new.{0} AS n", c.name),
                    vec![
                        Col {
                            name: "o".into(),
                            ty: c.ty.clone(),
                            nullable: true,
                        },
                        Col {
                            name: "n".into(),
                            ty: c.ty,
                            nullable: true,
                        },
                    ],
                )
            }
            2 if old_new => {
                let c = self.rng.pick(&rel.cols).clone();
                (
                    format!(
                        " RETURNING WITH (OLD AS was, NEW AS now) was.{0} AS o, now.{0} AS n",
                        c.name
                    ),
                    vec![
                        Col {
                            name: "o".into(),
                            ty: c.ty.clone(),
                            nullable: true,
                        },
                        Col {
                            name: "n".into(),
                            ty: c.ty,
                            nullable: true,
                        },
                    ],
                )
            }
            _ => {
                let mut list = Vec::new();
                let mut cols = Vec::new();
                for (i, c) in rel.cols.iter().enumerate() {
                    if i == 0 || self.rng.chance(35) {
                        list.push(format!("{alias}.{} AS r{i}", c.name));
                        cols.push(Col {
                            name: format!("r{i}"),
                            ty: c.ty.clone(),
                            nullable: true,
                        });
                    }
                }
                (format!(" RETURNING {}", list.join(", ")), cols)
            }
        }
    }

    /// An INSERT / UPDATE / DELETE with RETURNING, for a CTE.
    fn dml_returning(&mut self) -> Option<(String, Vec<Col>)> {
        let rel = self.writable(true)?;
        let alias = self.fresh("m");
        let sc = Scope::of(&alias, &rel.cols);
        let (ret, cols) = self.returning(&alias, &rel, false);
        let sql = match self.rng.below(3) {
            0 => format!(
                "DELETE FROM {} {alias} WHERE {}{ret}",
                rel.name,
                self.pred(&sc, 1)
            ),
            1 => {
                let set = self.set_list(&sc, &rel);
                format!(
                    "UPDATE {} {alias} SET {set} WHERE {}{ret}",
                    rel.name,
                    self.pred(&sc, 1)
                )
            }
            _ => {
                let (cols_list, values) = self.insert_values(&rel);
                format!(
                    "INSERT INTO {} AS {alias} {cols_list} {values}{ret}",
                    rel.name
                )
            }
        };
        Some((sql, cols))
    }

    fn set_list(&mut self, sc: &Scope, rel: &Rel) -> String {
        let settable = rel.write.as_ref().unwrap().settable.clone();
        let mut sets = Vec::new();
        let mut used = Vec::new();
        for _ in 0..1 + self.rng.below(2) {
            let k = *self.rng.pick(&settable);
            if used.contains(&k) {
                continue;
            }
            used.push(k);
            let c = &rel.cols[k];
            let value = match self.rng.below(8) {
                0 => format!("{}.{}", sc.items[0].0, c.name),
                1 => "DEFAULT".into(),
                2 => self.literal(&c.ty.clone()),
                _ => self.expr(sc, &c.ty.clone(), 1),
            };
            let value = if sc.items.len() == 1
                && value.starts_with(&format!("{}.", sc.items[0].0))
                && self.rng.chance(50)
            {
                c.name.clone()
            } else {
                value
            };
            sets.push(format!("{} = {value}", c.name));
        }
        sets.join(", ")
    }

    /// `(cols)` and `VALUES …` / `SELECT …` / `DEFAULT VALUES`.
    fn insert_values(&mut self, rel: &Rel) -> (String, String) {
        let settable = rel.write.as_ref().unwrap().settable.clone();
        let chosen: Vec<usize> = settable
            .iter()
            .copied()
            .filter(|&k| !rel.cols[k].nullable || self.rng.chance(60))
            .collect();
        if chosen.is_empty() {
            return (String::new(), "DEFAULT VALUES".into());
        }
        let names: Vec<String> = chosen.iter().map(|&k| rel.cols[k].name.clone()).collect();
        let list = format!("({})", names.join(", "));
        let types: Vec<Ty> = chosen.iter().map(|&k| rel.cols[k].ty.clone()).collect();
        if self.rng.chance(25) {
            let sel = self.select(&Scope::default(), Some(&types), 1, true);
            return (list, sel.sql);
        }
        let rows = 1 + usize::from(self.rng.chance(25));
        let id = 10 + self.rng.below(1000);
        let rows: Vec<String> = (0..rows)
            .map(|r| {
                let vals: Vec<String> = chosen
                    .iter()
                    .zip(&types)
                    .map(|(&k, ty)| {
                        let name = &rel.cols[k].name;
                        if name == "id" {
                            format!("{}", id + r)
                        } else if name.ends_with("_id") || name == "parent" {
                            self.rng.pick(&["1", "2", "NULL"]).to_string()
                        } else if self.rng.chance(15) {
                            "DEFAULT".into()
                        } else if self.rng.chance(15) {
                            self.expr(&Scope::default(), ty, 2)
                        } else {
                            self.value(ty)
                        }
                    })
                    .collect();
                format!("({})", vals.join(", "))
            })
            .collect();
        (list, format!("VALUES {}", rows.join(", ")))
    }

    fn update(&mut self) -> Option<String> {
        let rel = self.writable(false)?;
        let alias = self.fresh("u");
        let mut sc = Scope::of(&alias, &rel.cols);
        let mut from = String::new();
        if self.rng.chance(35) {
            let (text, fsc) = self.join_tree(&Scope::default(), &[], 1);
            from = format!(" FROM {text}");
            sc.items.extend(fsc.items);
        }
        let set = self.set_list(&sc, &rel);
        let only = if rel.parent && self.rng.chance(30) {
            "ONLY "
        } else {
            ""
        };
        let mut sql = format!("UPDATE {only}{} {alias} SET {set}{from}", rel.name);
        if self.rng.chance(90) {
            let _ = write!(sql, " WHERE {}", self.pred(&sc, 0));
        }
        if self.rng.chance(50) {
            let (ret, _) = self.returning(&alias, &rel, true);
            sql.push_str(&ret);
        }
        Some(sql)
    }

    fn delete(&mut self) -> Option<String> {
        let rel = self.writable(false)?;
        let alias = self.fresh("d");
        let mut sc = Scope::of(&alias, &rel.cols);
        let mut using = String::new();
        if self.rng.chance(35) {
            let (text, fsc) = self.join_tree(&Scope::default(), &[], 1);
            using = format!(" USING {text}");
            sc.items.extend(fsc.items);
        }
        let mut sql = format!("DELETE FROM {} {alias}{using}", rel.name);
        if self.rng.chance(90) {
            let _ = write!(sql, " WHERE {}", self.pred(&sc, 0));
        }
        if self.rng.chance(50) {
            let (ret, _) = self.returning(&alias, &rel, true);
            sql.push_str(&ret);
        }
        Some(sql)
    }

    fn insert(&mut self) -> Option<String> {
        let rel = self.writable(false)?;
        let alias = self.fresh("i");
        let (cols, values) = self.insert_values(&rel);
        let mut sql = format!("INSERT INTO {} AS {alias} {cols} {values}", rel.name);
        let conflict = rel.write.as_ref().unwrap().conflict.clone();
        if self.rng.chance(50) {
            let sc = Scope {
                items: vec![
                    (alias.clone(), rel.cols.clone()),
                    ("excluded".into(), rel.cols.clone()),
                ],
                outer: Vec::new(),
            };
            let target = if conflict.is_empty() || self.rng.chance(15) {
                None
            } else {
                Some(self.rng.pick(&conflict).clone())
            };
            match (target, self.rng.below(3)) {
                (Some(t), 0) => {
                    let settable = rel.write.as_ref().unwrap().settable.clone();
                    let k = *self.rng.pick(&settable);
                    let c = rel.cols[k].clone();
                    let value = if self.rng.chance(50) {
                        format!("EXCLUDED.{}", c.name)
                    } else {
                        self.expr(&sc, &c.ty, 1)
                    };
                    let _ = write!(sql, " ON CONFLICT {t} DO UPDATE SET {} = {value}", c.name);
                    if self.rng.chance(30) {
                        let _ = write!(sql, " WHERE {}", self.pred(&sc, 1));
                    }
                }
                (Some(t), 1) => {
                    let lock = *self.rng.pick(&["", " FOR UPDATE", " FOR SHARE"]);
                    let _ = write!(sql, " ON CONFLICT {t} DO SELECT{lock}");
                    if self.rng.chance(30) {
                        let _ = write!(
                            sql,
                            " WHERE {}",
                            self.pred(&Scope::of(&alias, &rel.cols), 1)
                        );
                    }
                    let (ret, _) = self.returning(&alias, &rel, false);
                    sql.push_str(&ret);
                    return Some(sql);
                }
                (Some(t), _) => {
                    let _ = write!(sql, " ON CONFLICT {t} DO NOTHING");
                }
                (None, _) => sql.push_str(" ON CONFLICT DO NOTHING"),
            }
        }
        if self.rng.chance(50) {
            let (ret, _) = self.returning(&alias, &rel, true);
            sql.push_str(&ret);
        }
        Some(sql)
    }

    fn merge(&mut self) -> Option<String> {
        let rel = self.writable(true)?;
        let alias = self.fresh("t");
        let (src, s_alias, s_cols) = if self.rng.chance(60) {
            let r = self.rng.pick(&self.schema.rels).clone();
            let a = self.fresh("src");
            (format!("{} {a}", r.name), a, r.cols)
        } else {
            let sel = self.select(&Scope::default(), None, 1, true);
            let a = self.fresh("src");
            (format!("({}) {a}", sel.sql), a, sel.cols)
        };
        let target = Scope::of(&alias, &rel.cols);
        let source = Scope::of(&s_alias, &s_cols);
        let mut both = target.clone();
        both.items.extend(source.items.clone());
        // The join condition: same-class columns.
        let cond = match rel.cols.iter().find_map(|c| {
            s_cols
                .iter()
                .find(|s| s.ty.base.class() == c.ty.base.class())
                .map(|s| (c, s))
        }) {
            Some((c, s)) => format!("{alias}.{} = {s_alias}.{}", c.name, s.name),
            None => self.pred(&both, 2),
        };
        let mut sql = format!("MERGE INTO {} {alias} USING {src} ON {cond}", rel.name);
        let settable = rel.write.as_ref().unwrap().settable.clone();
        // A clause without AND makes the later ones of its kind unreachable,
        // which PG rejects.
        let mut unconditional = [false; 3];
        for _ in 0..1 + self.rng.below(3) {
            let choice = self.rng.below(6);
            let class = match choice {
                0..=2 => 0,
                3 | 4 => 1,
                _ => 2,
            };
            if unconditional[class] {
                continue;
            }
            let and = if self.rng.chance(35) {
                format!(" AND {}", self.pred(&both, 2))
            } else {
                String::new()
            };
            let before = sql.len();
            match choice {
                0 | 1 => {
                    let set = self.set_list(&both, &rel);
                    let _ = write!(sql, " WHEN MATCHED{and} THEN UPDATE SET {set}");
                }
                2 => {
                    let _ = write!(
                        sql,
                        " WHEN MATCHED{and} THEN {}",
                        self.rng.pick(&["DELETE", "DO NOTHING"])
                    );
                }
                3 | 4 => {
                    let and = if self.rng.chance(35) {
                        format!(" AND {}", self.pred(&source, 2))
                    } else {
                        String::new()
                    };
                    let chosen: Vec<usize> = settable
                        .iter()
                        .copied()
                        .filter(|_| self.rng.chance(70))
                        .collect();
                    if chosen.is_empty() {
                        let _ = write!(sql, " WHEN NOT MATCHED{and} THEN INSERT DEFAULT VALUES");
                    } else {
                        let names: Vec<String> =
                            chosen.iter().map(|&k| rel.cols[k].name.clone()).collect();
                        let vals: Vec<String> = chosen
                            .iter()
                            .map(|&k| self.expr(&source, &rel.cols[k].ty.clone(), 2))
                            .collect();
                        let _ = write!(
                            sql,
                            " WHEN NOT MATCHED{} {and} THEN INSERT ({}) VALUES ({})",
                            if self.rng.chance(30) {
                                " BY TARGET"
                            } else {
                                ""
                            },
                            names.join(", "),
                            vals.join(", ")
                        );
                    }
                }
                _ => {
                    let and = if self.rng.chance(35) {
                        format!(" AND {}", self.pred(&target, 2))
                    } else {
                        String::new()
                    };
                    if self.rng.chance(50) {
                        let _ = write!(sql, " WHEN NOT MATCHED BY SOURCE{and} THEN DELETE");
                    } else {
                        let set = self.set_list(&target, &rel);
                        let _ = write!(
                            sql,
                            " WHEN NOT MATCHED BY SOURCE{and} THEN UPDATE SET {set}"
                        );
                    }
                }
            }
            unconditional[class] = !sql[before..]
                .split(" THEN ")
                .next()
                .is_some_and(|head| head.contains(" AND "));
        }
        if self.rng.chance(40) {
            let c = self.rng.pick(&rel.cols).name.clone();
            let _ = write!(sql, " RETURNING merge_action(), old.{c}, new.{c}");
        }
        Some(sql)
    }

    /// A table's key column(s), when it has a primary key or a unique
    /// NOT NULL column (its first conflict target over plain columns).
    fn key_of(&self, rel: &Rel) -> Option<Vec<String>> {
        let w = rel.write.as_ref()?;
        let target = w
            .conflict
            .iter()
            .find(|t| t.starts_with('(') && !t.contains("WHERE") && !t.contains("lower("))?;
        let cols: Vec<String> = target
            .trim_matches(|c| c == '(' || c == ')')
            .split(", ")
            .map(str::to_owned)
            .collect();
        cols.iter()
            .all(|c| rel.cols.iter().any(|col| &col.name == c && !col.nullable))
            .then_some(cols)
    }

    /// The shapes the key-based rules judge: a lookup by key (one row:
    /// `fetch_all_single_row`, `redundant_limit`), DISTINCT or GROUP BY
    /// over a key (`redundant_distinct`, `redundant_group_by`), a join to
    /// a key whose columns go unread (`redundant_join`), ORDER BY after a
    /// key (`redundant_order_by`).
    fn keyed_select(&mut self) -> Option<String> {
        let keyed: Vec<(Rel, Vec<String>)> = self
            .schema
            .rels
            .iter()
            .filter_map(|r| Some((r.clone(), self.key_of(r)?)))
            .collect();
        if keyed.is_empty() {
            return None;
        }
        let (rel, key) = self.rng.pick(&keyed).clone();
        let a = self.fresh("k");
        let mut sc = Scope::of(&a, &rel.cols);
        let mut from = format!("{} {a}", rel.name);
        // A join to another keyed table by its whole key, maybe unread.
        if self.rng.chance(40) {
            let (other, okey) = self.rng.pick(&keyed).clone();
            let b = self.fresh("k");
            let mut conds = Vec::new();
            for k in &okey {
                let kcol = other.cols.iter().find(|c| &c.name == k).unwrap().clone();
                match self.column_of(&sc, kcol.ty.base.class(), 0) {
                    Some((c, _)) => conds.push(format!("{b}.{k} = {c}")),
                    None => conds.push(format!("{b}.{k} = {}", self.literal(&kcol.ty))),
                }
            }
            let kind = *self.rng.pick(&["LEFT JOIN", "LEFT JOIN", "JOIN"]);
            let _ = write!(
                from,
                " {kind} {} {b} ON {}",
                other.name,
                conds.join(" AND ")
            );
            if self.rng.chance(40) {
                sc.items.push((b, other.cols.clone()));
            }
        }
        let keys: Vec<String> = key.iter().map(|k| format!("{a}.{k}")).collect();
        let mut items: Vec<String> = Vec::new();
        if self.rng.chance(70) {
            items.extend(keys.iter().cloned());
        }
        for _ in 0..1 + self.rng.below(2) {
            if let Some((c, _)) = self.any_column(&sc)
                && !items.contains(&c)
            {
                items.push(c);
            }
        }
        let grouped = self.rng.chance(25);
        let distinct = !grouped && self.rng.chance(30);
        let plain = items.clone();
        if grouped {
            items.push("count(*)".into());
        }
        let mut sql = format!(
            "SELECT {}{} FROM {from}",
            if distinct { "DISTINCT " } else { "" },
            items.join(", ")
        );
        if self.rng.chance(50) {
            let conds: Vec<String> = key
                .iter()
                .map(|k| {
                    let col = rel.cols.iter().find(|c| &c.name == k).unwrap().clone();
                    format!("{a}.{k} = {}", self.value(&col.ty))
                })
                .collect();
            let _ = write!(sql, " WHERE {}", conds.join(" AND "));
        } else if self.rng.chance(50) {
            let _ = write!(sql, " WHERE {}", self.pred(&sc, 1));
        }
        if grouped && !plain.is_empty() {
            let keys: Vec<String> = plain.iter().map(|k| self.maybe_parens(k)).collect();
            let _ = write!(sql, " GROUP BY {}", keys.join(", "));
        }
        if self.rng.chance(40) && !grouped && !distinct {
            let mut order = keys.clone();
            if let Some((c, _)) = self.any_column(&sc) {
                order.push(c);
            }
            let order: Vec<String> = order.iter().map(|k| self.maybe_parens(k)).collect();
            let _ = write!(sql, " ORDER BY {}", order.join(", "));
        }
        if self.rng.chance(30) {
            sql.push_str(" LIMIT 1");
        }
        Some(sql)
    }

    /// `INSERT INTO t VALUES (…)`: every column, positionally
    /// (`insert_without_columns`).
    fn insert_full_row(&mut self) -> Option<String> {
        let rel = self.writable(true)?;
        if rel.write.as_ref().unwrap().settable.len() != rel.cols.len() {
            return None;
        }
        let id = 10 + self.rng.below(1000);
        let vals: Vec<String> = rel
            .cols
            .iter()
            .map(|c| {
                if c.name == "id" {
                    format!("{id}")
                } else if c.name.ends_with("_id") || c.name == "parent" {
                    "NULL".to_owned()
                } else {
                    self.literal(&c.ty.clone())
                }
            })
            .collect();
        Some(format!(
            "INSERT INTO {} VALUES ({})",
            rel.name,
            vals.join(", ")
        ))
    }

    /// `item`, sometimes in parentheses of its own (`ORDER BY (a.id)`).
    fn maybe_parens(&mut self, item: &str) -> String {
        if self.rng.chance(15) {
            format!("({item})")
        } else {
            item.to_owned()
        }
    }

    /// One statement: mostly SELECTs, the rest DML.
    fn statement(&mut self) -> String {
        let dml = match self.rng.below(20) {
            0 | 1 => self.update(),
            2 | 3 => self.delete(),
            4 | 5 => self.insert(),
            6 => self.merge(),
            8..=10 => self.keyed_select(),
            11 => self.insert_full_row(),
            7 => {
                let (with, ctes) = self.with_clause(&Scope::default(), 0, true);
                if ctes.is_empty() {
                    None
                } else {
                    let (from, sc) = self.join_tree(&Scope::default(), &ctes, 1);
                    let t = self.some_type(&sc);
                    let e = self.expr(&sc, &t, 1);
                    Some(format!("{with}SELECT {e} FROM {from}"))
                }
            }
            _ => None,
        };
        dml.unwrap_or_else(|| self.select(&Scope::default(), None, 0, false).sql)
    }
}
