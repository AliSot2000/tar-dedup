# Tar-Dedup
A `tar` "wrapper" to deduplicate the file-tree prior to archiving to save extra space. It capability to interrupt the 
creation of large tar archives at the cost of lower compression ratios for compressed archives.

## Disclaimers

### THIS IS UNSTABLE CODE!!!
Any archives created with this tool prior to the v1.0.0 release have **no guarantees** that future commits will still
decompress them correctly and successfully! If you need this tool done soon, create an issue or star the project to
show interest.

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

| Short | Long                    | Required | Args | Description                                                                       | Notes |
|-------|-------------------------|----------|------|-----------------------------------------------------------------------------------|-------|
| -     | --no-recursion          | n        | 0    | Do not descend into directories                                                   |       |
| -     | --dereference           | n        | 0    | Follow symlinks; archive the files they point to (GNU tar `-h`)                   |       |
| -     | --one-file-system       | n        | 0    | Stay on one filesystem when walking input trees                                   |       |
| -     | --no-hardlink-detection | n        | 0    | Do not coalesce same (inode, device) hard links in hash/dedup                     | 4.    |
| -     | --no-strict-separation  | n        | 0    | Allow nested or duplicate `-i` / `-T` directory roots (still recorded and walked) |       |

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
implemented is the `--newer` ... filters. They are on the feature list but for the moment, this 
can be remedied by using `filter` in conjunction with `--files-from` to achieve the same filtering.

A single file is treated as follows: 
- The absolute path is matched first against all provided include filters. The first filter that matches, marks the 
  path as included and promotes it to the next stage. If no include filter is provided, a catch-all filter is used.
- If the absolute path passed the include section, the absolute path is then matched against all provided exclude 
  filters. If none match, the file is considered to be included in the set of paths for the tool to process. 

| Short | Long                  | Required | Args | Description                                                                                                                               | Notes |
|-------|-----------------------|----------|------|-------------------------------------------------------------------------------------------------------------------------------------------|-------|
| -     | --exclude             | n        | 1+   | Exclude paths matching regex PATTERN (repeatable)                                                                                         | 5.    |
| -X    | --exclude-from        | n        | 1+   | Read exclude regex patterns from FILE (repeatable)                                                                                        | 5.    |
| -     | --include             | n        | 1+   | Include only paths matching regex PATTERN (repeatable)                                                                                    | 5.    |
| -     | --include-from        | n        | 1+   | Read include regex patterns from FILE (repeatable)                                                                                        | 5.    |
| -     | --anchored            | n        | 0    | Patterns are matched against the absolute (raw on-disk) path, never a relative one. (default: unanchored)                                 |       |
| -     | --ignore-case         | n        | 0    | Case-insensitive pattern matching. (default: Case-sensitive)                                                                              |       |
| -     | --no-unicode          | n        | 0    | Compile filter regexes in bytes mode: `.` matches a byte, `\d \w \s` and `\b` become ASCII-only, and `\p{…}` Unicode classes are rejected |       |
| -     | --exclude-vcs         | n        | 0    | Exclude version control system directories (not implemented yet)                                                                          | 5.    |
| -     | --exclude-vcs-ignores | n        | 0    | Read VCS ignore files for exclusions (not implemented yet)                                                                                | 5.    |
| -     | --lazy-filter         | n        | 0    | Apply include/exclude filters after the hash phase                                                                                        |       |

**Dedup**  
The deduplication strategy and the optional content-defined sharding used to cluster unique payloads. 
Sharding is not implemented yet.

| Short | Long             | Required | Args | Description                                                                     | Notes |
|-------|------------------|----------|------|---------------------------------------------------------------------------------|-------|
| -     | --dedup-mode     | n        | 1    | Deduplication strategy                                                          |       |
| -     | --use-shards     | n        | 0    | Use content-defined chunking instead of whole-file hashes (not implemented yet) |       |
| -     | --max-shard-size | n        | 1    | Maximum shard size for content-defined chunking (requires `--use-shards`)       |       |

