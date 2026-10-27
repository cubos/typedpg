//! The machine's binary format: inputs, replies and steps.
//!
//! The format is hand-written so the module needs neither serde nor a
//! schema: a tag byte, little-endian `u32` lengths, and a null flag for an
//! optional string. The JS side (`typedpg_npm/src/core.ts`) mirrors it.
//! Everything below is UTF-8 text.
//!
//! # Input (`migrator_new`)
//!
//! ```text
//! u8 action                    0 Up, 1 Status, 2 Revert, 3 Accept
//!   Revert: optional string name, u8 force
//!   Accept: string name
//! string table
//! i64 lock_id
//! u8 use_transaction
//! u8 fail_on_drift
//! u32 migration count
//!   per migration: string name, string sql, optional string down_sql,
//!                  u8 no_transaction, u8 down_no_transaction
//! ```
//!
//! # Reply (`migrator_step`'s input)
//!
//! ```text
//! u8 tag                       0 Start, 1 Rows, 2 Done, 3 DbError
//!   Rows: u32 rows; per row: u32 cols; per value: optional string
//!   DbError: optional string code, string message, optional string severity,
//!            optional string detail, optional string hint,
//!            optional u32 position, optional u32 internal_position,
//!            optional string internal_query
//! ```
//!
//! # Step (`migrator_step`'s output)
//!
//! ```text
//! u8 tag                       0 Query, 1 Execute, 2 Sleep, 3 Warn,
//!                              4 Finish, 5 Fail
//!   Query:   string sql, u32 params; per param: optional string
//!   Execute: string sql
//!   Sleep:   u64 ms
//!   Warn:    string message
//!   Finish:  u8 outcome
//!              0 Applied:  u32 names; per name: string
//!              1 Status:   u32 rows; per row: string name, u8 applied,
//!                          optional string applied_at, u8 drifted
//!              2 Reverted: string name
//!              3 Accepted
//!   Fail:    u8 failure         0 Message: string, 1 Db
//! ```
//!
//! `string` is a `u32` byte length followed by that many UTF-8 bytes.
//! `optional X` is a `u8` tag (`0` absent, `1` present) followed by `X` when
//! present, so a null differs from an empty string.

use typedpg_core::migrations::machine::{
    Action, DbError, Failure, Input, MachineConfig, Migration, Outcome, Reply, Step,
};

/// Decode a `migrator_new` input.
pub fn decode_input(bytes: &[u8]) -> Result<Input, String> {
    let mut reader = Reader::new(bytes);
    let action = match reader.u8()? {
        0 => Action::Up,
        1 => Action::Status,
        2 => Action::Revert {
            name: reader.optional_string()?,
            force: reader.boolean()?,
        },
        3 => Action::Accept {
            name: reader.string()?,
        },
        tag => return Err(format!("typedpg_wasm: unknown migration action tag {tag}")),
    };
    let config = MachineConfig {
        table: reader.string()?,
        lock_id: reader.i64()?,
        use_transaction: reader.boolean()?,
        fail_on_drift: reader.boolean()?,
    };
    let count = reader.u32()?;
    let mut migrations = Vec::new();
    for _ in 0..count {
        migrations.push(Migration {
            name: reader.string()?,
            sql: reader.string()?,
            down_sql: reader.optional_string()?,
            no_transaction: reader.boolean()?,
            down_no_transaction: reader.boolean()?,
        });
    }
    reader.finish()?;
    Ok(Input {
        action,
        config,
        migrations,
    })
}

/// Decode a `migrator_step` reply.
pub fn decode_reply(bytes: &[u8]) -> Result<Reply, String> {
    let mut reader = Reader::new(bytes);
    let reply = match reader.u8()? {
        0 => Reply::Start,
        1 => {
            let count = reader.u32()?;
            let mut rows = Vec::new();
            for _ in 0..count {
                let columns = reader.u32()?;
                let mut row = Vec::new();
                for _ in 0..columns {
                    row.push(reader.optional_string()?);
                }
                rows.push(row);
            }
            Reply::Rows(rows)
        }
        2 => Reply::Done,
        3 => Reply::DbError(DbError {
            code: reader.optional_string()?,
            message: reader.string()?,
            severity: reader.optional_string()?,
            detail: reader.optional_string()?,
            hint: reader.optional_string()?,
            position: reader.optional_u32()?,
            internal_position: reader.optional_u32()?,
            internal_query: reader.optional_string()?,
        }),
        tag => return Err(format!("typedpg_wasm: unknown migration reply tag {tag}")),
    };
    reader.finish()?;
    Ok(reply)
}

