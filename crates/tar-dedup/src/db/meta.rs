//! Typed access to the `meta` key/value table.
//!
//! Key strings are scoped by validity:
//! - one phase → phase slug (`tar_writer_…`, `scan_tar_…`)
//! - one command → command slug (`archive_…`, `extract_…`)
//! - both commands → `tar_dedup_…` (reserved; unused today)

// EnumDiscriminatns << needed for the simpler MetaKey nomenclature.
use chrono::{DateTime, Utc};
use rusqlite::Connection;

use crate::config::{ExtractPipelinePhase, PipelinePhase};
use crate::db::common::{delete_meta, get_meta, upsert_meta};
use crate::error::{Error, Result};

/// Closed set of known `meta.key` strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MetaKey {
    ArchivePhase,
    ArchiveSnapshotTakenAt,
    ArchiveMaxWorkers,
    TarWriterBytesIn,
    TarWriterBytesOut,
    ExtractPhase,
    ExtractSnapshotsIngested,
    ScanTarSawManifestDb,
    ScanTarSawAnyMembers,
    ScanTarComplete,
    ScanTarFromFooter,
    ScanTarLastMemberIndex,
}

impl MetaKey {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ArchivePhase => "archive_phase",
            Self::ArchiveSnapshotTakenAt => "archive_snapshot_taken_at",
            Self::ArchiveMaxWorkers => "archive_max_workers",
            Self::TarWriterBytesIn => "tar_writer_bytes_in",
            Self::TarWriterBytesOut => "tar_writer_bytes_out",
            Self::ExtractPhase => "extract_phase",
            Self::ExtractSnapshotsIngested => "extract_snapshots_ingested",
            Self::ScanTarSawManifestDb => "scan_tar_saw_manifest_db",
            Self::ScanTarSawAnyMembers => "scan_tar_saw_any_members",
            Self::ScanTarComplete => "scan_tar_complete",
            Self::ScanTarFromFooter => "scan_tar_from_footer",
            Self::ScanTarLastMemberIndex => "scan_tar_last_member_index",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        Some(match raw {
            "archive_phase" => Self::ArchivePhase,
            "archive_snapshot_taken_at" => Self::ArchiveSnapshotTakenAt,
            "archive_max_workers" => Self::ArchiveMaxWorkers,
            "tar_writer_bytes_in" => Self::TarWriterBytesIn,
            "tar_writer_bytes_out" => Self::TarWriterBytesOut,
            "extract_phase" => Self::ExtractPhase,
            "extract_snapshots_ingested" => Self::ExtractSnapshotsIngested,
            "scan_tar_saw_manifest_db" => Self::ScanTarSawManifestDb,
            "scan_tar_saw_any_members" => Self::ScanTarSawAnyMembers,
            "scan_tar_complete" => Self::ScanTarComplete,
            "scan_tar_from_footer" => Self::ScanTarFromFooter,
            "scan_tar_last_member_index" => Self::ScanTarLastMemberIndex,
            _ => return None,
        })
    }

    pub const fn all() -> &'static [Self] {
        &[
            Self::ArchivePhase,
            Self::ArchiveSnapshotTakenAt,
            Self::ArchiveMaxWorkers,
            Self::TarWriterBytesIn,
            Self::TarWriterBytesOut,
            Self::ExtractPhase,
            Self::ExtractSnapshotsIngested,
            Self::ScanTarSawManifestDb,
            Self::ScanTarSawAnyMembers,
            Self::ScanTarComplete,
            Self::ScanTarFromFooter,
            Self::ScanTarLastMemberIndex,
        ]
    }
}

/// A known meta row with its typed payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MetaEntry {
    ArchivePhase(PipelinePhase),
    ArchiveSnapshotTakenAt(DateTime<Utc>),
    ArchiveMaxWorkers(usize),
    TarWriterBytesIn(u64),
    TarWriterBytesOut(u64),
    ExtractPhase(ExtractPipelinePhase),
    ExtractSnapshotsIngested(u32),
    ScanTarSawManifestDb(bool),
    ScanTarSawAnyMembers(bool),
    ScanTarComplete(bool),
    ScanTarFromFooter(bool),
    ScanTarLastMemberIndex(u64),
}

