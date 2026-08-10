# GNU tar flag matrix for tar-dedup

Tracking which GNU `tar` options map onto tar-dedup. Source help text also lives in [`Reflection.md`](Reflection.md).

## Status (`implement`)

| Value          | Meaning                                                   |
|----------------|-----------------------------------------------------------|
| `IMPLEMENTED`  | Behavior exists in tar-dedup today (CLI and/or pipeline). |
| `TO_IMPLEMENT` | Intended for the **current** development cycle.           |
| `FEATURE`      | Wanted in a **future** development cycle.                 |
| `DISCARDED`    | Out of scope; will not be implemented.                    |
| `FILL_ME_IN`   | Could not decide confidently from code/docs.              |

Columns: **shortopt**, **longopt**, **implement**, **description**, **comment**.

---

## Header

```
Usage: tar [OPTION...] [FILE]...
```

GNU `tar` saves many files together into a single tape or disk archive, and can restore individual files from the archive.

**Examples (GNU tar):**

```
tar -cf archive.tar foo bar  # Create archive.tar from files foo and bar.
tar -tvf archive.tar         # List all files in archive.tar verbosely.
tar -xf archive.tar          # Extract all files from archive.tar.
```

**tar-dedup today** uses subcommands (`archive` / `extract`) rather than GNU’s single-binary operation modes; see comments under Main operation mode.

---

## Main operation mode <=

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

---

## Operation modifiers <=

| shortopt | longopt                          | implement            | description                                                       | comment                                                                                                             | phase[s] / command |
|----------|----------------------------------|----------------------|-------------------------------------------------------------------|---------------------------------------------------------------------------------------------------------------------|--------------------|
|          | `--check-device`                 | DISCARDED            | check device numbers when creating incremental archives (default) | No GNU incremental dumps.                                                                                           | -                  |
| `-g`     | `--listed-incremental=FILE`      | DISCARDED            | handle new GNU-format incremental backup                          |                                                                                                                     | -                  |
| `-G`     | `--incremental`                  | DISCARDED            | handle old GNU-format incremental backup                          |                                                                                                                     | -                  |
|          | `--hole-detection=TYPE`          | IMPLEMENTED, FEATURE | technique to detect holes                                         | sparse version implemented. raw (basically what sparse pass does just writes sparse tar header, feature for future) | sparsify / archive |
|          | `--ignore-failed-read`           | FEATURE              | do not exit with nonzero on unreadable files                      | Related to fail-fast / error policy in Reflection.                                                                  | / archive          |
|          | `--level=NUMBER`                 | DISCARDED            | dump level for created listed-incremental archive                 | Incremental discarded.                                                                                              | -                  |
|          | `--no-check-device`              | DISCARDED            | do not check device numbers when creating incremental archives    |                                                                                                                     | -                  |
|          | `--no-seek`                      | DISCARDED            | archive is not seekable                                           | Pipe/tape model not targeted.                                                                                       | -                  |
| `-n`     | `--seek`                         | DISCARDED            | archive is seekable                                               |                                                                                                                     | -                  |
|          | `--occurrence[=NUMBER]`          | DISCARDED            | process only the NUMBERth occurrence of each file in the archive  | Needs `--delete`/`--diff`/`--list` workflows we lack.                                                               | -                  |
|          | `--sparse-version=MAJOR[.MINOR]` | DISCARDED            | set version of the sparse format to use (implies `--sparse`)      | Internal sparse format may not match GNU sparse headers.                                                            | -                  |
| `-S`     | `--sparse`                       | IMPLEMENTED          | handle sparse files efficiently                                   | Via sparsify stage + sparse-cp; not GNU `-S` flag name yet.                                                         | sparsify / archive |

> Sparse format: GNU tar "old-style" (oldgnu) sparse format — the pre-PAX encoding using GNUSparse typeflag and sparse offset/length pairs embedded in the header block (RMT extension blocks used if >4 segments). Not associated with any --sparse-version value; this predates that flag's versioning scheme, which only applies to PAX GNU.sparse.* extensions. Equivalent to running GNU tar with --sparse and GNU format (not PAX).

---

## Local file name selection <=

