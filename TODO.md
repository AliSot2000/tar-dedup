# TODO for the current scope of the project.

## Future Features:
- Sparse files (detect them ahead of time)
- Sparsification of files before archive (build holes on os side)
- Filter ACLS,
- [ ] Support for Windows permissions
- [X] Retry errored files during tar step
- [ ] Parallel compression (Dedup, Sparsify, Stage, Archive)
- [X] Filter first
- [ ] Force utf8 (any non-utf8 string panics and aborts.)


## General:
- [ ] Testing
- [ ] Rework error handling and log policy as well as log levels
- [X] Sequential / Parallel where possible
- [ ] Add Version of Tool to metadata
- [ ] Add Platform to metadata
- [ ] Capture Errors in database for review.
- [X] need to add source root -i flag to the metadata (solved as source table)
- [ ] Add --batch-size arg to control batch size for single threaded phases
- [ ] Add archive process started, archive process ended time stamps to the db.

## Phases
### CLI:
- [ ] Add all flags / subcommand to the cli parse
- [ ] Add validation for all flags for the cli parser
- [ ] Add basic CLI tests

### Inventory
- [X] Support arbitrary file types (file, dir, symlink, hardlink, socket, pipe, block device, char device)
- ~~[ ] Weird types: (Doors (Solaris), Whiteout (BSD))~~
- [X] Add POSIX ACLS
- [X] Add XATTRS
- [X] Add SELinux permissions
- [X] Add birth_time and ctime
- [X] Store ln -s target for windows (file/dir (recursively resolve softlinks until cycle or non-softlink file is reached))

### Filter
- [X] Research Filtering options of tar
- [X] Implement filtering on top of paths in the database.
- [ ] Parent resolve filter

### Hash
- ~~[ ] Docker style output (by default)~~
- [X] Check file for changes (based on times)
- [X] Added sparse file check. 

### Dedup
Should be done?
- [ ] Better logging?
- [X] Run in parallel and do so very well

### Sparsified
- [X] Create sparse files. 

### Staging
- [X] Basically done?

### Archive
- [X] Finish the FileEntry and ContentID structs
- [X] Finish the different compression algorithms
- [X] Finish plane
- ~~[ ] Finish shell-out use plain for that.~~

### Scan/Extract
- [ ] Live check the files
- [ ] Potentially data driven file extraction (DDFE)

### Hash
- [ ] Hash file on extraction
- [ ] Hash eager (DDFE)

### Move / Place
- [ ] Move eager (DDFE)
- [ ] Link into Place (!! Does not allow for apply permissions) => Do user vs read only

### Apply permissions
- [ ] Apply permissions to the files (bottom up - in case the user does not have the same rights as the user creating the files initially)
- [ ] Apply permissions eagerly (DDFE) + Warning might lock you out of file.

### Clear
- [ ] Clean up database and stage dir, in case the dir was not cleared already
- [ ] Emit any errors
- [ ] Delete database if needed.

# Tar Command TODOs:

## Done / Feature only

### Main operation mode

| shortopt | longopt                       | implement   | description                                      | comment                                                   | phase[s] / command |
|----------|-------------------------------|-------------|--------------------------------------------------|-----------------------------------------------------------|--------------------|
| `-A`     | `--catenate`, `--concatenate` | DISCARDED   | append tar files to an archive                   | Not a tar-dedup goal.                                     | -                  |
| `-c`     | `--create`                    | IMPLEMENTED | create a new archive                             | Exposed as `archive` subcommand, not `-c`.                | -                  |
|          | `--delete`                    | DISCARDED   | delete from the archive (not on mag tapes!)      | Rewriting members mid-archive is out of scope.            | -                  |
| `-d`     | `--diff`, `--compare`         | FEATURE     | find differences between archive and file system | Could reuse catalog hashes later; not scheduled.          | -                  |
| `-r`     | `--append`                    | DISCARDED   | append files to the end of an archive            | Sessions append within one run; not GNU `-r` UX.          | -                  |
|          | `--test-label`                | DISCARDED   | test the archive volume label and exit           | Volume labels unused.                                     | -                  |
| `-t`     | `--list`                      | FEATURE     | list the contents of an archive                  | Catalog/footer could support this later.                  | -                  |
| `-u`     | `--update`                    | DISCARDED   | only append files newer than copy in archive     | Related to extract mtime-only update ideas in Reflection. | -                  |
| `-x`     | `--extract`, `--get`          | IMPLEMENTED | extract files from an archive                    | Exposed as `extract` subcommand, not `-x`.                | -                  |

