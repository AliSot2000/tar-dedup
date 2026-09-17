use crate::config::RuntimeState;
use crate::db::flags::ErrorFlags;
use crate::db::flags::FileFlag;
use crate::db::types::{FileId, NewFileRecord};
use crate::db::{ErrorPhase, Recorder, meta};
use crate::error::{FileStatError, Result, ToPanic};
use path_clean::PathClean;
use rusqlite::{Connection, OptionalExtension, named_params};
use std::iter::zip;
use std::path::{Path, PathBuf};
use tracing;

const ERROR_PHASE: ErrorPhase = ErrorPhase::Pipeline(crate::config::PipelinePhase::Inventory);

pub fn file_id_by_abs_path(conn: &Connection, path: &Path) -> Result<Option<FileId>> {
    conn.query_row(
        "SELECT id FROM files WHERE abs_path = :abs_path",
        named_params! { ":abs_path": path.to_string_lossy() },
        |row| row.get(0).map(FileId),
    )
    .optional()
    .to_panic()
    .map_err(Into::into)
}

pub fn abs_path_exists(conn: &Connection, path: &Path) -> Result<bool> {
    Ok(file_id_by_abs_path(conn, path)?.is_some())
}

pub fn insert_file(conn: &Connection, record: &NewFileRecord) -> Result<bool> {
    debug_assert!(record.abs_path.is_absolute(), "Only abs_paths allowed in db");
    debug_assert_eq!(record.abs_path,
                     record.abs_path.to_path_buf().clean(),
                     "Paths must be normalized to enter the db");

    let changed = conn.execute(
        "INSERT OR IGNORE INTO files (
             abs_path, ext, size, mtime, atime, ctime, uid, gid, mode, ftype,
             xattr, acl, selinux, phase, link_dst, inode, dev, major, minor,
             win_perm
         ) VALUES (
             :abs_path, :ext, :size, :mtime, :atime, :ctime, :uid, :gid, :mode, :ftype,
             :xattr, :acl, :selinux, 'inventoried', :link_dst, :inode, :dev, :major, :minor,
             :win_perm
         )",
        named_params! {
            ":abs_path": record.abs_path.to_string_lossy(),
            ":ext": record.ext.as_str(),
            ":size": record.size,
            ":mtime": record.mtime.as_ref().map(|t| t.to_rfc3339()),
            ":atime": record.atime.as_ref().map(|t| t.to_rfc3339()),
            ":ctime": record.ctime.as_ref().map(|t| t.to_rfc3339()),
            ":uid": record.uid,
            ":gid": record.gid,
            ":mode": record.mode,
            ":ftype": record.ftype.map(|t| t.as_str()),
            ":xattr": record.xattrs.as_deref(),
            ":acl": record.posix_acl.as_deref(),
            ":selinux": record.selinux_ctx.as_deref(),
            ":link_dst": record
                .link_dst
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned()),
            ":inode": record.inode_id.map(|v| v as i64),
            ":dev": record.device_id.map(|v| v as i64),
            ":major": record.major.map(|v| v as i64),
            ":minor": record.minor.map(|v| v as i64),
            ":win_perm": record.win_perm.as_deref(),
        },
    ).to_panic()?;
    Ok(changed > 0)
}

pub fn load_runtime_state(conn: &Connection) -> Result<Option<RuntimeState>> {
    let Some(phase) = meta::get_archive_phase(conn)? else {
        return Ok(None);
    };
    let max_workers = meta::get_archive_max_workers(conn)?
        .ok_or_else(|| crate::error::Error::Config("missing archive_max_workers in meta".into()))
        .to_panic()?;
    let snapshot_taken_at = meta::get_archive_snapshot_taken_at(conn)?
        .ok_or_else(|| {
            crate::error::Error::Config("missing archive_snapshot_taken_at in meta".into())
        })
        .to_panic()?;

    Ok(Some(RuntimeState {
        snapshot_taken_at,
        phase,
        max_workers,
    }))
}

