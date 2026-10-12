use std::path::PathBuf;

use clap::{ArgAction, Args, Parser, Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};

use crate::common;
use crate::common::perms::MapResolutionTarget;

#[derive(Debug, Parser)]
#[command(name = "tar-dedup", about = "Deduplicating archival pipeline")]
pub struct Cli {
    /// Verbosity of the log output (overrides the `RUST_LOG` env var).
    #[arg(
        long = "log-level",
        global = true,
        value_enum,
        value_name = "LEVEL",
        help_heading = "Process Options"
    )]
    pub log_level: Option<LogLevel>,

    #[command(subcommand)]
    pub command: Command,
}

/// Log verbosity for `--log-level` (maps onto `tracing` levels).
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum LogLevel {
    Off,
    Error,
    Warn,
    Info,
    Debug,
    Trace,
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
    /// Show archive metadata needed to plan an extraction (catalog, policies).
    Inspect(ArchiveInputArgs),
    /// List file-tree members (rows from the `files` table).
    List(ArchiveInputArgs),
    /// Machine-formatted (csv/json) dump of the file-tree members.
    Dump(ArchiveInputArgs),
    /// Search paths / ids across catalog tables.
    Query(ArchiveInputArgs),
    /// Update stored archive/extract parameters and reset pipeline state.
    Reset(ResetArgs),
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
    #[arg(long = "null", default_value_t = false, action = ArgAction::SetTrue, help_heading = "Inputs")]
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
    #[arg(long = "no-recursion", default_value_t = false, action = ArgAction::SetTrue, help_heading = "Indexing")]
    pub no_recursion: bool,

    /// Follow symlinks; archive the files they point to (GNU tar `-h`).
    #[arg(long = "dereference", default_value_t = false, action = ArgAction::SetTrue, help_heading = "Indexing")]
    pub dereference: bool,

    /// Stay on one filesystem when walking input trees.
    #[arg(long = "one-file-system", default_value_t = false, action = ArgAction::SetTrue, help_heading = "Indexing")]
    pub one_file_system: bool,

    /// Do not coalesce same (inode, device) hard links in hash/dedup.
    #[arg(
        long = "no-hardlink-detection",
        default_value_t = false,
        action = ArgAction::SetTrue,
        help_heading = "Indexing"
    )]
    pub no_hardlink_detection: bool,

    /// Allow nested or duplicate `-i` / `-T` directory roots (still recorded and walked).
    #[arg(
        long = "no-strict-separation",
        default_value_t = false,
        action = ArgAction::SetTrue,
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
    #[arg(long = "exclude-vcs", default_value_t = false, action = ArgAction::SetTrue, help_heading = "Filtering")]
    pub exclude_vcs: bool,

    /// Read VCS ignore files for exclusions (not implemented yet).
    #[arg(
        long = "exclude-vcs-ignores",
        default_value_t = false,
        action = ArgAction::SetTrue,
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
    pub anchored: bool,

    /// Case-insensitive pattern matching.
    #[arg(
        long = "ignore-case",
        default_value_t = false,
        action = ArgAction::SetTrue,
        help_heading = "Filtering"
    )]
    pub ignore_case: bool,

    // --- File Attributes ---

    /// Capture POSIX ACLs (default: on via the capture umbrella).
    #[arg(long = "acls", default_value_t = false, action = ArgAction::SetTrue, help_heading = "File Attributes")]
    pub acls: bool,

    /// Do not capture POSIX ACLs.
    #[arg(long = "no-acls", default_value_t = false, action = ArgAction::SetTrue, help_heading = "File Attributes")]
    pub no_acls: bool,

    /// Capture extended attributes (default: on via the capture umbrella).
    #[arg(long = "xattrs", default_value_t = false, action = ArgAction::SetTrue, help_heading = "File Attributes")]
    pub xattrs: bool,

    /// Do not capture extended attributes.
    #[arg(long = "no-xattrs", default_value_t = false, action = ArgAction::SetTrue, help_heading = "File Attributes")]
    pub no_xattrs: bool,

    /// Capture SELinux contexts (default: on via the capture umbrella).
    #[arg(long = "selinux", default_value_t = false, action = ArgAction::SetTrue, help_heading = "File Attributes")]
    pub selinux: bool,

    /// Do not capture SELinux contexts.
    #[arg(long = "no-selinux", default_value_t = false, action = ArgAction::SetTrue, help_heading = "File Attributes")]
    pub no_selinux: bool,

    /// Default: capture every metadata attribute (xattrs, ACLs, SELinux, ...). The
    /// `--no-*` toggles override this on a per-bit basis.
    #[arg(long = "no-capture-all-metadata", default_value_t = false, action = ArgAction::SetTrue, help_heading = "File Attributes")]
    pub no_capture_all_metadata: bool,

    /// Record numeric uids/gids instead of resolving user/group names during inventory.
    #[arg(long = "numeric-ids-only", default_value_t = false, action = ArgAction::SetTrue, help_heading = "File Attributes")]
    pub numeric_ids_only: bool,

    /// Resolve user/group names during inventory (default; counters `--numeric-ids-only`).
    #[arg(long = "resolve-numeric-ids", default_value_t = false, action = ArgAction::SetTrue, help_heading = "File Attributes")]
    pub resolve_numeric_ids: bool,

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

    /// Force symbolic mode CHANGES for added members (GNU tar `--mode`). Applies
    /// chmod-style updates (e.g. `u+rwx,go-rx`) at extraction; stored as archive
    /// policy (meta).
    #[arg(long = "mode", value_name = "CHANGES", help_heading = "File Attributes")]
    pub mode: Option<String>,

    /// Record a sed-style name transform (GNU tar `--transform`/`--xform`,
    /// e.g. `s,^usr/,var/,`) to apply at extraction; stored as archive policy
    /// (meta). Only validated here; not applied at archive time.
    #[arg(long = "transform", alias = "xform", value_name = "EXPRESSION",
          help_heading = "File Attributes")]
    pub transform: Option<String>,

    // --- Sparse Files ---

    /// Run the sparsify phase (copy with seeks using `--page-size` / `--min-pages`).
    #[arg(long = "sparsify", default_value_t = false, action = ArgAction::SetTrue, help_heading = "Sparse Files")]
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

    /// Maximum concurrent io-bound workers (dedup, sparsify, place pools).
    #[arg(long = "io-jobs", value_name = "N", help_heading = "Process Options")]
    pub io_jobs: Option<usize>,

    /// Wipe existing work and archive, then start from inventory.
    #[arg(long = "fresh", help_heading = "Process Options")]
    pub fresh: bool,

    /// After success, keep a timestamped copy of snapshot.sqlite next to the archive.
    #[arg(long = "keep-db", help_heading = "Process Options")]
    pub keep_db: bool,

    /// After success, keep the `{stem}.astage` work directory.
    #[arg(long = "keep-stage", help_heading = "Process Options")]
    pub keep_stage: bool,

    /// Run through STAGE then exit cleanly (state saved).
    #[arg(
        long = "exit-after-stage",
        value_name = "STAGE",
        value_enum,
        help_heading = "Process Options"
    )]
    pub exit_after_stage: Option<ExitAfterStageArg>,

    /// Abort the run on the first hard failure (instead of soft-continuing where possible).
    #[arg(long = "fail-fast", default_value_t = false, action = ArgAction::SetTrue, help_heading = "Process Options")]
    pub fail_fast: bool,

    /// Do not record errors encountered while processing the entries in the database.
    #[arg(long = "no-errors", default_value_t = false, action = ArgAction::SetTrue, help_heading = "Process Options")]
    pub no_errors: bool,

    /// Apply include/exclude filters after the hash phase.
    #[arg(long = "lazy-filter", default_value_t = false, action = ArgAction::SetTrue, help_heading = "Process Options")]
    pub lazy_filter: bool,

    /// Deduplication strategy.
    #[arg(
        long = "dedup-mode",
        value_enum,
        default_value_t = DedupMode::Regular,
        help_heading = "Process Options"
    )]
    pub dedup_mode: DedupMode,

    /// Generate sorted combination of data pieces in the order necessary to write.
    /// (GNU tar `--sort` analog; applies at the tar writer.)
    #[arg(
        long = "ordering",
        value_enum,
        default_value_t = ArchiveOrdering::ExtSizeId,
        help_heading = "Process Options"
    )]
    pub ordering: ArchiveOrdering,

    /// Use content-defined chunking instead of whole-file hashes (not implemented yet).
    #[arg(long = "use-shards", default_value_t = false, action = ArgAction::SetTrue, help_heading = "Process Options")]
    pub use_shards: bool,

    /// Maximum shard size for content-defined chunking (requires `--use-shards`).
    #[arg(
        long = "max-shard-size",
        value_name = "BYTES",
        default_value_t = 0,
        help_heading = "Process Options"
    )]
    pub max_shard_size: u64,

    /// Skip entries whose path cannot be represented as UTF-8 (warn + continue).
    /// Mutually exclusive with `--force` and `--fail-fast`.
    #[arg(long = "relax-utf8", default_value_t = false, action = ArgAction::SetTrue, help_heading = "Process Options")]
    pub relax_utf8: bool,

    /// Number of error drafts buffered before an auto-flush to the database.
    #[arg(
        long = "auto-flush-limit",
        value_name = "N",
        default_value_t = common::defaults::DEFAULT_AUTO_FLUSH_LIMIT,
        help_heading = "Process Options"
    )]
    pub auto_flush_limit: u64,

    /// Rows pulled from the database per phase iteration.
    #[arg(
        long = "batch-size",
        value_name = "N",
        default_value_t = common::defaults::BATCH_SIZE,
        help_heading = "Process Options"
    )]
    pub batch_size: u64,

    /// Items in the producer/consumer work channel.
    #[arg(
        long = "work-capacity",
        value_name = "N",
        default_value_t = common::defaults::WORK_CAPACITY,
        help_heading = "Process Options"
    )]
    pub work_capacity: usize,

    /// Items in the producer/consumer result channel.
    #[arg(
        long = "out-capacity",
        value_name = "N",
        default_value_t = common::defaults::OUT_CAPACITY,
        help_heading = "Process Options"
    )]
    pub out_capacity: usize,

    /// Rows pulled from the database per feed round.
    #[arg(
        long = "feed-chunk",
        value_name = "N",
        default_value_t = common::defaults::FEED_CHUNK,
        help_heading = "Process Options"
    )]
    pub feed_chunk: usize,

    /// Outcomes committed per transaction.
    #[arg(
        long = "drain-chunk",
        value_name = "N",
        default_value_t = common::defaults::DRAIN_CHUNK,
        help_heading = "Process Options"
    )]
    pub drain_chunk: usize,

    /// Which progress bars to draw.
    #[arg(
        long = "progress-mode",
        value_enum,
        default_value_t = ProgressMode::PerThread,
        help_heading = "Process Options"
    )]
    pub progress_mode: ProgressMode,
}

