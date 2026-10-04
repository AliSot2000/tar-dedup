//! Apply permissions / metadata bottom-up (delay-restore).
//!
//! Iterates `out_tree` rows deepest-first, applying the archived metadata for each
//! canonical `Placed` row: owner/group (via the owner/group policy), mode, then
//! xattrs / ACLs / SELinux, then archived mtime/atime (only with `--apply-mtime` /
//! `--apply-atime`). Times are applied last since the extended-attribute setters may
//! touch mtime/atime. Hardlink duplicates share the inode with the canonical row, so
//! only the canonical row is touched. With `--overwrite-dir`, directory metadata is
//! applied as well.

use crate::common::batched_loop;
use crate::common::perms::{
    ModeSource, OwnerGroupPolicy, OwnerGroupSource, parse_mode_changes, resolve_owner_group,
};
use crate::common::xattr::{set_file_acl, set_file_selinux_data, set_file_xattrs};
use crate::config::ExtractConfig;
use crate::config::ExtractPipelinePhase;
use crate::db::flags::ErrorFlags;
use crate::db::types::{FileId, FilePhase, FileRecord, FileType, OutTreeId, OutTreeRecord};
use crate::db::{ErrorPhase, Recorder};
use crate::error::{Error, FileStatError, Result};
use crate::unarchive::ExtractRTArgs;
use chrono::{DateTime, Utc};
use filetime::{FileTime, set_file_atime, set_file_mtime, set_file_times};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

const BATCH_SIZE: u64 = 10_000;
const ERROR_PHASE: ErrorPhase = ErrorPhase::Extract(ExtractPipelinePhase::Permissions);

pub struct OwnerGroupMode {
    /// Capture the OwnerGroupPolicy that might need to be routet through the functions to make it a slimmer fujnction call
    ogp: Option<OwnerGroupPolicy>,
    mp: Option<file_mode::Mode>,
}

pub fn run(rt: &ExtractRTArgs) -> Result<()> {
    // Errors encountered while applying metadata are recorded per-file; the recorder
    // flushes them in a single txn at the end (unless `--no-errors`).
    let mut recorder = Recorder::new(rt.db, !rt.config.process.no_errors);
    // Resolve the owner/group policy: stored in the archive or provided on the CLI.
    let policy: Option<OwnerGroupPolicy> = resolve_owner_group_policy(rt)?;

    // Resolve the mode changes: explicit `--mode` (validated on the CLI), the
    // changes recorded in the archive (`--apply-mode`), or none.
    let mode_changes = resolve_mode(rt)?;

    let ps = OwnerGroupMode {
        ogp: policy,
        mp: mode_changes,
    };

    // Phase-bar accounting. Link-tree mode applies metadata on the `.sources`
    // canonical files (files table); the non-link mode on non-dir `out_tree`
    // rows. Directory rows are only counted — and applied — under
    // `--overwrite-dir`. Each count is `(pending, done)`: bar length = the full
    // workload, position = the already-handled rows (resume-correct).
    let (file_pending, file_done) = if rt.config.placement.link_tree {
        rt.db.count_canonical_files_for_permissions()?
    } else {
        rt.db.count_out_tree_for_permissions()?
    };
    let (dir_pending, dir_done) = if rt.config.attributes.force_overwrite_dir {
        rt.db.count_out_tree_for_permissions_dirs()?
    } else {
        (0, 0)
    };
    rt.progress.set_phase_total(file_pending + file_done + dir_pending + dir_done);
    rt.progress.set_phase_position(file_done + dir_done);

    if rt.config.placement.link_tree {
        apply_permissions_link_sources(&rt, &mut recorder, &ps)?;
    } else {
        // Files (and non-directory entries) first.
        process_batches(&rt, &mut recorder, &ps, false)?;
    }

    // Directories, only when `--overwrite-dir` is requested.
    if rt.config.attributes.force_overwrite_dir {
        process_batches(&rt, &mut recorder, &ps, true)?;
    }

    recorder.flush()?;

    // Propagate out_tree metadata flags up to the files table:
    // AppliedPermissions iff ALL rows applied; ErrorWhileApplyingPermissions iff ANY errored.
    let (applied, errored) = rt.db.apply_permissions_flags_to_files()?;
    tracing::info!(
        applied_files = applied,
        errored_files = errored,
        "permissions: propagated metadata flags to files"
    );

    // INFO: Since all relevant things are stored in out_tree, we cna blanket promote here.
    if !rt.shutdown.is_interrupted() {
        rt.db.global_mark_phase(FilePhase::PermissionsRestored)?;
    }
    Ok(())
}

/// Resolve the owner group policy based on the cli flags and the presence of an owner group policy
/// from the database.
fn resolve_owner_group_policy(rt: &ExtractRTArgs)
    -> Result<Option<OwnerGroupPolicy>> {
    let policy = match &rt.config.owner_policy {
        OwnerGroupSource::None => None,
        OwnerGroupSource::Cli(p) => Some(p.clone()),
        OwnerGroupSource::Stored => {
            match rt.db.get_archive_owner_policy()? {
                None => None,
                Some(p) => {
                    let mut out_policy = OwnerGroupPolicy::default();
                    if rt.config.owner_group.apply_owner {
                        out_policy.owner_map = p.owner_map;
                        out_policy.owner_override = p.owner_override;
                    }
                    if rt.config.owner_group.apply_group {
                        out_policy.group_map = p.group_map;
                        out_policy.group_override = p.group_override;
                    }
                    Some(out_policy)
                }
            }
        },
    };
    Ok(policy)
}