/// Encode a `migrator_step` step.
pub fn encode_step(step: &Step) -> Vec<u8> {
    let mut writer = Writer::new();
    match step {
        Step::Query { sql, params } => {
            writer.u8(0);
            writer.string(sql);
            writer.u32(params.len() as u32);
            for param in params {
                writer.optional_string(param.as_deref());
            }
        }
        Step::Execute { sql } => {
            writer.u8(1);
            writer.string(sql);
        }
        Step::Sleep { ms } => {
            writer.u8(2);
            writer.u64(*ms);
        }
        Step::Warn(message) => {
            writer.u8(3);
            writer.string(message);
        }
        Step::Finish(outcome) => {
            writer.u8(4);
            match outcome {
                Outcome::Applied(names) => {
                    writer.u8(0);
                    writer.u32(names.len() as u32);
                    for name in names {
                        writer.string(name);
                    }
                }
                Outcome::Status(rows) => {
                    writer.u8(1);
                    writer.u32(rows.len() as u32);
                    for row in rows {
                        writer.string(&row.name);
                        writer.u8(u8::from(row.applied));
                        writer.optional_string(row.applied_at.as_deref());
                        writer.u8(u8::from(row.drifted));
                    }
                }
                Outcome::Reverted(name) => {
                    writer.u8(2);
                    writer.string(name);
                }
                Outcome::Accepted => writer.u8(3),
            }
        }
        Step::Fail(failure) => {
            writer.u8(5);
            match failure {
                Failure::Message(message) => {
                    writer.u8(0);
                    writer.string(message);
                }
                Failure::Db => writer.u8(1),
            }
        }
    }
    writer.bytes
}

/// A bounds-checked reader over the input.
pub(crate) struct Reader<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> Reader<'a> {
    pub(crate) fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, position: 0 }
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8], String> {
        let end = self
            .position
            .checked_add(len)
            .filter(|end| *end <= self.bytes.len())
            .ok_or_else(|| "typedpg_wasm: truncated migration machine input".to_string())?;
        let slice = &self.bytes[self.position..end];
        self.position = end;
        Ok(slice)
    }

    fn u8(&mut self) -> Result<u8, String> {
        Ok(self.take(1)?[0])
    }

    pub(crate) fn bytes(&mut self, len: usize) -> Result<&'a [u8], String> {
        self.take(len)
    }

    fn boolean(&mut self) -> Result<bool, String> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err("typedpg_wasm: invalid migration machine boolean".to_string()),
        }
    }

    pub(crate) fn u32(&mut self) -> Result<u32, String> {
        let bytes = self.take(4)?;
        Ok(u32::from_le_bytes(bytes.try_into().expect("four bytes")))
    }

    fn i64(&mut self) -> Result<i64, String> {
        let bytes = self.take(8)?;
        Ok(i64::from_le_bytes(bytes.try_into().expect("eight bytes")))
    }

    fn optional_u32(&mut self) -> Result<Option<u32>, String> {
        Ok(match self.u8()? {
            0 => None,
            1 => Some(self.u32()?),
            _ => return Err("typedpg_wasm: invalid migration machine option tag".to_string()),
        })
    }

    pub(crate) fn string(&mut self) -> Result<String, String> {
        let len = self.u32()? as usize;
        let bytes = self.take(len)?;
        String::from_utf8(bytes.to_vec())
            .map_err(|_| "typedpg_wasm: migration machine string is not UTF-8".to_string())
    }

    fn optional_string(&mut self) -> Result<Option<String>, String> {
        Ok(match self.u8()? {
            0 => None,
            1 => Some(self.string()?),
            _ => return Err("typedpg_wasm: invalid migration machine option tag".to_string()),
        })
    }

    /// Reject trailing bytes: an input the JS side did not mean to send is a
    /// bug, not a forward-compatible extension.
    pub(crate) fn finish(&self) -> Result<(), String> {
        if self.position == self.bytes.len() {
            Ok(())
        } else {
            Err("typedpg_wasm: trailing bytes in a migration machine input".to_string())
        }
    }
}

/// A writer for the output; lengths are `u32`, as in the reader.
pub(crate) struct Writer {
    bytes: Vec<u8>,
}

impl Writer {
    pub(crate) fn new() -> Self {
        Self { bytes: Vec::new() }
    }

    fn u8(&mut self, value: u8) {
        self.bytes.push(value);
    }

    pub(crate) fn u32(&mut self, value: u32) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    fn u64(&mut self, value: u64) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    pub(crate) fn string(&mut self, value: &str) {
        self.u32(value.len() as u32);
        self.bytes.extend_from_slice(value.as_bytes());
    }