### Device selection and switching

| shortopt | longopt                                          | implement   | description                                   | comment           | phase[s] / command |
|----------|--------------------------------------------------|-------------|-----------------------------------------------|-------------------|--------------------|
|          | `--force-local`                                  | DISCARDED   | archive file is local even if it has a colon  | Local paths only. | -                  |
| `-f`     | `--file=ARCHIVE`                                 | IMPLEMENTED | use archive file or device ARCHIVE            | Both subcommands. | * / *              |
| `-F`     | `--info-script=NAME`, `--new-volume-script=NAME` | DISCARDED   | run script at end of each tape (implies `-M`) | No multi-volume.  | -                  |
| `-L`     | `--tape-length=NUMBER`                           | DISCARDED   | change tape after writing NUMBER x 1024 bytes |                   | -                  |
| `-M`     | `--multi-volume`                                 | DISCARDED   | create/list/extract multi-volume archive      |                   | -                  |
|          | `--rmt-command=COMMAND`                          | DISCARDED   | use given rmt COMMAND instead of rmt          |                   | -                  |
|          | `--rsh-command=COMMAND`                          | DISCARDED   | use remote COMMAND instead of rsh             |                   | -                  |
|          | `--volno-file=FILE`                              | DISCARDED   | use/update the volume number in FILE          |                   | -                  |

### Device blocking

| shortopt | longopt                    | implement | description                                 | comment                                                                                                                        | phase[s] / command |
|----------|----------------------------|-----------|---------------------------------------------|--------------------------------------------------------------------------------------------------------------------------------|--------------------|
| `-b`     | `--blocking-factor=BLOCKS` | DISCARDED | BLOCKS x 512 bytes per record               | rust `tar` crate defaults; not exposed.                                                                                        | -                  |
| `-B`     | `--read-full-records`      | DISCARDED | reblock as we read (for 4.2BSD pipes)       |                                                                                                                                | -                  |
| `-i`     | `--ignore-zeros`           | DISCARDED | ignore zeroed blocks in archive (means EOF) | Conflicts with our footer/EOF model. **Note:** tar-dedup uses `-i` for **input directory** on `archive`, not this GNU meaning. | -                  |
|          | `--record-size=NUMBER`     | DISCARDED | NUMBER of bytes per record, multiple of 512 |                                                                                                                                | -                  |

### Archive format selection

| shortopt | longopt                              | implement | description                          | comment                                           | phase[s] / command |
|----------|--------------------------------------|-----------|--------------------------------------|---------------------------------------------------|--------------------|
| `-H`     | `--format=FORMAT`                    | DISCARDED | create archive of the given format   | rust `tar` writer format not user-selectable yet. | -                  |
|          | `--old-archive`, `--portability`     | DISCARDED | same as `--format=v7`                |                                                   | -                  |
|          | `--pax-option=keyword[[:]=value]...` | DISCARDED | control pax keywords                 |                                                   | -                  |
|          | `--posix`                            | DISCARDED | same as `--format=posix`             |                                                   | -                  |
| `-V`     | `--label=TEXT`                       | DISCARDED | create archive with volume name TEXT |                                                   | -                  |

## Compression options