| shortopt | longopt                           | implement   | description                                                                             | comment                                                                              | phase[s] / command  |
|----------|-----------------------------------|-------------|-----------------------------------------------------------------------------------------|--------------------------------------------------------------------------------------|---------------------|
|          | `--add-file=FILE`                 | FEATURE     | add given FILE to the archive (useful if its name starts with a dash)                   | Paths come from `-i` tree walk today.                                                | inventory / archive |
| `-C`     | `--directory=DIR`                 | IMPLEMENTED | change to directory DIR                                                                 | **Asymmetry:** archive `-C` = work/stage dir; extract `-C` = output root (GNU-like). | inventory / archive |
|          | `--exclude=PATTERN`               | FEATURE     | exclude files, given as a PATTERN                                                       | TODO.md Filter / Exclude.                                                            | filter / archive    |
|          | `--exclude-backups`               | FEATURE     | exclude backup and lock files                                                           | Bundled with exclude work.                                                           | filter / archive    |
|          | `--exclude-caches`                | FEATURE     | exclude contents of directories containing CACHEDIR.TAG, except for the tag file itself |                                                                                      | filter / archive    |
|          | `--exclude-caches-all`            | FEATURE     | exclude directories containing CACHEDIR.TAG                                             |                                                                                      | filter / archive    |
|          | `--exclude-caches-under`          | FEATURE     | exclude everything under directories containing CACHEDIR.TAG                            |                                                                                      | filter / archive    |
|          | `--exclude-ignore=FILE`           | FEATURE     | read exclude patterns for each directory from FILE, if it exists                        |                                                                                      | filter / archive    |
|          | `--exclude-ignore-recursive=FILE` | FEATURE     | read exclude patterns for each directory and its subdirectories from FILE               |                                                                                      | filter / archive    |
|          | `--exclude-tag=FILE`              | FEATURE     | exclude contents of directories containing FILE, except for FILE itself                 |                                                                                      | filter / archive    |
|          | `--exclude-tag-all=FILE`          | FEATURE     | exclude directories containing FILE                                                     |                                                                                      | filter / archive    |
|          | `--exclude-tag-under=FILE`        | FEATURE     | exclude everything under directories containing FILE                                    |                                                                                      | filter / archive    |
|          | `--exclude-vcs`                   | FEATURE     | exclude version control system directories                                              |                                                                                      | filter / archive    |
|          | `--exclude-vcs-ignores`           | FEATURE     | read exclude patterns from the VCS ignore files                                         |                                                                                      | filter / archive    |
|          | `--no-null`                       | FEATURE     | disable the effect of the previous `--null` option                                      | With `--files-from`.                                                                 | filter / archive    |
|          | `--no-recursion`                  | FEATURE     | avoid descending automatically in directories                                           | Default today is recurse full tree.                                                  | inventory / archive |
|          | `--no-unquote`                    | FEATURE     | do not unquote input file or member names                                               |                                                                                      | filter / archive    |
|          | `--no-verbatim-files-from`        | FEATURE     | `-T` treats file names starting with dash as options (default)                          |                                                                                      | filter / archive    |
|          | `--null`                          | FEATURE     | `-T` reads null-terminated names; implies `--verbatim-files-from`                       |                                                                                      | filter / archive    |
|          | `--recursion`                     | IMPLEMENTED | recurse into directories (default)                                                      | Inventory walks recursively; no flag to disable yet.                                 | inventory / archive |
| `-T`     | `--files-from=FILE`               | FEATURE     | get names to extract or create from FILE                                                |                                                                                      | inventory / archive |
|          | `--unquote`                       | FEATURE     | unquote input file or member names (default)                                            |                                                                                      | filter / archive    |
|          | `--verbatim-files-from`           | FEATURE     | `-T` reads file names verbatim (no escape or option handling)                           |                                                                                      | filter / archive    |
| `-X`     | `--exclude-from=FILE`             | FEATURE     | exclude patterns listed in FILE                                                         |                                                                                      | filter / archive    |

---

## File name matching options (affect both exclude and include patterns) <=

