use std::path::PathBuf;

use clap::{ArgAction, Args, Parser, Subcommand, ValueEnum};

#[derive(Debug, Parser)]
#[command(name = "tar-dedup", about = "Deduplicating archival pipeline")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Walk, deduplicate, and write a resumable archive.
    /// Aborts if work or an archive already exists; use `--fresh` to replace.
    Archive(ArchiveArgs),
    /// Restore a tar-dedup archive (materialize: full copy at each original path).
    /// Aborts if extract work already exists; use `--fresh` to replace.
    Extract(ExtractArgs),
    /// Continue an interrupted archive or extract from its work directory.
    Resume(ResumeArgs),
}

#[derive(Debug, Args)]
pub struct ArchiveArgs {
    // --- Archive Paths ---

    /// Archive path (relative paths use `-C` / current directory).
    #[arg(short = 'f', value_name = "ARCHIVE", help_heading = "Archive Paths")]
    pub archive: PathBuf,

    /// Treat DIR as the working directory when resolving relative paths on this
    /// command line (`-f`, `-i`, `-T`, `--work-dir`, …). Absolute paths are
    /// unchanged. Default: process current directory.
    #[arg(
        short = 'C',
        long = "directory",
        value_name = "DIR",
        help_heading = "Archive Paths"
    )]
    pub directory: Option<PathBuf>,

    /// Stage directory for sqlite DB, staged payloads, and locks (defaults to
    /// `{stem}.astage` next to the archive). Relative paths use `-C` / cwd.
    #[arg(long = "work-dir", value_name = "DIR", help_heading = "Archive Paths")]
    pub work_dir: Option<PathBuf>,

    // --- Inputs ---

    /// Input directory to snapshot (repeatable; union of roots).
    #[arg(
        short = 'i',
        long = "input-dir",
        value_name = "DIR",
        action = ArgAction::Append,
        help_heading = "Inputs"
    )]
    pub input_dirs: Vec<PathBuf>,

    /// Read member paths from FILE (`-` = stdin). Repeatable; union with `-i`.
    #[arg(
        short = 'T',
        long = "files-from",
        value_name = "FILE",
        action = ArgAction::Append,
        help_heading = "Inputs"
    )]
    pub files_from: Vec<PathBuf>,

    /// `-T` records are NUL-terminated (default: newline-separated).
    #[arg(long = "null", default_value_t = false, help_heading = "Inputs")]
    pub null: bool,

    // --- Compression ---

    #[command(flatten, next_help_heading = "Compression")]
    pub compression: CompressionFlags,

    /// Compression level. Allowed range depends on filter: gzip/bzip2 1–9, xz 0–9, zstd 1–19.
    #[arg(long = "level", value_name = "N", help_heading = "Compression")]
    pub level: Option<u32>,

    /// xz `--extreme` / `-e` (OR into LZMA preset). Only valid with xz.
    #[arg(long = "xz-extreme", help_heading = "Compression")]
    pub xz_extreme: bool,

    // INFO: Rust does not expose bzip-small option.,

    /// Cap xz encoder RAM (bytes, MiB, GiB, or % of RAM). Like `xz --memlimit-compress`.
    #[arg(long = "memlimit-compress", value_name = "LIMIT", help_heading = "Compression")]
    pub memlimit_compress: Option<String>,

    // --- Indexing ---

    /// Do not descend into directories.
    #[arg(long = "no-recursion", default_value_t = false, help_heading = "Indexing")]
    pub no_recursion: bool,

    /// Follow symlinks; archive the files they point to (GNU tar `-h`).
    #[arg(long = "dereference", default_value_t = false, help_heading = "Indexing")]
    pub dereference: bool,

    /// Stay on one filesystem when walking input trees.
    #[arg(long = "one-file-system", default_value_t = false, help_heading = "Indexing")]
    pub one_file_system: bool,

    /// Do not coalesce same (inode, device) hard links in hash/dedup.
    #[arg(
        long = "no-hardlink-detection",
        default_value_t = false,
        help_heading = "Indexing"
    )]
    pub no_hardlink_detection: bool,

    /// Allow nested or duplicate `-i` / `-T` directory roots (still recorded and walked).
    #[arg(
        long = "no-strict-separation",
        default_value_t = false,
        help_heading = "Indexing"
    )]
    pub no_strict_separation: bool,

    // --- Filtering ---

    /// Exclude paths matching regex PATTERN (repeatable).
    #[arg(
        long = "exclude",
        value_name = "PATTERN",
        action = ArgAction::Append,
        help_heading = "Filtering"
    )]
    pub exclude: Vec<String>,

    /// Read exclude regex patterns from FILE (repeatable).
    #[arg(
        short = 'X',
        long = "exclude-from",
        value_name = "FILE",
        action = ArgAction::Append,
        help_heading = "Filtering"
    )]
    pub exclude_from: Vec<PathBuf>,

    /// Include only paths matching regex PATTERN (repeatable).
    #[arg(
        long = "include",
        value_name = "PATTERN",
        action = ArgAction::Append,
        help_heading = "Filtering"
    )]
    pub include: Vec<String>,

    /// Read include regex patterns from FILE (repeatable).
    #[arg(
        long = "include-from",
        value_name = "FILE",
        action = ArgAction::Append,
        help_heading = "Filtering"
    )]
    pub include_from: Vec<PathBuf>,

    /// Exclude version control system directories (not implemented yet).
    #[arg(long = "exclude-vcs", default_value_t = false, help_heading = "Filtering")]
    pub exclude_vcs: bool,

    /// Read VCS ignore files for exclusions (not implemented yet).
    #[arg(
        long = "exclude-vcs-ignores",
        default_value_t = false,
        help_heading = "Filtering"
    )]
    pub exclude_vcs_ignores: bool,

    /// Patterns match from the start of the relative path.
    #[arg(
        long = "anchored",
        default_value_t = false,
        action = ArgAction::SetTrue,
        help_heading = "Filtering"
    )]
    #[arg(long = "no-anchored", action = ArgAction::SetFalse)]
    pub anchored: bool,

    /// Case-insensitive pattern matching.
    #[arg(
        long = "ignore-case",
        default_value_t = false,
        action = ArgAction::SetTrue,
        help_heading = "Filtering"
    )]
    #[arg(long = "no-ignore-case", action = ArgAction::SetFalse)]
    pub ignore_case: bool,

    // --- File Attributes ---

    /// Capture POSIX ACLs (default: on).
    #[arg(
        long = "acls",
        default_value_t = true,
        action = ArgAction::SetTrue,
        help_heading = "File Attributes"
    )]
    #[arg(long = "no-acls", action = ArgAction::SetFalse)]
    pub acls: bool,

    /// Capture extended attributes (default: on).
    #[arg(
        long = "xattrs",
        default_value_t = true,
        action = ArgAction::SetTrue,
        help_heading = "File Attributes"
    )]
    #[arg(long = "no-xattrs", action = ArgAction::SetFalse)]
    pub xattrs: bool,

    /// Capture SELinux contexts (default: on).
    #[arg(
        long = "selinux",
        default_value_t = true,
        action = ArgAction::SetTrue,
        help_heading = "File Attributes"
    )]
    #[arg(long = "no-selinux", action = ArgAction::SetFalse)]
    pub selinux: bool,

    // TODO extraction?
    /// Force owner for archived members: `NAME`, `UID`, or `NAME:UID` (GNU tar).
    /// Stored as archive policy (meta); applied on extract.
    #[arg(long = "owner", value_name = "NAME[:UID]", help_heading = "File Attributes")]
    pub owner: Option<String>,

    /// Owner translation map file (GNU tar `--owner-map`).
    #[arg(long = "owner-map", value_name = "FILE", help_heading = "File Attributes")]
    pub owner_map: Option<PathBuf>,

    /// Force group for archived members: `NAME`, `GID`, or `NAME:GID` (GNU tar).
    /// Stored as archive policy (meta); applied on extract.
    #[arg(long = "group", value_name = "NAME[:GID]", help_heading = "File Attributes")]
    pub group: Option<String>,

    /// Group translation map file (GNU tar `--group-map`).
    #[arg(long = "group-map", value_name = "FILE", help_heading = "File Attributes")]
    pub group_map: Option<PathBuf>,

    // --- Sparse Files ---

    /// Run the sparsify phase (copy with seeks using `--page-size` / `--min-pages`).
    #[arg(long = "sparsify", default_value_t = false, help_heading = "Sparse Files")]
    pub sparsify: bool,

    /// Page size in bytes for hash zero-page counting and sparsify.
    #[arg(
        long = "page-size",
        value_name = "BYTES",
        default_value_t = 4096,
        help_heading = "Sparse Files"
    )]
    pub page_size: usize,

    /// Minimum empty-page count before sparsify treats a file as worth rewriting.
    #[arg(
        long = "min-pages",
        value_name = "PAGES",
        help_heading = "Sparse Files",
        default_value_t = 0
    )]
    pub min_pages: u64,

    // --- Process Options ---

    /// Maximum concurrent workers (rayon pools and xz threads).
    #[arg(long = "jobs", value_name = "N", help_heading = "Process Options")]
    pub jobs: Option<usize>,

    /// Wipe existing work and archive, then start from inventory.
    #[arg(long = "fresh", help_heading = "Process Options")]
    pub fresh: bool,

    /// After success, keep a timestamped copy of snapshot.sqlite next to the archive.
    #[arg(long = "keep-db", help_heading = "Process Options")]
    pub keep_db: bool,

    /// After success, keep the `{stem}.astage` work directory.
    #[arg(long = "keep-stage", help_heading = "Process Options")]
    pub keep_stage: bool,

    /// Run through STAGE then exit cleanly (state saved). STAGE: inventory, hash,
    /// filter, dedup, sparsify, stage, archive/tar, cleanup (alias scan for inventory).
    #[arg(
        long = "exit-after-stage",
        value_name = "STAGE",
        value_enum,
        help_heading = "Process Options"
    )]
    pub exit_after_stage: Option<ExitAfterStageArg>,

    /// Abort the run on the first hard failure (instead of soft-continuing where possible).
    #[arg(long = "fail-fast", default_value_t = false, help_heading = "Process Options")]
    pub fail_fast: bool,

    /// Record per-file error messages and continue instead of failing the run.
    #[arg(long = "no-errors", default_value_t = false, help_heading = "Process Options")]
    pub no_errors: bool,

    /// Apply include/exclude filters after the hash phase.
    #[arg(long = "lazy_filter", default_value_t = false, help_heading = "Process Options")]
    pub lazy_filter: bool,

    /// Skip the deduplication phase.
    #[arg(long = "no-dedup", default_value_t = false, help_heading = "Process Options")]
    pub no_dedup: bool,

    /// Stage/archive files that failed to obtain a SHA-1.
    #[arg(
        long = "retry-missing-sha",
        default_value_t = false,
        help_heading = "Process Options"
    )]
    pub retry_missing_sha: bool,
}

