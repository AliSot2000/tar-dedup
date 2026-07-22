use std::iter::zip;
use nix::libc::{gid_t, uid_t};
use rusqlite::{named_params, Connection, OptionalExtension};

use crate::config::{PipelinePhase, RuntimeState};
use crate::db::common::{upsert_meta, SqlFileRow};
use crate::db::types::{FileId, FilePhase, NewFileRecord};
use crate::error::Result;
use nix::unistd::{Gid, Group, Uid, User};

pub fn insert_file(conn: &Connection, record: &NewFileRecord) -> Result<bool> {
    let changed = conn.execute(
        "INSERT OR IGNORE INTO files (
             rel_path, size, mtime, atime, ctime, uid, gid, mode, ftype,
             xattr, acl, selinux, phase
         ) VALUES (
             :rel_path, :size, :mtime, :atime, :ctime, :uid, :gid, :mode, :ftype,
             :xattr, :acl, :selinux, 'inventoried'
         )",
        named_params! {
            ":rel_path": record.rel_path.to_string_lossy(),
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
        },
    )?;
    Ok(changed > 0)
}

pub fn count_files(conn: &Connection) -> Result<u64> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) AS count FROM files",
        [],
        |row| row.get("count"),
    )?;
    Ok(count as u64)
}

pub fn list_files_in_phase<R: SqlFileRow>(
    conn: &Connection,
    phase: FilePhase,
) -> Result<Vec<R>> {
    let cols = R::sql_columns();
    let mut stmt = conn.prepare(&format!(
        "SELECT {cols} FROM files WHERE phase = :phase ORDER BY id"
    ))?;

    let rows = stmt.query_map(
        named_params! { ":phase": phase.as_str() },
        R::from_row,
    )?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
}

// pub fn get_file_by_tar_path(conn: &Connection, tar_path: &str) -> Result<Option<FileRecord>> {
//     let mut stmt = conn.prepare(&format!(
//         "SELECT {FILES_SELECT} FROM files WHERE tar_path = :tar_path LIMIT 1"
//     ))?;
//     let mut rows = stmt.query(named_params! { ":tar_path": tar_path })?;
//     if let Some(row) = rows.next()? {
//         return Ok(Some(map_file_record(row)?));
//     }
//     Ok(None)
// }

// pub fn set_tar_path(conn: &Connection, file_id: FileId, tar_path: &str) -> Result<()> {
//     conn.execute(
//         "UPDATE files SET tar_path = :tar_path WHERE id = :id",
//         named_params! {
//             ":tar_path": tar_path,
//             ":id": file_id.0,
//         },
//     )?;
//     Ok(())
// }

pub fn mark_phase(conn: &Connection, file_id: FileId, phase: FilePhase) -> Result<()> {
    conn.execute(
        "UPDATE files SET phase = :phase WHERE id = :id",
        named_params! {
            ":phase": phase.as_str(),
            ":id": file_id.0,
        },
    )?;
    Ok(())
}

pub fn load_runtime_state(conn: &Connection) -> Result<Option<RuntimeState>> {
    let phase = conn
        .query_row(
            "SELECT value FROM meta WHERE key = :key",
            named_params! { ":key": "phase" },
            |row| row.get::<_, String>("value"),
        )
        .optional()?;

    let Some(phase_raw) = phase else {
        return Ok(None);
    };

    let max_workers: usize = conn
        .query_row(
            "SELECT value FROM meta WHERE key = :key",
            named_params! { ":key": "max_workers" },
            |row| row.get::<_, String>("value"),
        )?
        .parse()
        .map_err(|_| {
            crate::error::Error::Config("invalid max_workers in meta".into())
        })?;

    let snapshot_taken_at = conn
        .query_row(
            "SELECT value FROM meta WHERE key = :key",
            named_params! { ":key": "snapshot_taken_at" },
            |row| row.get::<_, String>("value"),
        )?
        .parse()
        .map_err(|_| {
            crate::error::Error::Config("invalid snapshot_taken_at in meta".into())
        })?;

    Ok(Some(RuntimeState {
        snapshot_taken_at,
        phase: PipelinePhase::parse(&phase_raw)?,
        max_workers,
    }))
}

