# Tar-Dedup
A `tar` "wrapper" to deduplicate the file-tree prior to archiving to save extra space. It capability to interrupt the 
creation of large tar archives at the cost of lower compression ratios for compressed archives.

## Usage
The `tar-dedup` consists of a few subcommands. 

### archive
This command takes a set of input files / directories and produces an archive.

**Archive Paths**

| Short | Long          | Required | Args | Description                                                                                       | Notes |
|-------|---------------|----------|------|---------------------------------------------------------------------------------------------------|-------|
| `-f`  | -             | y        | 1    | Specifies the archive file to operate on                                                          | 1.    |
| `-C`  | `--directory` | n        | 1    | Change current working directory to this argument. If relative, use cwd as the base.              | 1.    |
| -     | `--work-dir`  | n        | 1    | Change location of temp directory for process. **Might grow to the size of the files to archive** | 1.    |

**Inputs**

| Short | Long           | Required | Args | Description                                                                                                                         | Notes |
|-------|----------------|----------|------|-------------------------------------------------------------------------------------------------------------------------------------|-------|
| `-i`  | `--input-dir`  | y        | 0+   | Provide a path to a directory that should be archived. Multiple directories can be given with `-i`. Path MUST point to a directory. | 1. 2. |
| `-T`  | `--files-from` | y        | 0+   | Provide a path to a directory that should be archived. Multiple directories can be given with `-i`. Path MUST point to a file.      | 1. 2. |
| -     | `--null`       | n        | 0    | Applies to `-T`: Switch from a `\n` separated file to a `\0` file list.                                                             |       |

**Compression**

| Short | Long                  | Required | Args | Description                                                                               | Notes |
|-------|-----------------------|----------|------|-------------------------------------------------------------------------------------------|-------|
| `-a`  | `--auto-compress`     | n        | 0    | Infer compression method by the extension of `-f`. Conflicts `--no-auto-compress`         | 3.    |
| -     | `--no-auto-compress`  | n        | 0    | Do not infer the compression method by the extension of `-f`. Conflicts `--auto-compress` | 3.    |
| `-z`  | `--gzip`              | n        | 0    | Use gunzip to compress the archive.                                                       | 3.    |
| `-j`  | `--bzip2`             | n        | 0    | Use bzip2 to compress the archive.                                                        | 3.    |
| `-J`  | `--xz`                | n        | 0    | Use xz to compress the archive. (recommended)                                             | 3.    |
| -     | `--zstd`              | n        | 0    | Use zstd to compress the archive.                                                         | 3.    |
| -     | `--level`             | n        | 1    | Set the compression level of the algorithm gzip/bzip2 1-9, xz 0-9, zstd 1-19.             |       |
| -     | `--xz-extreme`        | n        | 0    | Use extreme preset in xz (xz only).                                                       |       |
| -     | `--memlimit-compress` | n        | 1    | Set memory limit for compression processes bytes and percentage supported. (xz only).     |       |

**Indexing**

| Short | Long                      | Required | Args | Description                                                                                                        | Notes |
|-------|---------------------------|----------|------|--------------------------------------------------------------------------------------------------------------------|-------|
| -     | `--no-recursion`          | n        | 0    | Disables recursion into subdirectories                                                                             |       |
| -     | `--dereference`           | n        | 0    | Follow symlinks                                                                                                    |       |
| -     | `--one-file-system`       | n        | 0    | Stay within a single file system.                                                                                  |       |
| `-P`  | `--absolute-names`        | n        | 0    | Function sets the default extraction mode.                                                                         | 4.    |
| -     | `--no-hardlink-detection` | n        | 0    | Hard Links are detected and merged in hash, dedup. If enabled, two hard linked files are treated as separate ones. | 4.    |

**Filtering**

`--include-from` and `--exclude-from` expects a file with `utf8` content and each pattern separated by a 
newline character. If you need to match a new line for some reason, use `\n` and never `0x0A`.
Regex is handled with rusts regex crate. TODO: Verify! This _should_ implement "PCRE-lite" - so a subset minus the 
backreferences and lookahead/lookbehind functionality.