pub fn save_runtime_state(conn: &mut Connection, state: &RuntimeState) -> Result<()> {
    meta::with_meta_txn(conn, |conn| {
        meta::set_archive_phase(conn, state.phase)?;
        meta::set_archive_snapshot_taken_at(conn, state.snapshot_taken_at)?;
        meta::set_archive_max_workers(conn, state.max_workers)?;
        Ok(())
    })
}

fn get_all_uids(conn: &Connection) -> Result<Vec<u32>> {
    let mut stmt = conn.prepare("SELECT DISTINCT uid FROM files WHERE uid IS NOT NULL").to_panic()?;
    let rows = stmt.query_map([], |row| {
        let uid: u32 = row.get(0)?;
        Ok(uid)
    }).to_panic()?;
    rows.collect::<rusqlite::Result<Vec<_>>>().to_panic().map_err(Into::into)
}

fn get_all_gids(conn: &Connection) -> Result<Vec<u32>> {
    let mut stmt = conn.prepare("SELECT DISTINCT gid FROM files WHERE gid is NOT NULL").to_panic()?;
    let rows = stmt.query_map([], |row| {
        let gid: u32 = row.get(0)?;
        Ok(gid)
    }).to_panic()?;
    rows.collect::<rusqlite::Result<Vec<_>>>().to_panic().map_err(Into::into)
}

/// Update all rows where uid matches given `uid` and set username column to `uname`
fn set_uname_from_uid(conn: &Connection, uid: &u32, uname: &str) -> Result<()> {
    conn.execute(
        "UPDATE files SET username = :username WHERE uid = :uid",
        named_params! {
            ":uid": uid,
            ":username": uname,
        },
    ).to_panic()?;
    Ok(())
}

/// Update all rows where gid matches given `gid` and set groupname column to `gname`
fn set_gname_from_gid(conn: &Connection, gid: &u32, gname: &str) -> Result<()> {
    conn.execute(
        "UPDATE files SET groupname = :groupname WHERE gid = :gid",
        named_params! {
            ":gid": gid,
            ":groupname": gname,
        },
    ).to_panic()?;
    Ok(())
}

/// Set the canonical id for tuples of (dev, inode)
pub fn set_hardlink_canonicals(conn: &Connection) -> Result<u64> {
    let changes = conn.execute(
        "UPDATE files
         SET flags = flags | :flag
         WHERE id IN (
             SELECT MIN(id)
             FROM files
             WHERE dev IS NOT NULL AND inode IS NOT NULL
             GROUP BY dev, inode
         )",
        named_params! {
            ":flag": FileFlag::FileHardlinkCanonical.mask_i64(),
        }).to_panic()?;
    Ok(changes as u64)
}

/// Clear the entire files table.
pub fn purge_entries(conn: &Connection) -> Result<u64> {
    let rows = conn.execute("DELETE * FROM files", []).to_panic()?;
    Ok(rows as u64)
}

#[cfg(unix)]
pub fn resolve_numeric_ids(conn: &Connection, recorder: &mut Recorder) -> Result<()> {
    use nix::libc::{gid_t, uid_t};
    use nix::unistd::{Gid, Group, Uid, User};

    // Get all present uids and gids
    let uids = get_all_uids(&conn)?;
    let gids = get_all_gids(&conn)?;

    let mut capture_error = |error| {
        recorder.record_session(
            ERROR_PHASE,
            FileStatError::Nix {path: PathBuf::new(), source: error},
            ErrorFlags::default());
    };

    // Resolve uids and gids.
    let resolves_names: Vec<Option<String>> = uids
        .iter()
        .map(|uid| {
            let resolved_user = match User::from_uid(Uid::from_raw(uid.clone() as uid_t)) {
                Ok(u) => u.map(|u| u.name),
                Err(e) => {
                    tracing::error!("Error while resolving uid {uid}: {e}");
                    capture_error(e);
                    None
                }
            };
            resolved_user
        }).collect();
    let resolved_groups: Vec<Option<String>> = gids
        .iter()
        .map(|gid| {
            let resolved_group = match Group::from_gid(Gid::from_raw(gid.clone() as gid_t)) {
                Ok(g) => g.map(|g| g.name),
                Err(e) => {
                    tracing::error!("Error while resolving gid {gid}: {e}");
                    capture_error(e);
                    None
                }
            };
            resolved_group
        }).collect();

    // Set the names now from lookup array.
    for (uid, o_uname) in zip(uids.iter(), resolves_names.iter()) {
        if o_uname.is_none() {
            tracing::warn!("Could not resolve {uid} to username");
            continue;
        }
        set_uname_from_uid(&conn, uid, o_uname.as_ref().unwrap())?;
    }
    for (gid, o_gname) in zip(gids.iter(), resolved_groups.iter()) {
        if o_gname.is_none() {
            tracing::warn!("Could not resolve {gid} to groupname");
            continue;
        }
        set_gname_from_gid(&conn, gid, o_gname.as_ref().unwrap())?;
    }
    Ok(())
}

