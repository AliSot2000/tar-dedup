# Tar-Dedup
A `tar` "wrapper" to deduplicate the file-tree prior to archiving to save extra space. It capability to interrupt the 
creation of large tar archives at the cost of lower compression ratios for compressed archives.

## Usage
The `tar-dedup` consists of a few subcommands. Here are their options as well as some additional information about 
their behavior.

### archive
This command takes a set of input files / directories and produces an archive.

**Archive Paths**

| Short | Long        | Required | Args | Description                                                                                                                                                                                 | Notes |
|-------|-------------|----------|------|---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|-------|
| -f    | -           | y        | 1    | Archive path (relative paths use `-C` / current directory)                                                                                                                                  | 1.    |
| -C    | --directory | n        | 1    | Treat DIR as the working directory when resolving relative paths on this command line (`-f`, `-i`, `-T`, `--work-dir`, …). Absolute paths are unchanged. Default: process current directory | 1.    |
| -     | --work-dir  | n        | 1    | Stage directory for sqlite DB, staged payloads, and locks (defaults to `{stem}.astage` next to the archive). Relative paths use `-C` / cwd                                                  | 1.    |

**Inputs**

| Short | Long         | Required | Args | Description                                                            | Notes |
|-------|--------------|----------|------|------------------------------------------------------------------------|-------|
| -i    | --input-dir  | n        | 1+   | Input directory to snapshot (repeatable; union of roots)               | 1. 2. |
| -T    | --files-from | n        | 1+   | Read member paths from FILE (`-` = stdin). Repeatable; union with `-i` | 1. 2. |
| -     | --null       | n        | 0    | `-T` records are NUL-terminated (default: newline-separated)           |       |

**Compression**

| Short | Long                   | Required | Args | Description                                                                                          | Notes |
|-------|------------------------|----------|------|------------------------------------------------------------------------------------------------------|-------|
| -z    | --gzip                 | n        | 0    | Filter the archive through gzip (overrides extension inference)                                      | 3.    |
| -j    | --bzip2                | n        | 0    | Filter the archive through bzip2 (overrides extension inference)                                     | 3.    |
| -J    | --xz                   | n        | 0    | Filter the archive through xz (overrides extension inference)                                        | 3.    |
| -     | --zstd                 | n        | 0    | Filter the archive through zstd (overrides extension inference)                                      | 3.    |
| -     | --level                | n        | 1    | Compression level. Allowed range depends on compression algorithm: gzip/bzip2 1–9, xz 0–9, zstd 1–19 |       |
| -     | --xz-extreme           | n        | 0    | xz `--extreme` / `-e` (OR into LZMA preset). Only valid with xz                                      |       |
| -     | --xz-memlimit-compress | n        | 1    | Cap xz encoder RAM (bytes, MiB, GiB, or % of RAM). Like `xz --memlimit-compress`                     |       |

**Indexing**  
The general assumptions when indexing are: 
- There's no writes to the files currently indexed. 
- There's no writes to the metadata of the files currently indexed.
- Files may be read.

The tool enforces that each inode is read exactly **once**. As it can become cumbersome to separate trees with a lot of 
links [...], the tool offers to loosen the restriction. Link cycles are still detected, however, every unique absolute 
path is treated as a new entity and has its metadata recorded.

| Short  | Long                    | Required | Args  | Description                                                                       | Notes                                      |
|--------|-------------------------|----------|-------|-----------------------------------------------------------------------------------|--------------------------------------------|
| -      | --no-recursion          | n        | 0     | Do not descend into directories                                                   |                                            |
| -      | --dereference           | n        | 0     | Follow symlinks; archive the files they point to (GNU tar `-h`)                   |                                            |
| -      | --one-file-system       | n        | 0     | Stay on one filesystem when walking input trees                                   |                                            |
| -      | --no-hardlink-detection | n        | 0     | Do not coalesce same (inode, device) hard links in hash/dedup                     | 4.                                         |
| -      | --no-strict-separation  | n        | 0     | Allow nested or duplicate `-i` / `-T` directory roots (still recorded and walked) |                                            |

**Filtering**  
`--include-from` and `--exclude-from` expects a file with `utf8` content and each pattern separated by a 
newline character. This can be relaxed with `--no-unicode` if needed. If you need to match a new line for some reason, 
use `\n` and never `0x0A`. Regex is handled with rusts regex crate. Regex is handled by Rust's regex crate. It is a 
linear-time (ReDoS-safe) engine whose syntax is a subset of PCRE/Perl — notably it does not support backreferences, 
look-around ((?=…), (?<=…), …), atomic groups, or possessive quantifiers. Unicode classes are on by default; 
--no-unicode switches to byte-wise (ASCII) matching.

Also, please note **The include/exclude feature deviates substantially from tar!!!** The tool creates an index of all 
provided sources first. In doing so, it stores the absolute path to each item as well as the original path in a raw 
byte buffer (in case the system was not utf-8 compliant). When the filtering is applied, it is not done to the relative 
path but the absolute path of the files provided. On non-compliant systems, the invalid characters are replaced with � 
and then filtered against that. (We needed absolute paths rather than relative paths to detect duplicates early - 
where their elimination is cheap) Furthermore, the filtering is applied to each item **independently** of the others. 
This means, if a `.git$` is processed, it will exclude the `.git` directory but not the files within `.git/index.html`. 
When extracting, this is modeled as any attributes the `.git` directory held will not be restored but the contents of 
it as well as their metadata will be restored as faithfully as possible. If this behavior is undesirable, consider 
using `--files-from` or `-T` to provide your own list of files to add. `-` for stdin is supported. Currently not 
implemented is the `--newer` ... filters. They are currently on the feature list but for the moment, this 
can be remedied by using `filter` in conjunction with `--files-from` to achieve the same filtering.