#[derive(Debug, Args, Default)]
pub struct CompressionFlags {
    /// Filter the archive through gzip (overrides extension inference).
    #[arg(short = 'z', long = "gzip", group = "compress_filter")]
    pub gzip: bool,

    /// Filter the archive through bzip2 (overrides extension inference).
    #[arg(short = 'j', long = "bzip2", group = "compress_filter")]
    pub bzip2: bool,

    /// Filter the archive through xz (overrides extension inference).
    #[arg(short = 'J', long = "xz", group = "compress_filter")]
    pub xz: bool,

    /// Filter the archive through zstd (overrides extension inference).
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
    /// command line (`-f`, `--work-dir`, …). Absolute paths are unchanged.
    /// Default: process current directory.
    #[arg(
        short = 'C',
        long = "directory",
        value_name = "DIR",
        help_heading = "Archive Paths"
    )]
    pub directory: Option<PathBuf>,

    /// Extract members into DIR (the restore root); default: `-C` / current dir.
    #[arg(
        long = "extract-to",
        value_name = "DIR",
        help_heading = "Archive Paths"
    )]
    pub extract_to: Option<PathBuf>,

    /// Stage directory for sqlite DB, cached payloads, and locks (defaults to
    /// `{stem}.estage` next to the archive). Relative paths use `-C` / cwd.
    #[arg(long = "work-dir", value_name = "DIR", help_heading = "Archive Paths")]
    pub work_dir: Option<PathBuf>,

    /// Restore using absolute catalog paths (`-P`). Extraction join abs paths under
    /// the extraction root (leading `,/` stripped). To extract to file system root,
    /// pass `/` as `-C`
    #[arg(
        short = 'P',
        long = "absolute",
        visible_alias = "absolute-names",
        default_value_t = false,
        action = ArgAction::SetTrue,
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

    // --- Compression ---

    /// Compression filter override. By default the archive suffix decides
    /// (extension inference / no compression); an explicit flag overrides it.
    #[command(flatten, next_help_heading = "Compression")]
    pub compression: CompressionFlags,

    // --- Rehash ---

    /// Do not run the rehash (content verification) phase. Default: on.
    #[arg(long = "no-rehash", default_value_t = false, action = ArgAction::SetTrue, help_heading = "Rehash")]
    pub no_rehash: bool,

    // --- Overwrite Control ---

    /// Preserve existing directory symlinks instead of replacing them (GNU tar
    /// `--keep-directory-symlink`).
    #[arg(
        long = "keep-directory-symlink",
        default_value_t = false,
        action = ArgAction::SetTrue,
        help_heading = "Overwrite Control"
    )]
    pub keep_dir_symlink: bool,

    /// Remove each existing file before extracting over it (GNU tar `-U` /
    /// `--unlink-first`).
    #[arg(
        short = 'U',
        long = "unlink-first",
        default_value_t = false,
        action = ArgAction::SetTrue,
        help_heading = "Overwrite Control"
    )]
    pub unlink_first: bool,

    /// Skip the directory tree create pass. Results in the members without a parent being skipped.
    /// Results in error with --fail-fast
    #[arg(
        long = "no-create-dirs",
        default_value_t = false,
        action = ArgAction::SetTrue,
        help_heading = "Overwrite Control"
    )]
    pub no_create_dir: bool,

    /// Do not apply archived metadata to existing directories (GNU tar
    /// `--no-overwrite-dir`). Default: apply directory metadata for placed
    /// members, matching GNU tar at the end of the pipeline.
    #[arg(
        long = "no-overwrite-dir",
        default_value_t = false,
        action = ArgAction::SetTrue,
        help_heading = "Overwrite Control"
    )]
    pub no_overwrite_dir: bool,

    // TODO implement!
    /// Also apply archived directory metadata to directories excluded by filters.
    /// Normally only placed members are updated (GNU tar equivalent); this flag
    /// matters because excluded paths remain in the catalog with full metadata.
    #[arg(
        long = "overwrite-dir",
        default_value_t = false,
        action = ArgAction::SetTrue,
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
        action = ArgAction::SetTrue,
        help_heading = "Overwrite Control"
    )]
    pub silent_conflicts: bool,

    /// On type mismatch at a path, remove the existing entry and extract the archive member.
    #[arg(
        long = "remove-and-replace",
        default_value_t = false,
        action = ArgAction::SetTrue,
        help_heading = "Overwrite Control"
    )]
    pub remove_and_replace: bool,

    /// On type mismatch at a path, remove the existing entry and extract the archive member.
    #[arg(
        long = "hard-link-grouping",
        value_enum,
        default_value_t = HardLinkGrouping::Global,
        help_heading = "Overwrite Control",
        value_name = "GROUPING-POLICY"
    )]
    pub hard_link_grouping: HardLinkGrouping,

    /// Remove the extraction root before extracting (only effective with `--one-top-level`).
    #[arg(
        long = "clean-target",
        default_value_t = false,
        action = ArgAction::SetTrue,
        help_heading = "Overwrite Control"
    )]
    pub clean_target: bool,

    // --- Link Tree ---

    /// Build the restored tree from symlinks/hard links into the extract stage instead
    /// of copying payloads (keeps the `.estage` cache).
    #[arg(
        long = "link-tree",
        default_value_t = false,
        action = ArgAction::SetTrue,
        help_heading = "Link Tree"
    )]
    pub link_tree: bool,

    /// With `--link-tree`, use hard links instead of symlinks.
    #[arg(
        long = "hard-links",
        default_value_t = false,
        action = ArgAction::SetTrue,
        help_heading = "Link Tree"
    )]
    pub use_hard_links: bool,

    /// With `--link-tree`, link using absolute paths instead of paths relative to the
    /// output root.
    #[arg(
        long = "absolute-links",
        default_value_t = false,
        action = ArgAction::SetTrue,
        help_heading = "Link Tree"
    )]
    pub absolute_links: bool,

    /// With `--link-tree`, use DIR as the directory the canonical payloads are
    /// materialized into before linking (default: the extract cache directory).
    #[arg(
        long = "link-source",
        value_name = "DIR",
        help_heading = "Link Tree"
    )]
    pub link_source: Option<PathBuf>,

    /// Do not attempt copy-on-write (reflink) copies when placing payloads.
    #[arg(
        long = "no-reflink",
        default_value_t = false,
        action = ArgAction::SetTrue,
        help_heading = "Placement"
    )]
    pub no_reflink: bool,

    /// Recreate non-regular entries (symlink, fifo, char/block device, socket).
    /// Off by default: those entries require privileges (`mknod`) most users lack.
    #[arg(
        long = "create-other-entries",
        default_value_t = false,
        action = ArgAction::SetTrue,
        help_heading = "Placement"
    )]
    pub create_other_entries: bool,

    // --- File Attributes ---

    /// Restore archived uid/gid when possible (GNU tar `--same-owner`; may require root).
    /// Default: inferred from the effective uid of the extracting process.
    #[arg(
        long = "same-owner",
        visible_alias = "restore-owner",
        default_value_t = false,
        action = ArgAction::SetTrue,
        help_heading = "File Attributes"
    )]
    pub restore_owner: bool,

    /// Do not restore archived ownership; files are owned by the extracting process.
    /// Default: inferred from the effective uid of the extracting process.
    #[arg(long = "no-same-owner", default_value_t = false, action = ArgAction::SetTrue, help_heading = "File Attributes")]
    pub no_same_owner: bool,

    /// How to emit the resolved owner/group: `ids`, `names`, `name-id`, or `id-name` (default `name-id`).
    #[arg(
        long = "map-target",
        value_name = "TARGET",
        value_parser = clap::value_parser!(MapResolutionTarget),
        default_value_t = MapResolutionTarget::NameId,
        help_heading = "File Attributes"
    )]
    pub map_target: MapResolutionTarget,

    /// Fail hard on map/override destinations that the chosen --map-target cannot emit.
    #[arg(long = "validate-maps", default_value_t = false, action = ArgAction::SetTrue, help_heading = "File Attributes")]
    pub validate_maps: bool,

    /// Force owner for restored members: `NAME`, `UID`, or `NAME:UID`.
    /// Identity is all-or-nothing: giving any of `--owner`/`--group`/`--owner-map`/
    /// `--group-map` uses the CLI for **both** identities and disables
    /// `--apply-stored-owner-map` / `--apply-stored-group-map` (no CLI/archive mix).
    #[arg(long = "owner", value_name = "NAME[:UID]", help_heading = "File Attributes")]
    pub owner: Option<String>,

    /// Owner translation map file (GNU tar `--owner-map`). See `--owner` for the
    /// all-or-nothing identity rule.
    #[arg(long = "owner-map", value_name = "FILE", help_heading = "File Attributes")]
    pub owner_map: Option<PathBuf>,

    /// Force group for restored members: `NAME`, `GID`, or `NAME:GID`.
    /// See `--owner` for the all-or-nothing identity rule.
    #[arg(long = "group", value_name = "NAME[:GID]", help_heading = "File Attributes")]
    pub group: Option<String>,

    /// Group translation map file (GNU tar `--group-map`). See `--owner` for the
    /// all-or-nothing identity rule.
    #[arg(long = "group-map", value_name = "FILE", help_heading = "File Attributes")]
    pub group_map: Option<PathBuf>,

    /// Apply the owner map recorded in the archive (alongside `--apply-stored-group-map`,
    /// the whole archived identity). Ignored if any of `--owner`/`--group`/
    /// `--owner-map`/`--group-map` is given: identity comes entirely from the
    /// command line or entirely from the archive — never a mix.
    #[arg(long = "apply-stored-owner-map", default_value_t = false, action = ArgAction::SetTrue, help_heading = "File Attributes")]
    pub apply_stored_owner_map: bool,

    /// Do not apply the owner map recorded in the archive.
    #[arg(long = "no-apply-stored-owner-map", default_value_t = false, action = ArgAction::SetTrue, help_heading = "File Attributes")]
    pub no_apply_stored_owner_map: bool,

    /// Apply the group map recorded in the archive (alongside `--apply-stored-owner-map`,
    /// the whole archived identity). Ignored if any of `--owner`/`--group`/
    /// `--owner-map`/`--group-map` is given: identity comes entirely from the
    /// command line or entirely from the archive — never a mix.
    #[arg(long = "apply-stored-group-map", default_value_t = false, action = ArgAction::SetTrue, help_heading = "File Attributes")]
    pub apply_stored_group_map: bool,

    /// Do not apply the group map recorded in the archive.
    #[arg(long = "no-apply-stored-group-map", default_value_t = false, action = ArgAction::SetTrue, help_heading = "File Attributes")]
    pub no_apply_stored_group_map: bool,

    /// Force symbolic mode CHANGES for restored members; takes precedence over `--apply-mode`.
    #[arg(long = "mode", value_name = "CHANGES", help_heading = "File Attributes")]
    pub mode: Option<String>,

    /// Apply the mode changes recorded in the archive (ignored if `--mode` given).
    #[arg(long = "apply-mode", default_value_t = false, action = ArgAction::SetTrue, help_heading = "File Attributes")]
    pub apply_mode: bool,

    /// Do not apply the mode changes recorded in the archive.
    #[arg(long = "no-apply-mode", default_value_t = false, action = ArgAction::SetTrue, help_heading = "File Attributes")]
    pub no_apply_mode: bool,

    /// Force a sed-style name transform (GNU tar `--transform`/`--xform`) at
    /// extraction; takes precedence over `--apply-transform`.
    #[arg(
        long = "transform",
        alias = "xform",
        value_name = "EXPRESSION",
        help_heading = "File Attributes"
    )]
    pub transform: Option<String>,

    /// Apply the name transform recorded in the archive (ignored if `--transform` given).
    #[arg(long = "apply-transform", default_value_t = false, action = ArgAction::SetTrue, help_heading = "File Attributes")]
    pub apply_transform: bool,

    /// Do not apply the name transform recorded in the archive.
    #[arg(long = "no-apply-transform", default_value_t = false, action = ArgAction::SetTrue, help_heading = "File Attributes")]
    pub no_apply_transform: bool,

    /// Strip NUMBER leading path components from file names before extraction.
    /// (GNU tar `--strip-components`; 0 = no-op.)
    #[arg(
        long = "strip-components",
        value_name = "NUMBER",
        default_value_t = 0,
        help_heading = "File Attributes"
    )]
    pub strip_components: u32,

    /// Restore archived access times from the database (default: leave untouched).
    #[arg(long = "apply-atime", default_value_t = false, action = ArgAction::SetTrue, help_heading = "File Attributes")]
    pub apply_atime: bool,

    /// Explicitly do not restore archived access times (overrides `--apply-all-metadata`).
    #[arg(long = "no-apply-atime", default_value_t = false, action = ArgAction::SetTrue, help_heading = "File Attributes")]
    pub no_apply_atime: bool,

    /// Restore archived modification times from the database (default: leave untouched).
    #[arg(long = "apply-mtime", default_value_t = false, action = ArgAction::SetTrue, help_heading = "File Attributes")]
    pub apply_mtime: bool,

    /// Explicitly do not restore archived modification times (overrides `--apply-all-metadata`).
    #[arg(long = "no-apply-mtime", default_value_t = false, action = ArgAction::SetTrue, help_heading = "File Attributes")]
    pub no_apply_mtime: bool,

    /// Apply archived extended attributes.
    #[arg(long = "xattrs", default_value_t = false, action = ArgAction::SetTrue, help_heading = "File Attributes")]
    pub xattrs: bool,

    /// Do not apply archived extended attributes.
    #[arg(long = "no-xattrs", default_value_t = false, action = ArgAction::SetTrue, help_heading = "File Attributes")]
    pub no_xattrs: bool,

    /// Apply archived POSIX ACLs.
    #[arg(long = "acls", default_value_t = false, action = ArgAction::SetTrue, help_heading = "File Attributes")]
    pub acls: bool,

    /// Do not apply archived POSIX ACLs.
    #[arg(long = "no-acls", default_value_t = false, action = ArgAction::SetTrue, help_heading = "File Attributes")]
    pub no_acls: bool,

    /// Apply archived SELinux contexts.
    #[arg(long = "selinux", default_value_t = false, action = ArgAction::SetTrue, help_heading = "File Attributes")]
    pub selinux: bool,

    /// Do not apply archived SELinux contexts.
    #[arg(long = "no-selinux", default_value_t = false, action = ArgAction::SetTrue, help_heading = "File Attributes")]
    pub no_selinux: bool,

    /// Apply archived permissions (mode). Permissions are restored by default.
    #[arg(long = "same-permissions", default_value_t = false, action = ArgAction::SetTrue, help_heading = "File Attributes")]
    pub same_permissions: bool,

    /// Do not apply archived permissions (mode).
    #[arg(long = "no-same-permissions", default_value_t = false, action = ArgAction::SetTrue, help_heading = "File Attributes")]
    pub no_same_permissions: bool,

    /// Apply every metadata attribute recorded in the archive (owner, stored
    /// owner/group maps, mode, transform, atime/mtime, xattrs/acls/selinux, perms).
    /// Individual `--no-*` toggles override this on a per-bit basis.
    #[arg(long = "apply-all-metadata", default_value_t = false, action = ArgAction::SetTrue, help_heading = "File Attributes")]
    pub apply_metadata: bool,

    // --- Process Options ---

    /// Maximum concurrent workers (rayon pools and xz threads).
    #[arg(long = "jobs", value_name = "N", help_heading = "Process Options")]
    pub jobs: Option<usize>,

    /// Maximum concurrent io-bound workers (dedup, sparsify, place pools).
    #[arg(long = "io-jobs", value_name = "N", help_heading = "Process Options")]
    pub io_jobs: Option<usize>,

    /// Wipe extract work (`.estage`) and start over.
    #[arg(long = "fresh", help_heading = "Process Options")]
    pub fresh: bool,

    /// Continue even if the tool cannot guarantee the integrity of the data.
    #[arg(long = "force", default_value_t = false, action = ArgAction::SetTrue, help_heading = "Process Options")]
    pub force: bool,

    /// Skip entries whose path cannot be represented as UTF-8 (warn + continue).
    /// Mutually exclusive with `--force` and `--fail-fast`.
    #[arg(long = "relax-utf8", default_value_t = false, action = ArgAction::SetTrue, help_heading = "Process Options")]
    pub relax_utf8: bool,

    /// Number of error drafts buffered before an auto-flush to the database.
    #[arg(
        long = "auto-flush-limit",
        value_name = "N",
        default_value_t = common::defaults::DEFAULT_AUTO_FLUSH_LIMIT,
        help_heading = "Process Options"
    )]
    pub auto_flush_limit: u64,

    /// Rows pulled from the database per phase iteration.
    #[arg(
        long = "batch-size",
        value_name = "N",
        default_value_t = common::defaults::BATCH_SIZE,
        help_heading = "Process Options"
    )]
    pub batch_size: u64,

    /// Items in the producer/consumer work channel.
    #[arg(
        long = "work-capacity",
        value_name = "N",
        default_value_t = common::defaults::WORK_CAPACITY,
        help_heading = "Process Options"
    )]
    pub work_capacity: usize,

    /// Items in the producer/consumer result channel.
    #[arg(
        long = "out-capacity",
        value_name = "N",
        default_value_t = common::defaults::OUT_CAPACITY,
        help_heading = "Process Options"
    )]
    pub out_capacity: usize,

    /// Rows pulled from the database per feed round.
    #[arg(
        long = "feed-chunk",
        value_name = "N",
        default_value_t = common::defaults::FEED_CHUNK,
        help_heading = "Process Options"
    )]
    pub feed_chunk: usize,

    /// Outcomes committed per transaction.
    #[arg(
        long = "drain-chunk",
        value_name = "N",
        default_value_t = common::defaults::DRAIN_CHUNK,
        help_heading = "Process Options"
    )]
    pub drain_chunk: usize,

    /// Which progress bars to draw.
    #[arg(
        long = "progress-mode",
        value_enum,
        default_value_t = ProgressMode::PerThread,
        help_heading = "Process Options"
    )]
    pub progress_mode: ProgressMode,

    // --- Filtering ---
    /// Exclude paths matching regex PATTERN from extraction (repeatable).
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

    /// Include only paths matching regex PATTERN for extraction (repeatable).
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

    /// Patterns match from the start of the path (default: unanchored).
    #[arg(long = "anchored", help_heading = "Filtering")]
    pub anchored: bool,

    /// Match patterns case-insensitively (default: case-sensitive).
    #[arg(long = "ignore-case", help_heading = "Filtering")]
    pub ignore_case: bool,

    /// Apply include/exclude filters after a later phase (scan) instead of eagerly.
    #[arg(long = "lazy-filter", default_value_t = false, action = ArgAction::SetTrue, help_heading = "Filtering")]
    pub lazy_filter: bool,

    // --- Sparse Files ---

    /// Re-create holes in the extract cache while unpacking (sparse writer over
    /// a plain copy; uses `--page-size` for hole detection).
    #[arg(
        long = "sparsify",
        default_value_t = false,
        action = ArgAction::SetTrue,
        help_heading = "Sparse Files"
    )]
    pub sparsify: bool,

    /// Page size in bytes for sparse-hole detection while unpacking.
    #[arg(
        long = "page-size",
        value_name = "BYTES",
        default_value_t = 4096,
        help_heading = "Sparse Files"
    )]
    pub page_size: usize,

    /// Abort on the first warning or error instead of continuing where possible.
    #[arg(
        long = "fail-fast",
        default_value_t = false,
        action = ArgAction::SetTrue,
        help_heading = "Process Options"
    )]
    pub fail_fast: bool,

    /// Do not record per-file errors in the database.
    #[arg(long = "no-errors", default_value_t = false, action = ArgAction::SetTrue, help_heading = "Process Options")]
    pub no_errors: bool,

    /// After success, keep a timestamped copy of snapshot.sqlite next to the archive.
    #[arg(long = "keep-db", help_heading = "Process Options")]
    pub keep_db: bool,

    /// After success, keep the `{stem}.estage` work directory.
    #[arg(long = "keep-stage", help_heading = "Process Options")]
    pub keep_stage: bool,
}

