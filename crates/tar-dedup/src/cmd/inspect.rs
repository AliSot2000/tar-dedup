//! `inspect` — show an archive the way it is.
//!
//! Reads whatever catalog is available (footer → leading `manifest.sqlite` →
//! nothing) without normalizing or promoting anything, and reports the rest as
//! missing: empty lines in text mode, `null` in JSON. Never hard-errors on a
//! partially readable archive — the degraded report explains what was seen.

use std::fs::File;
use std::io::{self as io, Write};
use std::path::{Path, PathBuf};

use path_clean::PathClean;
use serde::Serialize;

use crate::archive_footer::read_footer;
use crate::cli::{ArchiveInputArgs, CompressionFlags};
use crate::cmd::{CatalogSource, resolve_catalog_source};
use crate::common::defaults::SNAPSHOT_INIT_TAR_NAME;
use crate::common::perms::{IdentityMap, IdentitySpec};
use crate::config::resolve_compression;
use crate::db::Database;
use crate::db::flags::FileFlag;
use crate::error::{Error, Result};
use crate::tar_reader::open_tar_archive;

#[derive(Serialize)]
pub struct CatalogInfo {
    pub source: String,
    pub head_truncated: bool,
    pub first_member: Option<String>,
    /// Snapshot/payload member counts; only collected by the whole-archive scan
    /// (`--force`), so `null` until then.
    pub member_counts: Option<serde_json::Value>,
    pub note: Option<String>,
}

impl CatalogInfo {
    fn new() -> Self {
        Self {
            source: String::new(),
            head_truncated: false,
            first_member: None,
            member_counts: None,
            note: None,
        }
    }
}

#[derive(Serialize)]
pub struct InspectStats {
    pub counts_by_ftype: Vec<(String, u64)>,
    pub included: u64,
    pub excluded_by_include: u64,
    pub excluded_by_exclude: u64,
    pub unfiltered: u64,
    pub hashed: u64,
    pub hash_errors: u64,
    pub canonical_singletons: u64,
    pub canonical_files: u64,
    pub canonical_errors: u64,
    pub sparsified_files: u64,
    pub sparsified_errors: u64,
    pub bytes_all: u64,
    pub bytes_canonical: u64,
    pub bytes_archived: u64,
}

#[derive(Serialize)]
pub struct InspectReport {
    pub catalog: CatalogInfo,
    pub archive_size: Option<u64>,
    pub tool_version: Option<String>,
    pub os: Option<String>,
    pub archive_started_at: Option<String>,
    pub archive_completed_at: Option<String>,
    pub snapshot_taken_at: Option<String>,
    pub phase: Option<String>,
    pub session_count: Option<u64>,
    pub tar_writer_bytes_in: Option<u64>,
    pub tar_writer_bytes_out: Option<u64>,
    pub owner: Option<String>,
    pub group: Option<String>,
    pub owner_map: Option<Vec<String>>,
    pub group_map: Option<Vec<String>>,
    pub mode: Option<String>,
    pub transform: Option<String>,
    pub include_filters: Option<Vec<String>>,
    pub exclude_filters: Option<Vec<String>>,
    pub archive_arguments: Option<serde_json::Value>,
    pub extract_arguments: Option<serde_json::Value>,
    pub stats: Option<InspectStats>,
}

impl InspectReport {
    fn new() -> Self {
        Self {
            catalog: CatalogInfo::new(),
            archive_size: None,
            tool_version: None,
            os: None,
            archive_started_at: None,
            archive_completed_at: None,
            snapshot_taken_at: None,
            phase: None,
            session_count: None,
            tar_writer_bytes_in: None,
            tar_writer_bytes_out: None,
            owner: None,
            group: None,
            owner_map: None,
            group_map: None,
            mode: None,
            transform: None,
            include_filters: None,
            exclude_filters: None,
            archive_arguments: None,
            extract_arguments: None,
            stats: None,
        }
    }
}