| shortopt | longopt                          | implement   | description                                                    | comment                                                      | phase[s] / command                  |
|----------|----------------------------------|-------------|----------------------------------------------------------------|--------------------------------------------------------------|-------------------------------------|
| `-a`     | `--auto-compress`                | IMPLEMENTED | use archive suffix to determine the compression program        | Default unless `--no-auto-compress`.                         | tar_writer, scan / archive, extract |
| `-I`     | `--use-compress-program=PROG`    | DISCARDED   | filter through PROG (must accept `-d`)                         | Reflection: will support arbitrary compressors.              | -                                   |
| `-j`     | `--bzip2`                        | IMPLEMENTED | filter the archive through bzip2                               | Plus `--bzip-small`, `--level`.                              | tar_writer, scan / archive, extract |
| `-J`     | `--xz`                           | IMPLEMENTED | filter the archive through xz                                  | Plus `--xz-extreme`, `--memlimit-compress`, `--level`, jobs. | tar_writer, scan / archive, extract |
|          | `--lzip`                         | DISCARDED   | filter the archive through lzip                                | Not in supported crate set (Readme/Reflection).              | -                                   |
|          | `--lzma`                         | DISCARDED   | filter the archive through lzma                                | Use xz.                                                      | -                                   |
|          | `--lzop`                         | DISCARDED   | filter the archive through lzop                                |                                                              | -                                   |
|          | `--no-auto-compress`             | IMPLEMENTED | do not use archive suffix to determine the compression program |                                                              | tar_writer, scan / archive, extract |
|          | `--zstd`                         | IMPLEMENTED | filter the archive through zstd                                |                                                              | tar_writer, scan / archive, extract |
| `-z`     | `--gzip`, `--gunzip`, `--ungzip` | IMPLEMENTED | filter the archive through gzip                                |                                                              | tar_writer, scan / archive, extract |
| `-Z`     | `--compress`, `--uncompress`     | DISCARDED   | filter the archive through compress                            | Legacy; Reflection lists no useful args.                     | -                                   |

Uncompressed plain tar is supported by omitting filters / non-compressed suffix (no dedicated short flag).

## Compatibility options

| shortopt | longopt | implement | description                                                                        | comment                         | phase[s] / command |
|----------|---------|-----------|------------------------------------------------------------------------------------|---------------------------------|--------------------|
| `-o`     |         | DISCARDED | when creating, same as `--old-archive`; when extracting, same as `--no-same-owner` | Ambiguous GNU legacy short opt. | -                  |

### Operation modifiers

| shortopt | longopt                          | implement   | description                                                       | comment                                                                                                             | phase[s] / command |
|----------|----------------------------------|-------------|-------------------------------------------------------------------|---------------------------------------------------------------------------------------------------------------------|--------------------|
|          | `--check-device`                 | DISCARDED   | check device numbers when creating incremental archives (default) | No GNU incremental dumps.                                                                                           | -                  |
| `-g`     | `--listed-incremental=FILE`      | DISCARDED   | handle new GNU-format incremental backup                          |                                                                                                                     | -                  |
| `-G`     | `--incremental`                  | DISCARDED   | handle old GNU-format incremental backup                          |                                                                                                                     | -                  |
|          | `--hole-detection=TYPE`          | FEATURE     | technique to detect holes                                         | sparse version implemented. raw (basically what sparse pass does just writes sparse tar header, feature for future) | sparsify / archive |
|          | `--ignore-failed-read`           | IMPLEMENTED | do not exit with nonzero on unreadable files                      | Related to fail-fast / error policy in Reflection.                                                                  | / archive          |
|          | `--level=NUMBER`                 | DISCARDED   | dump level for created listed-incremental archive                 | Incremental discarded.                                                                                              | -                  |
|          | `--no-check-device`              | DISCARDED   | do not check device numbers when creating incremental archives    |                                                                                                                     | -                  |
|          | `--no-seek`                      | DISCARDED   | archive is not seekable                                           | Pipe/tape model not targeted.                                                                                       | -                  |
| `-n`     | `--seek`                         | DISCARDED   | archive is seekable                                               |                                                                                                                     | -                  |
|          | `--occurrence[=NUMBER]`          | DISCARDED   | process only the NUMBERth occurrence of each file in the archive  | Needs `--delete`/`--diff`/`--list` workflows we lack.                                                               | -                  |
|          | `--sparse-version=MAJOR[.MINOR]` | DISCARDED   | set version of the sparse format to use (implies `--sparse`)      | Internal sparse format may not match GNU sparse headers.                                                            | -                  |
| `-S`     | `--sparse`                       | IMPLEMENTED | handle sparse files efficiently                                   | Via sparsify stage + sparse-cp; not GNU `-S` flag name yet.                                                         | sparsify / archive |