/// Continue incomplete work. Policy comes from the work DB; only jobs, `--io-jobs`
/// and `--exit-after-stage` may be overridden.
#[derive(Debug, Args)]
pub struct ResumeArgs {
    /// Work directory (`.astage` / `.estage`) of the interrupted run.
    #[arg(long = "work-dir", value_name = "DIR")]
    pub work_dir: Option<PathBuf>,

    /// Path to the work database (`tar-dedup.sqlite`) directly; mutually exclusive with `--work-dir`.
    #[arg(long = "db", value_name = "FILE")]
    pub db: Option<PathBuf>,

    /// Maximum concurrent workers (rayon pools and xz threads).
    #[arg(long = "jobs", value_name = "N")]
    pub jobs: Option<usize>,

    /// Maximum concurrent io-bound workers (dedup, sparsify, place pools).
    #[arg(long = "io-jobs", value_name = "N")]
    pub io_jobs: Option<usize>,

    /// Run through STAGE then exit cleanly (state saved).
    #[arg(long = "exit-after-stage", value_name = "STAGE", value_enum)]
    pub exit_after_stage: Option<ExitAfterStageArg>,
}

/// Archive-target input for the catalog inspection commands (`inspect`, `list`,
/// `dump`, `query`). The catalog comes from the archive footer (`-f`) by default,
/// or from an existing work database directly (`--db` / `--work-dir`). Relative
/// paths resolve against the current directory.
#[derive(Debug, Args)]
pub struct ArchiveInputArgs {
    // --- Archive Target ---