| Short | Long                    | Required | Args | Description                                                                                      | Notes |
|-------|-------------------------|----------|------|--------------------------------------------------------------------------------------------------|-------|
| -     | `--exclude`             | n        | 1+   | Exclude all file system entries that match the REGEX                                             | 5.    |
| `-X`  | `--exclude-from`        | n        | 1+   | Provide a file containing one REGEX per line. Each line is then applied like an `--exclude` flag | 5.    |
| -     | `--include`             | n        | 1+   | Include all file system entries that match the REGEX                                             | 5.    |
| -     | `--include-from`        | n        | 1+   | Provide a file containing one REGEX per line. Each line is then applied like an `--include` flag | 5.    |
| -     | `--exclude-vcs`         | n        | 0    | Not implemented yet! Exclude version control system directories                                  | 5.    |
| -     | `--exclude-vcs-ignores` | n        | 0    | Not implemented yet! Read VCS ignore files for exclusions                                        | 5.    |
| -     | `--no-anchored`         | n        | 0    | Patterns match from the start of the relative path rather than any path component                | 5.    |
| -     | `--no-ignore-case `     | n        | 0    | Perform case insensitive matching                                                                | 5.    |

**File Attributes**
As a baseline, the tool attempts to capture as much data as possible. This includes (ctime, atime, mtime, uid, gid, mode, size
selinux policy, posix_acls, xattrs, file type. By default, the tool also attempts to resolve any uid, gid to a 
group name / owner name via the system database.

| Short | Long           | Required | Args | Description                                                           | Notes |
|-------|----------------|----------|------|-----------------------------------------------------------------------|-------|
| -     | `--no-acls`    | n        | 0    | Do not attempt to acquire the acls of the file system entries         |       |
| -     | `--no-xattrs`  | n        | 0    | Do not attempt to acquire the xattrs of the file system entries       |       |
| -     | `--no-selinux` | n        | 0    | Do not attempt to acquire the SELinux policy of the fiel system entry |       |
| -     | `--owner`      | n        | 1    | Store an override of the owner field in the database                  | 6.    |
| -     | `--owner-map`  | n        | 1    | Load the owner map into the database for.                             | 6.    |
| -     | `--group`      | n        | 1    | Store an override of the group field in the database                  | 6.    |
| -     | `--group-map`  | n        | 1    | Load the group map into the database for.                             | 6.    |

**Sparse Files**
The system relies on the tar reader detecting sparse files. To be able to add sparse files to the archive, they are 
sparse copied into the `--work-dir`, turning them into valid sparse files. Those sparse files are then used as the 
source of truth rather than the original files. (Note that this will lead to an additional scan files that were 
selected to be sparsified)

| Short | Long          | Required | Args | Description                                                                | Notes |
|-------|---------------|----------|------|----------------------------------------------------------------------------|-------|
| -     | `--sparsify`  | n        | 0    | Create sparse copies of the eligeable files                                |       |
| -     | `--page-size` | n        | 1    | Determine page size for sparse detection.                                  |       |
| -     | `--min-pages` | n        | 1    | Minimum number of empty pages a file needs to have before it's sparsified. |       |

**Process Options**

| Short | Long                   | Required | Args | Description                                                                                       | Notes |
|-------|------------------------|----------|------|---------------------------------------------------------------------------------------------------|-------|
| -     | `--jobs`               | n        | 1    | Maximum number of workers for rayon pools and xz encoder                                          |       |
| -     | `--fresh`              | n        | 0    | Wipe `--work-dir` if there's something.                                                           |       |
| -     | `--keep-db`            | n        | 0    | Retain the database after successfully finishing the archive                                      |       |
| -     | `--keep-stage`         | n        | 0    | Retain the stage directory (independent of database)                                              |       |
| -     | `--exit-after-stage`   | n        | 1    | Exit and save state after completing a given stage.                                               |       |
| -     | `--fail-fast`          | n        | 0    | Exit immediately if an error occurres. (E.g. Permission denied, Path does not Exist, ...)         |       |
| -     | `--no-errors`          | n        | 0    | Don't keep record of errors associated with a file.                                               |       |
| -     | `--eager-filter`       | n        | 0    | Perform filtering before the hash phase (faster, less information in database)                    |       |
| -     | `--no-dedup`           | n        | 0    | (Dangerous) Do not perform binary verification and assume (hash, file-size) match implies unique. |       |
| -     | `--retry-missing-sha ` | n        | 0    | Attempt to add a file to the archive regardless if it produced errors in the previous sections.   |       |

**Notes**
1. Path resolution. If the path is absolute, the path is taken as is. If the path is relative and `-C --directory` is not set, 
   the current working directory is taken from the environment and used as the root for the relative paths. If
   `-C --directory` is given, relative paths are resolved with this as a root instead.
2. At least one input must be given. So either one `-T` or `-i`. Multiples are supported. (so multiple `-i` and `-T`)
3. If no compression method is given or inferred, a normal tar archive is produced. Explicit flags like `--xz`, ... 
   take precedence over `--auto-compress`.
4. Internally, all files are stored with an absolute path and conversion to absolute or relative happens when extracting 
   canonical files are moved back into place.
5. Filtering always uses REGEX. No Glob or Shell expansions are supported. Files are first indexed. If given, only files
   matching one or more of the include filters are propagated. Lastly only files that don't match a single exclude 
   pattern are then eligible. If no a include filter is given, a catch-all filter is used.
6. The database will store the overrides separately from the file metadata. On extract, the overrides will be applied 
   rather than on archive.

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
Importantly: **THIS IS UNSTABLE CODE!!!**  
Any archives created with this tool prior to the v1.0.0 release have **no guarantees** that future commits will still 
decompress them correctly and successfully! If you need this tool done soon, create an issue or star the project to 
show interest.

The current offering of compression algorithms is due to both the availability of rust crates for the algorithms and
also a thought of recentness of the algorithm. The program will default to the most aggressive compression possible
(xz, cpu_count threads, extreme compression profile and no regard for RAM consumption) if the options of the command
exposes du not suite your needs, the command offers to write to a simple tar file you can compress yourself
(and also subsequently decompress if the format is not supported.)

As the tool is using rust, it expects all its inputs to be in valid utf-8.

The tool also sacrifices functionality the regular tar command offers. Since this tool is not intended to be backwards 
compatible the decision was made to only support a subset of the tar command options. We tried to retain a manageable 
overhead and an easy interface (e.g. `--exclude`, `--exclude-from`, `--files-from` are the main options supported from 
tar. However, the exclude options expect full regex to match paths against and `--files-from` is the alternative 
approach to exclude and supports \n or \0 terminated lines. Paths are expected to be unescaped.) Additionally, the
inverse can also be done `--include` and `--include-from` allow you to white list rather than black list. Defaults are 
treated as follows: no inclusion => everything is included, no exclusion => nothing is excluded. The tool has no rule 
order. The list of all members of the given directories and paths are scanned. A subset is then selected based on the 
include filters. Everything selected by the include filters is then passed through the exclude filters which then strip 
the remainder of entries before the hash phase begins. 

## Development

Features:
- [ ] For debugging purposes, the `force-utf8-encoding` can be used for better readability of the database. However, 
**if at any point a non-utf8 compliant byte is found, the binary will panic**. 

Compile hints on Debian:  
`sudo apt install libselinux-dev libclang-dev clang`

### Returned Archive
Note on the returned file. The file is a valid archive with an appended footer. This looks like this:
```
[ compressed(tar archive) | MAGIC | final-database.sqlite | sha1 of database | MAGIC | u64 offset ]
```
- `MAGIC` is `"Tar-Dedup-SQLite-Footer"` - to make finding the footer by hand analyzing the file with a program like `less`.
- `final-database.sqlite` is the database after the session was closed and contains the finished session info. 
  Besides that, it allows tar-dedup to instantly have the manifest in the correct state rather than having to apply one 
  snapshot after the other.
- `sha1 of database` is the hash of the database to check for corruption.
- `u64 offset` is the offset of the first MAGIC string relative to the beginning of the file

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