### Local file selection

| shortopt | longopt                                             | implement   | description                                                 | comment                                                                                                                                           | phase[s] / command  |
|----------|-----------------------------------------------------|-------------|-------------------------------------------------------------|---------------------------------------------------------------------------------------------------------------------------------------------------|---------------------|
|          | `--backup[=CONTROL]`                                | DISCARDED   | backup before removal, choose version CONTROL               | Extract overwrite policy TBD without GNU backups.                                                                                                 | -                   |
|          | `--hard-dereference`                                | IMPLEMENTED | follow hard links; archive and dump the files they refer to | Dedup already collapses identical content.                                                                                                        | inventory / archive |
| `-h`     | `--dereference`                                     | IMPLEMENTED | follow symlinks; archive and dump the files they point to   | Stage uses symlinks then tar follows (`follow_symlinks`); inventory stores link targets. GNU `-h` semantics may differ.                           | inventory / archive |
| `-K`     | `--starting-file=MEMBER-NAME`                       | DISCARDED   | begin at member MEMBER-NAME when reading the archive        |                                                                                                                                                   | -                   |
|          | `--newer-mtime=DATE`                                | DISCARDED   | compare date and time when data changed only                | Discarded due to inconsisten application and parsing rules.                                                                                       | filter / archive    |
| `-N`     | `--newer=DATE-OR-FILE`, `--after-date=DATE-OR-FILE` | DISCARDED   | only store files newer than DATE-OR-FILE                    | DiscaRDED due to inconcistent parinsg gules.                                                                                                      | filter / archive    |
|          | `--one-file-system`                                 | IMPLEMENTED | stay in local file system when creating archive             |                                                                                                                                                   | inventory / archive |
| `-P`     | `--absolute-names`                                  | IMPLEMENTED | don't strip leading `/`s from file names                    | Rel-paths from input root today. (Store original -i directory in the meta table) - alternatively use abs path always. Useful if --files-from=FILE | inventory / archive |
|          | `--suffix=STRING`                                   | DISCARDED   | backup before removal, override usual suffix                | With `--backup`.                                                                                                                                  | -                   |

### Local file name selection