#[derive(Debug, Args, Default)]
pub struct CompressionFlags {
    /// Use archive suffix to pick the compression filter.
    #[arg(short = 'a', long = "auto-compress", group = "compress_filter")]
    pub auto_compress: bool,

    /// Do not infer compression from the archive suffix.
    #[arg(long = "no-auto-compress")]
    pub no_auto_compress: bool,

    #[arg(short = 'z', long = "gzip", group = "compress_filter")]
    pub gzip: bool,

    #[arg(short = 'j', long = "bzip2", group = "compress_filter")]
    pub bzip2: bool,

    #[arg(short = 'J', long = "xz", group = "compress_filter")]
    pub xz: bool,

    #[arg(long = "zstd", group = "compress_filter")]
    pub zstd: bool,
}

#[derive(Debug, Args)]
pub struct ExtractArgs {
    // --- Archive Paths ---

    /// Archive path (relative paths use `-C` / current directory).
    #[arg(short = 'f', value_name = "ARCHIVE", help_heading = "Archive Paths")]
    pub archive: PathBuf,

    /// Treat DIR as the working directory when resolving relative paths on this
    /// command line (`-f`, `--work-dir`, …) and as the extraction root. Absolute
    /// paths are unchanged. Default: process current directory.
    #[arg(
        short = 'C',
        long = "directory",
        value_name = "DIR",
        help_heading = "Archive Paths"
    )]
    pub directory: Option<PathBuf>,

    /// Stage directory for sqlite DB, cached payloads, and locks (defaults to
    /// `{stem}.estage` next to the archive). Relative paths use `-C` / cwd.
    #[arg(long = "work-dir", value_name = "DIR", help_heading = "Archive Paths")]
    pub work_dir: Option<PathBuf>,

    /// Restore using absolute catalog paths (`-P`). With `-C`, join abs paths under
    /// the extraction root (leading `,/` stripped); without `-C`, restore under
    /// filesystem root. Alias: `--absolute-names`.
    #[arg(
        short = 'P',
        long = "absolute",
        visible_alias = "absolute-names",
        default_value_t = false,
        help_heading = "Archive Paths"
    )]
    pub absolute_names: bool,

    /// Create a subdirectory so loose archive members are not extracted directly into
    /// the extraction root (GNU tar `--one-top-level`). Optional DIR overrides the
    /// default (archive stem).
    #[arg(
        long = "one-top-level",
        value_name = "DIR",
        help_heading = "Archive Paths"
    )]
    pub one_top_level: Option<PathBuf>,

    // --- Overwrite Control ---

    /// Preserve existing directory symlinks instead of replacing them (GNU tar
    /// `--keep-directory-symlink`).
    #[arg(
        long = "keep-directory-symlink",
        default_value_t = false,
        help_heading = "Overwrite Control"
    )]
    pub keep_dir_symlink: bool,

    /// Remove each existing file before extracting over it (GNU tar `-U` /
    /// `--unlink-first`).
    #[arg(
        short = 'U',
        long = "unlink-first",
        default_value_t = false,
        help_heading = "Overwrite Control"
    )]
    pub unlink_first: bool,

    /// Skip members whose parent directory does not exist (error with `--fail-fast`).
    #[arg(
        long = "no-create-dirs",
        default_value_t = false,
        help_heading = "Overwrite Control"
    )]
    pub no_create_dir: bool,

    /// Do not apply archived metadata to existing directories (GNU tar
    /// `--no-overwrite-dir`). Default: apply directory metadata for placed
    /// members, matching GNU tar at the end of the pipeline.
    #[arg(
        long = "no-overwrite-dir",
        default_value_t = false,
        help_heading = "Overwrite Control"
    )]
    pub no_overwrite_dir: bool,

    /// Also apply archived directory metadata to directories excluded by filters.
    /// Normally only placed members are updated (GNU tar equivalent); this flag
    /// matters because excluded paths remain in the catalog with full metadata.
    #[arg(
        long = "overwrite-dir",
        default_value_t = false,
        help_heading = "Overwrite Control"
    )]
    pub force_overwrite_dir: bool,

    /// How to handle paths that already exist on disk.
    #[arg(
        long = "conflict-policy",
        value_enum,
        default_value_t = ConflictPolicy::Replace,
        help_heading = "Overwrite Control"
    )]
    pub conflict_policy: ConflictPolicy,

    /// Do not log path or type conflicts (pairs with `--conflict-policy`).
    #[arg(
        long = "silent-conflicts",
        default_value_t = false,
        help_heading = "Overwrite Control"
    )]
    pub silent_conflicts: bool,

    /// On type mismatch at a path, remove the existing entry and extract the archive member.
    #[arg(
        long = "remove-and-replace",
        default_value_t = false,
        help_heading = "Overwrite Control"
    )]
    pub remove_and_replace: bool,

    // --- Link Tree ---

    /// Build the restored tree from symlinks/hard links into the extract stage instead
    /// of copying payloads (keeps the `.estage` cache).
    #[arg(
        long = "link-tree",
        default_value_t = false,
        help_heading = "Link Tree"
    )]
    pub link_tree: bool,

    /// With `--link-tree`, use hard links instead of symlinks.
    #[arg(
        long = "hard-links",
        default_value_t = false,
        help_heading = "Link Tree"
    )]
    pub use_hard_links: bool,

    /// With `--link-tree`, link using absolute paths instead of paths relative to the
    /// output root.
    #[arg(
        long = "absolute-links",
        default_value_t = false,
        help_heading = "Link Tree"
    )]
    pub absolute_links: bool,

    /// Re-create hard links between extracted files that share `(dev, inode)` in the
    /// archive catalog (default: on).
    #[arg(
        long = "hardlink-reestablish",
        default_value_t = true,
        action = ArgAction::SetTrue,
        help_heading = "Link Tree"
    )]
    #[arg(long = "no-hardlink-reestablish", action = ArgAction::SetFalse)]
    pub hardlink_reestablish: bool,

    // --- File Attributes ---

    /// Restore archived uid/gid when possible (GNU tar `--same-owner`; may require root).
    #[arg(
        long = "same-owner",
        visible_alias = "restore-owner",
        default_value_t = false,
        help_heading = "File Attributes"
    )]
    pub restore_owner: bool,

    // --- Process Options ---

    /// Wipe extract work (`.estage`) and start over.
    #[arg(long = "fresh", help_heading = "Process Options")]
    pub fresh: bool,

    /// Abort on the first warning or error instead of continuing where possible.
    #[arg(
        long = "fail-fast",
        default_value_t = false,
        help_heading = "Process Options"
    )]
    pub fail_fast: bool,

    /// After success, keep a timestamped copy of snapshot.sqlite next to the archive.
    #[arg(long = "keep-db", help_heading = "Process Options")]
    pub keep_db: bool,

    /// After success, keep the `{stem}.estage` work directory.
    #[arg(long = "keep-stage", help_heading = "Process Options")]
    pub keep_stage: bool,
}