/// Resolve the symbolic mode changes based on the cli flags and the presence of an mode change
/// string in the database.
fn resolve_mode(rt: &ExtractRTArgs) -> Result<Option<file_mode::Mode>> {
    let res = match &rt.config.mode_policy {
        ModeSource::None => None,
        ModeSource::Cli(changes) => Some(parse_mode_changes(changes)?),
        ModeSource::Stored => match rt.db.get_archive_mode_changes()? {
            Some(changes) => Some(parse_mode_changes(&changes)?),
            None => None,
        },
    };
    Ok(res)
}

fn process_batches(rt: &ExtractRTArgs, recorder: &mut Recorder, ps: &OwnerGroupMode, dirs: bool)
    -> Result<()> {
    let mut add_err = |fid, oid, err| {
        recorder.record(
            fid,
            Some(oid),
            ERROR_PHASE,
            err,
            ErrorFlags::default(),
        );
    };
    if dirs {
        process_batches_dirs(&rt, &mut add_err, ps)
    } else {
        process_batched_files(&rt, &mut add_err, ps)
    }
}

fn process_batched_files(
    rt: &ExtractRTArgs,
    capture_err: &mut impl FnMut(Option<FileId>, OutTreeId, FileStatError) -> (),
    ps: &OwnerGroupMode)
    -> Result<()> {
    batched_loop(
        |bs| rt.db.list_out_tree_for_permissions_non_dir::<FileRecord>(bs),
        BATCH_SIZE,
        |entries| {
            let mut results: Vec<(OutTreeId, bool)> = Vec::with_capacity(entries.len());
            for (record, out) in entries
                .into_iter()
                .map(|(r, o)| (Some(r), o)) {
                rt.shutdown.check_between_files()?;
                inner_apply_permissions(rt, &record, out, ps, capture_err, &mut results)?;
            }
            rt.db.ingest_apply_permission_out_tree_results(&results)?;
            Ok(())
        }
    )
}

fn process_batches_dirs(
    rt: &ExtractRTArgs,
    capture_err: &mut impl FnMut(Option<FileId>, OutTreeId, FileStatError) -> (),
    ps: &OwnerGroupMode)
    -> Result<()> {
    batched_loop(
        |bs| rt.db.list_out_tree_for_permissions_dirs::<FileRecord>(bs),
        BATCH_SIZE,
        |entries| {
            let mut results: Vec<(OutTreeId, bool)> = Vec::with_capacity(entries.len());
            for (record, out) in entries {
                rt.shutdown.check_between_files()?;
                inner_apply_permissions(rt, &record, out, ps, capture_err, &mut results)?;
            }
            rt.db.ingest_apply_permission_out_tree_results(&results)?;
        Ok(())
        }
    )
}

fn inner_apply_permissions(
    rt: &ExtractRTArgs,
    record: &Option<FileRecord>,
    out: OutTreeRecord,
    ps: &OwnerGroupMode,
    capture_err: &mut impl FnMut(Option<FileId>, OutTreeId, FileStatError) -> (),
    results: &mut  Vec<(OutTreeId, bool)>)
    -> Result<()> {

    // Ancestor-only directory rows (`ensure_parent`) have no catalog row;
    // there is no metadata to apply, and the directory itself already exists.
    let errors = match record {
        None => Vec::<FileStatError>::new(),
        Some(r) => apply_one(rt.config, &r, &out.abs_path, &ps),
    };

    let error_empty = errors.is_empty();
    results.push((out.id, error_empty));

    for error in errors {
        capture_err(out.file_id, out.id, error);
    }
    if rt.config.process.fail_fast && error_empty {
        return Err(Error::Other(anyhow::anyhow!(
                    "metadata restore failed for {}",
                    out.abs_path.display()
                )));
    }
    rt.progress.inc_both(1);
    Ok(())
}

/// Apply the metadata to the files which were copied to the link source which are used as targets
/// for the links in link_tree
pub fn apply_permissions_link_sources(rt: &ExtractRTArgs, rec: &mut Recorder, ps: &OwnerGroupMode)
    -> Result<()> {
    let dir_name = match &rt.config.placement.link_source {
        None => PathBuf::from(".sources"),
        Some(v) => v.to_path_buf(),
    };
    let base_dir = rt.config.paths.extraction_root().join(dir_name);
    batched_loop(
        |bs| rt.db.list_canonical_files_for_permissions(bs),
        BATCH_SIZE,
        |files: Vec<FileRecord>| {
            let mut results: Vec<(FileId, bool)> = Vec::with_capacity(files.len());
            for file in files {
                let id = file
                    .content_id()
                    .expect("Copied requires content_id to exist");
                let tgt_path = base_dir.join(id.0);
                let errs = apply_one(&rt.config, &file, &tgt_path, &ps);
                results.push((file.id, errs.is_empty()));
                for err in errs {
                    rec.record_file(file.id, ERROR_PHASE, err, ErrorFlags::default());
                }
                rt.progress.inc_both(1);
            }
            rt.db.ingest_apply_permission_file_results(&results)?;
            Ok(())
        }
    )
}