| shortopt | longopt                      | implement | description                                          | comment                         | phase[s] / command |
|----------|------------------------------|-----------|------------------------------------------------------|---------------------------------|--------------------|
|          | `--anchored`                 | FEATURE   | patterns match file name start                       | Meaningful once exclude exists. | filter / archive   |
|          | `--ignore-case`              | FEATURE   | ignore case                                          |                                 | filter / archive   |
|          | `--no-anchored`              | FEATURE   | patterns match after any `/` (default for exclusion) |                                 | filter / archive   |
|          | `--no-ignore-case`           | FEATURE   | case sensitive matching (default)                    |                                 | filter / archive   |
|          | `--no-wildcards`             | FEATURE   | verbatim string matching                             |                                 | filter / archive   |
|          | `--no-wildcards-match-slash` | FEATURE   | wildcards do not match `/`                           |                                 | filter / archive   |
|          | `--wildcards`                | FEATURE   | use wildcards (default for exclusion)                |                                 | filter / archive   |
|          | `--wildcards-match-slash`    | FEATURE   | wildcards match `/` (default for exclusion)          |                                 | filter / archive   |

---

## Overwrite control <=

| shortopt | longopt                    | implement    | description                                                           | comment                                                                                                                                | phase[s] / command    |
|----------|----------------------------|--------------|-----------------------------------------------------------------------|----------------------------------------------------------------------------------------------------------------------------------------|-----------------------|
|          | `--keep-directory-symlink` | FEATURE      | preserve existing symlinks to directories when extracting             | if path is symlink `name -> something`. defualt `rm name && mkdir name` flag changes to follow symlink instead.                        | place / extract       |
|          | `--keep-newer-files`       | FEATURE      | don't replace existing files that are newer than their archive copies | Aligns with extract mtime-only update notes.                                                                                           | place / extract       |
| `-k`     | `--keep-old-files`         | FEATURE      | don't replace existing files when extracting, treat them as errors    |                                                                                                                                        | place / extract       |
|          | `--no-overwrite-dir`       | FEATURE      | preserve metadata of existing directories                             |                                                                                                                                        | permissions / extract |
|          | `--one-top-level[=DIR]`    | FEATURE      | create a subdirectory to avoid having loose files extracted           | In tar, if a loose file without dir starts archive, create tarname as defualt or DIR if arg provided. Related to master/ layout ideas. | place / extract       |
|          | `--overwrite`              | TO_IMPLEMENT | overwrite existing files when extracting                              | Place currently copies; policy flags not wired.                                                                                        | place / extract       |
|          | `--overwrite-dir`          | TO_IMPLEMENT | overwrite metadata of existing directories when extracting (default)  |                                                                                                                                        | permissions / extract |
|          | `--recursive-unlink`       | FEATURE      | empty hierarchies prior to extracting directory                       | If we extract /foo/bar/baz and baz already exists, with this option rm -rf /foo/bar/baz/* is called and files extracted into later.    | place / extract       |
|          | `--remove-files`           | DISCARDED    | remove files after adding them to the archive                         | Dangerous; not a tar-dedup goal.                                                                                                       | -                     |
|          | `--skip-old-files`         | FEATURE      | don't replace existing files when extracting, silently skip over them |                                                                                                                                        | place / extract       |
| `-U`     | `--unlink-first`           | FEATURE      | remove each file prior to extracting over it                          | call rm on file first and wirte into fresh file. (prevent hardlink issues)                                                             | place / extract       |
| `-W`     | `--verify`                 | DISCARDED    | attempt to verify the archive after writing it                        | Hash/rehash on extract is closer to our model.                                                                                         | -                     |

---

## Select output stream <=

| shortopt | longopt                     | implement | description                                    | comment                                    | phase[s] / command |
|----------|-----------------------------|-----------|------------------------------------------------|--------------------------------------------|--------------------|
|          | `--ignore-command-error`    | DISCARDED | ignore exit codes of children                  | No external compress-program children yet. | -                  |
|          | `--no-ignore-command-error` | DISCARDED | treat non-zero exit codes of children as error |                                            | -                  |
| `-O`     | `--to-stdout`               | FEATURE   | extract files to standard output               | Listed in Reflection specials.             | place / extract    |
|          | `--to-command=COMMAND`      | DISCARDED | pipe extracted files to another program        |                                            | -                  |

---

## Handling of file attributes <=

| shortopt | longopt                                        | implement    | description                                                                                           | comment                                                                     | phase[s] / command    |
|----------|------------------------------------------------|--------------|-------------------------------------------------------------------------------------------------------|-----------------------------------------------------------------------------|-----------------------|
|          | `--atime-preserve[=METHOD]`                    | TO_IMPLEMENT | preserve access times on dumped files                                                                 | Times captured in inventory; restore policy unclear.                        | permissions / extract |
|          | `--clamp-mtime`                                | FEATURE      | only set time when the file is more recent than what was given with `--mtime`                         | No `--mtime` force path yet.                                                | permissions / extract |
|          | `--delay-directory-restore`                    | TO_IMPLEMENT | delay setting modification times and permissions of extracted directories until the end of extraction | Matches bottom-up permissions plan in TODO.                                 | permissions / extract |
|          | `--group=NAME`                                 | FEATURE      | force NAME as group for added files                                                                   |                                                                             | permissions / extract |
|          | `--group-map=FILE`                             | FEATURE      | use FILE to map file owner GIDs and names                                                             |                                                                             | permissions / extract |
|          | `--mode=CHANGES`                               | FEATURE      | force (symbolic) mode CHANGES for added files                                                         | ??? What is this ???                                                        | permissions / extract |
|          | `--mtime=DATE-OR-FILE`                         | FEATURE      | set mtime for added files from DATE-OR-FILE                                                           |                                                                             | permissions / extract |
| `-m`     | `--touch`                                      | FEATURE      | don't extract file modified time                                                                      | ???`Just leave the mtime in place ???                                       | permissions / extract |
|          | `--no-delay-directory-restore`                 | TO_IMPLEMENT | cancel the effect of `--delay-directory-restore`                                                      | ??? How --no-delay-directory-restore --delay-directory-restore              | permissions / extract |
|          | `--no-same-owner`                              | IMPLEMENTED  | extract files as yourself (default for ordinary users)                                                | Default when `--restore-owner` omitted.                                     | permissions / extract |
|          | `--no-same-permissions`                        | TO_IMPLEMENT | apply the user's umask when extracting permissions from the archive                                   | Permissions stage still stub.                                               | permissions / extract |
|          | `--numeric-owner`                              | TO_IMPLEMENT | always use numbers for user/group names                                                               | DB stores numeric uid/gid already.                                          | permissions / extract |
|          | `--owner=NAME`                                 | FEATURE      | force NAME as owner for added files                                                                   |                                                                             | permissions / extract |
|          | `--owner-map=FILE`                             | FEATURE      | use FILE to map file owner UIDs and names                                                             |                                                                             | permissions / extract |
| `-p`     | `--preserve-permissions`, `--same-permissions` | TO_IMPLEMENT | extract information about file permissions                                                            | Capture on archive done; apply on extract TODO.                             | permissions / extract |
|          | `--same-owner`                                 | TO_IMPLEMENT | try extracting files with the same ownership as exists in the archive                                 | CLI `--restore-owner` exists; full apply still TODO.                        | permissions / extract |
|          | `--sort=ORDER`                                 | FEATURE      | directory sorting order: none (default), name or inode                                                | Staging orders by ext/size/id for compression. Inode not recorded research! | inventory / archive   |
| `-s`     | `--preserve-order`, `--same-order`             | DISCARDED    | member arguments are listed in the same order as the files in the archive                             | CLI file-list order not used.                                               |                       |

---

## Handling of extended file attributes <=

| shortopt | longopt                 | implement    | description                                | comment                                                    | phase[s] / command    |
|----------|-------------------------|--------------|--------------------------------------------|------------------------------------------------------------|-----------------------|
|          | `--acls`                | IMPLEMENTED  | Enable the POSIX ACLs support              | Captured at inventory (`do_posix_acl`); no CLI toggle yet. | permissions / extract |
|          | `--no-acls`             | TO_IMPLEMENT | Disable the POSIX ACLs support             | Config knob exists; not on CLI.                            | permissions / extract |
|          | `--no-selinux`          | TO_IMPLEMENT | Disable the SELinux context support        | Same.                                                      | permissions / extract |
|          | `--no-xattrs`           | TO_IMPLEMENT | Disable extended attributes support        | Same.                                                      | permissions / extract |
|          | `--selinux`             | IMPLEMENTED  | Enable the SELinux context support         | Captured when available.                                   | permissions / extract |
|          | `--xattrs`              | IMPLEMENTED  | Enable extended attributes support         | Captured at inventory.                                     | permissions / extract |
|          | `--xattrs-exclude=MASK` | FEATURE      | specify the exclude pattern for xattr keys |                                                            | permissions / extract |
|          | `--xattrs-include=MASK` | FEATURE      | specify the include pattern for xattr keys |                                                            | permissions / extract |

---

## Device selection and switching <=

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

---

## Device blocking <=

| shortopt | longopt                    | implement | description                                 | comment                                                                                                                        | phase[s] / command |
|----------|----------------------------|-----------|---------------------------------------------|--------------------------------------------------------------------------------------------------------------------------------|--------------------|
| `-b`     | `--blocking-factor=BLOCKS` | DISCARDED | BLOCKS x 512 bytes per record               | rust `tar` crate defaults; not exposed.                                                                                        | -                  |
| `-B`     | `--read-full-records`      | DISCARDED | reblock as we read (for 4.2BSD pipes)       |                                                                                                                                | -                  |
| `-i`     | `--ignore-zeros`           | DISCARDED | ignore zeroed blocks in archive (means EOF) | Conflicts with our footer/EOF model. **Note:** tar-dedup uses `-i` for **input directory** on `archive`, not this GNU meaning. | -                  |
|          | `--record-size=NUMBER`     | DISCARDED | NUMBER of bytes per record, multiple of 512 |                                                                                                                                | -                  |

---

## Archive format selection <=

| shortopt | longopt                              | implement | description                          | comment                                           | phase[s] / command |
|----------|--------------------------------------|-----------|--------------------------------------|---------------------------------------------------|--------------------|
| `-H`     | `--format=FORMAT`                    | DISCARDED | create archive of the given format   | rust `tar` writer format not user-selectable yet. | -                  |
|          | `--old-archive`, `--portability`     | DISCARDED | same as `--format=v7`                |                                                   | -                  |
|          | `--pax-option=keyword[[:]=value]...` | DISCARDED | control pax keywords                 |                                                   | -                  |
|          | `--posix`                            | DISCARDED | same as `--format=posix`             |                                                   | -                  |
| `-V`     | `--label=TEXT`                       | DISCARDED | create archive with volume name TEXT |                                                   | -                  |

**FORMAT values (reference only, not flags):** `gnu`, `oldgnu`, `pax`, `posix`, `ustar`, `v7`.

---

## Compression options <=

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

---

## Local file selection <=

| shortopt | longopt                                             | implement  | description                                                 | comment                                                                                                                 | phase[s] / command  |
|----------|-----------------------------------------------------|------------|-------------------------------------------------------------|-------------------------------------------------------------------------------------------------------------------------|---------------------|
|          | `--backup[=CONTROL]`                                | DISCARDED  | backup before removal, choose version CONTROL               | Extract overwrite policy TBD without GNU backups.                                                                       | -                   |
|          | `--hard-dereference`                                | FILL_ME_IN | follow hard links; archive and dump the files they refer to | Dedup already collapses identical content.                                                                              | inventory / archive |
| `-h`     | `--dereference`                                     | FEATURE    | follow symlinks; archive and dump the files they point to   | Stage uses symlinks then tar follows (`follow_symlinks`); inventory stores link targets. GNU `-h` semantics may differ. | inventory / archive |
| `-K`     | `--starting-file=MEMBER-NAME`                       | DISCARDED  | begin at member MEMBER-NAME when reading the archive        |                                                                                                                         | -                   |
|          | `--newer-mtime=DATE`                                | FEATURE    | compare date and time when data changed only                |                                                                                                                         | filter / archive    |
| `-N`     | `--newer=DATE-OR-FILE`, `--after-date=DATE-OR-FILE` | FEATURE    | only store files newer than DATE-OR-FILE                    |                                                                                                                         | filter / archive    |
|          | `--one-file-system`                                 | FEATURE    | stay in local file system when creating archive             |                                                                                                                         | inventory / archive |
| `-P`     | `--absolute-names`                                  | FEATURE    | don't strip leading `/`s from file names                    | Rel-paths from input root today. (Store original -i directory in the meta table                                         | inventory / archive |
|          | `--suffix=STRING`                                   | DISCARDED  | backup before removal, override usual suffix                | With `--backup`.                                                                                                        | -                   |

---

## File name transformations

| shortopt | longopt                                        | implement | description                                                   | comment                   | phase[s] / command |
|----------|------------------------------------------------|-----------|---------------------------------------------------------------|---------------------------|--------------------|
|          | `--strip-components=NUMBER`                    | FEATURE   | strip NUMBER leading components from file names on extraction | Requires new field in db! | place / extract    |
|          | `--transform=EXPRESSION`, `--xform=EXPRESSION` | FEATURE   | use sed replace EXPRESSION to transform file names            | Requires new field in db! | place / extract    |

---

## Informative output

| shortopt | longopt                                           | implement    | description                                                                         | comment                                                             | phase[s] / command |
|----------|---------------------------------------------------|--------------|-------------------------------------------------------------------------------------|---------------------------------------------------------------------|--------------------|
|          | `--checkpoint[=NUMBER]`                           | DISCARDED    | display progress messages every NUMBERth record (default 10)                        | We use indicatif progress instead.                                  | -                  |
|          | `--checkpoint-action=ACTION`                      | DISCARDED    | execute ACTION on each checkpoint                                                   |                                                                     | -                  |
|          | `--full-time`                                     | FEATURE      | print file time to its full resolution                                              | --list only relevant                                                | * / extract        |
|          | `--index-file=FILE`                               | FEATURE      | send verbose output to FILE                                                         | TODO: good logging / separate log stream.                           | * / *              |
| `-l`     | `--check-links`                                   | DISCARDED    | print a message if not all links are dumped                                         | We transform any way and this is not something we want to check     | -                  |
|          | `--no-quote-chars=STRING`                         | DISCARDED    | disable quoting for characters from STRING                                          |                                                                     | -                  |
|          | `--quote-chars=STRING`                            | DISCARDED    | additionally quote characters from STRING                                           |                                                                     | -                  |
|          | `--quoting-style=STYLE`                           | DISCARDED    | set name quoting style                                                              |                                                                     | -                  |
| `-R`     | `--block-number`                                  | DISCARDED    | show block number within archive with each message                                  |                                                                     | -                  |
|          | `--show-defaults`                                 | FEATURE      | show tar defaults                                                                   | Could dump Config defaults.                                         | * / *              |
|          | `--show-omitted-dirs`                             | FEATURE      | when listing or extracting, list each directory that does not match search criteria | (SELECT * FROM files WHERE exclusion_id IS NOT NULL)                | filter, scan / *   |
|          | `--show-snapshot-field-ranges`                    | DISCARDED    | show valid ranges for snapshot-file fields                                          | GNU incremental snapshots.                                          | -                  |
|          | `--show-transformed-names`, `--show-stored-names` | FEATURE      | show file or archive names after transformation                                     | Will be possible with the other changes for these features from db. | * / *              |
|          | `--totals[=SIGNAL]`                               | FEATURE      | print total bytes after processing the archive                                      | Meta tallies exist internally.                                      | * / *              |
|          | `--utc`                                           | FEATURE      | print file modification times in UTC                                                | DB stores UTC timestamps. --list only relevant                      | * / *              |
| `-v`     | `--verbose`                                       | TO_IMPLEMENT | verbosely list files processed                                                      | TODO: different verbosities.                                        | * / *              |
|          | `--warning=KEYWORD`                               | FEATURE      | warning control                                                                     | (Not clear if we do this. Use grep on the output?)                  | * / *              |
| `-w`     | `--interactive`, `--confirmation`                 | DISCARDED    | ask for confirmation for every action                                               | Non-interactive pipeline tool.                                      | -                  |

---

## Compatibility options <=

| shortopt | longopt | implement | description                                                                        | comment                         | phase[s] / command |
|----------|---------|-----------|------------------------------------------------------------------------------------|---------------------------------|--------------------|
| `-o`     |         | DISCARDED | when creating, same as `--old-archive`; when extracting, same as `--no-same-owner` | Ambiguous GNU legacy short opt. | -                  |

---

## Other options

| shortopt | longopt      | implement    | description                                     | comment                                                          | phase[s] / command |
|----------|--------------|--------------|-------------------------------------------------|------------------------------------------------------------------|--------------------|
| `-?`     | `--help`     | IMPLEMENTED  | give this help list                             | clap `--help` on tar-dedup CLI.                                  | * / *              |
|          | `--restrict` | DISCARDED    | disable use of some potentially harmful options |                                                                  | -                  |
|          | `--usage`    | IMPLEMENTED  | give a short usage message                      | clap usage.                                                      | * / *              |
|          | `--version`  | TO_IMPLEMENT | print program version                           | TODO: add tool version to metadata; clap version may be partial. | * / *              |

---

## Footer (GNU help notes)

Mandatory or optional arguments to long options are also mandatory or optional for any corresponding short options.

The backup suffix is `~`, unless set with `--suffix` or `SIMPLE_BACKUP_SUFFIX`.
The version control may be set with `--backup` or `VERSION_CONTROL`, values are:

| Value             | Meaning                                              |
|-------------------|------------------------------------------------------|
| `none`, `off`     | never make backups                                   |
| `t`, `numbered`   | make numbered backups                                |
| `nil`, `existing` | numbered if numbered backups exist, simple otherwise |
| `never`, `simple` | always make simple backups                           |

Valid arguments for the `--quoting-style` option are: `literal`, `shell`, `shell-always`, `shell-escape`, `shell-escape-always`, `c`, `c-maybe`, `escape`, `locale`, `clocale`.

**This GNU tar’s defaults** (from the captured help, not tar-dedup):

```
--format=gnu -f- -b20 --quoting-style=escape
--rmt-command=…/gnutar-1.35/libexec/rmt
```

---

## tar-dedup options without a GNU tar equivalent

Not from `tar --help`; listed so the matrix stays honest about our CLI.

| shortopt | longopt                      | implement    | description                             | comment                                                    |
|----------|------------------------------|--------------|-----------------------------------------|------------------------------------------------------------|
| `-i`     |                              | IMPLEMENTED  | input directory to snapshot (`archive`) | Collides with GNU `-i` `--ignore-zeros` (DISCARDED above). |
|          | `--jobs`                     | IMPLEMENTED  | max concurrent workers                  |                                                            |
|          | `--level`                    | IMPLEMENTED  | compression level                       |                                                            |
|          | `--xz-extreme`               | IMPLEMENTED  | xz extreme preset bit                   |                                                            |
|          | `--bzip-small`               | IMPLEMENTED  | bzip2 small/100k blocks                 |                                                            |
|          | `--memlimit-compress`        | IMPLEMENTED  | xz encoder RAM cap                      |                                                            |
|          | `--page-size`                | IMPLEMENTED  | sparse/hash zero-page size              |                                                            |
|          | `--min-pages`                | IMPLEMENTED  | min empty pages before sparsify rewrite |                                                            |
|          | `--resume` / `--fresh`       | IMPLEMENTED  | start policy                            |                                                            |
|          | `--keep-db` / `--keep-stage` | IMPLEMENTED  | post-success cleanup keeps              |                                                            |
|          | `--exit-after-stage`         | IMPLEMENTED  | stop after named pipeline phase         |                                                            |
|          | `--restore-owner`            | TO_IMPLEMENT | extract uid/gid                         | Flag present; apply path incomplete.                       |
|          | `--bridge`                   | FEATURE      | dedup + link back into place            | Reflection.                                                |
|          | `--link-in-place`            | FEATURE      | extract tree as links into stage        | Reflection.                                                |
|          | `--retry-missing-sha`        | FEATURE      | archive files missing sha               | Config knob / Reflection.                                  |