/// Continue incomplete work. Policy comes from the work DB; only jobs and
/// `--exit-after-stage` may be overridden.
#[derive(Debug, Args)]
pub struct ResumeArgs {
    /// Work directory (`.astage` / `.estage`) of the interrupted run.
    #[arg(long = "work-dir", value_name = "DIR")]
    pub work_dir: PathBuf,

    /// Maximum concurrent workers (rayon pools and xz threads).
    #[arg(long = "jobs", value_name = "N")]
    pub jobs: Option<usize>,

    /// Run through STAGE then exit cleanly (state saved).
    #[arg(long = "exit-after-stage", value_name = "STAGE", value_enum)]
    pub exit_after_stage: Option<ExitAfterStageArg>,
}

/// Pipeline stop point for `--exit-after-stage`.
///
/// Names follow archive file-phase progression (plus `cleanup`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "lower")]
pub enum ExitAfterStageArg {
    /// Walk the input tree / capture metadata (`inventoried`).
    #[value(alias = "scan")]
    Inventory,
    /// Content hash + sparse-page scan (`hashed`).
    Hash,
    /// Include/exclude selection (`filtered`).
    Filter,
    /// Canonical / duplicate resolution (`deduped`).
    Dedup,
    /// Sparse materialization (`sparsified`).
    Sparsify,
    /// Symlink canonical files into the work directory (`staged`).
    #[value(alias = "symlink")]
    Stage,
    /// Write the compressed tar archive (`archived`).
    #[value(alias = "tar")]
    Archive,
    /// Full pipeline then clean the work directory (honours `--keep-db` / `--keep-stage`).
    Cleanup,
}

/// How extraction handles paths that already exist on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum)]
#[value(rename_all = "kebab-case")]
pub enum ConflictPolicy {
    /// Keep existing files; warn on conflict (GNU tar `--keep-old-files`).
    #[value(alias = "keep-old-files")]
    PreserveExisting,
    /// Keep whichever copy has the newer mtime (GNU tar `--keep-newer-files`).
    #[value(alias = "keep-newer-files")]
    PreferNewer,
    /// Overwrite existing paths (GNU tar `--overwrite`; default).
    #[default]
    #[value(alias = "overwrite")]
    Replace,
}