/// Show archive metadata needed to plan an extraction (catalog, policies).
pub fn run(args: &ArchiveInputArgs) -> Result<()> {
    // INFO: `--force` is accepted but not implemented (whole-archive scan →
    // prefer the last snapshot). The degraded report below always fires.
    let source = resolve_catalog_source(args)?;
    let mut rep = InspectReport::new();

    match source {
        CatalogSource::Database(path) => {
            rep.catalog.source = "work-db".to_string();
            let db = Database::open(&path)?;
            rep.archive_size = config_archive_size(&db)?;
            fill_from_db(&db, &mut rep)?;
        }
        CatalogSource::Archive(path) => {
            rep.archive_size = file_size(&path);
            let dest = tmp_catalog_path();
            let mut db: Option<Database> = None;
            let mut footer_err: Option<String> = None;
            match read_footer(&path, &dest) {
                Ok(_) => {
                    db = Some(Database::open(&dest)?);
                    rep.catalog.source = "footer".to_string();
                }
                Err(e) => footer_err = Some(e.to_string()),
            }

            if db.is_none() {
                peek_first_member(&path, &dest, &mut db, &mut rep)?;
                if db.is_some() {
                    rep.catalog.source = "leading-manifest".to_string();
                } else {
                    rep.catalog.source = "none".to_string();
                    rep.catalog.head_truncated = true;
                    let first = rep
                        .catalog
                        .first_member
                        .as_ref()
                        .map(|n| format!("`{n}`"))
                        .unwrap_or_else(|| String::from("(nothing)"));
                    let footer_note = match footer_err.as_ref() {
                        Some(e) if !e.starts_with("archive too small") => format!(
                            "footer unusable ({e}); "
                        ),
                        _ => String::new(),
                    };
                    rep.catalog.note = Some(format!(
                        "{footer_note}first tar member is {first} — not the leading \
                         manifest.sqlite. Use inspect --force (once implemented) to scan \
                         for snapshots."
                    ));
                }
            }
            if let Some(edb) = db.as_mut() {
                fill_from_db(&edb, &mut rep)?;
            }
            drop(db);
            let _ = std::fs::remove_file(&dest);
        }
    }

    render(&rep, args)?;
    Ok(())
}

/// Peek the first tar member. When it is the leading `manifest.sqlite`, capture
/// it into `tmp` and open it as-is; otherwise record its name for the degraded
/// report. Only the first member is examined — the whole-archive scan is the
/// future `--force` behavior.
fn peek_first_member(
    archive: &Path,
    tmp: &Path,
    db: &mut Option<Database>,
    rep: &mut InspectReport)
    -> Result<()> {
    let format = resolve_compression(&CompressionFlags::default(), archive)?;
    let mut arc = open_tar_archive(archive, &format)?;
    let entries = arc.entries().map_err(|e| Error::io(archive, e))?;
    for (m_idx, wrapped) in entries.enumerate() {
        if m_idx > 0 {
            break;
        }
        let mut entry = wrapped.map_err(|e| Error::io(archive, e))?;
        let name = match entry.path().map_err(|e| Error::io(archive, e)) {
            Ok(p) => match p.clean().file_name() {
                Some(f) => f.to_string_lossy().into_owned(),
                None => String::from("<unnamed>"),
            },
            Err(_) => String::from("<unreadable>"),
        };
        rep.catalog.first_member = Some(name.clone());
        if name == SNAPSHOT_INIT_TAR_NAME {
            let dest = tmp.join("catalog.sqlite");
            let mut out = File::create(&dest).map_err(|e| Error::io(&dest, e))?;
            match io::copy(&mut entry, &mut out) {
                Ok(_) => (),
                Err(e) => {
                    drop(out);
                    return Err(Error::io(&dest, e));
                }
            }
            drop(out);
            *db = Some(Database::open(&dest)?);
        }
        break;
    }
    Ok(())
}