pub fn save_runtime_state(conn: &Connection, state: &RuntimeState) -> Result<()> {
    upsert_meta(conn, "phase", state.phase.as_str())?;
    upsert_meta(conn, "snapshot_taken_at", &state.snapshot_taken_at.to_rfc3339())?;
    upsert_meta(conn, "max_workers", &state.max_workers.to_string())?;
    Ok(())
}

fn get_all_uids(conn: &Connection) -> Result<Vec<u32>> {
    let mut stmt = conn.prepare("SELECT DISTINCT uid FROM files WHERE uid IS NOT NULL")?;
    let rows = stmt.query_map([], |row| {
        let uid: u32 = row.get(0)?;
        Ok(uid)
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
}

fn get_all_gids(conn: &Connection) -> Result<Vec<u32>> {
    let mut stmt = conn.prepare("SELECT DISTINCT gid FROM files WHERE gid is NOT NULL")?;
    let rows = stmt.query_map([], |row| {
        let gid: u32 = row.get(0)?;
        Ok(gid)
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
}

/// Update all rows where uid matches given `uid` and set username column to `uname`
fn set_uname_from_uid(conn: &Connection, uid: &u32, uname: &str) -> Result<()> {
    conn.execute("UPDATE files SET username = :username WHERE uid = :uid",
                 named_params! {
        ":uid": uid,
        ":username": uname,
    })?;
    Ok(())
}

/// Update all rows where gid matches given `gid` and set groupname column to `gname`
fn set_gname_from_gid(conn: &Connection, gid: &u32, gname: &str) -> Result<()> {
    conn.execute("UPDATE files SET groupname = :groupname WHERE gid = :gid",
                 named_params! {
        ":gid": gid,
        ":groupname": gname,
    })?;
    Ok(())
}

#[cfg(unix)]
fn resolve_numeric_ids(conn: &Connection) -> Result<()> {
    // Get all present uids and gids
    let uids = get_all_uids(&conn)?;
    let gids = get_all_gids(&conn)?;

    // Resolve uids and gids.
    let resolves_names: Vec<Option<String>> = uids
        .iter()
        .map(|uid| {
            let resolved_user = match User::from_uid(Uid::from_raw(uid.clone() as uid_t)) {
                Ok(u) => u.map(|u| u.name),
                Err(e) => {
                    println!("Error while resolving uid {uid}: {e}");
                    None
                },
            };
            resolved_user
        }).collect();
    let resolved_groups: Vec<Option<String>> = gids
        .iter()
        .map(|gid| {
            let resolved_group = match Group::from_gid(Gid::from_raw(gid.clone() as gid_t)) {
                Ok(g) => g.map(|g| g.name),
                Err(e) => {
                    println!("Error while resolving gid {gid}: {e}");
                    None
                }
            };
            resolved_group
        }).collect();

    // Set the names now from lookup array.
    for (uid, o_uname) in zip(uids.iter(), resolves_names.iter()){
        if o_uname.is_none() {
            println!("Could not resolve {uid} to username");
            continue;
        }
        set_uname_from_uid(&conn, uid, o_uname.as_ref().unwrap())?;
    }
    for (gid, o_gname) in zip(gids.iter(), resolved_groups.iter()){
        if o_gname.is_none() {
            println!("Could not resolve {gid} to groupname");
            continue;
        }
        set_gname_from_gid(&conn, gid, o_gname.as_ref().unwrap())?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn resolve_numeric_ids(conn: &Connection) -> Result<()> {
    Err("Resolve Numeric Ids not available on this platform")
}


// TODO more metadata commandd:
//  Add the archive version + archiver version
//  Add read out methods.
//  Populate username, group name