    pub(crate) fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }

    fn optional_string(&mut self, value: Option<&str>) {
        match value {
            Some(value) => {
                self.u8(1);
                self.string(value);
            }
            None => self.u8(0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use typedpg_core::migrations::machine::StatusRow;

    fn input_bytes(action: u8) -> Writer {
        let mut writer = Writer::new();
        writer.u8(action);
        if action == 2 {
            writer.optional_string(Some("0001_a"));
            writer.u8(0);
        }
        if action == 3 {
            writer.string("0001_a");
        }
        writer.string("public._migrations");
        writer.bytes.extend_from_slice(&713705_i64.to_le_bytes());
        writer.u8(1);
        writer.u8(1);
        writer.u32(1);
        writer.string("0001_a");
        writer.string("CREATE TABLE a (id INT);");
        writer.optional_string(None);
        writer.u8(0);
        writer.u8(0);
        writer
    }

    #[test]
    fn a_machine_input_round_trips() {
        for (tag, action) in [
            (0, Action::Up),
            (1, Action::Status),
            (
                2,
                Action::Revert {
                    name: Some("0001_a".to_string()),
                    force: false,
                },
            ),
            (
                3,
                Action::Accept {
                    name: "0001_a".to_string(),
                },
            ),
        ] {
            let input = decode_input(&input_bytes(tag).bytes).unwrap();
            assert_eq!(input.action, action);
            assert_eq!(input.config.table, "public._migrations");
            assert_eq!(input.config.lock_id, 713705);
            assert!(input.config.use_transaction);
            assert!(input.config.fail_on_drift);
            assert_eq!(input.migrations.len(), 1);
            assert_eq!(input.migrations[0].name, "0001_a");
            assert_eq!(input.migrations[0].sql, "CREATE TABLE a (id INT);");
            assert_eq!(input.migrations[0].down_sql, None);
            assert!(!input.migrations[0].no_transaction);
        }
    }

    #[test]
    fn a_machine_reply_round_trips() {
        let mut writer = Writer::new();
        writer.u8(1);
        writer.u32(1);
        writer.u32(2);
        writer.optional_string(Some("0001_a"));
        writer.optional_string(None);
        assert_eq!(
            decode_reply(&writer.bytes).unwrap(),
            Reply::Rows(vec![vec![Some("0001_a".to_string()), None]])
        );

        let mut writer = Writer::new();
        writer.u8(3);
        writer.optional_string(Some("42601"));
        writer.string("syntax error");
        writer.optional_string(Some("ERROR"));
        writer.optional_string(None);
        writer.optional_string(Some("a hint"));
        writer.u8(1);
        writer.u32(34);
        writer.optional_string(None);
        writer.optional_string(None);
        let DbError {
            code,
            message,
            severity,
            hint,
            position,
            internal_position,
            ..
        } = match decode_reply(&writer.bytes).unwrap() {
            Reply::DbError(error) => error,
            other => panic!("expected a DbError, got {other:?}"),
        };
        assert_eq!(code.as_deref(), Some("42601"));
        assert_eq!(message, "syntax error");
        assert_eq!(severity.as_deref(), Some("ERROR"));
        assert_eq!(hint.as_deref(), Some("a hint"));
        assert_eq!(position, Some(34));
        assert_eq!(internal_position, None);
    }

    #[test]
    fn truncated_and_trailing_inputs_are_rejected() {
        let bytes = input_bytes(0).bytes;
        assert!(decode_input(&bytes[..bytes.len() - 1]).is_err());
        let mut trailing = bytes.clone();
        trailing.push(0);
        assert!(decode_input(&trailing).is_err());
        assert!(decode_reply(&[9]).is_err());
        assert!(decode_input(&[9]).is_err());
    }

    #[test]
    fn every_step_shape_encodes() {
        let steps = [
            Step::Query {
                sql: "SELECT ($1::pg_catalog.text)".to_string(),
                params: vec![Some("x".to_string()), None],
            },
            Step::Execute {
                sql: "BEGIN".to_string(),
            },
            Step::Sleep { ms: 20 },
            Step::Warn("warning: x".to_string()),
            Step::Finish(Outcome::Applied(vec!["0001_a".to_string()])),
            Step::Finish(Outcome::Status(vec![StatusRow {
                name: "0001_a".to_string(),
                applied: true,
                applied_at: Some("2026-01-02T03:04:05.000000Z".to_string()),
                drifted: false,
            }])),
            Step::Finish(Outcome::Reverted("0001_a".to_string())),
            Step::Finish(Outcome::Accepted),
            Step::Fail(Failure::Message("boom".to_string())),
            Step::Fail(Failure::Db),
        ];
        for step in steps {
            let encoded = encode_step(&step);
            assert_eq!(
                encoded[0],
                match &step {
                    Step::Query { .. } => 0,
                    Step::Execute { .. } => 1,
                    Step::Sleep { .. } => 2,
                    Step::Warn(_) => 3,
                    Step::Finish(_) => 4,
                    Step::Fail(_) => 5,
                }
            );
            assert!(!encoded.is_empty());
        }
    }
}