fn fill_from_db(db: &Database, rep: &mut InspectReport) -> Result<()> {
    rep.tool_version = db.get_tar_dedup_version()?;
    rep.os = db.get_tar_dedup_os()?;

    let (started, finished, total) = db.inspect_session_bounds()?;
    rep.archive_started_at = started;
    rep.archive_completed_at = finished;
    rep.session_count = Some(total);
    rep.snapshot_taken_at = db
        .get_archive_snapshot_taken_at()?
        .map(|d| d.to_rfc3339());

    match db.get_archive_phase()? {
        Some(p) => rep.phase = Some(p.as_str().to_string()),
        None => match db.get_extract_phase()? {
            Some(p) => rep.phase = Some(p.as_str().to_string()),
            None => (),
        },
    }
    rep.tar_writer_bytes_in = db.get_tar_writer_bytes_in()?;
    rep.tar_writer_bytes_out = db.get_tar_writer_bytes_out()?;

    if let Some(policy) = db.get_archive_owner_policy()? {
        rep.owner = policy.owner_override.as_ref().map(identity_str);
        rep.group = policy.group_override.as_ref().map(identity_str);
        rep.owner_map = policy.owner_map.as_ref().map(identity_map_lines);
        rep.group_map = policy.group_map.as_ref().map(identity_map_lines);
    }
    rep.mode = db.get_archive_mode_changes()?;
    rep.transform = db.get_archive_transform()?;
    rep.include_filters = opt_nonempty(&db
        .get_filters_archive(false)?
        .into_iter()
        .filter(|f| !f.is_internal())
        .map(|f| f.expression.clone())
        .collect::<Vec<_>>());
    rep.exclude_filters = opt_nonempty(&db
        .get_filters_archive(true)?
        .into_iter()
        .filter(|f| !f.is_internal())
        .map(|f| f.expression.clone())
        .collect::<Vec<_>>());

    rep.archive_arguments = db.get_archive_config()?.as_ref().map(config_to_value).transpose()?;
    rep.extract_arguments = db.get_extract_config()?.as_ref().map(config_to_value).transpose()?;

    rep.stats = Some(gather_stats(db)?);
    Ok(())
}

fn opt_nonempty(v: &Vec<String>) -> Option<Vec<String>> {
    if v.is_empty() { None } else { Some(v.clone()) }
}

fn gather_stats(db: &Database) -> Result<InspectStats> {
    let (included, excl_inc, excl_exc, unfiltered) = db.inspect_filter_buckets()?;
    Ok(InspectStats {
        counts_by_ftype: db.inspect_by_ftype()?,
        included,
        excluded_by_include: excl_inc,
        excluded_by_exclude: excl_exc,
        unfiltered,
        hashed: db.inspect_count_hashed()?,
        hash_errors: db.inspect_count_flag(FileFlag::ErrorWhileHash)?,
        canonical_singletons: db.inspect_count_canonical_singletons()?,
        canonical_files: db.inspect_count_canonical_files()?,
        canonical_errors: db.inspect_count_flag(FileFlag::ErrorWhileDedup)?,
        sparsified_files: db.inspect_count_flag(FileFlag::HasSparse)?,
        sparsified_errors: db.inspect_count_flag(FileFlag::ErrorWhileSparsify)?,
        bytes_all: db.inspect_sum_bytes_all()?,
        bytes_canonical: db.inspect_sum_bytes_canonical()?,
        bytes_archived: db.inspect_sum_bytes_archived()?,
    })
}

/// Serialize a config struct to a reusable JSON value.
fn config_to_value<T: Serialize>(v: &T) -> Result<serde_json::Value> {
    let raw = serde_json::to_string(v)
        .map_err(|e| Error::Config(format!("inspect serialization failed: {e}")))?;
    Ok(serde_json::from_str::<serde_json::Value>(&raw)
        .map_err(|e| Error::Config(format!("inspect serialization failed: {e}")))?)
}

/// Render `NAME:UID` / `NAME` / `+UID` (the `--owner-map` file grammar).
fn identity_str(spec: &IdentitySpec) -> String {
    match (spec.name.as_ref(), spec.id.as_ref()) {
        (Some(n), Some(id)) => format!("{n}:{}", id),
        (Some(n), None) => n.to_string(),
        (None, Some(id)) => format!("+{}", id),
        (None, None) => String::new(),
    }
}