impl MetaEntry {
    pub fn key(&self) -> MetaKey {
        match self {
            Self::ArchivePhase(_) => MetaKey::ArchivePhase,
            Self::ArchiveSnapshotTakenAt(_) => MetaKey::ArchiveSnapshotTakenAt,
            Self::ArchiveMaxWorkers(_) => MetaKey::ArchiveMaxWorkers,
            Self::TarWriterBytesIn(_) => MetaKey::TarWriterBytesIn,
            Self::TarWriterBytesOut(_) => MetaKey::TarWriterBytesOut,
            Self::ExtractPhase(_) => MetaKey::ExtractPhase,
            Self::ExtractSnapshotsIngested(_) => MetaKey::ExtractSnapshotsIngested,
            Self::ScanTarSawManifestDb(_) => MetaKey::ScanTarSawManifestDb,
            Self::ScanTarSawAnyMembers(_) => MetaKey::ScanTarSawAnyMembers,
            Self::ScanTarComplete(_) => MetaKey::ScanTarComplete,
            Self::ScanTarFromFooter(_) => MetaKey::ScanTarFromFooter,
            Self::ScanTarLastMemberIndex(_) => MetaKey::ScanTarLastMemberIndex,
        }
    }

    pub fn encode(&self) -> String {
        match self {
            Self::ArchivePhase(v) => v.as_str().to_string(),
            Self::ArchiveSnapshotTakenAt(v) => v.to_rfc3339(),
            Self::ArchiveMaxWorkers(v) => v.to_string(),
            Self::TarWriterBytesIn(v) | Self::TarWriterBytesOut(v) | Self::ScanTarLastMemberIndex(v) => {
                v.to_string()
            }
            Self::ExtractPhase(v) => v.as_str().to_string(),
            Self::ExtractSnapshotsIngested(v) => v.to_string(),
            Self::ScanTarSawManifestDb(v)
            | Self::ScanTarSawAnyMembers(v)
            | Self::ScanTarComplete(v)
            | Self::ScanTarFromFooter(v) => {
                if *v { "1" } else { "0" }.to_string()
            }
        }
    }

    pub fn decode(key: MetaKey, raw: &str) -> Result<Self> {
        match key {
            MetaKey::ArchivePhase => Ok(Self::ArchivePhase(PipelinePhase::parse(raw)?)),
            MetaKey::ArchiveSnapshotTakenAt => {
                let ts = raw.parse::<DateTime<Utc>>().map_err(|_| {
                    Error::Config(format!("invalid meta `{}`: {raw}", key.as_str()))
                })?;
                Ok(Self::ArchiveSnapshotTakenAt(ts))
            }
            MetaKey::ArchiveMaxWorkers => {
                let n = raw.parse::<usize>().map_err(|_| {
                    Error::Config(format!("invalid meta `{}`: {raw}", key.as_str()))
                })?;
                Ok(Self::ArchiveMaxWorkers(n))
            }
            MetaKey::TarWriterBytesIn => Ok(Self::TarWriterBytesIn(parse_u64(key, raw)?)),
            MetaKey::TarWriterBytesOut => Ok(Self::TarWriterBytesOut(parse_u64(key, raw)?)),
            MetaKey::ExtractPhase => Ok(Self::ExtractPhase(ExtractPipelinePhase::parse(raw)?)),
            MetaKey::ExtractSnapshotsIngested => {
                let n = raw.parse::<u32>().map_err(|_| {
                    Error::Config(format!("invalid meta `{}`: {raw}", key.as_str()))
                })?;
                Ok(Self::ExtractSnapshotsIngested(n))
            }
            MetaKey::ScanTarSawManifestDb => Ok(Self::ScanTarSawManifestDb(parse_bool(key, raw)?)),
            MetaKey::ScanTarSawAnyMembers => Ok(Self::ScanTarSawAnyMembers(parse_bool(key, raw)?)),
            MetaKey::ScanTarComplete => Ok(Self::ScanTarComplete(parse_bool(key, raw)?)),
            MetaKey::ScanTarFromFooter => Ok(Self::ScanTarFromFooter(parse_bool(key, raw)?)),
            MetaKey::ScanTarLastMemberIndex => {
                Ok(Self::ScanTarLastMemberIndex(parse_u64(key, raw)?))
            }
        }
    }
}

