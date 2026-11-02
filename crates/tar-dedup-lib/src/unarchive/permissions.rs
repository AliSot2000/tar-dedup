//! Apply permissions / metadata bottom-up (delay-restore).
//!
//! Iterates `out_tree` rows deepest-first, applying the archived metadata for each
//! canonical `Placed` row: owner/group (via the owner/group policy), mode, then
//! xattrs / ACLs / SELinux, then archived mtime/atime (only with `--apply-mtime` /
//! `--apply-atime`). Times are applied last since the extended-attribute setters may
//! touch mtime/atime. Hardlink duplicates share the inode with the canonical row, so
//! only the canonical row is touched. With `--overwrite-all-dir`, directory metadata is
//! applied as well.

use crate::common::batched_loop;
use crate::common::defaults::SIDECAR_MAX_NAME_ATTEMPTS;
use crate::common::perms::{
    ModeSource, OwnerGroupPolicy, OwnerGroupSource, parse_mode_changes, resolve_owner_group,
};
use crate::common::xattr::{set_file_acl, set_file_selinux_data, set_file_xattrs};
use crate::config::ExtractConfig;
use crate::config::ExtractPipelinePhase;
use crate::config::SidecarMode;
use crate::db::flags::ErrorFlags;
use crate::db::types::{FileId, FilePhase, FileRecord, FileType, OutTreeId, OutTreeRecord, SidecarEntry};
use crate::db::{ErrorPhase, Recorder};
use crate::error::{Error, FileStatError, Result};
use crate::unarchive::ExtractRTArgs;
use chrono::{DateTime, Utc};
use filetime::{FileTime, set_file_atime, set_file_mtime, set_file_times};
use std::fs;
use std::fs::OpenOptions;
use std::io;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

const ERROR_PHASE: ErrorPhase = ErrorPhase::Extract(ExtractPipelinePhase::Permissions);

pub struct OwnerGroupMode {
    /// Capture the OwnerGroupPolicy that might need to be routed through the functions to make it
    /// a slimmer function call.
    ogp: Option<OwnerGroupPolicy>,
    mp: Option<file_mode::Mode>,
}

fn local_err(rec: &mut Recorder, fid: Option<FileId>, oid: OutTreeId, err: FileStatError) {
    rec.record(
        fid,
        Some(oid),
        ERROR_PHASE,
        err,
        ErrorFlags::default(),
    );
}