| shortopt | longopt                           | implement   | description                                                                             | comment                                                                              | phase[s] / command  |
|----------|-----------------------------------|-------------|-----------------------------------------------------------------------------------------|--------------------------------------------------------------------------------------|---------------------|
|          | `--add-file=FILE`                 | DISCARDED   | add given FILE to the archive (useful if its name starts with a dash)                   | Paths come from `-i` tree walk today.                                                | inventory / archive |
| `-C`     | `--directory=DIR`                 | IMPLEMENTED | change to directory DIR                                                                 | **Asymmetry:** archive `-C` = work/stage dir; extract `-C` = output root (GNU-like). | inventory / archive |
|          | `--exclude=PATTERN`               | IMPLEMENTED | exclude files, given as a PATTERN                                                       | TODO.md Filter / Exclude.                                                            | filter / archive    |
|          | `--exclude-backups`               | DISCARDED   | exclude backup and lock files                                                           | Bundled with exclude work.                                                           | filter / archive    |
|          | `--exclude-caches`                | DISCARDED   | exclude contents of directories containing CACHEDIR.TAG, except for the tag file itself |                                                                                      | filter / archive    |
|          | `--exclude-caches-all`            | DISCARDED   | exclude directories containing CACHEDIR.TAG                                             |                                                                                      | filter / archive    |
|          | `--exclude-caches-under`          | DISCARDED   | exclude everything under directories containing CACHEDIR.TAG                            |                                                                                      | filter / archive    |
|          | `--exclude-ignore=FILE`           | DISCARDED   | read exclude patterns for each directory from FILE, if it exists                        |                                                                                      | filter / archive    |
|          | `--exclude-ignore-recursive=FILE` | DISCARDED   | read exclude patterns for each directory and its subdirectories from FILE               |                                                                                      | filter / archive    |
|          | `--exclude-tag=FILE`              | DISCARDED   | exclude contents of directories containing FILE, except for FILE itself                 |                                                                                      | filter / archive    |
|          | `--exclude-tag-all=FILE`          | DISCARDED   | exclude directories containing FILE                                                     |                                                                                      | filter / archive    |
|          | `--exclude-tag-under=FILE`        | DISCARDED   | exclude everything under directories containing FILE                                    |                                                                                      | filter / archive    |
|          | `--exclude-vcs`                   | FEATURE     | exclude version control system directories                                              |                                                                                      | filter / archive    |
|          | `--exclude-vcs-ignores`           | FEATURE     | read exclude patterns from the VCS ignore files                                         |                                                                                      | filter / archive    |
|          | `--no-null`                       | IMPLEMENTED | disable the effect of the previous `--null` option                                      | With `--files-from`.                                                                 | filter / archive    |
|          | `--no-recursion`                  | IMPLEMENTED | avoid descending automatically in directories                                           | Default today is recurse full tree.                                                  | inventory / archive |
|          | `--no-unquote`                    | DISCARDED   | do not unquote input file or member names                                               |                                                                                      | filter / archive    |
|          | `--no-verbatim-files-from`        | DISCARDED   | `-T` treats file names starting with dash as options (default)                          |                                                                                      | filter / archive    |
|          | `--null`                          | IMPLEMENTED | `-T` reads null-terminated names; implies `--verbatim-files-from`                       |                                                                                      | filter / archive    |
|          | `--recursion`                     | IMPLEMENTED | recurse into directories (default)                                                      | Inventory walks recursively; no flag to disable yet.                                 | inventory / archive |
| `-T`     | `--files-from=FILE`               | IMPLEMENTED | get names to extract or create from FILE                                                |                                                                                      | inventory / archive |
|          | `--unquote`                       | DISCARDED   | unquote input file or member names (default)                                            |                                                                                      | filter / archive    |
|          | `--verbatim-files-from`           | DISCARDED   | `-T` reads file names verbatim (no escape or option handling)                           |                                                                                      | filter / archive    |
| `-X`     | `--exclude-from=FILE`             | FEATURE     | exclude patterns listed in FILE                                                         |                                                                                      | filter / archive    |

### File name matching options (affect both exclude and include patterns)

| shortopt | longopt                      | implement   | description                                          | comment                         | phase[s] / command |
|----------|------------------------------|-------------|------------------------------------------------------|---------------------------------|--------------------|
|          | `--anchored`                 | IMPLEMENTED | patterns match file name start                       | Meaningful once exclude exists. | filter / archive   |
|          | `--ignore-case`              | IMPLEMENTED | ignore case                                          |                                 | filter / archive   |
|          | `--no-anchored`              | IMPLEMENTED | patterns match after any `/` (default for exclusion) |                                 | filter / archive   |
|          | `--no-ignore-case`           | IMPLEMENTED | case sensitive matching (default)                    |                                 | filter / archive   |
|          | `--no-wildcards`             | DISCARDED   | verbatim string matching                             |                                 | filter / archive   |
|          | `--no-wildcards-match-slash` | DISCARDED   | wildcards do not match `/`                           |                                 | filter / archive   |
|          | `--wildcards`                | DISCARDED   | use wildcards (default for exclusion)                |                                 | filter / archive   |
|          | `--wildcards-match-slash`    | DISCARDED   | wildcards match `/` (default for exclusion)          |                                 | filter / archive   |

### Select output stream

| shortopt | longopt                     | implement | description                                    | comment                                    | phase[s] / command |
|----------|-----------------------------|-----------|------------------------------------------------|--------------------------------------------|--------------------|
|          | `--ignore-command-error`    | DISCARDED | ignore exit codes of children                  | No external compress-program children yet. | -                  |
|          | `--no-ignore-command-error` | DISCARDED | treat non-zero exit codes of children as error |                                            | -                  |
| `-O`     | `--to-stdout`               | FEATURE   | extract files to standard output               | Listed in Reflection specials.             | place / extract    |
|          | `--to-command=COMMAND`      | DISCARDED | pipe extracted files to another program        |                                            | -                  |