#[cfg(not(unix))]
pub fn resolve_numeric_ids(_conn: &Connection) -> Result<()> {
    // No POSIX uid/gid namespace on this platform; owner resolution can be
    // wired to Windows SIDs at a later point.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;
    use crate::db::flags::{SourceFlag, SourceFlags};
    use crate::db::types::{FileRecord, FileType};
    use std::path::{Path, PathBuf};

    fn record(path: &str) -> NewFileRecord {
        NewFileRecord {
            abs_path: PathBuf::from(path),
            ext: String::new(),
            size: 1,
            mtime: None,
            atime: None,
            ctime: None,
            uid: None,
            gid: None,
            mode: None,
            ftype: None,
            xattrs: None,
            posix_acl: None,
            selinux_ctx: None,
            win_perm: None,
            link_dst: None,
            device_id: None,
            inode_id: None,
            major: None,
            minor: None,
        }
    }

    fn dir_flags() -> SourceFlags {
        SourceFlags::default().with(SourceFlag::IsDirectory, true)
    }

    #[test]
    fn two_sources_share_one_file_row() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(&dir.path().join("t.sqlite")).unwrap();
        let s1 = db
            .add_get_source(Path::new("/root-a"), "--input-dir", Some(0), None, dir_flags())
            .unwrap();
        let s2 = db
            .add_get_source(Path::new("/root-b"), "--input-dir", Some(1), None, dir_flags())
            .unwrap();
        let rec = record("/shared/file.txt");
        assert!(db.insert_file_and_ref(s1, &rec).unwrap());
        let id = db.file_id_by_abs_path(Path::new("/shared/file.txt")).unwrap().unwrap();
        assert!(db.add_ref(s2, id).unwrap());
        assert!(!db.add_ref(s2, id).unwrap());
        assert_eq!(
            db.file_id_by_abs_path(Path::new("/shared/file.txt"))
                .unwrap()
                .unwrap(),
            id
        );
    }

    #[test]
    fn major_minor_null_for_regular_file() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(&dir.path().join("t.sqlite")).unwrap();
        assert!(db.insert_file(&record("/plain.txt")).unwrap());
        let id = db.file_id_by_abs_path(Path::new("/plain.txt")).unwrap().unwrap();
        let loaded: FileRecord = db.get_file_by_id(id).unwrap().expect("row");
        assert_eq!(loaded.major, None);
        assert_eq!(loaded.minor, None);
    }

    #[test]
    fn major_minor_round_trip_for_char_device() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(&dir.path().join("t.sqlite")).unwrap();
        let mut rec = record("/dev/nullish");
        rec.ftype = Some(FileType::CharacterDevice);
        rec.major = Some(1);
        rec.minor = Some(3);
        rec.device_id = Some(42);
        rec.inode_id = Some(99);
        assert!(db.insert_file(&rec).unwrap());
        let id = db
            .file_id_by_abs_path(Path::new("/dev/nullish"))
            .unwrap()
            .unwrap();
        let loaded: FileRecord = db.get_file_by_id(id).unwrap().expect("row");
        assert_eq!(loaded.ftype, FileType::CharacterDevice);
        assert_eq!(loaded.major, Some(1));
        assert_eq!(loaded.minor, Some(3));
        assert_eq!(loaded.device_id, Some(42));
        assert_eq!(loaded.inode_id, Some(99));
    }
}