    /// Archive path; the catalog is read from its seekable sqlite footer, else from
    /// the leading `manifest.sqlite` member.
    #[arg(short = 'f', value_name = "ARCHIVE", help_heading = "Archive Target")]
    pub archive: Option<PathBuf>,

    /// Read the catalog from an existing work database file directly.
    #[arg(long = "db", value_name = "FILE", help_heading = "Archive Target")]
    pub db: Option<PathBuf>,

    /// Work directory (`.astage` / `.estage`) containing `tar-dedup.sqlite`.
    #[arg(long = "work-dir", value_name = "DIR", help_heading = "Archive Target")]
    pub work_dir: Option<PathBuf>,

    // --- Output ---

    /// Emit a single machine-readable JSON document instead of text sections.
    #[arg(long = "json", default_value_t = false, action = ArgAction::SetTrue, help_heading = "Output")]
    pub json: bool,

    /// Compact JSON output (no wrapping/indent); only meaningful with `--json`.
    #[arg(long = "no-pretty", default_value_t = false, action = ArgAction::SetTrue, help_heading = "Output")]
    pub no_pretty: bool,

    /// Text mode: join records within a section on one line (`; `) instead of
    /// one per line.
    #[arg(long = "collapse", default_value_t = false, action = ArgAction::SetTrue, help_heading = "Output")]
    pub collapse: bool,