fn parse_bool(key: MetaKey, raw: &str) -> Result<bool> {
    match raw {
        "0" | "false" => Ok(false),
        "1" | "true" => Ok(true),
        other => Err(Error::Config(format!(
            "invalid bool meta `{}`: {other}",
            key.as_str()
        ))),
    }
}

fn parse_u64(key: MetaKey, raw: &str) -> Result<u64> {
    raw.parse().map_err(|_| {
        Error::Config(format!("invalid u64 meta `{}`: {raw}", key.as_str()))
    })
}

/// Run `f` inside an immediate transaction. `f` receives `&Connection` (the
/// transaction derefs to one) so typed setters work inside or outside a txn.
pub fn with_meta_txn<T>(conn: &Connection, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
    let tx = conn.unchecked_transaction()?;
    let out = f(&tx)?;
    tx.commit()?;
    Ok(out)
}

fn get_typed<T>(
    conn: &Connection,
    key: MetaKey,
    extract: impl FnOnce(MetaEntry) -> Option<T>,
) -> Result<Option<T>> {
    match get_meta(conn, key.as_str())? {
        None => Ok(None),
        Some(raw) => {
            let entry = MetaEntry::decode(key, &raw)?;
            extract(entry).ok_or_else(|| {
                Error::Config(format!(
                    "internal meta type mismatch for `{}`",
                    key.as_str()
                ))
            }).map(Some)
        }
    }
}

fn set_entry(conn: &Connection, entry: &MetaEntry) -> Result<()> {
    upsert_meta(conn, entry.key().as_str(), &entry.encode())
}

pub fn get_archive_phase(conn: &Connection) -> Result<Option<PipelinePhase>> {
    get_typed(conn, MetaKey::ArchivePhase, |e| match e {
        MetaEntry::ArchivePhase(v) => Some(v),
        _ => None,
    })
}

pub fn set_archive_phase(conn: &Connection, value: PipelinePhase) -> Result<()> {
    set_entry(conn, &MetaEntry::ArchivePhase(value))
}

pub fn get_archive_snapshot_taken_at(conn: &Connection) -> Result<Option<DateTime<Utc>>> {
    get_typed(conn, MetaKey::ArchiveSnapshotTakenAt, |e| match e {
        MetaEntry::ArchiveSnapshotTakenAt(v) => Some(v),
        _ => None,
    })
}

pub fn set_archive_snapshot_taken_at(conn: &Connection, value: DateTime<Utc>) -> Result<()> {
    set_entry(conn, &MetaEntry::ArchiveSnapshotTakenAt(value))
}

pub fn get_archive_max_workers(conn: &Connection) -> Result<Option<usize>> {
    get_typed(conn, MetaKey::ArchiveMaxWorkers, |e| match e {
        MetaEntry::ArchiveMaxWorkers(v) => Some(v),
        _ => None,
    })
}

pub fn set_archive_max_workers(conn: &Connection, value: usize) -> Result<()> {
    set_entry(conn, &MetaEntry::ArchiveMaxWorkers(value))
}

pub fn get_tar_writer_bytes_in(conn: &Connection) -> Result<Option<u64>> {
    get_typed(conn, MetaKey::TarWriterBytesIn, |e| match e {
        MetaEntry::TarWriterBytesIn(v) => Some(v),
        _ => None,
    })
}

pub fn set_tar_writer_bytes_in(conn: &Connection, value: u64) -> Result<()> {
    set_entry(conn, &MetaEntry::TarWriterBytesIn(value))
}

pub fn get_tar_writer_bytes_out(conn: &Connection) -> Result<Option<u64>> {
    get_typed(conn, MetaKey::TarWriterBytesOut, |e| match e {
        MetaEntry::TarWriterBytesOut(v) => Some(v),
        _ => None,
    })
}

pub fn set_tar_writer_bytes_out(conn: &Connection, value: u64) -> Result<()> {
    set_entry(conn, &MetaEntry::TarWriterBytesOut(value))
}

pub fn get_extract_phase(conn: &Connection) -> Result<Option<ExtractPipelinePhase>> {
    get_typed(conn, MetaKey::ExtractPhase, |e| match e {
        MetaEntry::ExtractPhase(v) => Some(v),
        _ => None,
    })
}