| Short | Long                  | Required | Args  | Description                                                                            | Notes                                         |
|-------|-----------------------|----------|-------|----------------------------------------------------------------------------------------|-----------------------------------------------|
| -     | --exclude             | n        | 1+    | Exclude paths matching regex PATTERN (repeatable)                                      | 5.                                            |
| -X    | --exclude-from        | n        | 1+    | Read exclude regex patterns from FILE (repeatable)                                     | 5.                                            |
| -     | --include             | n        | 1+    | Include only paths matching regex PATTERN (repeatable)                                 | 5.                                            |
| -     | --include-from        | n        | 1+    | Read include regex patterns from FILE (repeatable)                                     | 5.                                            |
| -     | --exclude-vcs         | n        | 0     | Exclude version control system directories (not implemented yet)                       | 5.                                            |
| -     | --exclude-vcs-ignores | n        | 0     | Read VCS ignore files for exclusions (not implemented yet)                             | 5.                                            |
| -     | --anchored            | n        | 0     | Patterns match from the start of the relative path                                     |                                               |
| -     | --ignore-case         | n        | 0     | Case-insensitive pattern matching                                                      |                                               |
| -     | --no-unicode          | n        | 0     | Compile filter regexes in bytes mode (ASCII-only classes, `.` matches a byte)          |                                               |

