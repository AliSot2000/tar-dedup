//! Persistent, resumable error log (`errors` table).
//!
//! One row per failed filesystem operation. Ties to `files` and/or `out_tree` where the
//! failing entry is known. Session/config-scoped errors (no file/out_tree) are flagged
//! `ErrorFlag::SessionError`. Insertion is batched: a `Recorder` accumulates
//! [`RecordDraft`]s and flushes them in a single transaction at a batch/phase boundary.
//! `--no-errors` gates recording via `Recorder::enabled`.

use chrono::{DateTime, ParseError, Utc};
use rusqlite::{Connection, OptionalExtension, named_params};
use std::path::PathBuf;

use crate::config::{ExtractPipelinePhase, PipelinePhase};
use crate::db::flags::{ErrorFlags, ErrorScope};
use crate::db::types::{FileId, OutTreeId};
use crate::error::{FileStatError, Result};

/// Phase during which an error was recorded (archive or extract pipeline).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorPhase {
    Pipeline(PipelinePhase),
    Extract(ExtractPipelinePhase),
}

impl ErrorPhase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pipeline(p) => p.as_str(),
            Self::Extract(p) => p.as_str(),
        }
    }

    pub fn parse(raw: &str) -> Result<Self> {
        if let Ok(p) = PipelinePhase::parse(raw) {
            return Ok(Self::Pipeline(p));
        }
        if let Ok(p) = ExtractPipelinePhase::parse(raw) {
            return Ok(Self::Extract(p));
        }
        Err(crate::error::Error::Config(format!("unknown error phase: {raw}")))
    }
}

/// A to-be-inserted error row.
#[derive(Debug)]
pub struct RecordDraft {
    pub file_id: Option<FileId>,
    pub out_tree_id: Option<OutTreeId>,
    pub phase: ErrorPhase,
    pub error: FileStatError,
    /// Reserved bits ([`ErrorFlag`]), e.g. `SessionError` for session-scoped errors.
    pub flags: ErrorFlags,
}

impl RecordDraft {
    /// The absolute path the error is about, if derivable from the error.
    pub fn abs_path(&self) -> Option<PathBuf> {
        self.error.io_path()
    }
}

/// Extract the structured `error_misc` JSON (only when the error carries more than
/// `(msg, type, path)`), or `None`.
fn error_misc(_e: &FileStatError) -> Option<String> {
    None
}

fn error_msg(e: &FileStatError) -> String {
    match e {
        FileStatError::General { message, .. } => message.clone(),
        other => other.to_string(),
    }
}

/// Insert a batch of drafts in a single transaction. Returns rows inserted.
/// Borrows the drafts: on failure (rollback) they are left untouched so the
/// caller may retain and retry them.
pub fn insert_errors(conn: &mut Connection, drafts: &[RecordDraft]) -> Result<u64> {
    let count = drafts.len();
    if count == 0 {
        return Ok(0);
    }
    let now = Utc::now();
    let tx = conn.transaction()?;
    let mut stmt = tx.prepare_cached(
        "INSERT INTO errors (
             file_id, out_tree_id, abs_path, error_msg, error_type, phase,
             error_misc, error_datetime, flags
         ) VALUES (
             :file_id, :out_tree_id, :abs_path, :error_msg, :error_type, :phase,
             :error_misc, :error_datetime, :flags
         )")?;
    for draft in drafts {
        stmt.execute(named_params! {
            ":file_id": draft.file_id.map(|f| f.0),
            ":out_tree_id": draft.out_tree_id.map(|o| o.0),
            ":abs_path": draft
                .abs_path()
                .map(|p| p.to_string_lossy().into_owned()),
            ":error_msg": error_msg(&draft.error),
            ":error_type": draft.error.kind(),
            ":phase": draft.phase.as_str(),
            ":error_misc": error_misc(&draft.error),
            ":error_datetime": now.to_rfc3339(),
            ":flags": draft.flags.to_i64(),
        })?;
    }
    drop(stmt);
    tx.commit()?;
    Ok(count as u64)
}

/// A materialized `errors` row.
#[derive(Debug, Clone)]
pub struct ErrorRecord {
    pub id: i64,
    pub file_id: Option<FileId>,
    pub out_tree_id: Option<OutTreeId>,
    pub abs_path: Option<PathBuf>,
    pub error_msg: String,
    pub error_type: String,
    pub phase: ErrorPhase,
    pub error_misc: Option<String>,
    pub error_datetime: DateTime<Utc>,
    pub flags: ErrorFlags,
}

const ERROR_COLUMNS: &str =
    "id, file_id, out_tree_id, abs_path, error_msg, error_type, phase, \
     error_misc, error_datetime, flags";

/// Fetch a single error row.
pub fn get_record_by_id(conn: &Connection, id: i64) -> Result<Option<ErrorRecord>> {
    conn.query_row(
        &format!("SELECT {ERROR_COLUMNS} FROM errors WHERE id = :id"),
        named_params! { ":id": id },
        parse_row,
    )
    .optional()
    .map_err(Into::into)
}

