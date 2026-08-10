use nix::libc::{gid_t, uid_t};
use rusqlite::{Connection, named_params};
use std::iter::zip;

use crate::config::RuntimeState;
use crate::db::meta;
use crate::db::types::NewFileRecord;
use crate::error::Result;
use nix::unistd::{Gid, Group, Uid, User};

pub fn insert_file(conn: &Connection, record: &NewFileRecord) -> Result<bool> {
    let changed = conn.execute(
        "INSERT OR IGNORE INTO files (
             rel_path, ext, size, mtime, atime, ctime, uid, gid, mode, ftype,
             xattr, acl, selinux, phase, link_dst
         ) VALUES (
             :rel_path, :ext, :size, :mtime, :atime, :ctime, :uid, :gid, :mode, :ftype,
             :xattr, :acl, :selinux, 'inventoried', :link_dst
         )",
        named_params! {
            ":rel_path": record.rel_path.to_string_lossy(),
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
        },
    )?;
    Ok(changed > 0)
}

pub fn load_runtime_state(conn: &Connection) -> Result<Option<RuntimeState>> {
    let Some(phase) = meta::get_archive_phase(conn)? else {
        return Ok(None);
    };
    let max_workers = meta::get_archive_max_workers(conn)?.ok_or_else(|| {
        crate::error::Error::Config("missing archive_max_workers in meta".into())
    })?;
    let snapshot_taken_at = meta::get_archive_snapshot_taken_at(conn)?.ok_or_else(|| {
        crate::error::Error::Config("missing archive_snapshot_taken_at in meta".into())
    })?;

    Ok(Some(RuntimeState {
        snapshot_taken_at,
        phase,
        max_workers,
    }))
}

pub fn save_runtime_state(conn: &Connection, state: &RuntimeState) -> Result<()> {
    meta::with_meta_txn(conn, |conn| {
        meta::set_archive_phase(conn, state.phase)?;
        meta::set_archive_snapshot_taken_at(conn, state.snapshot_taken_at)?;
        meta::set_archive_max_workers(conn, state.max_workers)?;
        Ok(())
    })
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
pub fn resolve_numeric_ids(conn: &Connection) -> Result<()> {
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
    // TODO no more println
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
pub fn resolve_numeric_ids(conn: &Connection) -> Result<()> {
    // TODO Emmit to tracing, stderr
    Err("Resolve Numeric Ids not available on this platform")
}