**File Attributes**  
As a baseline, the tool attempts to capture as much data as possible. This includes (ctime, atime, mtime, uid, gid, 
mode, size, selinux policy, posix_acls, xattrs, inode type, link destination and link type too for symlinks. By 
default, the tool also attempts to resolve any uid, gid to a group name / owner name via the system database. 

Global default: By default, everything is turned on and can be turned off. This behavior can be inverted with 
`--no-capture-all-metadata`. That way, the default becomes off and needs to be selectively re-enabled with e.g. 
`--acls`.

| Short | Long                      | Required | Args | Description                                                                                                                                                                                         | Notes |
|-------|---------------------------|----------|------|-----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|-------|
| -     | --acls                    | n        | 0    | Capture POSIX ACLs (default: on via the capture umbrella)                                                                                                                                           |       |
| -     | --no-acls                 | n        | 0    | Do not capture POSIX ACLs                                                                                                                                                                           |       |
| -     | --xattrs                  | n        | 0    | Capture extended attributes (default: on via the capture umbrella)                                                                                                                                  |       |
| -     | --no-xattrs               | n        | 0    | Do not capture extended attributes                                                                                                                                                                  |       |
| -     | --selinux                 | n        | 0    | Capture SELinux contexts (default: on via the capture umbrella)                                                                                                                                     |       |
| -     | --no-selinux              | n        | 0    | Do not capture SELinux contexts                                                                                                                                                                     |       |
| -     | --no-capture-all-metadata | n        | 0    | Default: capture every metadata attribute (xattrs, ACLs, SELinux, ...). The `--no-*` toggles override this on a per-bit basis                                                                       |       |
| -     | --numeric-ids-only        | n        | 0    | Record numeric uids/gids instead of resolving user/group names during inventory                                                                                                                     |       |
| -     | --resolve-numeric-ids     | n        | 0    | Resolve user/group names during inventory (default; counters `--numeric-ids-only`)                                                                                                                  |       |
| -     | --owner                   | n        | 1    | Force owner for archived members: `NAME`, `UID`, or `NAME:UID` (GNU tar). Stored as archive policy (meta); applied on extract                                                                       | 6.    |
| -     | --owner-map               | n        | 1    | Owner translation map file (GNU tar `--owner-map`)                                                                                                                                                  | 6.    |
| -     | --group                   | n        | 1    | Force group for archived members: `NAME`, `GID`, or `NAME:GID` (GNU tar). Stored as archive policy (meta); applied on extract                                                                       | 6.    |
| -     | --group-map               | n        | 1    | Group translation map file (GNU tar `--group-map`)                                                                                                                                                  | 6.    |
| -     | --mode                    | n        | 1    | Force symbolic mode CHANGES for added members (GNU tar `--mode`). Applies chmod-style updates (e.g. `u+rwx,go-rx`) at extraction; stored as archive policy (meta)                                   | 6.    |
| -     | --transform               | n        | 1    | Record a sed-style name transform (GNU tar `--transform`/`--xform`, e.g. `s,^usr/,var/,`) to apply at extraction; stored as archive policy (meta). Only validated here; not applied at archive time | 6.    |

**Sparse Files**  
The system relies on the tar reader detecting sparse files and encoding them according to the tar format. As the tar 
builder from the rust tar crate cannot create sparse files inflight, the tool first creates valid sparse files in the 
`--work-dir` such that the tar builder can correctly detect their sparseness when it reads them back. Notes:
- This sparse copy step will only apply to files which are detected to be sparse during the inventory pass, not all of 
  them. Detection happens via the `--page-size` and `--min-pages` options.
- If a compression algorithm is used, it is recommended to rely on that algorithm to deal with sparseness rather than 
  the tool's `sparsify` stage as it will produce the archive in less time.
- Since the sparse copy relies on the underlying filesystem being able to represent sparse files, the sparsification 
  via sparse copy will not work on a file system that is not able to represent sparse files.

| Short | Long        | Required | Args | Description                                                                  | Notes |
|-------|-------------|----------|------|------------------------------------------------------------------------------|-------|
| -     | --sparsify  | n        | 0    | Run the sparsify phase (copy with seeks using `--page-size` / `--min-pages`) |       |
| -     | --page-size | n        | 1    | Page size in bytes for hash zero-page counting and sparsify                  |       |
| -     | --min-pages | n        | 1    | Minimum empty-page count before sparsify treats a file as worth rewriting    |       |

**Process Options**  

| Short | Long               | Required | Args | Description                                                                                                                                              | Notes |
|-------|--------------------|----------|------|----------------------------------------------------------------------------------------------------------------------------------------------------------|-------|
| -     | --jobs             | n        | 1    | Maximum concurrent workers (rayon pools and xz threads)                                                                                                  |       |
| -     | --io-jobs          | n        | 1    | Maximum concurrent io-bound workers (dedup, sparsify, place pools)                                                                                       |       |
| -     | --fresh            | n        | 0    | Wipe existing work and archive, then start from inventory                                                                                                |       |
| -     | --db-retention     | n        | 1    | When to keep a timestamped copy of the work database next to the archive. `always`: always; `on-error`: only if the run recorded errors; `never`: delete |       |
| -     | --keep-stage       | n        | 0    | After success, keep the `{stem}.astage` work directory                                                                                                   |       |
| -     | --exit-after-stage | n        | 1    | Run through STAGE then exit cleanly (state saved)                                                                                                        |       |
| -     | --fail-fast        | n        | 0    | Abort the run on the first hard failure (instead of soft-continuing where possible)                                                                      |       |
| -     | --no-errors        | n        | 0    | Do not record errors encountered while processing the entries in the database                                                                            |       |
| -     | --lazy-filter      | n        | 0    | Apply include/exclude filters after the hash phase, not after inventory phase.                                                                           |       |
| -     | --dedup-mode       | n        | 1    | Deduplication strategy                                                                                                                                   |       |
| -     | --ordering         | n        | 1    | Generate sorted combination of data pieces in the order necessary to write. (GNU tar `--sort` analog; applies at the tar writer.)                        |       |
| -     | --use-shards       | n        | 0    | Use content-defined chunking instead of whole-file hashes (not implemented yet)                                                                          |       |
| -     | --max-shard-size   | n        | 1    | Maximum shard size for content-defined chunking (requires `--use-shards`)                                                                                |       |
| -     | --relax-utf8       | n        | 0    | Lossily convert non-UTF-8 paths (`to_string_lossy`, never skip); raw bytes kept. Mutually exclusive with `--force`                                       | 7.    |
| -     | --auto-flush-limit | n        | 1    | Number of error drafts buffered before an auto-flush to the database                                                                                     | 8.    |
| -     | --batch-size       | n        | 1    | Rows pulled from the database per phase iteration                                                                                                        | 8.    |
| -     | --work-capacity    | n        | 1    | Items in the producer/consumer work channel                                                                                                              | 8.    |
| -     | --out-capacity     | n        | 1    | Items in the producer/consumer result channel                                                                                                            | 8.    |
| -     | --feed-chunk       | n        | 1    | Rows pulled from the database per feed round                                                                                                             | 8.    |
| -     | --drain-chunk      | n        | 1    | Outcomes committed per transaction                                                                                                                       | 8.    |
| -     | --progress-mode    | n        | 1    | Which progress bars to draw                                                                                                                              | 8.    |
| -     | --log-level        | n        | 1    | Verbosity of the log output (overrides the `RUST_LOG` env var)                                                                                           | 8.    |

**Notes**
1. Path resolution: If the path is absolute, the path is taken as is. If the path is relative and `-C --directory` is not set, 
   the current process working directory is taken from the environment and used as the root for the relative paths. If
   `-C --directory` is given, relative paths are resolved with this as a root instead.
2. At least one input must be given. So either one `-T` or `-i`. Multiples are supported. (so multiple `-i` and `-T`)
3. If no compression is given, the compression is inferred by the extension. E.g. `.tar.xz` maps to the xz compression. 
   An uncompressed archive can be produced by simply supplying `.tar` as the file extension.
4. Disabling the hardlink_detection can increase the runtime of the dedup stage because the tool will compare inode 
   with itself `n-1` times (i.e. if it was recorded `n` times.)
5. Filtering always uses REGEX TODO FORMAT!!!. No Glob or Shell expansions are supported. Files are first indexed. If given, only files
   matching one or more of the include filters are propagated. Lastly only files that don't match a single exclude 
   pattern are then eligible. If no include filter is given, a catch-all filter is used.
6. The database will store the overrides separately from the file metadata. During extraction, the overrides will be applied 
   rather than on archive. Before the overrides are stored in the database, the tool will parse them and error out, if they are invalid.
7. All utf-8 conform paths will be processed regardless. For None utf-8 paths, the `--relax-utf8` option takes 
   priority. If given, any path without valid utf-8 is converted to utf-8, replacing any non-utf8 characters with �. 
   The original path is stored in the database for reference but is currently not treated further. If without 
   `--relax-utf8` the treatment of paths depends on `--force`. Without `--force` the path is skipped with a warning 
   and not recorded anywhere. With `--force` any non-utf8 path leads to an exit.
8. Options with this note are used for testing purposes and performance benchmarks. Sensible defaults should be set. 
   Changes can negatively impact tool performance by increasing the number of transactions in the database. Changes can 
   also use more RAM, leading to worse system performance.

### extract

Restore a tar-dedup archive (materialize: full copy at each original path). Aborts if extract work already exists; use `--fresh` to replace.

**Archive Paths**

| Short | Long                | Required | Args | Description                                                                                                                                                                           | Notes |
|-------|---------------------|----------|------|---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|-------|
| -f    | -                   | y        | 1    | Archive path (relative paths use `-C` / current directory)                                                                                                                            |       |
| -C    | --directory         | n        | 1    | Treat DIR as the working directory when resolving relative paths on this command line (`-f`, `--work-dir`, …). Absolute paths are unchanged. Default: process current directory       |       |
| -     | --extract-to        | n        | 1    | Extract members into DIR (the restore root); default: `-C` / current dir                                                                                                              |       |
| -     | --work-dir          | n        | 1    | Stage directory for sqlite DB, cached payloads, and locks (defaults to `{stem}.estage` next to the archive). Relative paths use `-C` / cwd                                            |       |
| -     | --work-dir-location | n        | 1    | Where `{stem}.estage` is placed when `--work-dir` is not given: `beside-archive` (default) or `beside-output` (next to the extraction root)                                           |       |
| -P    | --absolute          | n        | 0    | Restore using absolute catalog paths (`-P`). Extraction join abs paths under the extraction root (leading `/` stripped). To extract to file system root, pass `/` as `-C`             |       |
| -     | --one-top-level     | n        | 0..1 | Extract loose members into a subdirectory under the extraction root (GNU tar `--one-top-level`). A bare `--one-top-level` uses the archive name as the subdirectory; DIR overrides it |       |
| -     | --clean-target      | n        | 0    | Remove the extraction root before extracting (only effective with `--one-top-level`)                                                                                                  |       |

**Compression**

| Short | Long    | Required | Args | Description                                                      | Notes |
|-------|---------|----------|------|------------------------------------------------------------------|-------|
| -z    | --gzip  | n        | 0    | Filter the archive through gzip (overrides extension inference)  |       |
| -j    | --bzip2 | n        | 0    | Filter the archive through bzip2 (overrides extension inference) |       |
| -J    | --xz    | n        | 0    | Filter the archive through xz (overrides extension inference)    |       |
| -     | --zstd  | n        | 0    | Filter the archive through zstd (overrides extension inference)  |       |

**Rehash**

| Short | Long        | Required | Args | Description                                                     | Notes |
|-------|-------------|----------|------|-----------------------------------------------------------------|-------|
| -     | --no-rehash | n        | 0    | Do not run the rehash (content verification) phase. Default: on |       |

**Overwrite Control**

| Short | Long                     | Required | Args | Description                                                                                                                                                                                                       | Notes |
|-------|--------------------------|----------|------|-------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|-------|
| -     | --keep-directory-symlink | n        | 0    | Preserve existing directory symlinks instead of replacing them (GNU tar `--keep-directory-symlink`)                                                                                                               |       |
| -U    | --unlink-first           | n        | 0    | Remove each existing file before extracting over it (GNU tar `-U` / `--unlink-first`)                                                                                                                             |       |
| -     | --no-create-dirs         | n        | 0    | Skip the directory tree create pass. Results in the members without a parent being skipped. Results in error with --fail-fast                                                                                     |       |
| -     | --no-overwrite-dir       | n        | 0    | Do not apply archived metadata to existing directories (GNU tar `--no-overwrite-dir`). Default: apply directory metadata for placed members, matching GNU tar at the end of the pipeline                          |       |
| -     | --overwrite-all-dir      | n        | 0    | Also apply archived directory metadata to directories excluded by filters (normally only placed members are updated). Not the negation of `--no-overwrite-dir`: both flags together disable all directory updates |       |
| -     | --conflict-policy        | n        | 1    | How to handle paths that already exist on disk                                                                                                                                                                    |       |
| -     | --silent-conflicts       | n        | 0    | Do not log path or type conflicts (pairs with `--conflict-policy`)                                                                                                                                                |       |
| -     | --remove-and-replace     | n        | 0    | On type mismatch at a path, remove the existing entry and extract the archive member                                                                                                                              |       |
| -     | --hard-link-grouping     | n        | 1    | Adjust how hardlinks are extracted                                                                                                                                                                                |       |

**Link Tree**

| Short | Long             | Required | Args | Description                                                                                                                                     | Notes |
|-------|------------------|----------|------|-------------------------------------------------------------------------------------------------------------------------------------------------|-------|
| -     | --link-tree      | n        | 0    | Build the restored tree from symlinks/hard links into the extract stage instead of copying payloads (keeps the `.estage` cache)                 |       |
| -     | --hard-links     | n        | 0    | With `--link-tree`, use hard links instead of symlinks                                                                                          |       |
| -     | --absolute-links | n        | 0    | With `--link-tree`, link using absolute paths instead of paths relative to the output root                                                      |       |
| -     | --link-source    | n        | 1    | With `--link-tree`, use DIR as the directory the canonical payloads are materialized into before linking (default: the extract cache directory) |       |

**Placement**

| Short | Long                   | Required | Args | Description                                                                                                                                 | Notes |
|-------|------------------------|----------|------|---------------------------------------------------------------------------------------------------------------------------------------------|-------|
| -     | --no-reflink           | n        | 0    | Do not attempt copy-on-write (reflink) copies when placing payloads                                                                         |       |
| -     | --create-other-entries | n        | 0    | Recreate non-regular entries (symlink, fifo, char/block device, socket). Off by default: those entries require `mknod` privileges or `root` |       |

**File Attributes**

| Short | Long                        | Required | Args | Description                                                                                                                                                                                                                                                                               | Notes |
|-------|-----------------------------|----------|------|-------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|-------|
| -     | --same-owner                | n        | 0    | Restore archived uid/gid when possible (GNU tar `--same-owner`; may require root). Default: inferred from the effective uid of the extracting process                                                                                                                                     |       |
| -     | --no-same-owner             | n        | 0    | Do not restore archived ownership; files are owned by the extracting process. Default: inferred from the effective uid of the extracting process                                                                                                                                          |       |
| -     | --map-target                | n        | 1    | How to emit the resolved owner/group: `ids`, `names`, `name-id`, or `id-name` (default `name-id`)                                                                                                                                                                                         |       |
| -     | --validate-maps             | n        | 0    | Fail hard on map/override destinations that the chosen --map-target cannot emit                                                                                                                                                                                                           |       |
| -     | --owner                     | n        | 1    | Force owner for restored members: `NAME`, `UID`, or `NAME:UID`. Identity is all-or-nothing: giving any of `--owner`/`--group`/`--owner-map`/ `--group-map` uses the CLI for **both** identities and disables `--apply-stored-owner-map` / `--apply-stored-group-map` (no CLI/archive mix) |       |
| -     | --owner-map                 | n        | 1    | Owner translation map file (GNU tar `--owner-map`). See `--owner` for the all-or-nothing identity rule                                                                                                                                                                                    |       |
| -     | --group                     | n        | 1    | Force group for restored members: `NAME`, `GID`, or `NAME:GID`. See `--owner` for the all-or-nothing identity rule                                                                                                                                                                        |       |
| -     | --group-map                 | n        | 1    | Group translation map file (GNU tar `--group-map`). See `--owner` for the all-or-nothing identity rule                                                                                                                                                                                    |       |
| -     | --apply-stored-owner-map    | n        | 0    | Apply the owner map recorded in the archive (alongside `--apply-stored-group-map`, the whole archived identity). Ignored if any of `--owner`/`--group`/ `--owner-map`/`--group-map` is given: identity comes entirely from the command line or entirely from the archive — never a mix    |       |
| -     | --no-apply-stored-owner-map | n        | 0    | Do not apply the owner map recorded in the archive                                                                                                                                                                                                                                        |       |
| -     | --apply-stored-group-map    | n        | 0    | Apply the group map recorded in the archive (alongside `--apply-stored-owner-map`, the whole archived identity). Ignored if any of `--owner`/`--group`/ `--owner-map`/`--group-map` is given: identity comes entirely from the command line or entirely from the archive — never a mix    |       |
| -     | --no-apply-stored-group-map | n        | 0    | Do not apply the group map recorded in the archive                                                                                                                                                                                                                                        |       |
| -     | --mode                      | n        | 1    | Force symbolic mode CHANGES for restored members; takes precedence over `--apply-mode`                                                                                                                                                                                                    |       |
| -     | --apply-mode                | n        | 0    | Apply the mode changes recorded in the archive (ignored if `--mode` given)                                                                                                                                                                                                                |       |
| -     | --no-apply-mode             | n        | 0    | Do not apply the mode changes recorded in the archive                                                                                                                                                                                                                                     |       |
| -     | --transform                 | n        | 1    | Force a sed-style name transform (GNU tar `--transform`/`--xform`) at extraction; takes precedence over `--apply-transform`                                                                                                                                                               |       |
| -     | --apply-transform           | n        | 0    | Apply the name transform recorded in the archive (ignored if `--transform` given)                                                                                                                                                                                                         |       |
| -     | --no-apply-transform        | n        | 0    | Do not apply the name transform recorded in the archive                                                                                                                                                                                                                                   |       |
| -     | --strip-components          | n        | 1    | Strip NUMBER leading path components from file names before extraction. (GNU tar `--strip-components`; 0 = no-op.)                                                                                                                                                                        |       |
| -     | --apply-atime               | n        | 0    | Restore archived access times from the database (default: leave untouched)                                                                                                                                                                                                                |       |
| -     | --no-apply-atime            | n        | 0    | Explicitly do not restore archived access times (overrides `--apply-all-metadata`)                                                                                                                                                                                                        |       |
| -     | --apply-mtime               | n        | 0    | Restore archived modification times from the database (default: leave untouched)                                                                                                                                                                                                          |       |
| -     | --no-apply-mtime            | n        | 0    | Explicitly do not restore archived modification times (overrides `--apply-all-metadata`)                                                                                                                                                                                                  |       |
| -     | --xattrs                    | n        | 0    | Apply archived extended attributes                                                                                                                                                                                                                                                        |       |
| -     | --no-xattrs                 | n        | 0    | Do not apply archived extended attributes                                                                                                                                                                                                                                                 |       |
| -     | --acls                      | n        | 0    | Apply archived POSIX ACLs                                                                                                                                                                                                                                                                 |       |
| -     | --no-acls                   | n        | 0    | Do not apply archived POSIX ACLs                                                                                                                                                                                                                                                          |       |
| -     | --selinux                   | n        | 0    | Apply archived SELinux contexts                                                                                                                                                                                                                                                           |       |
| -     | --no-selinux                | n        | 0    | Do not apply archived SELinux contexts                                                                                                                                                                                                                                                    |       |
| -     | --same-permissions          | n        | 0    | Apply archived permissions (mode). Permissions are restored by default                                                                                                                                                                                                                    |       |
| -     | --no-same-permissions       | n        | 0    | Do not apply archived permissions (mode)                                                                                                                                                                                                                                                  |       |
| -     | --apply-all-metadata        | n        | 0    | Apply every metadata attribute recorded in the archive (owner, stored owner/group maps, mode, transform, atime/mtime, xattrs/acls/selinux, perms). Individual `--no-*` toggles override this on a per-bit basis                                                                           |       |

**Process Options**

| Short | Long               | Required | Args | Description                                                                                                                                                      | Notes |
|-------|--------------------|----------|------|------------------------------------------------------------------------------------------------------------------------------------------------------------------|-------|
| -     | --jobs             | n        | 1    | Maximum concurrent workers (rayon pools and xz threads)                                                                                                          |       |
| -     | --io-jobs          | n        | 1    | Maximum concurrent io-bound workers (dedup, sparsify, place pools)                                                                                               |       |
| -     | --fresh            | n        | 0    | Wipe extract work (`.estage`) and start over                                                                                                                     |       |
| -     | --force            | n        | 0    | Continue even if the tool cannot guarantee the integrity of the data                                                                                             |       |
| -     | --relax-utf8       | n        | 0    | Lossily convert non-UTF-8 paths (`to_string_lossy`, never skip); raw bytes kept. Mutually exclusive with `--force`                                               |       |
| -     | --auto-flush-limit | n        | 1    | Number of error drafts buffered before an auto-flush to the database                                                                                             |       |
| -     | --batch-size       | n        | 1    | Rows pulled from the database per phase iteration                                                                                                                |       |
| -     | --work-capacity    | n        | 1    | Items in the producer/consumer work channel                                                                                                                      |       |
| -     | --out-capacity     | n        | 1    | Items in the producer/consumer result channel                                                                                                                    |       |
| -     | --feed-chunk       | n        | 1    | Rows pulled from the database per feed round                                                                                                                     |       |
| -     | --drain-chunk      | n        | 1    | Outcomes committed per transaction                                                                                                                               |       |
| -     | --progress-mode    | n        | 1    | Which progress bars to draw                                                                                                                                      |       |
| -     | --fail-fast        | n        | 0    | Abort on the first warning or error instead of continuing where possible                                                                                         |       |
| -     | --no-errors        | n        | 0    | Do not record per-file errors in the database                                                                                                                    |       |
| -     | --db-retention     | n        | 1    | When to keep a timestamped copy of the work database (under the extraction root). `always`: always; `on-error`: only if the run recorded errors; `never`: delete |       |
| -     | --keep-stage       | n        | 0    | After success, keep the `{stem}.estage` work directory                                                                                                           |       |
| -     | --exit-after-stage | n        | 1    | Run through STAGE then exit cleanly (state saved)                                                                                                                |       |

**Sidecar**

| Short | Long             | Required | Args | Description                                                                                                                                     | Notes |
|-------|------------------|----------|------|-------------------------------------------------------------------------------------------------------------------------------------------------|-------|
| -     | --sidecar        | n        | 1    | Write a per-entry JSON metadata manifest beside each placed entry. `on-error` (default) writes a sidecar only where metadata application failed |       |
| -     | --no-sidecar     | n        | 0    | Alias for `--sidecar never`: never write sidecar manifests                                                                                      |       |
| -     | --hidden-sidecar | n        | 0    | Prefix sidecar names with a leading dot (`{name}.json` → `.{name}.json`)                                                                        |       |

**Filtering**

| Short | Long           | Required | Args | Description                                                                   | Notes |
|-------|----------------|----------|------|-------------------------------------------------------------------------------|-------|
| -     | --exclude      | n        | 1+   | Exclude paths matching regex PATTERN from extraction (repeatable)             |       |
| -X    | --exclude-from | n        | 1+   | Read exclude regex patterns from FILE (repeatable)                            |       |
| -     | --include      | n        | 1+   | Include only paths matching regex PATTERN for extraction (repeatable)         |       |
| -     | --include-from | n        | 1+   | Read include regex patterns from FILE (repeatable)                            |       |
| -     | --anchored     | n        | 0    | Patterns match from the start of the path (default: unanchored)               |       |
| -     | --ignore-case  | n        | 0    | Match patterns case-insensitively (default: case-sensitive)                   |       |
| -     | --no-unicode   | n        | 0    | Compile filter regexes in bytes mode (ASCII-only classes, `.` matches a byte) |       |
| -     | --lazy-filter  | n        | 0    | Apply include/exclude filters after a later phase (scan) instead of eagerly   |       |

**Sparse Files**

| Short | Long        | Required | Args | Description                                                                                                                   | Notes |
|-------|-------------|----------|------|-------------------------------------------------------------------------------------------------------------------------------|-------|
| -     | --sparsify  | n        | 0    | Re-create holes in the extract cache while unpacking (sparse writer over a plain copy; uses `--page-size` for hole detection) |       |
| -     | --page-size | n        | 1    | Page size in bytes for sparse-hole detection while unpacking                                                                  |       |

### resume

Continue incomplete work. Policy comes from the work DB; only `--jobs`, `--io-jobs` and `--exit-after-stage` may be overridden.

**Options**

| Short | Long               | Required | Args | Description                                                                                   | Notes |
|-------|--------------------|----------|------|-----------------------------------------------------------------------------------------------|-------|
| -     | --work-dir         | n        | 1    | Work directory (`.astage` / `.estage`) of the interrupted run                                 |       |
| -     | --db               | n        | 1    | Path to the work database (`tar-dedup.sqlite`) directly; mutually exclusive with `--work-dir` |       |
| -     | --jobs             | n        | 1    | Maximum concurrent workers (rayon pools and xz threads)                                       |       |
| -     | --io-jobs          | n        | 1    | Maximum concurrent io-bound workers (dedup, sparsify, place pools)                            |       |
| -     | --exit-after-stage | n        | 1    | Run through STAGE then exit cleanly (state saved)                                             |       |

### inspect

Show archive metadata needed to plan an extraction (catalog, policies).

**Archive Target**

| Short | Long       | Required | Args | Description                                                                                                       | Notes |
|-------|------------|----------|------|-------------------------------------------------------------------------------------------------------------------|-------|
| -f    | -          | n        | 1    | Archive path; the catalog is read from its seekable sqlite footer, else from the leading `manifest.sqlite` member |       |
| -     | --db       | n        | 1    | Read the catalog from an existing work database file directly                                                     |       |
| -     | --work-dir | n        | 1    | Work directory (`.astage` / `.estage`) containing `tar-dedup.sqlite`                                              |       |

**Output**

| Short | Long        | Required | Args | Description                                                                                                    | Notes |
|-------|-------------|----------|------|----------------------------------------------------------------------------------------------------------------|-------|
| -     | --json      | n        | 0    | Emit a single machine-readable JSON document instead of text sections                                          |       |
| -     | --no-pretty | n        | 0    | Compact JSON output (no wrapping/indent); only meaningful with `--json`                                        |       |
| -     | --collapse  | n        | 0    | Text mode: join records within a section on one line (`; `) instead of one per line                            |       |
| -     | --only      | n        | 1    | Text mode: emit only the section(s) whose header contains SUBSTR (case-insensitive), e.g. `--only "owner map"` |       |
| -     | --force     | n        | 0    | Prefer the last `snapshot.sqlite` member over the footer / leading manifest (not implemented yet)              |       |

### list

List file-tree members (rows from the `files` table).

**Archive Target**

| Short | Long       | Required | Args | Description                                                                                                       | Notes |
|-------|------------|----------|------|-------------------------------------------------------------------------------------------------------------------|-------|
| -f    | -          | n        | 1    | Archive path; the catalog is read from its seekable sqlite footer, else from the leading `manifest.sqlite` member |       |
| -     | --db       | n        | 1    | Read the catalog from an existing work database file directly                                                     |       |
| -     | --work-dir | n        | 1    | Work directory (`.astage` / `.estage`) containing `tar-dedup.sqlite`                                              |       |

**Output**

| Short | Long        | Required | Args | Description                                                                                                    | Notes |
|-------|-------------|----------|------|----------------------------------------------------------------------------------------------------------------|-------|
| -     | --json      | n        | 0    | Emit a single machine-readable JSON document instead of text sections                                          |       |
| -     | --no-pretty | n        | 0    | Compact JSON output (no wrapping/indent); only meaningful with `--json`                                        |       |
| -     | --collapse  | n        | 0    | Text mode: join records within a section on one line (`; `) instead of one per line                            |       |
| -     | --only      | n        | 1    | Text mode: emit only the section(s) whose header contains SUBSTR (case-insensitive), e.g. `--only "owner map"` |       |
| -     | --force     | n        | 0    | Prefer the last `snapshot.sqlite` member over the footer / leading manifest (not implemented yet)              |       |

### dump

Machine-formatted (csv/json) dump of the file-tree members.

**Archive Target**

| Short | Long       | Required | Args | Description                                                                                                       | Notes |
|-------|------------|----------|------|-------------------------------------------------------------------------------------------------------------------|-------|
| -f    | -          | n        | 1    | Archive path; the catalog is read from its seekable sqlite footer, else from the leading `manifest.sqlite` member |       |
| -     | --db       | n        | 1    | Read the catalog from an existing work database file directly                                                     |       |
| -     | --work-dir | n        | 1    | Work directory (`.astage` / `.estage`) containing `tar-dedup.sqlite`                                              |       |

**Output**

| Short | Long        | Required | Args | Description                                                                                                    | Notes |
|-------|-------------|----------|------|----------------------------------------------------------------------------------------------------------------|-------|
| -     | --json      | n        | 0    | Emit a single machine-readable JSON document instead of text sections                                          |       |
| -     | --no-pretty | n        | 0    | Compact JSON output (no wrapping/indent); only meaningful with `--json`                                        |       |
| -     | --collapse  | n        | 0    | Text mode: join records within a section on one line (`; `) instead of one per line                            |       |
| -     | --only      | n        | 1    | Text mode: emit only the section(s) whose header contains SUBSTR (case-insensitive), e.g. `--only "owner map"` |       |
| -     | --force     | n        | 0    | Prefer the last `snapshot.sqlite` member over the footer / leading manifest (not implemented yet)              |       |

### query

Search paths / ids across catalog tables.

**Archive Target**

| Short | Long       | Required | Args | Description                                                                                                       | Notes |
|-------|------------|----------|------|-------------------------------------------------------------------------------------------------------------------|-------|
| -f    | -          | n        | 1    | Archive path; the catalog is read from its seekable sqlite footer, else from the leading `manifest.sqlite` member |       |
| -     | --db       | n        | 1    | Read the catalog from an existing work database file directly                                                     |       |
| -     | --work-dir | n        | 1    | Work directory (`.astage` / `.estage`) containing `tar-dedup.sqlite`                                              |       |

**Output**

| Short | Long        | Required | Args | Description                                                                                                    | Notes |
|-------|-------------|----------|------|----------------------------------------------------------------------------------------------------------------|-------|
| -     | --json      | n        | 0    | Emit a single machine-readable JSON document instead of text sections                                          |       |
| -     | --no-pretty | n        | 0    | Compact JSON output (no wrapping/indent); only meaningful with `--json`                                        |       |
| -     | --collapse  | n        | 0    | Text mode: join records within a section on one line (`; `) instead of one per line                            |       |
| -     | --only      | n        | 1    | Text mode: emit only the section(s) whose header contains SUBSTR (case-insensitive), e.g. `--only "owner map"` |       |
| -     | --force     | n        | 0    | Prefer the last `snapshot.sqlite` member over the footer / leading manifest (not implemented yet)              |       |

### reset

Reset an interrupted run: re-point its stored parameters and rewind the pipeline to a consistent state. The work database is named explicitly.

**Options**

| Short | Long       | Required | Args | Description                                                                   | Notes |
|-------|------------|----------|------|-------------------------------------------------------------------------------|-------|
| -     | --db       | n        | 1    | Path to the work database (`tar-dedup.sqlite`) directly                       |       |
| -     | --work-dir | n        | 1    | Work directory of the run (`.astage` / `.estage`), holding `tar-dedup.sqlite` |       |

### check

Verify the integrity of an archive via its footer trailer: the sha1 of the finished compressed
archive content and the sha1 of the stored compressed catalog (xz) are recomputed and compared
against the values recorded in the footer. Exits non-zero when no footer is present, the footer
format is invalid, or a hash does not match.

**Options**

| Short | Long    | Required | Args | Description                                                              | Notes |
|-------|---------|----------|------|--------------------------------------------------------------------------|-------|
| -f    | -       | y        | 1    | Archive path whose footer integrity is verified (relative paths use cwd) |       |
| -q    | --quiet | n        | 0    | Suppress the verdict message and the progress bar; exit code only        |       |

## Motivation
This tool is written to improve the compression of unstructured data. A good example would be a hard drive of a desktop. 
Most humans don't tend to have perfect order across all their systems. An intersection between Desktop, Downloads, 
Photos, Videos and Documents is to be expected. Sometimes duplication is even desired. Manually sorting data to save 
space prior to archival is not feasible. So such datasets are compressed _as is_ and losses in compression efficiency 
are accepted. 

This tool aims to enhance the use case of unstructured cold data, that needs to be stored as efficiently as possible 
for long periods of time. It assumes that delete, modify and append operations won't be performed on the archive 
(thus no implementation for it exists) and that either a subset or the full dataset itself are unarchived. To compress 
the data as best as possible, we rely on tar and xz as the base. We explcitly do not use zip archives since the per-file 
compression as well as manifest make the files less compact than tar archives. This optimization is further motivated 
by our omission of delete, modify, append. As our assumption is, that this data is cold, and we aim to store it as 
efficiently as possibe, the work is very asymetrically distributed. The act of creating an archive has a very high 
up-front cost with 3 / 4 reads per file as the lower bound, with additional scans happening as a consequence of 
deduplication, <1 to 2 writes per file (depending on if the sparse materialization pass happens or not) and 
compression results. On the other hand, decompression is fairly easy with a single scan of the archive, 
a single write of the uncompressed archive size or a single write of the original tree size depending on the 
restoration mode. 

## Disclaimers

### THIS IS UNSTABLE CODE!!!  
Any archives created with this tool prior to the v1.0.0 release have **no guarantees** that future commits will still 
decompress them correctly and successfully! If you need this tool done soon, create an issue or star the project to 
show interest.


### Compression Algorithms
The current offering of compression algorithms is due to both the availability of rust crates for the algorithms and
also a thought of recentness of the algorithm. The program will default to the most aggressive compression possible
(xz, cpu_count threads, extreme compression profile and no regard for RAM consumption) if the options of the command
exposes du not suite your needs, the command offers to write to a simple tar file you can compress yourself
(and also subsequently decompress if the format is not supported.)

As the tool is using rust, it expects all its inputs to be in valid utf-8. // TODO further enhancement

### Divergence from the `tar` command

#### File Filtering 
The tool also sacrifices functionality the regular tar command offers. Since this tool is not intended to be backwards 
compatible the decision was made to only support a subset of the tar command options. We tried to retain a manageable 
overhead and an easy interface (e.g. `--exclude`, `--exclude-from`, `--files-from` are the main options supported from 
tar. However, the exclude options expect full regex to match paths against and `--files-from` is the alternative 
approach to exclude and supports \n or \0 terminated lines. Paths are expected to be unescaped.) Additionally, the
inverse can also be done `--include` and `--include-from` allow you to whitelist rather than black list. Defaults are 
treated as follows: no inclusion => everything is included, no exclusion => nothing is excluded. The tool has no rule 
order. The list of all members of the given directories and paths are scanned. A subset is then selected based on the 
include filters. Everything selected by the include filters is then passed through the exclude filters which then strip 
the remainder of entries before the hash phase begins. 

Filtering is not recursive. The filters are applied to each element separately. This is a consequence how the tool 
optimizes path traversal and allocation of files (we want to minimize the archive size after all). All regex for filters 
are matched against each absolute path recorded in during the inventory stage.

// Todo entry vs path restoration.path
#### Directory scanning.
By default, tar-dedup expects to scan disjoint trees (consequence of the mapping and )

### Security    
As this tool is meant to produce a coherent and complete picture of a file tree [...] for archival purposes, the security concerns 
are higher for this tool compared to the regular `tar` command! The sqlite manifest stores absolute paths to all files 
archived and the full metadata of files is also stored in plain format. Please use caution when using this tool to 
produce archives ment to be publicly accessible.

## Development

Features:
- [ ] For debugging purposes, the `force-utf8-encoding` can be used for better readability of the database. However, 
**if at any point a non-utf8 compliant byte is found, the binary will panic**. 

Compile hints on Debian:  
`sudo apt install libselinux-dev libclang-dev clang`

### Returned Archive
Note on the returned file. The file is a valid archive with an appended footer. This looks like this:
```
[ compressed(tar archive) | MAGIC | sha1 of archive content | xz(-9e) of final-database.sqlite | sha1 of xz blob | MAGIC | u64 offset ]
```
- `MAGIC` is `"Tar-Dedup-SQLite-Footer"` - to make finding the footer by hand analyzing the file with a program like `less`.
- `sha1 of archive content` is the hash of the finished compressed archive (the bytes before the footer) to detect corruption of the stream. It is verified once when the footer catalog is first installed during extraction; under `--force` a mismatch is downgraded to a warning.
- `xz(-9e) of final-database.sqlite` is the finalized work database compressed with a fixed `xz -9 -e` preset (independent of tar stream compression). It contains the finished session info and allows tar-dedup to instantly have the manifest in the correct state rather than having to apply one snapshot after the other.
- `sha1 of xz blob` is the hash of the compressed footer payload (not the raw sqlite) to check for corruption.
- `u64 offset` is the offset of the first `MAGIC` string relative to the beginning of the file

Archives with the previous raw-sqlite footer format are not supported.

The returned file can be opened by associated compressor or the tar archive. The footer will most likely be ignored 
by the compression algorithm and the tar stream and the contents can be viewed and used as is. A warning is also 
possible.


### Archive Content
The tool expects to encounter canonical files or databases. It will refuse to proceed, if it encounters any file that 
does not match the established contract.

#### Canonical Files
The tool saves storage by only storing one copy per file cluster. These unique or canonical files have special naming 
pattern to ensure they can be correctly mapped to the database and the metadata.
```
{hash_b64}.{fsize_b64}.{fid_b64}.ext
```
All values are encoded using base64 with an url-safe alphabet.

#### Databases
Besides the files themselves there are at least two but possibly more databases inside the archive. The first file in 
the archive should always be `manifest.sqlite`. This database is used during decompression to keep track of the files 
seen in the archive. Every time the archive process was stopped (regardless whether it was because there were no more 
canonical files to add or because the user sent a graceful interrupt) a `snapshot.sqlite` is added to the database. 
This `snapshot.sqlite` confirms to the extractor that, up to here, the files marked as `Archived` inside the database
were added to the archive. Since a rescan of the archive / the removal of the last appended `snapshot.sqlite` is not
efficiently possible, the tool opts to just add another `snapshot.sqlite` when it halts the next time. This design 
choice was intentional for data security but with frequent stops and starts of the tool, this will lead to bloat. 
`snapshot.sqlite` databases have no positional requirements. However, an archive with `manifest.sqlite` not in first 
position are considered invalid and will not be processed.