/// Emit an identity map as copy-paste-able `from to` lines (round-trips through
/// [`crate::common::perms::parse_map_lines`]).
fn identity_map_lines(map: &IdentityMap) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    out.push(String::from("# identity map: `FROM TO` (comments and blanks ignored)"));
    for (src, dst) in map.by_name.iter() {
        out.push(format!("{} {}", *src, identity_str(dst)));
    }
    for (src_id, dst) in map.by_id.iter() {
        out.push(format!("+{} {}", *src_id, identity_str(dst)));
    }
    out.sort();
    out
}

/// Best-effort size of the archive the work DB refers to.
fn config_archive_size(db: &Database) -> Result<Option<u64>> {
    match db.get_archive_config()? {
        Some(config) => Ok(file_size(&config.paths.archive_path)),
        None => Ok(None),
    }
}

/// Unique scratch path under the system temp dir (no leftover cleanup on error).
fn tmp_catalog_path() -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!(
        "tar-dedup-inspect-{}-{nanos}.sqlite",
        std::process::id()
    ))
}

fn pretty_json(v: &serde_json::Value) -> Option<String> {
    match serde_json::to_string_pretty(v) {
        Ok(s) => Some(s),
        Err(_) => None,
    }
}

fn file_size(path: &Path) -> Option<u64> {
    if !path.is_file() {
        return None;
    }
    match std::fs::metadata(path) {
        Ok(meta) => Some(meta.len()),
        Err(_) => None,
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

fn render(rep: &InspectReport, args: &ArchiveInputArgs) -> Result<()> {
    if args.json {
        let s = (if args.no_pretty {
            serde_json::to_string(rep)
        } else {
            serde_json::to_string_pretty(rep)
        })
        .map_err(|e| Error::Config(format!("inspect json serialization failed: {e}")))?;
        stdout_write(s)
    } else {
        stdout_write(render_text(rep, args.collapse, args.only.as_ref()))
    }
}

fn render_text(rep: &InspectReport, collapse: bool, only: Option<&String>) -> String {
    let mut out: Vec<String> = Vec::new();
    for (title, lines) in sections(rep) {
        if let Some(substr) = only {
            let title_lower = title.to_ascii_lowercase();
            let needle = substr.to_ascii_lowercase();
            if !title_lower.contains(&needle) {
                continue;
            }
        }
        out.push(format!("--- {title} ---"));
        if !lines.is_empty() {
            if collapse {
                out.push(lines.join("; "));
            } else {
                out.extend(lines);
            }
        }
        out.push(String::new());
    }
    out.join("\n")
}

/// `key: value` line (or nothing when the value is absent).
fn scalar(label: &str, value: Option<String>) -> Option<String> {
    match value {
        Some(v) => Some(format!("{label}: {v}")),
        None => None,
    }
}

fn push_opt(out: &mut Vec<String>, opt: Option<String>) {
    if let Some(line) = opt {
        out.push(line);
    }
}

fn sections(rep: &InspectReport) -> Vec<(String, Vec<String>)> {
    let mut secs: Vec<(String, Vec<String>)> = Vec::new();
    let catalog = &rep.catalog;

    let mut catalog_lines: Vec<String> = Vec::new();
    push_opt(&mut catalog_lines, scalar("source", Some(catalog.source.clone())));
    push_opt(&mut catalog_lines, scalar(
        "head_truncated",
        Some((if catalog.head_truncated { "true" } else { "false" }).to_string()),
    ));
    push_opt(&mut catalog_lines, scalar("first_member", catalog.first_member.clone()));
    push_opt(&mut catalog_lines, scalar("note", catalog.note.clone()));
    secs.push(("Catalog".to_string(), catalog_lines));

    let mut size_lines: Vec<String> = Vec::new();
    push_opt(
        &mut size_lines,
        scalar("archive_size", rep.archive_size.as_ref().map(|n| n.to_string())),
    );
    secs.push(("Archive Size".to_string(), size_lines));

    let mut scalar_section = |title: &str, label: &str, value: Option<String>| {
        let mut lines: Vec<String> = Vec::new();
        push_opt(&mut lines, scalar(label, value));
        secs.push((title.to_string(), lines));
    };
    scalar_section("Tool Version", "tool_version", rep.tool_version.clone());
    scalar_section("OS", "os", rep.os.clone());
    scalar_section("Archive Started At", "started_at", rep.archive_started_at.clone());
    scalar_section("Archive Completed At", "completed_at", rep.archive_completed_at.clone());
    scalar_section("Snapshot Taken At", "snapshot_taken_at", rep.snapshot_taken_at.clone());
    scalar_section("Phase", "phase", rep.phase.clone());
    scalar_section("Owner", "owner", rep.owner.clone());
    scalar_section("Group", "group", rep.group.clone());
    scalar_section("Mode Changes", "mode", rep.mode.clone());
    scalar_section("Transform", "transform", rep.transform.clone());

    let mut lines: Vec<String> = Vec::new();
    push_opt(
        &mut lines,
        scalar("session_count", rep.session_count.as_ref().map(|n| n.to_string())),
    );
    secs.push(("Session Count".to_string(), lines));

    let mut lines: Vec<String> = Vec::new();
    push_opt(
        &mut lines,
        scalar("bytes_in", rep.tar_writer_bytes_in.as_ref().map(|n| n.to_string())),
    );
    push_opt(
        &mut lines,
        scalar("bytes_out", rep.tar_writer_bytes_out.as_ref().map(|n| n.to_string())),
    );
    secs.push(("Tar Writer Bytes".to_string(), lines));

    let mut push_lines = |title: &str, opt: Option<&Vec<String>>| {
        let mut lines: Vec<String> = Vec::new();
        if let Some(ms) = opt {
            lines.extend(ms.clone());
        }
        secs.push((title.to_string(), lines));
    };
    push_lines("Owner Map", rep.owner_map.as_ref());
    push_lines("Group Map", rep.group_map.as_ref());
    push_lines("Include Filters", rep.include_filters.as_ref());
    push_lines("Exclude Filters", rep.exclude_filters.as_ref());

    let mut json_section = |title: &str, label: &str, value: Option<&serde_json::Value>| {
        let mut lines: Vec<String> = Vec::new();
        if let Some(v) = value {
            if let Some(pretty) = pretty_json(v) {
                push_opt(&mut lines, scalar(label, Some(pretty)));
            }
        }
        secs.push((title.to_string(), lines));
    };
    json_section("Archive Arguments", "json", rep.archive_arguments.as_ref());
    json_section("Extract Arguments", "json", rep.extract_arguments.as_ref());

    if let Some(st) = rep.stats.as_ref() {
        let mut lines: Vec<String> = Vec::new();
        for (ftype, count) in &st.counts_by_ftype {
            lines.push(format!("{ftype}: {count}"));
        }
        for (label, value) in [
            ("included", st.included),
            ("excluded_by_include", st.excluded_by_include),
            ("excluded_by_exclude", st.excluded_by_exclude),
            ("unfiltered", st.unfiltered),
            ("hashed", st.hashed),
            ("hash_errors", st.hash_errors),
            ("canonical_singletons", st.canonical_singletons),
            ("canonical_files", st.canonical_files),
            ("canonical_errors", st.canonical_errors),
            ("sparsified_files", st.sparsified_files),
            ("sparsified_errors", st.sparsified_errors),
            ("bytes_all", st.bytes_all),
            ("bytes_canonical", st.bytes_canonical),
            ("bytes_archived", st.bytes_archived),
        ] {
            lines.push(format!("{label}: {value}"));
        }
        secs.push(("Stats".to_string(), lines));
    }
    secs
}

fn stdout_write(s: String) -> Result<()> {
    std::io::stdout()
        .write_all(s.as_bytes())
        .map_err(|e| Error::io(Path::new("<stdout>"), e))?;
    Ok(())
}