pub fn set_extract_phase(conn: &Connection, value: ExtractPipelinePhase) -> Result<()> {
    set_entry(conn, &MetaEntry::ExtractPhase(value))
}

pub fn get_extract_snapshots_ingested(conn: &Connection) -> Result<Option<u32>> {
    get_typed(conn, MetaKey::ExtractSnapshotsIngested, |e| match e {
        MetaEntry::ExtractSnapshotsIngested(v) => Some(v),
        _ => None,
    })
}

pub fn set_extract_snapshots_ingested(conn: &Connection, value: u32) -> Result<()> {
    set_entry(conn, &MetaEntry::ExtractSnapshotsIngested(value))
}

pub fn get_scan_tar_saw_manifest_db(conn: &Connection) -> Result<Option<bool>> {
    get_typed(conn, MetaKey::ScanTarSawManifestDb, |e| match e {
        MetaEntry::ScanTarSawManifestDb(v) => Some(v),
        _ => None,
    })
}

pub fn set_scan_tar_saw_manifest_db(conn: &Connection, value: bool) -> Result<()> {
    set_entry(conn, &MetaEntry::ScanTarSawManifestDb(value))
}

pub fn get_scan_tar_saw_any_members(conn: &Connection) -> Result<Option<bool>> {
    get_typed(conn, MetaKey::ScanTarSawAnyMembers, |e| match e {
        MetaEntry::ScanTarSawAnyMembers(v) => Some(v),
        _ => None,
    })
}

pub fn set_scan_tar_saw_any_members(conn: &Connection, value: bool) -> Result<()> {
    set_entry(conn, &MetaEntry::ScanTarSawAnyMembers(value))
}

pub fn get_scan_tar_complete(conn: &Connection) -> Result<Option<bool>> {
    get_typed(conn, MetaKey::ScanTarComplete, |e| match e {
        MetaEntry::ScanTarComplete(v) => Some(v),
        _ => None,
    })
}

pub fn set_scan_tar_complete(conn: &Connection, value: bool) -> Result<()> {
    set_entry(conn, &MetaEntry::ScanTarComplete(value))
}

pub fn get_scan_tar_from_footer(conn: &Connection) -> Result<Option<bool>> {
    get_typed(conn, MetaKey::ScanTarFromFooter, |e| match e {
        MetaEntry::ScanTarFromFooter(v) => Some(v),
        _ => None,
    })
}

pub fn set_scan_tar_from_footer(conn: &Connection, value: bool) -> Result<()> {
    set_entry(conn, &MetaEntry::ScanTarFromFooter(value))
}

pub fn get_scan_tar_last_member_index(conn: &Connection) -> Result<Option<u64>> {
    get_typed(conn, MetaKey::ScanTarLastMemberIndex, |e| match e {
        MetaEntry::ScanTarLastMemberIndex(v) => Some(v),
        _ => None,
    })
}

pub fn set_scan_tar_last_member_index(conn: &Connection, value: u64) -> Result<()> {
    set_entry(conn, &MetaEntry::ScanTarLastMemberIndex(value))
}

pub fn delete_scan_tar_last_member_index(conn: &Connection) -> Result<()> {
    delete_meta(conn, MetaKey::ScanTarLastMemberIndex.as_str())
}