pub fn run(rt: &ExtractRTArgs) -> Result<()> {
    let mut recorder = Recorder::new(
        rt.db, !rt.config.process.no_errors, Some(rt.config.process.auto_flush_limit));

    let policy: Option<OwnerGroupPolicy> = resolve_owner_group_policy(rt)?;
    let mode_changes = resolve_mode(rt)?;

    let ps = OwnerGroupMode {
        ogp: policy,
        mp: mode_changes,
    };

    // Phase-bar accounting. Link-tree mode applies metadata on the `.sources`
    // canonical files (files table); the non-link mode on non-dir `out_tree`
    // rows. Directory rows are counted — and applied — unless `--no-overwrite-dir`
    // (with `--overwrite-all-dir` the set includes filter-excluded dirs). Each count
    // is `(pending, done)`: bar length = the full workload, position = the
    // already-handled rows (resume-correct).
    let (file_pending, file_done) = if rt.config.placement.link_tree {
        rt.db.count_canonical_files_for_permissions()?
    } else {
        rt.db.count_out_tree_for_permissions()?
    };
    let (dir_pending, dir_done) = if !rt.config.attributes.no_overwrite_dir {
        rt.db.count_out_tree_for_permissions_dirs()?
    } else {
        (0, 0)
    };
    rt.progress.set_phase_total(file_pending + file_done + dir_pending + dir_done);
    rt.progress.set_phase_position(file_done + dir_done);

    if rt.config.placement.link_tree {
        apply_permissions_link_sources(&rt, &mut recorder, &ps)?;
        // INFO: Symlinks don't have metadata, so dump them in sidecar on request.
        write_link_tree_sidecars(&rt, &mut recorder)?;
    } else {
        // Files (and non-directory entries) first.
        process_batched_files(
            &rt,
            &mut |fid, oid, err| local_err(&mut recorder, fid, oid, err),
            &ps)?;
    }

    // Directories, unless `--no-overwrite-dir`.
    if !rt.config.attributes.no_overwrite_dir {
        process_batches_dirs(
            &rt,
            &mut |fid, oid, err| local_err(&mut recorder, fid, oid, err),
            &ps)?;
    }

    recorder.flush()?;

    // Propagate out_tree metadata flags up to the files table:
    // AppliedPermissions iff ALL rows applied; ErrorWhileApplyingPermissions iff ANY errored.
    let (applied, errored) = rt.db.apply_permissions_flags_to_files()?;
    tracing::info!(
        "permissions: propagated {applied} applied and {errored} error metadata flags to files"
    );
    tracing::info!(
        "permissions done: processed {} of {}, {} error(s)",
        file_pending + dir_pending,
        file_pending + file_done + dir_pending + dir_done,
        rt.db.count_errors(Some("permissions"))?,
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
    // INFO: Future versions might have to alter this course of action if the mode change is
    //  altered since the "parse worked" precondition might not hold across time.
    let res = match &rt.config.mode_policy {
        ModeSource::None => None,
        ModeSource::Cli(changes) => Some(parse_mode_changes(changes)
            .expect("Mode changes should be validated at parse time!")),
        ModeSource::Stored => match rt.db.get_archive_mode_changes()? {
            Some(changes) => Some(parse_mode_changes(&changes)
                .expect("Mode changes should be validated at parse time!")),
            None => None,
        },
    };
    Ok(res)
}

fn process_batched_files(
    rt: &ExtractRTArgs,
    capture_err: &mut impl FnMut(Option<FileId>, OutTreeId, FileStatError) -> (),
    ps: &OwnerGroupMode)
    -> Result<()> {
    batched_loop(
        |bs| rt.db.list_out_tree_for_permissions_non_dir::<FileRecord>(bs),
        rt.config.process.batch_size,
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
        // No filter here: filter-selected vs all-dirs is encoded in out_tree
        // at population time (force_overwrite_all_dir bypasses the filter there).
        |bs| rt.db.list_out_tree_for_permissions_dirs::<FileRecord>(bs),
        rt.config.process.batch_size,
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
    results: &mut Vec<(OutTreeId, bool)>)
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
    if rt.config.process.fail_fast && !error_empty {
        return Err(Error::Other(anyhow::anyhow!(
                    "metadata restore failed for {}",
                    out.abs_path.display()
                )));
    }

    let wanted = match rt.config.effective_sidecar_mode() {
        SidecarMode::Always => true,
        SidecarMode::OnError => !error_empty,
        SidecarMode::Never => false,
    };
    if wanted {
        if let Some(side) = record.as_ref().map(SidecarEntry::from) {
            let side = side.with_out_tree(out.id, &out.abs_path);
            if let Err(fse) = write_sidecar(
                &out.abs_path,
                &side,
                rt.config.hidden_sidecar,
                SIDECAR_MAX_NAME_ATTEMPTS) {

                capture_err(out.file_id, out.id, fse);
            }
        }
    }

    rt.progress.inc_both(1);
    Ok(())
}

/// Apply the metadata to the files which were copied to the link source which are used as targets
/// for the links in link_tree
fn apply_permissions_link_sources(rt: &ExtractRTArgs, rec: &mut Recorder, ps: &OwnerGroupMode)
    -> Result<()> {
    let dir_name = match &rt.config.placement.link_source {
        None => PathBuf::from(".sources"),
        Some(v) => v.to_path_buf(),
    };
    let base_dir = rt.config.effective_extraction_root().join(dir_name);
    batched_loop(
        |bs| rt.db.list_canonical_files_for_permissions(bs),
        rt.config.process.batch_size,
        |files: Vec<FileRecord>| {
            let mut results: Vec<(FileId, bool)> = Vec::with_capacity(files.len());
            for file in files {
                let id = file
                    .content_id()
                    .expect("Copied requires content_id to exist");
                let tgt_path = base_dir.join(&id.0);
                let errs = apply_one(&rt.config, &file, &tgt_path, &ps);
                let errs_empty = errs.is_empty();
                results.push((file.id, errs.is_empty()));
                for err in errs {
                    rec.record_file(file.id, ERROR_PHASE, err, ErrorFlags::default());
                }
                if rt.config.process.fail_fast && !errs_empty {
                    return Err(Error::Other(anyhow::anyhow!(
                    "metadata restore failed for {}",
                    id.0
                )));
                }
                rt.progress.inc_both(1);
            }
            rt.db.ingest_apply_permission_file_results(&results)?;
            Ok(())
        }
    )
}

/// `--sidecar always` for link-tree: walk the links in the output tree and write
/// a manifest beside each. `on-error` never reaches this (degraded to `never` in
/// `effective_sidecar_mode`; link metadata errors surface at the `.sources`
/// canonicals, not per link).
fn write_link_tree_sidecars(rt: &ExtractRTArgs, rec: &mut Recorder) -> Result<()> {
    if rt.config.effective_sidecar_mode() != SidecarMode::Always {
        return Ok(());
    }
    let hidden = rt.config.hidden_sidecar;
    // Page forward by `o.id`; the getter is conditioned on the canonicals'
    // permission flags (which sidecar writing does not mutate), so an explicit
    // cursor is the only terminator.
    let mut last = OutTreeId(0);
    loop {
        let batch = rt.db
            .list_out_tree_links_for_sidecar::<FileRecord>(last, rt.config.process.batch_size)?;
        if batch.is_empty() {
            break;
        }
        for (record, out) in batch {
            rt.shutdown.check_between_files()?;
            let sid = SidecarEntry::from(&record).with_out_tree(out.id, &out.abs_path);
            if let Err(fse) = write_sidecar(
                &out.abs_path,
                &sid,
                hidden,
                SIDECAR_MAX_NAME_ATTEMPTS,
            ) {
                rec.record_file(record.id, ERROR_PHASE, fse, ErrorFlags::default());
            }
            last = out.id;
        }
    }
    Ok(())
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
        Some(pol) => resolve_owner_group(
            record.uid,
            record.gid,
            record.username.as_ref().map(|s| s.as_ref()),
            record.groupname.as_ref().map(|s| s.as_ref()),
            &pol,
            config.owner_group.target,
            config.owner_group.same_owner),
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
        tracing::warn!(
            path = %tgt_path.display(),
            error = %error,
            "metadata restore failure; marking ErrorWhileApplyingMetadata"
        );
    }
    debug_assert!(errors.len() <= 6,
                  "INVARIANT ERROR: Expected at most 6 errors in apply_one, got {}",
                  errors.len());
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

// ---------------------------------------------------------------------------------------------------------------------
// Sidecar manifests
// ---------------------------------------------------------------------------------------------------------------------

/// Runtime seed for sidecar-name randomization. Wall-clock nanoseconds are not
/// cryptographic — the component only needs to beat a name collision within one
/// directory, and tests drive the generator with fixed seeds directly.
fn sidecar_seed() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

/// SplitMix64 step; pure and seed-injected so tests are deterministic.
fn sidecar_rand(seed: &mut u64) -> u64 {
    *seed = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *seed;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Claim a fresh sidecar file next to `target`, never overwriting: first the
/// deterministic `{name}.json` (or `.{name}.json` with `hidden`), then up to
/// `attempts` random-suffix candidates `{name}.{08x}.json`. The open file is
/// returned; on exhaustion a `General` error names the target's directory.
fn claim_sidecar_file(
    target: &Path,
    hidden: bool,
    attempts: usize,
    seed: &mut u64,
) -> std::result::Result<fs::File, FileStatError> {
    let parent = target.parent().ok_or_else(|| {
        FileStatError::general(None, format!("sidecar for {} has no parent", target.display()))
    })?;
    let name = target.file_name().ok_or_else(|| {
        FileStatError::general(None, format!("sidecar for {} has no file name", target.display()))
    })?;
    let prefix = if hidden {
        format!(".{}", name.to_string_lossy())
    } else {
        name.to_string_lossy().into_owned()
    };
    let open_new = |cand: PathBuf| -> io::Result<fs::File> {
        OpenOptions::new().write(true).create_new(true).open(cand)
    };
    if let Ok(f) = open_new(parent.join(format!("{prefix}.json"))) {
        return Ok(f);
    }
    for _ in 0..attempts {
        let cand = parent.join(format!("{prefix}.{:08x}.json", sidecar_rand(seed)));
        if let Ok(f) = open_new(cand) {
            return Ok(f);
        }
    }
    Err(FileStatError::general(
        Some(parent),
        "failed to find suitable sidecar name, no sidecar file written".into(),
    ))
}

/// Serialize `entry` to pretty JSON (keys come out alphabetically sorted: the
/// `Map` of `to_value` is a BTreeMap by default) and write it beside `target`.
fn write_sidecar(
    target: &Path,
    entry: &SidecarEntry,
    hidden: bool,
    attempts: usize,
) -> std::result::Result<(), FileStatError> {
    let mut seed = sidecar_seed();
    let mut file = claim_sidecar_file(target, hidden, attempts, &mut seed)?;
    let value = serde_json::to_value(entry).map_err(|e| FileStatError::json(target, e))?;
    let json = serde_json::to_string_pretty(&value).map_err(|e| FileStatError::json(target, e))?;
    file.write_all(json.as_bytes()).map_err(|e| FileStatError::io(target, e))?;
    Ok(())
}

// ---------------------------------------------------------------------------------------------------------------------
// TEST
// ---------------------------------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::files::original_extension;
    use crate::common::perms::{IdentityMap, IdentitySpec};
    use crate::common::xattr::tests::{probe_acl, probe_selinux, probe_xattr};
    use crate::db::flags::{FileFlag, FileFlags, OutTreeFlag, OutTreeFlags};
    use crate::db::types::{LinkType, NewFileRecord, NewOutTreeRow, StrippedRecord};
    use crate::test_support::{ExtractWorld, seed_filter_reasons};
    use chrono::TimeZone;
    use rusqlite::named_params;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use std::panic::{AssertUnwindSafe, catch_unwind};

    // ---------------------------------------------------------------------
    // PermissionsWorld: local seeder impl on the shared `ExtractWorld`.
    // ---------------------------------------------------------------------

    impl ExtractWorld {
        /// Permissions-test world: `for_place_test` base re-rooted under the
        /// tempdir (`out/` = extraction root, `estage/` = work dir), a single
        /// worker, per-test config tune.
        fn permissions_world(tune: impl FnOnce(&mut ExtractConfig)) -> Self {
            Self::with_config(|dp| {
                let mut config = ExtractConfig::for_place_test(dp.join("out"), false, 4);
                config.paths.work_dir = dp.join("estage");
                config.process.io_jobs = 1;
                config.process.jobs = 1;
                tune(&mut config);
                config
            })
        }

        fn proot(&self) -> PathBuf {
            self.config.paths.extraction_root().to_path_buf()
        }

        /// Write `payload` under the extraction root (creating parents).
        fn write_file(&self, rel: &str, payload: &[u8]) -> PathBuf {
            let p = self.proot().join(rel);
            fs::create_dir_all(p.parent().expect("parent")).expect("parent dirs");
            fs::write(&p, payload).expect("write file");
            p
        }

        /// Insert a self-canonical, rehashed `files` row (digest `[7;20]`) with
        /// an on-disk file at the same relative path.
        fn p_add_file(&self, rel: &str, payload: &[u8], tune: impl FnOnce(&mut NewFileRecord)) -> FileId {
            self.seed_filter();
            let p = self.write_file(rel, payload);
            let mut rec = NewFileRecord {
                abs_path: p.clone(),
                abs_path_raw: None,
                ext: original_extension(Path::new(rel)),
                size: payload.len() as u64,
                mtime: None, atime: None, ctime: None,
                uid: None, gid: None, mode: None,
                ftype: Some(FileType::File),
                xattrs: None, posix_acl: None, selinux_ctx: None, win_perm: None,
                link_dst: None, link_dst_raw: None,
                device_id: None, inode_id: None, major: None, minor: None,
            };
            tune(&mut rec);
            self.db.insert_file(&rec).expect("insert file");
            let id = self.db.file_id_by_abs_path(&p).expect("lookup").expect("row");
            seed_filter_reasons(&self.db, id);
            self.db.mark_self_canonical(id).expect("self canonical");
            self.db.update_file_inspection_per_id(id, [7; 20], 0, false).expect("digest");
            self.db.mark_file_phase(id, FilePhase::Rehashed).expect("phase");
            id
        }

        /// `files.username` / `files.groupname` (not carried by `NewFileRecord`).
        fn set_names(&self, id: FileId, username: Option<&str>, groupname: Option<&str>) {
            self.db.with_transaction(|conn| {
                conn.execute(
                    "UPDATE files SET username = :u, groupname = :g WHERE id = :id",
                    named_params! { ":u": username, ":g": groupname, ":id": id.0 },
                ).expect("set names");
                Ok(())
            }).expect("names tx");
        }

        /// Un-eject the row so `content_id` resolution fails (the link-sources
        /// precondition panic): `content_id` fills `;` for a missing digest but
        /// returns `None` for a non-self-canonical row.
        fn un_canonicalize(&self, id: FileId) {
            self.db.with_transaction(|conn| {
                conn.execute("UPDATE files SET canonical_id = NULL WHERE id = :id",
                             named_params! { ":id": id.0 }).expect("un canonicalize");
                Ok(())
            }).expect("un canonicalize tx");
        }

        /// Insert an `out_tree` row (self-canonical) for an on-disk target.
        fn padd_out(&self, rel: &str, file_id: FileId, flags: OutTreeFlags) -> OutTreeId {
            let p = self.proot().join(rel);
            let rows = vec![NewOutTreeRow { abs_path: p, file_id: Some(file_id), flags }];
            let id = self.db.insert_out_tree_rows(&rows, false).expect("insert out")[0];
            self.db.with_transaction(|conn| {
                conn.execute("UPDATE out_tree SET canonical_id = id WHERE id = :id",
                             named_params! { ":id": id.0 }).expect("set canonical");
                Ok(())
            }).expect("canonical tx");
            id
        }

        fn p_out_flag(&self, id: OutTreeId, flag: OutTreeFlag) -> bool {
            self.db.get_out_tree_flag(id, flag).expect("get flag")
        }

        fn p_file_flag(&self, id: FileId, flag: FileFlag) -> bool {
            self.db.get_file_flag(id, flag).expect("get flag")
        }

        fn file_phase(&self, id: FileId) -> FilePhase {
            self.db.get_file_by_id::<StrippedRecord>(id)
                .expect("get row").expect("row present").phase
        }

        fn file_mode(&self, id: FileId) -> Option<u32> {
            self.db.get_file_by_id::<FileRecord>(id)
                .expect("get row").expect("row present").mode
        }

        fn any_file_has_phase(&self, phase: FilePhase) -> bool {
            self.db.t_files_in_phase::<StrippedRecord>(phase)
                .expect("list").len() > 0
        }
    }

    /// Pure `FileRecord` builder with full metadata (no DB).
    #[allow(clippy::too_many_arguments)]
    fn mk_file(
        abs: &str,
        ftype: FileType,
        mode: Option<u32>,
        mtime: Option<DateTime<Utc>>,
        atime: Option<DateTime<Utc>>,
        uid: Option<u32>,
        gid: Option<u32>,
        username: Option<&str>,
        groupname: Option<&str>,
        xattrs: Option<String>,
        acl: Option<String>,
        selinux: Option<Vec<u8>>) -> FileRecord {
        FileRecord {
            id: FileId(1),
            abs_path: PathBuf::from(abs),
            ext: original_extension(Path::new(abs)),
            size: 0,
            sha1: Some([7; 20]),
            mtime, atime, ctime: None,
            uid, gid,
            username: username.map(|s| s.to_string()),
            groupname: groupname.map(|s| s.to_string()),
            mode,
            ftype,
            device_id: None, inode_id: None, major: None, minor: None,
            xattrs, posix_acl: acl, selinux_ctx: selinux, win_perm: None,
            link_dst: None,
            include_reason_archive: None, exclude_reason_archive: None,
            include_reason_extract: None, exclude_reason_extract: None,
            canonical_id: Some(FileId(1)),
            flags: FileFlags::default(),
            phase: FilePhase::Rehashed,
            new_name: None,
        }
    }

    /// A `Placed` canonical non-dir out_tree flag set.
    fn placed_flags() -> OutTreeFlags {
        let mut f = OutTreeFlags::default();
        f.set(OutTreeFlag::Placed, true);
        f
    }

    fn ps_none() -> OwnerGroupMode {
        OwnerGroupMode { ogp: None, mp: None }
    }

    fn ps_ogp(policy: OwnerGroupPolicy) -> OwnerGroupMode {
        OwnerGroupMode { ogp: Some(policy), mp: None }
    }

    fn on_disk_mode(p: &Path) -> u32 {
        fs::metadata(p).expect("meta").permissions().mode() & 0o7777
    }

    #[cfg(unix)]
    fn current_uid() -> u32 {
        nix::unistd::Uid::current().as_raw()
    }

    #[cfg(unix)]
    fn current_gid() -> u32 {
        nix::unistd::Gid::current().as_raw()
    }

    fn dt(secs: i64, nanos: u32) -> DateTime<Utc> {
        Utc.timestamp_opt(secs, nanos).unwrap()
    }

    fn empty_acl_json() -> String {
        r#"{"user-obj":7,"group-obj":5,"other":0}"#.to_string()
    }

    fn xattr_json() -> String {
        r#"{"encoding":"utf8","data":{"user.meta":"v"}}"#.to_string()
    }

    // ---------------------------------------------------------------------
    // resolve_owner_group_policy
    // ---------------------------------------------------------------------

    #[test]
    fn policy_none_is_none() {
        let world = ExtractWorld::permissions_world(|c| c.owner_policy = OwnerGroupSource::None);
        assert!(resolve_owner_group_policy(&world.rt()).expect("resolve").is_none());
    }

    #[test]
    fn policy_cli_is_cloned() {
        let p = OwnerGroupPolicy {
            owner_override: Some(IdentitySpec { name: Some("alice".into()), id: None }),
            ..OwnerGroupPolicy::default()
        };
        let world = ExtractWorld::permissions_world(|c| c.owner_policy = OwnerGroupSource::Cli(p.clone()));
        assert_eq!(resolve_owner_group_policy(&world.rt()).expect("resolve"), Some(p));
    }

    #[test]
    fn policy_stored_without_meta_is_none() {
        let world = ExtractWorld::permissions_world(|c| c.owner_policy = OwnerGroupSource::Stored);
        assert!(resolve_owner_group_policy(&world.rt()).expect("resolve").is_none());
    }

    /// `--apply-stored-owner-map/-group-map` partial transfer of the four fields.
    #[test]
    fn policy_stored_partial_application_matrix() {
        for (apply_owner, apply_group) in [(true, true), (true, false), (false, true), (false, false)] {
            let world = ExtractWorld::permissions_world(|c| {
                c.owner_policy = OwnerGroupSource::Stored;
                c.owner_group.apply_owner = apply_owner;
                c.owner_group.apply_group = apply_group;
            });
            let stored = OwnerGroupPolicy {
                owner_map: Some(IdentityMap::default()),
                owner_override: Some(IdentitySpec { name: Some("o".into()), id: None }),
                group_map: Some(IdentityMap::default()),
                group_override: Some(IdentitySpec { name: None, id: Some(1) }),
            };
            world.db.set_archive_owner_policy(&stored).expect("set stored");
            let got = resolve_owner_group_policy(&world.rt()).expect("resolve").expect("some");
            assert_eq!(got.owner_map.is_some(), apply_owner,
                       "apply_owner={apply_owner} apply_group={apply_group}: owner_map");
            assert_eq!(got.owner_override.is_some(), apply_owner,
                       "apply_owner={apply_owner} apply_group={apply_group}: owner_override");
            assert_eq!(got.group_map.is_some(), apply_group,
                       "apply_owner={apply_owner} apply_group={apply_group}: group_map");
            assert_eq!(got.group_override.is_some(), apply_group,
                       "apply_owner={apply_owner} apply_group={apply_group}: group_override");
        }
    }

    // ---------------------------------------------------------------------
    // resolve_mode
    // ---------------------------------------------------------------------

    #[test]
    fn resolve_mode_none_is_none() {
        let world = ExtractWorld::permissions_world(|c| c.mode_policy = ModeSource::None);
        assert!(resolve_mode(&world.rt()).expect("resolve").is_none());
    }

    #[test]
    fn resolve_mode_cli_parses() {
        let world = ExtractWorld::permissions_world(|c| c.mode_policy = ModeSource::Cli("u+x".into()));
        assert!(resolve_mode(&world.rt()).expect("resolve").is_some());
    }

    #[test]
    fn resolve_mode_cli_invalid_panics() {
        let world = ExtractWorld::permissions_world(|c| c.mode_policy = ModeSource::Cli("u+vv".into()));
        let res = catch_unwind(AssertUnwindSafe(|| resolve_mode(&world.rt())));
        assert!(res.is_err(), "invalid --mode must trip the parse-time precondition");
    }

    #[test]
    fn resolve_mode_stored_reads_meta() {
        let world = ExtractWorld::permissions_world(|c| c.mode_policy = ModeSource::Stored);
        world.db.set_archive_mode_changes("u+w").expect("set stored mode");
        assert!(resolve_mode(&world.rt()).expect("resolve").is_some());
    }

    #[test]
    fn resolve_mode_stored_without_meta_is_none() {
        let world = ExtractWorld::permissions_world(|c| c.mode_policy = ModeSource::Stored);
        assert!(resolve_mode(&world.rt()).expect("resolve").is_none());
    }

    // ---------------------------------------------------------------------
    // apply_one — ownership
    // ---------------------------------------------------------------------

    #[test]
    fn apply_one_ogp_none_leaves_ownership() {
        let world = ExtractWorld::permissions_world(|_| {});
        let tgt = world.write_file("report.docx", b"docx");
        let rec = mk_file("report.docx", FileType::File, Some(0o644), None, None,
                          Some(current_uid()), Some(current_gid()), None, None, None, None, None);
        let errs = apply_one(&world.config, &rec, &tgt, &ps_none());
        assert!(errs.is_empty());
        let md = fs::metadata(&tgt).expect("meta");
        assert_eq!(md.uid(), current_uid());
        assert_eq!(md.gid(), current_gid());
    }

    #[cfg(unix)]
    #[test]
    fn apply_one_chown_gid_applies() {
        let world = ExtractWorld::permissions_world(|_| {});
        let tgt = world.write_file("photo.jpg", b"jpeg");
        let gid = current_gid();
        let mut policy = OwnerGroupPolicy::default();
        policy.group_override = Some(IdentitySpec { name: None, id: Some(gid) });
        let rec = mk_file("photo.jpg", FileType::File, None, None, None, None, None, None, None,
                          None, None, None);
        let errs = apply_one(&world.config, &rec, &tgt, &ps_ogp(policy));
        assert!(errs.is_empty(), "gid -> own group must succeed unprivileged: {errs:?}");
        assert_eq!(fs::metadata(&tgt).expect("meta").gid(), gid);
    }

    #[test]
    fn apply_one_chown_missing_target_errors() {
        let world = ExtractWorld::permissions_world(|_| {});
        let tgt = world.proot().join("missing.bin");
        let mut policy = OwnerGroupPolicy::default();
        policy.owner_override = Some(IdentitySpec { name: None, id: Some(current_uid()) });
        let rec = mk_file("missing.bin", FileType::File, None, None, None, None, None, None, None,
                          None, None, None);
        let errs = apply_one(&world.config, &rec, &tgt, &ps_ogp(policy));
        assert_eq!(errs.len(), 1, "a single chown failure on a missing target");
    }

    #[cfg(unix)]
    #[test]
    fn apply_one_chown_uid_permission_denied_when_unprivileged() {
        let world = ExtractWorld::permissions_world(|_| {});
        let tgt = world.write_file("raw.arw", b"arw");
        let mut policy = OwnerGroupPolicy::default();
        policy.owner_override = Some(IdentitySpec { name: None, id: Some(0) });
        let rec = mk_file("raw.arw", FileType::File, None, None, None, None, None, None, None,
                          None, None, None);
        let errs = apply_one(&world.config, &rec, &tgt, &ps_ogp(policy));
        if nix::unistd::geteuid().is_root() {
            assert!(errs.is_empty(), "root may chown");
        } else {
            assert_eq!(errs.len(), 1, "unprivileged chown to uid 0 must fail");
        }
    }

    // ---------------------------------------------------------------------
    // apply_one — mode
    // ---------------------------------------------------------------------

    #[test]
    fn apply_one_mode_applies() {
        let world = ExtractWorld::permissions_world(|_| {});
        let tgt = world.write_file("config.xml", b"<xml/>");
        let rec = mk_file("config.xml", FileType::File, Some(0o640), None, None, None, None,
                          None, None, None, None, None);
        let errs = apply_one(&world.config, &rec, &tgt, &ps_none());
        assert!(errs.is_empty());
        assert_eq!(on_disk_mode(&tgt), 0o640);
    }

    #[test]
    fn apply_one_mode_changes_applied_on_top() {
        let world = ExtractWorld::permissions_world(|_| {});
        let tgt = world.write_file("style.css", b"body{}");
        let rec = mk_file("style.css", FileType::File, Some(0o644), None, None, None, None,
                          None, None, None, None, None);
        let ps = OwnerGroupMode { ogp: None, mp: Some(parse_mode_changes("u+x").expect("mode")) };
        let errs = apply_one(&world.config, &rec, &tgt, &ps);
        assert!(errs.is_empty());
        assert_eq!(on_disk_mode(&tgt), 0o744);
    }

    #[test]
    fn apply_one_no_same_permissions_skips_mode() {
        let world = ExtractWorld::permissions_world(|c| c.attributes.no_same_permissions = true);
        let tgt = world.write_file("index.html", b"<html/>");
        let rec = mk_file("index.html", FileType::File, Some(0o600), None, None, None, None,
                          None, None, None, None, None);
        let errs = apply_one(&world.config, &rec, &tgt, &ps_none());
        assert!(errs.is_empty());
        assert_eq!(on_disk_mode(&tgt), 0o644, "mode untouched with --no-same-permissions");
    }

    #[cfg(unix)]
    #[test]
    fn apply_one_symlink_skips_mode_only() {
        let world = ExtractWorld::permissions_world(|_| {});
        let real = world.write_file("target.txt", b"t");
        let tgt = world.proot().join("link.txt");
        std::os::unix::fs::symlink(&real, &tgt).expect("symlink");
        let rec = mk_file("link.txt", FileType::Symlink(LinkType::File), Some(0o600),
                          None, None, None, None, None, None, None, None, None);
        let errs = apply_one(&world.config, &rec, &tgt, &ps_none());
        assert!(errs.is_empty());
        // mode is kernel-fixed on symlinks; the target keeps its own mode.
        assert_eq!(on_disk_mode(&real), 0o644);
    }

    #[test]
    fn apply_one_null_mode_is_noop() {
        let world = ExtractWorld::permissions_world(|_| {});
        let tgt = world.write_file("readme.txt", b"hi");
        let rec = mk_file("readme.txt", FileType::File, None, None, None, None, None, None, None,
                          None, None, None);
        let errs = apply_one(&world.config, &rec, &tgt, &ps_none());
        assert!(errs.is_empty());
        assert_eq!(on_disk_mode(&tgt), 0o644, "no record mode -> untouched");
    }

    #[test]
    fn apply_one_mode_error_on_missing_target() {
        let world = ExtractWorld::permissions_world(|_| {});
        let tgt = world.proot().join("missing.pdf");
        let rec = mk_file("missing.pdf", FileType::File, Some(0o600), None, None, None, None,
                          None, None, None, None, None);
        let errs = apply_one(&world.config, &rec, &tgt, &ps_none());
        assert_eq!(errs.len(), 1, "chmod on a missing target is a single error");
    }

    // ---------------------------------------------------------------------
    // apply_one — xattrs / ACL / SELinux
    // ---------------------------------------------------------------------

    #[test]
    fn apply_one_xattr_applies_when_supported() {
        let world = ExtractWorld::permissions_world(|_| {});
        let scratch = world.path("probe");
        fs::create_dir_all(&scratch).expect("scratch");
        if let Err(e) = probe_xattr(&scratch) {
            eprintln!("skipped (unsupported): xattr: {e}");
            return;
        }
        let tgt = world.write_file("paper.pdf", b"%PDF");
        let rec = mk_file("paper.pdf", FileType::File, None, None, None, None, None, None, None,
                          Some(xattr_json()), None, None);
        let errs = apply_one(&world.config, &rec, &tgt, &ps_none());
        assert!(errs.is_empty(), "xattr restore must succeed: {errs:?}");
        let raw = crate::common::xattr::get_file_xattr(&tgt).expect("recapture");
        let map = crate::common::xattr::tests::decoded_xattr_map(&raw);
        assert_eq!(map.get("user.meta"), Some(&b"v".to_vec()),
                   "xattr present after restore: {raw}");
    }

    #[test]
    fn apply_one_no_xattrs_gate_skips() {
        let world = ExtractWorld::permissions_world(|c| c.attributes.no_xattrs = true);
        let tgt = world.write_file("notes.typst", b"= hi");
        let rec = mk_file("notes.typst", FileType::File, None, None, None, None, None, None, None,
                          Some(xattr_json()), None, None);
        let errs = apply_one(&world.config, &rec, &tgt, &ps_none());
        assert!(errs.is_empty());
        let raw = crate::common::xattr::get_file_xattr(&tgt).expect("recapture");
        assert!(crate::common::xattr::tests::decoded_xattr_map(&raw).is_empty(),
                "nothing applied with --no-xattrs: {raw}");
    }

    #[test]
    fn apply_one_acl_applies_when_supported() {
        let world = ExtractWorld::permissions_world(|_| {});
        let scratch = world.path("probe");
        fs::create_dir_all(&scratch).expect("scratch");
        if let Err(e) = probe_acl(&scratch) {
            eprintln!("skipped (unsupported): acl: {e}");
            return;
        }
        let tgt = world.write_file("book.xlsx", b"xlsx");
        let rec = mk_file("book.xlsx", FileType::File, None, None, None, None, None, None, None,
                          None, Some(empty_acl_json()), None);
        let errs = apply_one(&world.config, &rec, &tgt, &ps_none());
        assert!(errs.is_empty(), "acl restore must succeed: {errs:?}");
        let raw = crate::common::xattr::get_file_acl(&tgt).expect("recapture");
        assert!(raw.contains("user-obj\":7"), "acl present after restore: {raw}");
    }

    #[test]
    fn apply_one_no_acls_gate_skips() {
        let world = ExtractWorld::permissions_world(|c| c.attributes.no_acls = true);
        let tgt = world.write_file("rules.yaml", b"a: 1");
        let rec = mk_file("rules.yaml", FileType::File, None, None, None, None, None, None, None,
                          None, Some(empty_acl_json()), None);
        let errs = apply_one(&world.config, &rec, &tgt, &ps_none());
        assert!(errs.is_empty());
        let raw = crate::common::xattr::get_file_acl(&tgt).expect("recapture");
        assert!(!raw.contains("user-obj\":7"), "nothing applied with --no-acls: {raw}");
    }

    #[test]
    fn apply_one_selinux_applies_when_supported() {
        let world = ExtractWorld::permissions_world(|_| {});
        let scratch = world.path("probe");
        fs::create_dir_all(&scratch).expect("scratch");
        if let Err(e) = probe_selinux(&scratch) {
            eprintln!("skipped (unsupported): selinux: {e}");
            return;
        }
        let tgt = world.write_file("video.mp4", b"mp4");
        let ctx = crate::common::xattr::get_file_selinux_data(&scratch.join("probe_selinux"))
            .expect("base context");
        let rec = mk_file("video.mp4", FileType::File, None, None, None, None, None, None, None,
                          None, None, Some(ctx.clone()));
        let errs = apply_one(&world.config, &rec, &tgt, &ps_none());
        assert!(errs.is_empty(), "selinux restore must succeed: {errs:?}");
        assert_eq!(crate::common::xattr::get_file_selinux_data(&tgt).expect("recapture"), ctx);
    }

    #[test]
    fn apply_one_no_selinux_gate_skips() {
        let world = ExtractWorld::permissions_world(|c| c.attributes.no_selinux = true);
        let tgt = world.write_file("anim.gif", b"GIF89a");
        let rec = mk_file("anim.gif", FileType::File, None, None, None, None, None, None, None,
                          None, None, Some(b"system_u:object_r:etc_t:s0".to_vec()));
        let errs = apply_one(&world.config, &rec, &tgt, &ps_none());
        assert!(errs.is_empty(), "no-selinux must not attempt the set");
    }

    // ---------------------------------------------------------------------
    // apply_one — times
    // ---------------------------------------------------------------------

    #[test]
    fn apply_times_four_combos() {
        let world = ExtractWorld::permissions_world(|_| {});
        let t = world.path("times.bin");
        fs::write(&t, b"x").expect("write");
        let ft = FileTime::from_unix_time(1_600_000_000, 42);

        apply_times(&t, Some(ft), None).expect("set atime");
        assert_eq!(FileTime::from_system_time(fs::metadata(&t).expect("meta").accessed().expect("a")), ft);
        assert_ne!(FileTime::from_system_time(fs::metadata(&t).expect("meta").modified().expect("m")), ft);

        apply_times(&t, None, Some(ft)).expect("set mtime");
        assert_eq!(FileTime::from_system_time(fs::metadata(&t).expect("meta").modified().expect("m")), ft);

        apply_times(&t, Some(ft), Some(ft)).expect("set both");
        assert_eq!(FileTime::from_system_time(fs::metadata(&t).expect("meta").accessed().expect("a")), ft);
        assert_eq!(FileTime::from_system_time(fs::metadata(&t).expect("meta").modified().expect("m")), ft);

        apply_times(&t, None, None).expect("no-op");
        assert_eq!(FileTime::from_system_time(fs::metadata(&t).expect("meta").accessed().expect("a")), ft);
    }

    #[test]
    fn apply_one_times_respect_gate_axes() {
        // The gate is (apply_mtime && mtime) || (apply_atime && atime); each
        // axis applies independently. Sweep the four gate combos.
        let combos: &[(bool, bool)] = &[(true, true), (true, false), (false, true), (false, false)];
        for (apply_mtime, apply_atime) in combos {
            let world = ExtractWorld::permissions_world(|c| {
                c.attributes.apply_mtime = *apply_mtime;
                c.attributes.apply_atime = *apply_atime;
            });
            let tgt = world.write_file("pic.heic", b"heic");
            let m_old = dt(1_500_000_000, 0);
            let m_new = dt(1_600_000_000, 0);
            let a_old = dt(1_400_000_000, 0);
            let a_new = dt(1_700_000_000, 0);
            apply_times(
                &tgt,
                Some(FileTime::from_system_time(SystemTime::from(a_old))),
                Some(FileTime::from_system_time(SystemTime::from(m_old))),
            ).expect("seed times");
            let rec = mk_file("pic.heic", FileType::File, None, Some(m_new), Some(a_new),
                              None, None, None, None, None, None, None);
            let errs = apply_one(&world.config, &rec, &tgt, &ps_none());
            assert!(errs.is_empty());
            let live_m = FileTime::from_system_time(fs::metadata(&tgt).expect("meta").modified().expect("m"));
            let live_a = FileTime::from_system_time(fs::metadata(&tgt).expect("meta").accessed().expect("a"));
            assert_eq!(live_m, FileTime::from_system_time(SystemTime::from(
                if *apply_mtime { m_new } else { m_old })), "apply_mtime={apply_mtime}");
            assert_eq!(live_a, FileTime::from_system_time(SystemTime::from(
                if *apply_atime { a_new } else { a_old })), "apply_atime={apply_atime}");
        }
    }

    /// The OR short-circuit: only an mtime present while only the atime gate is
    /// open must NOT touch the file.
    #[test]
    fn apply_one_times_or_gate_short_circuit() {
        let world = ExtractWorld::permissions_world(|c| {
            c.attributes.apply_mtime = false;
            c.attributes.apply_atime = true;
        });
        let tgt = world.write_file("scan.jpeg", b"jpeg");
        let m_old = dt(1_500_000_000, 0);
        apply_times(&tgt, None, Some(FileTime::from_system_time(SystemTime::from(m_old))))
            .expect("seed mtime");
        // record has an mtime but NO atime -> neither gate holds.
        let rec = mk_file("scan.jpeg", FileType::File, None, Some(dt(1_600_000_000, 0)), None,
                          None, None, None, None, None, None, None);
        let errs = apply_one(&world.config, &rec, &tgt, &ps_none());
        assert!(errs.is_empty());
        let live_m = FileTime::from_system_time(fs::metadata(&tgt).expect("meta").modified().expect("m"));
        assert_eq!(live_m, FileTime::from_system_time(SystemTime::from(m_old)),
                   "nothing must be applied when no gate holds");
    }

    // ---------------------------------------------------------------------
    // apply_one — error accounting
    // ---------------------------------------------------------------------

    #[test]
    fn apply_one_missing_target_cascades() {
        let world = ExtractWorld::permissions_world(|c| {
            c.attributes.apply_mtime = true;
            c.attributes.apply_atime = true;
        });
        let tgt = world.proot().join("missing.bin");
        let rec = mk_file("missing.bin", FileType::File, Some(0o600),
                          Some(dt(1_500_000_000, 0)), Some(dt(1_600_000_000, 0)),
                          None, None, None, None,
                          Some(xattr_json()), Some(empty_acl_json()),
                          Some(b"system_u:object_r:etc_t:s0".to_vec()));
        // mode + xattr + acl + selinux + times all fail on a missing target.
        let errs = apply_one(&world.config, &rec, &tgt, &ps_none());
        assert_eq!(errs.len(), 5, "each applied facet reports exactly one error: {errs:?}");
    }

    #[test]
    fn apply_one_counts_only_later_failures() {
        let world = ExtractWorld::permissions_world(|_| {});
        let tgt = world.write_file("slide.pptx", b"pptx");
        // step 2 (mode) succeeds; step 3 (xattr) fails on a malformed blob.
        let rec = mk_file("slide.pptx", FileType::File, Some(0o600), None, None,
                          None, None, None, None,
                          Some("not-json".to_string()), None, None);
        let errs = apply_one(&world.config, &rec, &tgt, &ps_none());
        assert_eq!(errs.len(), 1, "only the xattr parse failed: {errs:?}");
        assert_eq!(on_disk_mode(&tgt), 0o600, "mode was applied despite the later error");
    }

    // ---------------------------------------------------------------------
    // inner_apply_permissions
    // ---------------------------------------------------------------------

    fn out_record(id: i64, rel: &str, file_id: Option<FileId>, is_dir: bool) -> OutTreeRecord {
        let mut flags = OutTreeFlags::default();
        flags.set(OutTreeFlag::IsDirectory, is_dir);
        OutTreeRecord {
            id: OutTreeId(id),
            abs_path: PathBuf::from(rel),
            file_id,
            flags,
            canonical_id: if is_dir { None } else { Some(OutTreeId(id)) },
        }
    }

    #[test]
    fn inner_apply_ancestor_no_record_is_clean() {
        let world = ExtractWorld::permissions_world(|_| {});
        let out = out_record(1, "/ed/docs", None, true);
        let mut results: Vec<(OutTreeId, bool)> = Vec::new();
        let mut captured: Vec<(Option<FileId>, OutTreeId)> = Vec::new();
        inner_apply_permissions(
            &world.rt(), &None, out.clone(),
            &ps_none(),
            &mut |fid, oid, _err| captured.push((fid, oid)),
            &mut results,
        ).expect("inner");
        assert_eq!(results, vec![(out.id, true)], "ancestor rows are 'applied'");
        assert!(captured.is_empty(), "no errors to capture");
    }

    #[test]
    fn inner_apply_fail_fast_raises() {
        let world = ExtractWorld::permissions_world(|c| c.process.fail_fast = true);
        let rec = mk_file("x.bin", FileType::File, Some(0o600), None, None, None, None,
                          None, None, None, None, None);
        let out = out_record(2, "/ed/x.bin", Some(FileId(1)), false);
        let mut results: Vec<(OutTreeId, bool)> = Vec::new();
        let res = inner_apply_permissions(
            &world.rt(), &Some(rec), out.clone(),
            &ps_none(),
            &mut |_, _, _| {},
            &mut results,
        );
        // target `/ed/x.bin` does not exist -> mode fails -> fail_fast raises.
        assert!(matches!(res, Err(Error::Other(_))), "fail-fast must raise: {res:?}");
        assert_eq!(results, vec![(out.id, false)]);
    }

    #[test]
    fn inner_apply_without_fail_fast_captures_and_returns_ok() {
        let world = ExtractWorld::permissions_world(|_| {});
        let rec = mk_file("x.bin", FileType::File, Some(0o600), None, None, None, None,
                          None, None, None, None, None);
        let out = out_record(3, "/ed/x.bin", Some(FileId(1)), false);
        let mut results: Vec<(OutTreeId, bool)> = Vec::new();
        let mut captured: Vec<(Option<FileId>, OutTreeId)> = Vec::new();
        inner_apply_permissions(
            &world.rt(), &Some(rec), out.clone(),
            &ps_none(),
            &mut |fid, oid, _err| captured.push((fid, oid)),
            &mut results,
        ).expect("inner");
        assert_eq!(results, vec![(out.id, false)]);
        assert_eq!(captured, vec![(Some(FileId(1)), out.id)]);
    }

    // ---------------------------------------------------------------------
    // apply_permissions_link_sources
    // ---------------------------------------------------------------------

    #[test]
    fn link_sources_applies_metadata() {
        let world = ExtractWorld::permissions_world(|c| c.placement.link_tree = true);
        let id = world.p_add_file("photo.jpg", b"jpeg", |r| r.mode = Some(0o640));
        world.db.set_file_flag(id, FileFlag::AtLinkSource, true).expect("at link source");
        let cid = world.db.get_file_by_id::<FileRecord>(id)
            .expect("get").expect("row").content_id().expect("cid").0;
        let src = world.proot().join(".sources").join(&cid);
        fs::create_dir_all(src.parent().expect("parent")).expect("src dir");
        fs::write(&src, b"jpeg").expect("write src");

        let mut recorder = Recorder::new(&world.db, true, Some(0));
        apply_permissions_link_sources(&world.rt(), &mut recorder, &ps_none()).expect("apply");
        assert!(world.p_file_flag(id, FileFlag::AppliedMetadata));
        assert_eq!(on_disk_mode(&src), 0o640);
        assert_eq!(recorder.buffered(), 0);
    }

    #[test]
    fn link_sources_custom_dir() {
        let world = ExtractWorld::permissions_world(|c| {
            c.placement.link_tree = true;
            c.placement.link_source = Some(PathBuf::from("links"));
        });
        let id = world.p_add_file("pic.heif", b"heif", |r| r.mode = Some(0o600));
        world.db.set_file_flag(id, FileFlag::AtLinkSource, true).expect("at link source");
        let cid = world.db.get_file_by_id::<FileRecord>(id)
            .expect("get").expect("row").content_id().expect("cid").0;
        let src = world.proot().join("links").join(&cid);
        fs::create_dir_all(src.parent().expect("parent")).expect("src dir");
        fs::write(&src, b"heif").expect("write src");

        let mut recorder = Recorder::new(&world.db, true, Some(0));
        apply_permissions_link_sources(&world.rt(), &mut recorder, &ps_none()).expect("apply");
        assert!(world.p_file_flag(id, FileFlag::AppliedMetadata));
        assert_eq!(on_disk_mode(&src), 0o600);
    }

    #[test]
    fn link_sources_panics_without_content_id() {
        let world = ExtractWorld::permissions_world(|c| c.placement.link_tree = true);
        let id = world.p_add_file("cr2", b"cr2", |_| {});
        world.db.set_file_flag(id, FileFlag::AtLinkSource, true).expect("at link source");
        world.un_canonicalize(id);
        let mut recorder = Recorder::new(&world.db, true, Some(0));
        let res = catch_unwind(AssertUnwindSafe(|| {
            apply_permissions_link_sources(&world.rt(), &mut recorder, &ps_none())
        }));
        assert!(res.is_err(), "a digest-less AtLinkSource row must trip the expect");
    }

    #[test]
    fn link_sources_fail_fast_raises() {
        let world = ExtractWorld::permissions_world(|c| {
            c.placement.link_tree = true;
            c.process.fail_fast = true;
        });
        let id = world.p_add_file("dng", b"dng", |r| r.mode = Some(0o600));
        world.db.set_file_flag(id, FileFlag::AtLinkSource, true).expect("at link source");
        // .sources file NOT created -> mode restore fails -> fail_fast.
        let mut recorder = Recorder::new(&world.db, true, Some(0));
        let res = apply_permissions_link_sources(&world.rt(), &mut recorder, &ps_none());
        assert!(matches!(res, Err(Error::Other(_))), "fail-fast must raise: {res:?}");
    }

    // ---------------------------------------------------------------------
    // process_batched_files / process_batches_dirs
    // ---------------------------------------------------------------------

    #[test]
    fn process_batched_files_ingests_results() {
        let world = ExtractWorld::permissions_world(|_| {});
        let id = world.p_add_file("data.json", b"{}", |r| r.mode = Some(0o600));
        let out = world.padd_out("data.json", id,
                                placed_flags());
        let ps = ps_none();
        let mut failures: Vec<(OutTreeId, FileStatError)> = Vec::new();
        process_batched_files(
            &world.rt(),
            &mut |_fid, oid, err| failures.push((oid, err)),
            &ps,
        ).expect("process files");
        assert!(world.p_out_flag(out, OutTreeFlag::AppliedMetadata));
        assert!(!world.p_out_flag(out, OutTreeFlag::ErrorWhileApplyingMetadata));
        assert_eq!(on_disk_mode(&world.proot().join("data.json")), 0o600);
        assert!(failures.is_empty());
    }

    #[test]
    fn process_batches_dirs_ingests_results() {
        let world = ExtractWorld::permissions_world(|_| {});
        let dir = world.proot().join("docs");
        fs::create_dir_all(&dir).expect("docs dir");
        world.db.insert_file(&NewFileRecord {
            abs_path: dir.clone(),
            abs_path_raw: None,
            ext: String::new(),
            size: 0,
            mtime: None, atime: None, ctime: None,
            uid: None, gid: None, mode: Some(0o750),
            ftype: Some(FileType::Directory),
            xattrs: None, posix_acl: None, selinux_ctx: None, win_perm: None,
            link_dst: None, link_dst_raw: None,
            device_id: None, inode_id: None, major: None, minor: None,
        }).expect("insert dir row");
        let id = world.db.file_id_by_abs_path(&dir).expect("lookup").expect("row");
        let mut dir_flags = OutTreeFlags::default();
        dir_flags.set(OutTreeFlag::IsDirectory, true);
        let out = world.padd_out("docs", id, dir_flags);
        let mut failures: Vec<(OutTreeId, FileStatError)> = Vec::new();
        process_batches_dirs(
            &world.rt(),
            &mut |_fid, oid, err| failures.push((oid, err)),
            &ps_none(),
        ).expect("process dirs");
        assert!(world.p_out_flag(out, OutTreeFlag::AppliedMetadata));
        assert_eq!(on_disk_mode(&dir), 0o750);
        assert!(failures.is_empty());
    }

    #[test]
    fn process_batch_graceful_interrupt_stops() {
        let world = ExtractWorld::permissions_world(|_| {});
        let id = world.p_add_file("rules.yaml", b"a", |r| r.mode = Some(0o600));
        let out = world.padd_out("rules.yaml", id,
                                placed_flags());
        world.shutdown.request_graceful();
        let res = process_batched_files(
            &world.rt(),
            &mut |_, _, _| {},
            &ps_none(),
        );
        assert!(matches!(res, Err(Error::Interrupted)), "graceful must stop: {res:?}");
        assert!(!world.p_out_flag(out, OutTreeFlag::AppliedMetadata),
                "no partial ingestion on interrupt");
    }

    // ---------------------------------------------------------------------
    // run
    // ---------------------------------------------------------------------

    #[test]
    fn run_applies_non_dir_and_dirs() {
        let world = ExtractWorld::permissions_world(|_| {});
        let fid = world.p_add_file("report.docx", b"docx", |r| r.mode = Some(0o640));
        let fout = world.padd_out("report.docx", fid,
                                 placed_flags());

        let dir = world.proot().join("docs");
        fs::create_dir_all(&dir).expect("docs");
        let mut flags = OutTreeFlags::default();
        flags.set(OutTreeFlag::IsDirectory, true);
        world.db.insert_file(&NewFileRecord {
            abs_path: dir.clone(), abs_path_raw: None, ext: String::new(), size: 0,
            mtime: None, atime: None, ctime: None, uid: None, gid: None, mode: Some(0o700),
            ftype: Some(FileType::Directory), xattrs: None, posix_acl: None,
            selinux_ctx: None, win_perm: None, link_dst: None, link_dst_raw: None,
            device_id: None, inode_id: None, major: None, minor: None,
        }).expect("insert dir");
        let did = world.db.file_id_by_abs_path(&dir).expect("lookup").expect("row");
        let dout = world.padd_out("docs", did, flags);

        run(&world.rt()).expect("run");

        assert_eq!(on_disk_mode(&world.proot().join("report.docx")), 0o640);
        assert_eq!(on_disk_mode(&dir), 0o700);
        assert!(world.p_out_flag(fout, OutTreeFlag::AppliedMetadata));
        assert!(world.p_out_flag(dout, OutTreeFlag::AppliedMetadata));
    }

    #[test]
    fn run_no_overwrite_dir_skips_dirs() {
        let world = ExtractWorld::permissions_world(|c| c.attributes.no_overwrite_dir = true);
        let fid = world.p_add_file("config.xml", b"<x/>", |r| r.mode = Some(0o600));
        let fout = world.padd_out("config.xml", fid,
                                 placed_flags());

        let dir = world.proot().join("docs");
        fs::create_dir_all(&dir).expect("docs");
        let mut flags = OutTreeFlags::default();
        flags.set(OutTreeFlag::IsDirectory, true);
        let drec = NewFileRecord {
            abs_path: dir.clone(), abs_path_raw: None, ext: String::new(), size: 0,
            mtime: None, atime: None, ctime: None, uid: None, gid: None, mode: Some(0o700),
            ftype: Some(FileType::Directory), xattrs: None, posix_acl: None,
            selinux_ctx: None, win_perm: None, link_dst: None, link_dst_raw: None,
            device_id: None, inode_id: None, major: None, minor: None,
        };
        world.db.insert_file(&drec).expect("insert dir");
        let did = world.db.file_id_by_abs_path(&dir).expect("lookup").expect("row");
        let dout = world.padd_out("docs", did, flags);

        run(&world.rt()).expect("run");

        assert!(world.p_out_flag(fout, OutTreeFlag::AppliedMetadata));
        assert!(!world.p_out_flag(dout, OutTreeFlag::AppliedMetadata),
                "--no-overwrite-dir leaves dirs untouched");
        assert_eq!(world.file_mode(did), Some(0o700));
    }

    #[test]
    fn run_link_tree_applies_sources() {
        let world = ExtractWorld::permissions_world(|c| c.placement.link_tree = true);
        let id = world.p_add_file("photo.jpg", b"jpeg", |r| r.mode = Some(0o640));
        world.db.set_file_flag(id, FileFlag::AtLinkSource, true).expect("at link source");
        let cid = world.db.get_file_by_id::<FileRecord>(id)
            .expect("get").expect("row").content_id().expect("cid").0;
        let src = world.proot().join(".sources").join(&cid);
        fs::create_dir_all(src.parent().expect("parent")).expect("src dir");
        fs::write(&src, b"jpeg").expect("write src");

        run(&world.rt()).expect("run");

        assert!(world.p_file_flag(id, FileFlag::AppliedMetadata));
        assert_eq!(on_disk_mode(&src), 0o640);
    }

    /// `global_mark_phase` promotes EVERY file row, even ones never seen by the
    /// out_tree (e.g. hardlink duplicates without an out row).
    #[test]
    fn run_marks_every_file_permissions_restored() {
        let world = ExtractWorld::permissions_world(|_| {});
        let fid = world.p_add_file("paper.pdf", b"%PDF", |r| r.mode = Some(0o640));
        world.padd_out("paper.pdf", fid, placed_flags());
        // an orphaned file row with no out_tree entry at all
        let orphan = world.p_add_file("orphan.txt", b"x", |_| {});

        run(&world.rt()).expect("run");

        assert_eq!(world.file_phase(fid), FilePhase::PermissionsRestored);
        assert_eq!(world.file_phase(orphan), FilePhase::PermissionsRestored);
        let count = world.db.t_files_in_phase::<StrippedRecord>(FilePhase::PermissionsRestored)
            .expect("list").len();
        assert_eq!(count, 2, "both rows promoted regardless of phase participation");
    }

    // ---------------------------------------------------------------------
    // sidecar manifests
    // ---------------------------------------------------------------------

    fn sidecar_names(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = fs::read_dir(dir).expect("read dir")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    #[test]
    fn claim_sidecar_deterministic_then_random_fallback() {
        let d = tempfile::tempdir().expect("tempdir");
        let target = d.path().join("name.txt");
        fs::write(&target, b"x").expect("target");

        let mut seed = 1;
        let f = claim_sidecar_file(&target, false, 10, &mut seed).expect("claim");
        assert_eq!(sidecar_names(d.path()), vec!["name.txt", "name.txt.json"]);
        drop(f);

        // Deterministic name occupied -> random-suffix candidate.
        let f2 = claim_sidecar_file(&target, false, 10, &mut seed).expect("claim");
        let names = sidecar_names(d.path());
        assert_eq!(names.len(), 3, "one extra file claimed: {names:?}");
        let sid = &names[2];
        assert!(sid.starts_with("name.txt.") && sid.ends_with(".json"), "{names:?}");
        drop(f2);
    }

    #[test]
    fn claim_sidecar_hidden_prefixes_dot() {
        let d = tempfile::tempdir().expect("tempdir");
        let target = d.path().join("note.md");
        fs::write(&target, b"x").expect("target");
        let mut seed = 7;
        let f = claim_sidecar_file(&target, true, 1, &mut seed).expect("claim");
        assert_eq!(sidecar_names(d.path()), vec![".note.md.json", "note.md"]);
        drop(f);
    }

    #[test]
    fn claim_sidecar_exhaustion_returns_general_error() {
        let d = tempfile::tempdir().expect("tempdir");
        let target = d.path().join("busy.txt");
        fs::write(&target, b"x").expect("target");
        // Occupy the deterministic name and the first three candidate names the
        // seed produces, so `attempts = 3` cannot land a free one.
        fs::write(d.path().join("busy.txt.json"), b"x").expect("occupy deterministic");
        let mut seed = 0;
        for _ in 0..3 {
            fs::write(
                d.path().join(format!("busy.txt.{:08x}.json", sidecar_rand(&mut seed))),
                b"x",
            ).expect("occupy candidate");
        }
        seed = 0;
        let err = claim_sidecar_file(&target, false, 3, &mut seed)
            .expect_err("must exhaust");
        assert!(matches!(err, FileStatError::General { .. }), "{err:?}");
        assert!(
            err.to_string().contains("no sidecar file written"),
            "message: {err}"
        );
    }

    /// `on-error` sidecars land beside entries whose metadata application failed.
    #[test]
    fn run_on_error_writes_sidecar_where_metadata_failed() {
        let world = ExtractWorld::permissions_world(|c| c.sidecar = SidecarMode::OnError);
        // Remove the on-disk file so the metadata restore fails deterministically
        // (mode set on a missing target -> one FileStatError).
        let fid = world.p_add_file("broken.bin", b"data", |r| r.mode = Some(0o600));
        fs::remove_file(world.proot().join("broken.bin")).expect("remove");
        world.padd_out("broken.bin", fid, placed_flags());
        // A healthy entry must NOT get an on-error sidecar.
        let ok_id = world.p_add_file("ok.txt", b"ok", |r| r.mode = Some(0o644));
        world.padd_out("ok.txt", ok_id, placed_flags());

        run(&world.rt()).expect("run");

        assert!(world.proot().join("broken.bin.json").is_file(), "on-error sidecar");
        assert!(!world.proot().join("ok.txt.json").exists(), "no sidecar on success");
    }

    #[test]
    fn run_always_writes_sidecar_for_every_placed_entry() {
        let world = ExtractWorld::permissions_world(|c| c.sidecar = SidecarMode::Always);
        let fid = world.p_add_file("a.txt", b"a", |_| {});
        world.padd_out("a.txt", fid, placed_flags());

        run(&world.rt()).expect("run");

        assert!(world.proot().join("a.txt.json").is_file(), "always sidecar");
    }

    #[test]
    fn run_never_writes_no_sidecar_even_on_error() {
        let world = ExtractWorld::permissions_world(|c| c.sidecar = SidecarMode::Never);
        let fid = world.p_add_file("gone.bin", b"d", |r| r.mode = Some(0o600));
        fs::remove_file(world.proot().join("gone.bin")).expect("remove");
        world.padd_out("gone.bin", fid, placed_flags());

        run(&world.rt()).expect("run");

        assert!(!world.proot().join("gone.bin.json").exists(), "never mode");
    }

    #[test]
    fn run_sidecar_on_error_folds_to_never_with_no_errors() {
        let world = ExtractWorld::permissions_world(|c| {
            c.sidecar = SidecarMode::OnError;
            c.process.no_errors = true;
        });
        let fid = world.p_add_file("gone2.bin", b"d", |r| r.mode = Some(0o600));
        fs::remove_file(world.proot().join("gone2.bin")).expect("remove");
        world.padd_out("gone2.bin", fid, placed_flags());

        run(&world.rt()).expect("run");

        assert_eq!(world.config.effective_sidecar_mode(), SidecarMode::Never);
        assert!(!world.proot().join("gone2.bin.json").exists(), "folded to never");
    }

    #[test]
    fn run_link_tree_always_writes_sidecar_per_link() {
        let world = ExtractWorld::permissions_world(|c| {
            c.placement.link_tree = true;
            c.sidecar = SidecarMode::Always;
        });
        let id = world.p_add_file("photo.jpg", b"jpeg", |r| r.mode = Some(0o640));
        world.db.set_file_flag(id, FileFlag::AtLinkSource, true).expect("at link source");
        let cid = world.db.get_file_by_id::<FileRecord>(id)
            .expect("get").expect("row").content_id().expect("cid").0;
        let src = world.proot().join(".sources").join(&cid);
        fs::create_dir_all(src.parent().expect("parent")).expect("src dir");
        fs::write(&src, b"jpeg").expect("write src");
        // The out-tree link row (Placed) for the walk.
        world.padd_out("photo.jpg", id, placed_flags());

        run(&world.rt()).expect("run");

        assert!(world.proot().join("photo.jpg.json").is_file(), "sidecar beside the link");
    }
}