/// All error rows bound to a file.
/// (`// INFO:` a file is expected to have far fewer than 100 errors, so this fits in RAM.)
pub fn get_records_by_file_id(conn: &Connection, file_id: FileId) -> Result<Vec<ErrorRecord>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {ERROR_COLUMNS} FROM errors WHERE file_id = :file_id ORDER BY id"
    ))?;
    let rows = stmt.query_map(
        named_params! { ":file_id": file_id.0 },
        parse_row,
    )?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
}

/// All error rows bound to an out_tree row.
pub fn get_records_by_out_tree_id(
    conn: &Connection, out_tree_id: OutTreeId) -> Result<Vec<ErrorRecord>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {ERROR_COLUMNS} FROM errors WHERE out_tree_id = :out_tree_id ORDER BY id"
    ))?;
    let rows = stmt.query_map(
        named_params! { ":out_tree_id": out_tree_id.0 },
        parse_row,
    )?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
}

/// List error rows, batched. `scope` is an OR-able bitset over File/OutTree/Session;
/// empty = everything. `reemit` filters `errors.flags` when `Some((on, mask))`.
pub fn list_records(
    conn: &Connection,
    scope: ErrorScope,
    reemit: Option<(bool, i64)>,
    last_id: i64,
    batch_size: u64,
) -> Result<Vec<ErrorRecord>> {
    let (scope_clause, reemit_clause) = build_filters(scope, reemit);
    let mut stmt = conn.prepare(&format!(
        "SELECT {ERROR_COLUMNS} FROM errors
         WHERE id > :last_id {scope_clause} {reemit_clause}
         ORDER BY id LIMIT :batch_size"
    ))?;
    let rows = stmt.query_map(
        named_params! { ":last_id": last_id, ":batch_size": batch_size },
        parse_row,
    )?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
}

/// Count error rows matching scope + flag filter.
pub fn count_records(
    conn: &Connection,
    scope: ErrorScope,
    reemit: Option<(bool, i64)>,
) -> Result<u64> {
    let (scope_clause, reemit_clause) = build_filters(scope, reemit);
    let n: i64 = conn.query_row(
        &format!("SELECT COUNT(*) FROM errors WHERE 1 = 1 {scope_clause} {reemit_clause}"),
        [],
        |r| r.get(0),
    )?;
    Ok(n as u64)
}

/// Build SQL fragments (SQLite booleans + bits are inlined as integer literals).
fn build_filters(scope: ErrorScope, reemit: Option<(bool, i64)>) -> (String, String) {
    let bits = scope.bits();
    // Empty bitset (all zero) → every partition matches.
    let scope_clause = if bits == 0 {
        String::new()
    } else {
        let f = (bits >> 0) & 1;
        let o = (bits >> 1) & 1;
        let s = (bits >> 2) & 1;
        format!(
            "AND 1 = ({f} AND file_id IS NOT NULL) \
               OR 1 = ({o} AND out_tree_id IS NOT NULL) \
               OR 1 = ({s} AND file_id IS NULL AND out_tree_id IS NULL)"
        )
    };
    let reemit_clause = match reemit {
        None => String::new(),
        Some((true, mask)) => format!("AND (flags & {mask}) != 0"),
        Some((false, mask)) => format!("AND (flags & {mask}) = 0"),
    };
    (scope_clause, reemit_clause)
}

fn parse_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ErrorRecord> {
    let phase = ErrorPhase::parse(row.get::<_, String>("phase")?.as_ref());
    let datetime = DateTime::parse_from_rfc3339(row.get::<_, String>("error_datetime")?.as_ref());
    Ok(ErrorRecord {
        id: row.get::<_, i64>("id")?,
        file_id: row.get::<_, Option<i64>>("file_id")?.map(FileId),
        out_tree_id: row.get::<_, Option<i64>>("out_tree_id")?.map(OutTreeId),
        abs_path: row.get::<_, Option<String>>("abs_path")?.map(PathBuf::from),
        error_msg: row.get::<_, String>("error_msg")?,
        error_type: row.get::<_, String>("error_type")?,
        phase: phase.map_err(sql_parse_error)?,
        error_misc: row.get::<_, Option<String>>("error_misc")?,
        error_datetime: datetime
            .map(|dt| dt.with_timezone(&Utc))
            .map_err(sql_datetime_error)?,
        flags: ErrorFlags::from_i64(row.get::<_, i64>("flags")?),
    })
}

fn sql_parse_error(e: crate::error::Error) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        0,
        rusqlite::types::Type::Text,
        Box::new(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            e.to_string(),
        )),
    )
}

fn sql_datetime_error(e: ParseError) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        0,
        rusqlite::types::Type::Text,
        Box::new(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            e.to_string(),
        )),
    )
}