    /// Text mode: emit only the section(s) whose header contains SUBSTR
    /// (case-insensitive), e.g. `--only "owner map"`.
    #[arg(long = "only", value_name = "SUBSTR", help_heading = "Output")]
    pub only: Option<String>,

    /// Prefer the last `snapshot.sqlite` member over the footer / leading
    /// manifest (not implemented yet).
    #[arg(long = "force", default_value_t = false, action = ArgAction::SetTrue, help_heading = "Output")]
    pub force: bool,
}

/// Reset an interrupted run: re-point its stored parameters and rewind the
/// pipeline to a consistent state. The work database is named explicitly.
#[derive(Debug, Args)]
pub struct ResetArgs {
    /// Path to the work database (`tar-dedup.sqlite`) directly.
    #[arg(long = "db", value_name = "FILE")]
    pub db: Option<PathBuf>,

    /// Work directory of the run (`.astage` / `.estage`), holding `tar-dedup.sqlite`.
    #[arg(long = "work-dir", value_name = "DIR")]
    pub work_dir: Option<PathBuf>,
}

/// Pipeline stop point for `--exit-after-stage`.
///
/// Names follow archive file-phase progression (plus `cleanup`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, ValueEnum)]
#[value(rename_all = "kebab-case")]
pub enum ConflictPolicy {
    /// Keep existing files; warn on conflict (GNU tar `--keep-old-files`).
    #[value(alias = "keep-old-files")]
    PreferOlder,
    /// Keep whichever copy has the newer mtime (GNU tar `--keep-newer-files`).
    #[value(alias = "keep-newer-files")]
    PreferNewer,
    /// Overwrite existing paths (GNU tar `--overwrite`; default).
    #[default]
    #[value(alias = "overwrite")]
    Replace,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[value(rename_all = "lower")]
pub enum HardLinkGrouping {
    /// Do not hard link any files which had the same (dev, inode) as on the source file system
    None,
    /// Only consider files within a tree of a single source for hard links.
    Source,
    /// Hard link files across all sources.
    Global,
}

/// Deduplication strategy for the archive pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, ValueEnum)]
#[value(rename_all = "kebab-case")]
pub enum DedupMode {
    /// Byte-compare duplicates within `(sha1, size)` groups (default).
    #[default]
    Regular,
    /// Trust the digest: `min(id)` of each `(sha1, size)` group is the
    /// self-canonical, all other members point at it. No byte verification.
    Hash,
    /// No content dedup. With hardlink detection, only `(sha1, size, dev, ino)`
    /// hard-link groups collapse; without it, every eligible file is self-canonical.
    None,
}

/// Ordering of payloads in the tar stream; affects staging and packing order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, ValueEnum)]
#[value(rename_all = "lower")]
pub enum ArchiveOrdering {
    /// `ext, size, id` (default).
    #[default]
    #[value(name = "size")]
    ExtSizeId,
    /// `ext, filename, size, id`.
    #[value(name = "name")]
    ExtNameSizeId,
    /// Debug override: plain `id` (only compiled with the `debug-ordering` feature).
    #[cfg(feature = "debug-ordering")]
    #[value(name = "id")]
    Id,
}

/// Which progress bars are drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, ValueEnum)]
#[value(rename_all = "kebab-case")]
pub enum ProgressMode {
    /// Global, phase and sub bars, but no per-thread worker bars.
    Global,
    /// No progress bars at all (clean stdout for log piping).
    None,
    /// Everything, including per-thread worker bars (default).
    #[default]
    PerThread,
}