/// Apply metadata for a single canonical `Placed` row. Errors are collected and
/// returned (never early-aborted) so a single row reports all application failures.
fn apply_one(
    config: &ExtractConfig,
    record: &FileRecord,
    tgt_path: &Path,
    ps: &OwnerGroupMode)
    -> Vec<FileStatError> {
    let mut errors: Vec<FileStatError> = Vec::new();

    // Owner/group via policy resolution.
    let (uid, gid) = match ps.ogp.as_ref() {
        Some(pol) => match resolve_owner_group(
            record.uid,
            record.gid,
            record.username.as_ref().map(|s| s.as_ref()),
            record.groupname.as_ref().map(|s| s.as_ref()),
            &pol,
            config.owner_group.target,
            config.owner_group.same_owner) {

            Ok((u, g)) => (u, g),
            Err(e) => {
                errors.push(FileStatError::Io {
                    path: tgt_path.to_path_buf(),
                    source: io::Error::new(
                        io::ErrorKind::Other,
                        format!("owner/group resolution failed: {e}"),
                    ),
                });
                (None, None)
            }
        },
        None => (None, None),
    };

    // chown (best-effort; may require root).
    if uid.is_some() || gid.is_some() {
        match chown_for_path(tgt_path, uid, gid) {
            Ok(()) => {}
            Err(e) => errors.push(FileStatError::io(tgt_path, e)),
        }
    }

    // mode. Symlinks are skipped: on unix their mode is kernel-fixed 0777 and
    // chmod would dereference and clobber the link target.
    if !config.attributes.no_same_permissions {
        let type_match = matches!(record.ftype, FileType::Symlink(_));
        if let (Some(mode), false) = (record.mode, type_match) {
            let effective = match ps.mp.as_ref() {
                Some(changes) => changes.apply_to(mode),
                None => mode,
            };
            match apply_mode(tgt_path, effective) {
                Ok(()) => {}
                Err(e) => errors.push(FileStatError::io(tgt_path, e)),
            }
        }
    }

    // xattrs / ACLs / SELinux (inode-scoped: same for all hardlinks; apply once).
    if !config.attributes.no_xattrs {
        if let Some(raw) = &record.xattrs {
            match set_file_xattrs(tgt_path, raw) {
                Ok(()) => {}
                Err(e) => errors.push(e),
            }
        }
    }
    if !config.attributes.no_acls {
        if let Some(raw) = &record.posix_acl {
            match set_file_acl(tgt_path, raw) {
                Ok(()) => {}
                Err(e) => errors.push(e),
            }
        }
    }
    if !config.attributes.no_selinux {
        if let Some(ctx) = &record.selinux_ctx {
            match set_file_selinux_data(tgt_path, ctx) {
                Ok(()) => {}
                Err(e) => errors.push(e),
            }
        }
    }

    // times last: the extended-attribute setters may update mtime/atime.
    if config.attributes.apply_mtime && record.mtime.is_some()
        || config.attributes.apply_atime && record.atime.is_some() {
        let atime = if config.attributes.apply_atime {
            record.atime.as_ref().map(|t| to_file_time(*t))
        } else {
            None
        };
        let mtime = if config.attributes.apply_mtime {
            record.mtime.as_ref().map(|t| to_file_time(*t))
        } else {
            None
        };
        match apply_times(tgt_path, atime, mtime) {
            Ok(()) => {}
            Err(e) => errors.push(FileStatError::io(tgt_path, e)),
        }
    }

    // This function already handles the errors.
    for error in &errors {
        tracing::error!(
            path = %tgt_path.display(),
            error = %error,
            "metadata restore failure; marking ErrorWhileApplyingMetadata"
        );
    }
    errors
}

#[cfg(unix)]
fn chown_for_path(target: &Path, uid: Option<u32>, gid: Option<u32>) -> io::Result<()> {
    use std::os::unix::fs::lchown;
    // lchown: do not follow symlinks (metadata is on the link itself).
    lchown(target, uid, gid)
}

#[cfg(not(unix))]
fn chown_for_path(_target: &Path, _uid: Option<u32>, _gid: Option<u32>) -> io::Result<()> {
    Ok(())
}

fn apply_mode(target: &Path, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(target, fs::Permissions::from_mode(mode))
}

fn apply_times(target: &Path, atime: Option<FileTime>, mtime: Option<FileTime>) -> io::Result<()> {
    match (atime, mtime) {
        (Some(a), Some(m)) => set_file_times(target, a, m),
        (Some(a), None) => set_file_atime(target, a),
        (None, Some(m)) => set_file_mtime(target, m),
        (None, None) => Ok(()),
    }
}

fn to_file_time(dt: DateTime<Utc>) -> FileTime {
    FileTime::from_system_time(SystemTime::from(dt))
}