// TODO Delete the entirety of the archive keys.
/// Drop tar-writer byte counters. Archive/extract phase keys are left standing.
pub fn clear_archive_meta(conn: &Connection) -> Result<()> {
    with_meta_txn(conn, |conn| {
        delete_meta(conn, MetaKey::TarWriterBytesIn.as_str())?;
        delete_meta(conn, MetaKey::TarWriterBytesOut.as_str())?;
        Ok(())
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MetaDump {
    pub known: Vec<MetaEntry>,
    pub unknown_keys: Vec<(String, String)>,
    pub invalid: Vec<(String, String, String)>,
}

/// Classify every `meta` row: known+parseable, unknown key, or known+unparseable.
pub fn dump_meta(conn: &Connection) -> Result<MetaDump> {
    let mut stmt = conn.prepare("SELECT key, value FROM meta ORDER BY key")?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;

    let mut dump = MetaDump::default();
    for row in rows {
        let (key, value) = row?;
        match MetaKey::parse(&key) {
            None => dump.unknown_keys.push((key, value)),
            Some(meta_key) => match MetaEntry::decode(meta_key, &value) {
                Ok(entry) => dump.known.push(entry),
                Err(e) => dump.invalid.push((key, value, e.to_string())),
            },
        }
    }
    Ok(dump)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn open_conn() -> (tempfile::TempDir, Connection) {
        let dir = tempfile::tempdir().expect("tempdir");
        let conn = Connection::open(dir.path().join("meta.sqlite")).expect("open");
        super::super::schema::initialize(&conn).expect("schema");
        (dir, conn)
    }

    #[test]
    fn meta_key_round_trips() {
        for key in MetaKey::all() {
            assert_eq!(MetaKey::parse(key.as_str()), Some(*key));
        }
        assert_eq!(MetaKey::parse("phase"), None);
        assert_eq!(MetaKey::parse("archive_bytes_in"), None);
    }

    #[test]
    fn typed_set_get_and_dump() {
        let (_dir, conn) = open_conn();

        let ts = Utc.with_ymd_and_hms(2026, 1, 2, 3, 4, 5).unwrap();
        set_archive_phase(&conn, PipelinePhase::Hash).unwrap();
        set_archive_snapshot_taken_at(&conn, ts).unwrap();
        set_archive_max_workers(&conn, 8).unwrap();
        set_tar_writer_bytes_in(&conn, 11).unwrap();
        set_tar_writer_bytes_out(&conn, 22).unwrap();
        set_extract_phase(&conn, ExtractPipelinePhase::Place).unwrap();
        set_extract_snapshots_ingested(&conn, 3).unwrap();
        set_scan_tar_saw_manifest_db(&conn, true).unwrap();
        set_scan_tar_last_member_index(&conn, 7).unwrap();

        assert_eq!(get_archive_phase(&conn).unwrap(), Some(PipelinePhase::Hash));
        assert_eq!(get_archive_snapshot_taken_at(&conn).unwrap(), Some(ts));
        assert_eq!(get_archive_max_workers(&conn).unwrap(), Some(8));
        assert_eq!(get_tar_writer_bytes_in(&conn).unwrap(), Some(11));
        assert_eq!(get_tar_writer_bytes_out(&conn).unwrap(), Some(22));
        assert_eq!(
            get_extract_phase(&conn).unwrap(),
            Some(ExtractPipelinePhase::Place)
        );
        assert_eq!(get_extract_snapshots_ingested(&conn).unwrap(), Some(3));
        assert_eq!(get_scan_tar_saw_manifest_db(&conn).unwrap(), Some(true));
        assert_eq!(get_scan_tar_last_member_index(&conn).unwrap(), Some(7));

        delete_scan_tar_last_member_index(&conn).unwrap();
        assert_eq!(get_scan_tar_last_member_index(&conn).unwrap(), None);

        upsert_meta(&conn, "mystery", "x").unwrap();
        upsert_meta(&conn, MetaKey::ArchivePhase.as_str(), "not-a-phase").unwrap();

        let dump = dump_meta(&conn).unwrap();
        assert!(dump.unknown_keys.iter().any(|(k, v)| k == "mystery" && v == "x"));
        assert!(dump
            .invalid
            .iter()
            .any(|(k, v, _)| k == "archive_phase" && v == "not-a-phase"));
        assert!(dump
            .known
            .iter()
            .any(|e| matches!(e, MetaEntry::TarWriterBytesIn(11))));
    }

    #[test]
    fn clear_archive_meta_drops_only_byte_counters() {
        let (_dir, conn) = open_conn();

        set_archive_phase(&conn, PipelinePhase::Archive).unwrap();
        set_tar_writer_bytes_in(&conn, 1).unwrap();
        set_tar_writer_bytes_out(&conn, 2).unwrap();
        set_extract_phase(&conn, ExtractPipelinePhase::ScanTar).unwrap();

        clear_archive_meta(&conn).unwrap();

        assert_eq!(get_tar_writer_bytes_in(&conn).unwrap(), None);
        assert_eq!(get_tar_writer_bytes_out(&conn).unwrap(), None);
        assert_eq!(
            get_archive_phase(&conn).unwrap(),
            Some(PipelinePhase::Archive)
        );
        assert_eq!(
            get_extract_phase(&conn).unwrap(),
            Some(ExtractPipelinePhase::ScanTar)
        );
    }
}