---

## TODO

### Overwrite control

| shortopt | longopt                    | implement    | description                                                           | comment                                                                                                                                | phase[s] / command    |
|----------|----------------------------|--------------|-----------------------------------------------------------------------|----------------------------------------------------------------------------------------------------------------------------------------|-----------------------|
|          | `--keep-directory-symlink` | TO_IMPLEMENT | preserve existing symlinks to directories when extracting             | if path is symlink `name -> something`. defualt `rm name && mkdir name` flag changes to follow symlink instead.                        | place / extract       |
|          | `--keep-newer-files`       | TO_IMPLEMENT | don't replace existing files that are newer than their archive copies | Aligns with extract mtime-only update notes.                                                                                           | place / extract       |
| `-k`     | `--keep-old-files`         | TO_IMPLEMENT | don't replace existing files when extracting, treat them as errors    |                                                                                                                                        | place / extract       |
|          | `--no-overwrite-dir`       | TO_IMPLEMENT | preserve metadata of existing directories                             |                                                                                                                                        | permissions / extract |
|          | `--one-top-level[=DIR]`    | TO_IMPLEMENT | create a subdirectory to avoid having loose files extracted           | In tar, if a loose file without dir starts archive, create tarname as defualt or DIR if arg provided. Related to master/ layout ideas. | place / extract       |
|          | `--overwrite`              | TO_IMPLEMENT | overwrite existing files when extracting                              | Place currently copies; policy flags not wired.                                                                                        | place / extract       |
|          | `--overwrite-dir`          | TO_IMPLEMENT | overwrite metadata of existing directories when extracting (default)  |                                                                                                                                        | permissions / extract |
|          | `--recursive-unlink`       | TO_IMPLEMENT | empty hierarchies prior to extracting directory                       | If we extract /foo/bar/baz and baz already exists, with this option rm -rf /foo/bar/baz/* is called and files extracted into later.    | place / extract       |
|          | `--remove-files`           | DISCARDED    | remove files after adding them to the archive                         | Dangerous; not a tar-dedup goal.                                                                                                       | -                     |
|          | `--skip-old-files`         | TO_IMPLEMENT | don't replace existing files when extracting, silently skip over them |                                                                                                                                        | place / extract       |
| `-U`     | `--unlink-first`           | TO_IMPLEMENT | remove each file prior to extracting over it                          | call rm on file first and wirte into fresh file. (prevent hardlink issues)                                                             | place / extract       |
| `-W`     | `--verify`                 | DISCARDED    | attempt to verify the archive after writing it                        | Hash/rehash on extract is closer to our model.                                                                                         | -                     |

### Handling of file attributes

| shortopt | longopt                                        | implement    | description                                                                                           | comment                                                                     | phase[s] / command    |
|----------|------------------------------------------------|--------------|-------------------------------------------------------------------------------------------------------|-----------------------------------------------------------------------------|-----------------------|
|          | `--atime-preserve[=METHOD]`                    | TO_IMPLEMENT | preserve access times on dumped files                                                                 | Times captured in inventory; restore policy unclear.                        | permissions / extract |
|          | `--clamp-mtime`                                | TO_IMPLEMENT | only set time when the file is more recent than what was given with `--mtime`                         | No `--mtime` force path yet.                                                | permissions / extract |
|          | `--delay-directory-restore`                    | TO_IMPLEMENT | delay setting modification times and permissions of extracted directories until the end of extraction | Matches bottom-up permissions plan in TODO.                                 | permissions / extract |
|          | `--group=NAME`                                 | FEATURE      | force NAME as group for added files                                                                   |                                                                             | permissions / extract |
|          | `--group-map=FILE`                             | FEATURE      | use FILE to map file owner GIDs and names                                                             |                                                                             | permissions / extract |
|          | `--mode=CHANGES`                               | TO_IMPLEMENT | force (symbolic) mode CHANGES for added files                                                         | ??? What is this ???                                                        | permissions / extract |
|          | `--mtime=DATE-OR-FILE`                         | TO_IMPLEMENT | set mtime for added files from DATE-OR-FILE                                                           |                                                                             | permissions / extract |
| `-m`     | `--touch`                                      | TO_IMPLEMENT | don't extract file modified time                                                                      | ???`Just leave the mtime in place ???                                       | permissions / extract |
|          | `--no-delay-directory-restore`                 | TO_IMPLEMENT | cancel the effect of `--delay-directory-restore`                                                      | ??? How --no-delay-directory-restore --delay-directory-restore              | permissions / extract |
|          | `--no-same-owner`                              | IMPLEMENTED  | extract files as yourself (default for ordinary users)                                                | Default when `--restore-owner` omitted.                                     | permissions / extract |
|          | `--no-same-permissions`                        | TO_IMPLEMENT | apply the user's umask when extracting permissions from the archive                                   | Permissions stage still stub.                                               | permissions / extract |
|          | `--numeric-owner`                              | TO_IMPLEMENT | always use numbers for user/group names                                                               | DB stores numeric uid/gid already.                                          | permissions / extract |
|          | `--owner=NAME`                                 | FEATURE      | force NAME as owner for added files                                                                   |                                                                             | permissions / extract |
|          | `--owner-map=FILE`                             | FEATURE      | use FILE to map file owner UIDs and names                                                             |                                                                             | permissions / extract |
| `-p`     | `--preserve-permissions`, `--same-permissions` | TO_IMPLEMENT | extract information about file permissions                                                            | Capture on archive done; apply on extract TODO.                             | permissions / extract |
|          | `--same-owner`                                 | TO_IMPLEMENT | try extracting files with the same ownership as exists in the archive                                 | CLI `--restore-owner` exists; full apply still TODO.                        | permissions / extract |
|          | `--sort=ORDER`                                 | DISCARDED    | directory sorting order: none (default), name or inode                                                | Staging orders by ext/size/id for compression. Inode not recorded research! | inventory / archive   |
| `-s`     | `--preserve-order`, `--same-order`             | DISCARDED    | member arguments are listed in the same order as the files in the archive                             | CLI file-list order not used.                                               |                       |

### Handling of extended file attributes

| shortopt | longopt                 | implement    | description                                | comment                                                    | phase[s] / command    |
|----------|-------------------------|--------------|--------------------------------------------|------------------------------------------------------------|-----------------------|
|          | `--acls`                | IMPLEMENTED  | Enable the POSIX ACLs support              | Captured at inventory (`do_posix_acl`); no CLI toggle yet. | permissions / extract |
|          | `--selinux`             | IMPLEMENTED  | Enable the SELinux context support         | Captured when available.                                   | permissions / extract |
|          | `--xattrs`              | IMPLEMENTED  | Enable extended attributes support         | Captured at inventory.                                     | permissions / extract |
|          | `--no-acls`             | TO_IMPLEMENT | Disable the POSIX ACLs support             | Config knob exists; not on CLI.                            | permissions / extract |
|          | `--no-selinux`          | TO_IMPLEMENT | Disable the SELinux context support        | Same.                                                      | permissions / extract |
|          | `--no-xattrs`           | TO_IMPLEMENT | Disable extended attributes support        | Same.                                                      | permissions / extract |
|          | `--xattrs-exclude=MASK` | FEATURE      | specify the exclude pattern for xattr keys |                                                            | permissions / extract |
|          | `--xattrs-include=MASK` | FEATURE      | specify the include pattern for xattr keys |                                                            | permissions / extract |

### File name transformations

| shortopt | longopt                                        | implement    | description                                                   | comment                   | phase[s] / command |
|----------|------------------------------------------------|--------------|---------------------------------------------------------------|---------------------------|--------------------|
|          | `--strip-components=NUMBER`                    | TO_IMPLEMENT | strip NUMBER leading components from file names on extraction | Requires new field in db! | place / extract    |
|          | `--transform=EXPRESSION`, `--xform=EXPRESSION` | TO_IMPLEMENT | use sed replace EXPRESSION to transform file names            | Requires new field in db! | place / extract    |