**File Attributes**
As a baseline, the tool attempts to capture as much data as possible. This includes (ctime, atime, mtime, uid, gid, 
mode, size, selinux policy, posix_acls, xattrs, inode type, link destination and link type too for symlinks. By 
default, the tool also attempts to resolve any uid, gid to a group name / owner name via the system database. 

Global default: By default, everything is turned on and can be turned off. This behavior can be inverted with 
`--no-capture-all-metadata`. That way, the default becomes off and needs to be selectively re-enabled with e.g. 
`--acls`.

| Short | Long                      | Required | Args | Description                                                                                                                       | Notes |
|-------|---------------------------|----------|------|-----------------------------------------------------------------------------------------------------------------------------------|-------|
| -     | --acls                    | n        | 0    | Capture POSIX ACLs (default: on via the capture umbrella)                                                                         |       |
| -     | --no-acls                 | n        | 0    | Do not capture POSIX ACLs                                                                                                         |       |
| -     | --xattrs                  | n        | 0    | Capture extended attributes (default: on via the capture umbrella)                                                                |       |
| -     | --no-xattrs               | n        | 0    | Do not capture extended attributes                                                                                                |       |
| -     | --selinux                 | n        | 0    | Capture SELinux contexts (default: on via the capture umbrella)                                                                   |       |
| -     | --no-selinux              | n        | 0    | Do not capture SELinux contexts                                                                                                   |       |
| -     | --no-capture-all-metadata | n        | 0    | Default: capture every metadata attribute (xattrs, ACLs, SELinux, ...). The `--no-*` toggles override this on a per-bit basis     |       |
| -     | --numeric-ids-only        | n        | 0    | Record numeric uids/gids instead of resolving user/group names during inventory                                                   |       |
| -     | --resolve-numeric-ids     | n        | 0    | Resolve user/group names during inventory (default; counters `--numeric-ids-only`)                                                |       |
| -     | --owner                   | n        | 1    | Force owner for archived members: `NAME`, `UID`, or `NAME:UID` (GNU tar)                                                          | 6.    |
| -     | --owner-map               | n        | 1    | Owner translation map file (GNU tar `--owner-map`)                                                                                | 6.    |
| -     | --group                   | n        | 1    | Force group for archived members: `NAME`, `GID`, or `NAME:GID` (GNU tar)                                                          | 6.    |
| -     | --group-map               | n        | 1    | Group translation map file (GNU tar `--group-map`)                                                                                | 6.    |
| -     | --mode                    | n        | 1    | Force symbolic mode CHANGES for added members (GNU tar `--mode`). Applies chmod-style updates (e.g. `u+rwx,go-rx`) to the entried | 6.    |
| -     | --transform               | n        | 1    | Record a sed-style name transform (GNU tar `--transform`/`--xform`, e.g. `s,^usr/,var/,`) to apply to paths                       | 6.    |

#### Sparse Files
The system relies on the tar reader detecting sparse files and encoding them according to the tar format. As the tar 
builder from the rust tar crate cannot create sparse files inflight, the tool first creates valid sparse files in the 
`--work-dir` such that the tar builder can correctly detect their sparseness when it reads them back. Notes:
- This sparse copy step will only apply to files which are detected to be sparse during the inventory pass, not all of 
  them. Detection happens via the `--page-size` and `--min-pages` options.
- If a compression algorithm is used, it is recommended to rely on that algorithm to deal with sparseness rather than 
  the tool's `sparsify` stage as it will produce the archive in less time.
- Since the sparse copy relies on the underlying filesystem being able to represent sparse files, the sparsification 
  via sparse copy will not work on a file system that is not able to represent sparse files.

| Short | Long        | Required | Args | Description                                                                                                         | Notes |
|-------|-------------|----------|------|---------------------------------------------------------------------------------------------------------------------|-------|
| -     | --sparsify  | n        | 0    | Run the sparsify phase (copy with seeks using `--page-size` increments)                                             |       |
| -     | --page-size | n        | 1    | Page size in bytes: segment a file into pages of this size and search for completely empty pages for sparsification |       |
| -     | --min-pages | n        | 1    | Minimum empty-page count before sparsify treats a file as worth rewriting in sparse                                 |       |

**Process Options**  

| Short | Long               | Required | Args | Description                                                                                                                                              | Notes |
|-------|--------------------|----------|------|----------------------------------------------------------------------------------------------------------------------------------------------------------|-------|
| -     | --jobs             | n        | 1    | Maximum concurrent compute bound workers                                                                                                                 |       |
| -     | --io-jobs          | n        | 1    | Maximum concurrent io-bound workers                                                                                                                      |       |
| -     | --fresh            | n        | 0    | Wipe existing work and archive, then start from inventory                                                                                                |       |
| -     | --db-retention     | n        | 1    | When to keep a timestamped copy of the work database next to the archive. `always`: always; `on-error`: only if the run recorded errors; `never`: delete |       |
| -     | --keep-stage       | n        | 0    | After success, keep the `{stem}.astage` work directory                                                                                                   |       |
| -     | --exit-after-stage | n        | 1    | Run through STAGE then exit gracefully                                                                                                                   |       |
| -     | --fail-fast        | n        | 0    | Abort the run on the first hard failure instead of continuing according to the best-effort principle where possible                                      |       |
| -     | --no-errors        | n        | 0    | Do not record errors encountered while processing the entries in the database. Downgrades a db-retention of `on-error` to `never`                        |       |
| -     | --ordering         | n        | 1    | Ordering used in which the files are added to the archive to improve compression                                                                         |       |
| -     | --relax-utf8       | n        | 0    | Relax utf-8 policy to also process entries without a valid utf-8 path                                                                                    | 7.    |

**Logging**  

| Short | Long            | Required | Args | Description                                                    | Notes |
|-------|-----------------|----------|------|----------------------------------------------------------------|-------|
| -     | --progress-mode | n        | 1    | Which progress bars to draw                                    |       |
| -     | --log-level     | n        | 1    | Verbosity of the log output (overrides the `RUST_LOG` env var) |       |

**Batching**  
Options below tune the internal buffering and transaction batching. Sensible defaults should be set. Changes can 
negatively impact tool performance by increasing the number of transactions in the database, and can use more RAM, 
leading to worse system performance.

| Short | Long               | Required | Args | Description                                                          | Notes |
|-------|--------------------|----------|------|----------------------------------------------------------------------|-------|
| -     | --auto-flush-limit | n        | 1    | Number of error drafts buffered before an auto-flush to the database |       |
| -     | --batch-size       | n        | 1    | Rows pulled from the database per batch iteration                    |       |
| -     | --work-capacity    | n        | 1    | Max items in the producer/consumer work channel                      |       |
| -     | --out-capacity     | n        | 1    | Max items in the producer/consumer result channel                    |       |
| -     | --feed-chunk       | n        | 1    | Rows pulled from the database per feed round                         |       |
| -     | --drain-chunk      | n        | 1    | Outcomes committed per transaction                                   |       |

**Notes**
1. Path resolution: If the path is absolute, the path is taken as is. If the path is relative and `-C --directory` is not set, 
   the current process working directory is taken from the environment and used as the root for the relative paths. If
   `-C --directory` is given, relative paths are resolved with this as a root instead.
2. At least one input must be given. So either one `-T` or `-i`. Multiples are supported. (so multiple `-i` and `-T`)
3. If no compression is given, the compression is inferred by the extension. E.g. `.tar.xz` maps to the xz compression. 
   An uncompressed archive can be produced by simply supplying `.tar` as the file extension.
4. Disabling the hardlink_detection can increase the runtime of the dedup stage because the tool will compare inode 
   with itself `n-1` times (i.e. if it was recorded `n` times.)
5. Filtering always uses PCRE-style regex (Rust regex crate; no backreferences or look-around). No Glob or 
   Shell expansions are supported
6. The database will store the overrides separately from the file metadata. During extraction, the overrides will be applied 
   rather than on archive. Before the overrides are stored in the database, the tool will parse them and error out, if they are invalid.
7. All utf-8 conform paths will be processed regardless. For None utf-8 paths, the `--relax-utf8` option takes 
   priority. If given, any path without valid utf-8 is converted to utf-8, replacing any non-utf8 characters with �. 
   The original path is stored in the database for reference but is currently not treated further. If without 
   `--relax-utf8` the treatment of paths depends on `--force`. Without `--force` the path is skipped with a warning 
   and not recorded anywhere. With `--force` any non-utf8 path leads to an exit.

### extract

Restore a tar-dedup archive (materialize: full copy at each original path). Aborts if extract work already exists; use `--fresh` to replace.

**Archive Paths**

The extraction has four important locations:
1. The archive: The file that is intended to be extracted.
2. The work directory: Locations of temporary files, database, lock files, extraction cache (temporarily consume as 
   much space as the .tar archive itself.)
3. The extraction directory: The directory where the captured file tree is rebuilt during the extraction
4. The source directory: When building a link tree, this directory contains the actual files the links reference. 

The archive is the only mandatory input. Everything else has defaults.

| Short | Long                | Required | Args | Description                                                                                                                                                                           | Notes |
|-------|---------------------|----------|------|---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|-------|
| -f    | -                   | y        | 1    | Archive path (relative paths use `-C` / current directory)                                                                                                                            | 1.    |
| -C    | --directory         | n        | 1    | Treat DIR as the working directory when resolving relative paths on this command line (`-f`, `--work-dir`, …). Absolute paths are unchanged. Default: process current directory       |       |
| -     | --extract-to        | n        | 1    | Extract members into DIR (the restore root); default: `-C` / current dir                                                                                                              | 1.    |
| -     | --work-dir          | n        | 1    | Stage directory for sqlite DB, cached payloads, and locks (defaults to `{stem}.estage` next to the archive). Relative paths use `-C` / cwd. Overrides `--work-dir-location`           | 1.    |
| -     | --work-dir-location | n        | 1    | Where `{stem}.estage` is placed when `--work-dir` is not given: `beside-archive` (default) or `beside-output` (next to the extraction root)                                           |       |
| -P    | --absolute          | n        | 0    | Restore using absolute catalog paths (`-P`). Extraction join abs paths under the extraction root (leading `/` stripped). To extract to file system root, pass `/` as `-C`             | 1. 2. |
| -     | --one-top-level     | n        | 0..1 | Extract loose members into a subdirectory under the extraction root (GNU tar `--one-top-level`). A bare `--one-top-level` uses the archive name as the subdirectory; DIR overrides it | 2.    |
| -     | --clean-target      | n        | 0    | Remove the extraction root before extracting (only effective with `--one-top-level`)                                                                                                  |       |

**Compression**

By default, the file extension is used to infer the compression if any. These options will override the inference.

| Short | Long    | Required | Args | Description                                                      | Notes |
|-------|---------|----------|------|------------------------------------------------------------------|-------|
| -z    | --gzip  | n        | 0    | Filter the archive through gzip (overrides extension inference)  |       |
| -j    | --bzip2 | n        | 0    | Filter the archive through bzip2 (overrides extension inference) |       |
| -J    | --xz    | n        | 0    | Filter the archive through xz (overrides extension inference)    |       |
| -     | --zstd  | n        | 0    | Filter the archive through zstd (overrides extension inference)  |       |

**Rehash**

After the files are extracted, they are hashed again to check if any data corruption has taken place.

| Short | Long        | Required | Args | Description                                                     | Notes |
|-------|-------------|----------|------|-----------------------------------------------------------------|-------|
| -     | --no-rehash | n        | 0    | Do not run the rehash (content verification) phase. Default: on |       |

**Overwrite Control**

| Short | Long                     | Required | Args | Description                                                                                                                                                                                                        | Notes |
|-------|--------------------------|----------|------|--------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|-------|
| -     | --keep-directory-symlink | n        | 0    | Preserve existing directory symlinks instead of replacing them (GNU tar `--keep-directory-symlink`)                                                                                                                |       |
| -U    | --unlink-first           | n        | 0    | Remove each existing file system entry (directory entry) before extracting over it (GNU tar `-U` / `--unlink-first`)                                                                                               |       |
| -     | --no-create-dirs         | n        | 0    | Skip the directory tree create pass. Results in the members without a parent being skipped. Results in error with --fail-fast                                                                                      |       |
| -     | --no-overwrite-dir       | n        | 0    | Do not apply archived metadata to existing directories (GNU tar `--no-overwrite-dir`). Default: apply directory metadata for placed members, matching GNU tar at the end of the pipeline                           |       |
| -     | --overwrite-all-dir      | n        | 0    | Also apply archived directory metadata to directories excluded by filters (normally only placed members are updated). `--no-overwrite-dir` takes precedence: with both flags set, no directory metadata is applied |       |
| -     | --conflict-policy        | n        | 1    | How to handle paths that already exist on disk                                                                                                                                                                     |       |
| -     | --silent-conflicts       | n        | 0    | Do not log any messages when a path encountered a conflict                                                                                                                                                         |       |
| -     | --remove-and-replace     | n        | 0    | On type mismatch at a path, remove the existing entry and extract the archive member. Skip by default                                                                                                              |       |
| -     | --hard-link-grouping     | n        | 1    | Adjust how hardlinks are extracted                                                                                                                                                                                 | 3.    |

**Link Tree**

| Short | Long             | Required | Args | Description                                                                                                                                      | Notes |
|-------|------------------|----------|------|--------------------------------------------------------------------------------------------------------------------------------------------------|-------|
| -     | --link-tree      | n        | 0    | Build the restored tree from symlinks/hard links into the extract stage instead of copying payloads. Payloads moved to `--link-source` directory |       |
| -     | --hard-links     | n        | 0    | With `--link-tree`, use hard links instead of symlinks                                                                                           |       |
| -     | --absolute-links | n        | 0    | With `--link-tree`, link using absolute paths instead of paths relative to the output root                                                       |       |
| -     | --link-source    | n        | 1    | With `--link-tree`, use DIR as the directory the canonical payloads are materialized into before linking (default: {extract-to}/.sources)        |       |

**Placement**

| Short | Long                   | Required | Args | Description                                                                                                                                 | Notes |
|-------|------------------------|----------|------|---------------------------------------------------------------------------------------------------------------------------------------------|-------|
| -     | --no-reflink           | n        | 0    | Do not attempt copy-on-write (reflink) copies when placing payloads                                                                         |       |
| -     | --create-other-entries | n        | 0    | Recreate non-regular entries (symlink, fifo, char/block device, socket). Off by default: those entries require `mknod` privileges or `root` |       |

**File Attributes**

File Attributers also have on/off flags. Providing both at the same time produces an error. Default is everything off. 
The default can be swapped to on with `--apply-all-metadata`. Then every `--no-*` override can be used to disable 
selected attributes.

| Short | Long                        | Required | Args | Description                                                                                                                                                                                                                                                                               | Notes |
|-------|-----------------------------|----------|------|-------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|-------|
| -     | --same-owner                | n        | 0    | Restore archived uid/gid when possible (GNU tar `--same-owner`; may require root). Default: inferred from the effective uid of the extracting process                                                                                                                                     |       |
| -     | --no-same-owner             | n        | 0    | Do not restore archived ownership; files are owned by the extracting process. Default: inferred from the effective uid of the extracting process                                                                                                                                          |       |
| -     | --map-target                | n        | 1    | How to emit the resolved owner/group: `ids`, `names`, `name-id`, or `id-name` (default `name-id`)                                                                                                                                                                                         |       |
| -     | --validate-maps             | n        | 0    | Fail hard on map/override destinations that the chosen --map-target cannot emit. (Missing values, missing owner/group name, ...)                                                                                                                                                          |       |
| -     | --owner                     | n        | 1    | Force owner for restored members: `NAME`, `UID`, or `NAME:UID`. Identity is all-or-nothing: giving any of `--owner`/`--group`/`--owner-map`/ `--group-map` uses the CLI for **both** identities and disables `--apply-stored-owner-map` / `--apply-stored-group-map` (no CLI/archive mix) | 4.    |
| -     | --owner-map                 | n        | 1    | Owner translation map file (GNU tar `--owner-map`). See `--owner` for the all-or-nothing identity rule                                                                                                                                                                                    | 4.    |
| -     | --group                     | n        | 1    | Force group for restored members: `NAME`, `GID`, or `NAME:GID`. See `--owner` for the all-or-nothing identity rule                                                                                                                                                                        | 4.    |
| -     | --group-map                 | n        | 1    | Group translation map file (GNU tar `--group-map`). See `--owner` for the all-or-nothing identity rule                                                                                                                                                                                    | 4.    |
| -     | --apply-stored-owner-map    | n        | 0    | Apply the owner map recorded in the archive (alongside `--apply-stored-group-map`, the whole archived identity). Ignored if any of `--owner`/`--group`/ `--owner-map`/`--group-map` is given: identity comes entirely from the command line or entirely from the archive — never a mix    | 4.    |
| -     | --no-apply-stored-owner-map | n        | 0    | Do not apply the owner map recorded in the archive                                                                                                                                                                                                                                        | 4.    |
| -     | --apply-stored-group-map    | n        | 0    | Apply the group map recorded in the archive (alongside `--apply-stored-owner-map`, the whole archived identity). Ignored if any of `--owner`/`--group`/ `--owner-map`/`--group-map` is given: identity comes entirely from the command line or entirely from the archive — never a mix    | 4.    |
| -     | --no-apply-stored-group-map | n        | 0    | Do not apply the group map recorded in the archive                                                                                                                                                                                                                                        | 4.    |
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
| -     | --jobs             | n        | 1    | Maximum concurrent compute bound workers                                                                                                                         |       |
| -     | --io-jobs          | n        | 1    | Maximum concurrent io-bound workers                                                                                                                              |       |
| -     | --fresh            | n        | 0    | Wipe extract work (`.estage`) and start over; does not reset the placement progress                                                                              |       |
| -     | --force            | n        | 0    | Continue even if the tool cannot guarantee the integrity of the data                                                                                             |       |
| -     | --relax-utf8       | n        | 0    | Relax utf-8 policy to also process entries without a valid utf-8 path                                                                                            |       |
| -     | --fail-fast        | n        | 0    | Abort the run on the first hard failure instead of continuing according to the best-effort principle where possible                                              |       |
| -     | --no-errors        | n        | 0    | Do not record errors encountered while processing the entries in the database. Downgrades a db-retention of `on-error` to `never`                                |       |
| -     | --db-retention     | n        | 1    | When to keep a timestamped copy of the work database (under the extraction root). `always`: always; `on-error`: only if the run recorded errors; `never`: delete |       |
| -     | --keep-stage       | n        | 0    | After success, keep the `{stem}.estage` work directory                                                                                                           |       |
| -     | --exit-after-stage | n        | 1    | Run through STAGE then exit gracefully                                                                                                                           |       |

**Logging**

| Short | Long            | Required | Args | Description                                                    | Notes |
|-------|-----------------|----------|------|----------------------------------------------------------------|-------|
| -     | --progress-mode | n        | 1    | Which progress bars to draw                                    |       |
| -     | --log-level     | n        | 1    | Verbosity of the log output (overrides the `RUST_LOG` env var) |       |

**Batching**  
Options below tune the internal buffering and transaction batching. Sensible defaults should be set. Changes can 
negatively impact tool performance by increasing the number of transactions in the database, and can use more RAM, 
leading to worse system performance.

| Short | Long               | Required | Args | Description                                                          | Notes |
|-------|--------------------|----------|------|----------------------------------------------------------------------|-------|
| -     | --auto-flush-limit | n        | 1    | Number of error drafts buffered before an auto-flush to the database |       |
| -     | --batch-size       | n        | 1    | Rows pulled from the database per phase iteration                    |       |
| -     | --work-capacity    | n        | 1    | Items in the producer/consumer work channel                          |       |
| -     | --out-capacity     | n        | 1    | Items in the producer/consumer result channel                        |       |
| -     | --feed-chunk       | n        | 1    | Rows pulled from the database per feed round                         |       |
| -     | --drain-chunk      | n        | 1    | Outcomes committed per transaction                                   |       |

**Sidecar**

If a file system cannot accept a given piece of metadata, the tool can store the metadata in a sidecar json sidecar file.
By default, the name is the `{target_name}.json` with hidden `.{target_name}.json` Should the name be taken, the tool 
attempts to find a free random suffix to add to the file like: `.{target_name}.{08x}.json`. After a certain number of 
attempts, the tool give ups and records an error in the database.

| Short | Long             | Required | Args | Description                                                                                                                                     | Notes |
|-------|------------------|----------|------|-------------------------------------------------------------------------------------------------------------------------------------------------|-------|
| -     | --sidecar        | n        | 1    | Write a per-entry JSON metadata manifest beside each placed entry. `on-error` (default) writes a sidecar only where metadata application failed |       |
| -     | --no-sidecar     | n        | 0    | Alias for `--sidecar never`: never write sidecar manifests                                                                                      |       |
| -     | --hidden-sidecar | n        | 0    | Prefix sidecar names with a leading dot (`{name}.json` → `.{name}.json`)                                                                        |       |

**Filtering**

When extracting, the tool allows to filter all files which were archived again. The logic is exactly the same as 
for the archive step. Allowed files are now the ones that passed both the archive and the extract filter.

| Short | Long           | Required | Args | Description                                                                                                                               | Notes |
|-------|----------------|----------|------|-------------------------------------------------------------------------------------------------------------------------------------------|-------|
| -     | --exclude      | n        | 1+   | Exclude paths matching regex PATTERN (repeatable)                                                                                         |       |
| -X    | --exclude-from | n        | 1+   | Read exclude regex patterns from FILE (repeatable)                                                                                        |       |
| -     | --include      | n        | 1+   | Include only paths matching regex PATTERN (repeatable)                                                                                    |       |
| -     | --include-from | n        | 1+   | Read include regex patterns from FILE (repeatable)                                                                                        |       |
| -     | --anchored     | n        | 0    | Patterns are matched against the absolute (raw on-disk) path, never a relative one. (default: unanchored)                                 |       |
| -     | --ignore-case  | n        | 0    | Case-insensitive pattern matching. (default: Case-sensitive)                                                                              |       |
| -     | --no-unicode   | n        | 0    | Compile filter regexes in bytes mode: `.` matches a byte, `\d \w \s` and `\b` become ASCII-only, and `\p{…}` Unicode classes are rejected |       |
| -     | --lazy-filter  | n        | 0    | Apply include/exclude filters after a later phase (scan) instead of eagerly                                                               |       |

#### Sparse Files

As the tool has to perform file copies anyway, the sparse-copy function can be employed to attempt to punch holes in 
the extracted files.

| Short | Long        | Required | Args | Description                                                                                                         | Notes |
|-------|-------------|----------|------|---------------------------------------------------------------------------------------------------------------------|-------|
| -     | --sparsify  | n        | 0    | Attempt to punch holes while extracting and copying files                                                           |       |
| -     | --page-size | n        | 1    | Page size in bytes: segment a file into pages of this size and search for completely empty pages for sparsification |       |

**Notes**
1. **Path resolution:**

   If the path is absolute, the path is taken as is. If the path is relative and `-C --directory` is not set,
   the current process working directory is taken from the environment and used as the root for the relative paths. If
   `-C --directory` is given, relative paths are resolved with this as a root instead.
2. **Materialization:** 

   When materializing, the path given is taken as is and the extraction tree grafted onto that. 
   Given `/home/user/Desktop/archive` and `file.txt` plus `dir/deep.txt`, the result will be:
   - `/home/user/Desktop/archive/file.txt`
   - `/home/user/Desktop/archive/dir/deep.txt`
   
   If absolute paths are selected and the two entries originally belonged to `/home/user2/temp`, extraction becomes:
   - `/home/user/Desktop/archive/home/user2/temp/file.txt`
   - `/home/user/Desktop/archive/home/user2/temp/dir/deep.txt`
   
   `--one-top-level` adds another nameable directory into the path s.t. the two entries don't float around in the 
   directory (useful, if the directory wasn't empty to begin with). Without absolute paths again:
   - `/home/user/Desktop/archive/one-top-level-dir/file.txt`
   - `/home/user/Desktop/archive/one-top-level-dir/dir/deep.txt`
    
   Path reconstruction in relative mode:  
   Since the tool internally operates with absolute_paths, relative paths are reconstructed as follows:
   - `-i ../other/../../var` is first resolved against the directory `--directory` 
   - `/home/user` + `../other/../../var` => `/home/user/../other/../../var` then normalized => `/var`
   - `/var` is then added to the queue of starting directories to index.
   
   Suppose we found `/var/log/app.log`, we compute the extract path as follows:
   - Strip abs prefix and trailing `/`: `/var/log/app.log` - `/var/` => `log/app.log`
   - Graft relative prefix => `../other/../../var` + `log/app.log` => `../other/../../var/log/app.log`
   - Normalize: `../other/../../var/log/app.log` => `../../var/log/app.log`
   - Strip parent: `../../var/log/app.log` => `var/log/app.log`
   - Attach to the effective extraction root: `/home/User/Desktop/archive/{one-top-level}` + `var/log/app.log` 
   => `/home/User/Desktop/archive/{one-top-level}/var/log/app.log`

3. **Hardlink Groups:**

   Hardlinks have three modes how they are grouped.
   - `None` - should be self-explanatory. No hardlinking is performed at all.
   - `Global` - Across the entire file tree that is created, any entry that shares its original (dev, inode) with 
     another entry in the extract file tree, will become a hardlink.
   - `Source` - Equivalent to `Global` without `--no-strict-separation`. If not, only restore hardlinks between files 
     inside a subtree reachable via a single source. e.g. `-i ../../a` `-i ../../../c/a/` with `a/` containing `a.txt` 
     and `b.txt` which are hardlinks would extract to:
     `a/a.txt (x, y)`  
     `a/b.txt (x, y)`  
     `c/a/a.txt (u, v)`  
     `c/a/b.txt (u, v)`  

4. To avoid having complex merge procedures between what is given in the cli and what might be present in the archive 
   an all or nothing policy is used. Once any of `--owner`/`--group`/`--owner-map`/ `--group-map` is supplied, all maps 
   are **exclusively** taken from the cli and not the database. I.e. `--apply-stored-owner-map` /
   `--apply-stored-group-map` have no effect. The `inspect` subcommand can dump the map structures 
   for the user to rebuild them outside the tar-dedup process. Another intended side effect is to shift the 
   responsibility of providing a fully sound map to the user rather than the tar-dedup tool to merge the maps.

5. **Resolution logic:**

   For any user or group, the resolution works as follows:
   - Resolve target user via the map if provided.
   - If no map was provided or the map didn't return a result, defer to owner or group override.
   - If `--same-owner` is provided, query the database for the owner / group.
   - If no result is found, the owner / group is skipped and the extracting process owner / group is used.

   If `ids` or `name` are provided for `--map-target` this resolution process is done once. For `name-id` or `id-name` 
   the first the process for the first type. Only if this failed to produce a result, the other type is queried. The 
   order is vertical and not in parallel (so not uid owner map, then name owner map, ...). 

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
the data as best as possible, we rely on tar and xz as the base. We explicitly do not use zip archives since the per-file 
compression as well as manifest make the files less compact than tar archives. 

The tool is constructed in an asymmetric fashion. The act of creating an archive has a very high 
up-front cost with 3 / 4 reads per file as the lower bound, with additional scans happening as a consequence of 
deduplication, <1 to 2 writes per file (depending on if the sparse materialization pass happens or not) and 
compressed archive. On the other hand, decompression is fairly easy. The files are first cached and then reflinked or 
copied into place. This gives the archive size in writes plus the writes of the fully extracted tree.

### Compression Algorithms
The current offering of compression algorithms is due to both the availability of rust crates for the algorithms and
also a thought of recentness of the algorithm. The recommendation is to use the xz algorithm, since the tool has been 
built with a focus on this algorithm. Giving a simple `.tar` extension and no compression override will produce a 
regular tar file. Since compression usually deals with sparse files well, sparsification is mainly here to facilitate 
smaller archives for regular tar files. For better performance it is not recommended to perform a sparse pass and 
compress.  
As the tool is using rust, it expects all its inputs to be in valid utf-8. Escape hatches are available for non-utf8 
filters and indexing but the tool currently lacks good support for extraction with non-utf8 paths. **Extraction will 
coerce non-utf8 paths to a lossy format, replacing invalid characters with �.**

### Divergence from the `tar` command

#### File Filtering 
The tool also sacrifices functionality the regular tar command offers. Since this tool is not intended to be backwards 
compatible the decision was made to only support a subset of the tar command options. I tried to retain a manageable 
overhead and an easy interface. Filtering in the two passes (include, exclude) should offer a sufficiently ergonomic 
way to express restrictions. An escape hatch is provided via `--files-from`.

Filtering is not recursive. The filters are applied to each element separately. This is a consequence how the tool 
optimizes path traversal and allocation of files (we want to minimize the archive size after all). All regex for filters 
are matched against each absolute path recorded in during the inventory stage.

#### Directory scanning.
By default, tar-dedup expects to scan disjoint trees. If multiple roots are provided and a following root is either 
identical to a previous root, is a previous root's parent or is a child of a previous root, the input is rejected. 
This also applies for any directories encountered inside the `--files-from` inputs and leads to an abort during the 
indexing stage. Should it not be possible to provide disjoint sources, the `--no-strict-separation` escape hatch 
disables the check. 

Any file system entry is recorded by its absolute path. If the same path is encountered twice, it will only be 
recorded once. What is not checked if the same inode is reached via a symlink. Then the same file gets two entries in 
the archive. 

#### Sparsification
Since the tar archive writer inside rust currently does not support detecting sparse files on the fly but instead 
requires the files to be sparse on disk, the tool opts to perform a sparse copy to the staging directory for the tar 
library to detect the sparse file. This has the sad side effect, that sparsification with this tool does not work on a 
file system that does not support sparse files. As the tool is geared towards compression anyway, this was deemed an 
acceptable trade-off for the moment.

### Security    
As this tool is only meant to produce a coherent and complete picture of a file tree for archival purposes, this tool 
offers less security than the normal `tar` command. The sqlite manifest stores absolute paths to all files 
archived and the full metadata of files is also stored in plain format. Please use caution when using this tool to 
produce archives meant to be publicly accessible. 

Also, the tool claims no robustness against injection of malicious content or tar offset attacks. Please only extract 
archives you trust with the tool. As I'm currently writing this as a set-it-and-forget-it tool it probably won't see 
24h support in case a vulnerability inside any of the dependencies is published.

The tool includes hashes of all its members as well as of the database in the footer and the final archive. However, it 
has **no healing ability**. The tool offers error detection but no error recovery. Storing data in an archive like this 
is not meant to replace your 3-2-1 backup strategy!

Compile hints on Ubuntu:  
`sudo apt install libselinux-dev libclang-dev clang`

### Returned Archive
Note on the returned file. The file is a valid tar archive with an appended footer. This looks like this:
```
[ compressed(tar archive) | MAGIC | sha1 of archive content | xz(-9e) of final-database.sqlite | sha1 of xz blob | MAGIC | u64 offset ]
```
- `MAGIC` is `"Tar-Dedup-SQLite-Footer"` - to make finding the footer by hand analyzing the file with a program like `less`.
- `sha1 of archive content` is the hash of the finished compressed archive (the bytes before the footer) to detect 
  corruption of the stream. It is verified once when the footer catalog is first installed during extraction; 
  under `--force` a mismatch is downgraded to a warning.
- `xz(-9e) of final-database.sqlite` is the finalized work database compressed with a fixed `xz -9 -e` preset 
  (independent of tar stream compression). It contains the finished session info and allows tar-dedup to instantly have 
  the manifest in the correct state rather than having to apply one snapshot after the other.
- `sha1 of xz blob` is the hash of the compressed footer payload (not the raw sqlite) to check for corruption.
- `u64 offset` is the offset of the first `MAGIC` string relative to the beginning of the file

The returned file can be opened by associated compressor or the tar archive. The footer will most likely be ignored 
by the compression algorithm and the tar stream and the contents can be viewed and used as is. A warning is also 
possible.

A **valid** archive is one that starts with an initial snapshot of the database, followed by canonical files, 
(a snapshot of the database in progress, followed by canonical files), one last snapshot of the database. Multiple in 
progress snapshots are expected, if the compression process is interrupted at least once. The extractor will allow 
archives that are truncated at the beginning, at the end or on both sides. Invalid members, no database, not valid 
canonical files, or an initial database not as the first member will cause an immediate abort. Truncated archives need 
to be extracted with `--force`, since they consitute archive the tool cannot guaranteed the integrity for anymore.

### Archive Content

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
choice was intentional for data security but with frequent stop-and-go of the tool, this will lead to bloat. 
`snapshot.sqlite` databases have no positional requirements. However, an archive with `manifest.sqlite` not in first 
position are considered invalid and will not be processed.

## Development
- Unittests are only run in dev-mode (the tool uses debug_asserts). This is sound, since we do not gate any section of
  code with release builds.
- Documentation / Docstrings are currently only placed sparsely. The docstrings (unless written by the LLM try to be
  sparse and explain the WHY. (the other Ws are visible in the code.))
- To be able to run all unittests, a file system with SELinux, posix_acl and xattr capabilities is needed.
- `test-all` is the feature to run all the test. There are debug features which break tests, so don't use 
  `--all-features` when testing.

## Trivia
- The code was developed in conjunction with Cursor with auto-routing (100\$) and OpenCode with 
  `DeepSeek V4 Flash 0731` (122\$). 
- All (non-test) code was vetted by my at least twice.
- Test design as well as cases to cover were determined by me and implemented by the LLM.
- I don't follow the rust-fmt standard to the letter. Close but I find the exact look somewhat ugly :) with 
  `) {` lines, ...
- Development was about four months of more than the legal 42h week in Switzerland.
- Additionally, [Mammouth](https://mammouth.ai) was used for the higher level discussions of the design choices.
- The IDE of choice is Rust-Rover.