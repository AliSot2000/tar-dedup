# GNU tar flag matrix for tar-dedup

Tracking which GNU `tar` options map onto tar-dedup. Source help text also lives in [`Reflection.md`](Reflection.md).

## Status (`implement`)

| Value | Meaning |
|-------|---------|
| `IMPLEMENTED` | Behavior exists in tar-dedup today (CLI and/or pipeline). |
| `TO_IMPLEMENT` | Intended for the **current** development cycle. |
| `FEATURE` | Wanted in a **future** development cycle. |
| `DISCARDED` | Out of scope; will not be implemented. |
| `FILL_ME_IN` | Could not decide confidently from code/docs. |

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

## Main operation mode

| shortopt | longopt | implement | description | comment |
|----------|---------|-----------|-------------|---------|
| `-A` | `--catenate`, `--concatenate` | DISCARDED | append tar files to an archive | Not a tar-dedup goal. |
| `-c` | `--create` | IMPLEMENTED | create a new archive | Exposed as `archive` subcommand, not `-c`. |
| | `--delete` | DISCARDED | delete from the archive (not on mag tapes!) | Rewriting members mid-archive is out of scope. |
| `-d` | `--diff`, `--compare` | FEATURE | find differences between archive and file system | Could reuse catalog hashes later; not scheduled. |
| `-r` | `--append` | DISCARDED | append files to the end of an archive | Sessions append within one run; not GNU `-r` UX. |
| | `--test-label` | DISCARDED | test the archive volume label and exit | Volume labels unused. |
| `-t` | `--list` | FEATURE | list the contents of an archive | Catalog/footer could support this later. |
| `-u` | `--update` | FEATURE | only append files newer than copy in archive | Related to extract mtime-only update ideas in Reflection. |
| `-x` | `--extract`, `--get` | IMPLEMENTED | extract files from an archive | Exposed as `extract` subcommand, not `-x`. |

---

## Operation modifiers

| shortopt | longopt | implement | description | comment |
|----------|---------|-----------|-------------|---------|
| | `--check-device` | DISCARDED | check device numbers when creating incremental archives (default) | No GNU incremental dumps. |
| `-g` | `--listed-incremental=FILE` | DISCARDED | handle new GNU-format incremental backup | |
| `-G` | `--incremental` | DISCARDED | handle old GNU-format incremental backup | |
| | `--hole-detection=TYPE` | FILL_ME_IN | technique to detect holes | Sparse path uses page-based zero detection; TYPE API undecided. |
| | `--ignore-failed-read` | FEATURE | do not exit with nonzero on unreadable files | Related to fail-fast / error policy in Reflection. |
| | `--level=NUMBER` | DISCARDED | dump level for created listed-incremental archive | Incremental discarded. |
| | `--no-check-device` | DISCARDED | do not check device numbers when creating incremental archives | |
| | `--no-seek` | DISCARDED | archive is not seekable | Pipe/tape model not targeted. |
| `-n` | `--seek` | DISCARDED | archive is seekable | |
| | `--occurrence[=NUMBER]` | DISCARDED | process only the NUMBERth occurrence of each file in the archive | Needs `--delete`/`--diff`/`--list` workflows we lack. |
| | `--sparse-version=MAJOR[.MINOR]` | FILL_ME_IN | set version of the sparse format to use (implies `--sparse`) | Internal sparse format may not match GNU sparse headers. |
| `-S` | `--sparse` | IMPLEMENTED | handle sparse files efficiently | Via sparsify stage + sparse-cp; not GNU `-S` flag name yet. |

---

## Local file name selection

| shortopt | longopt | implement | description | comment |
|----------|---------|-----------|-------------|---------|
| | `--add-file=FILE` | FILL_ME_IN | add given FILE to the archive (useful if its name starts with a dash) | Paths come from `-i` tree walk today. |
| `-C` | `--directory=DIR` | IMPLEMENTED | change to directory DIR | **Asymmetry:** archive `-C` = work/stage dir; extract `-C` = output root (GNU-like). |
| | `--exclude=PATTERN` | FEATURE | exclude files, given as a PATTERN | TODO.md Filter / Exclude. |
| | `--exclude-backups` | FEATURE | exclude backup and lock files | Bundled with exclude work. |
| | `--exclude-caches` | FEATURE | exclude contents of directories containing CACHEDIR.TAG, except for the tag file itself | |
| | `--exclude-caches-all` | FEATURE | exclude directories containing CACHEDIR.TAG | |
| | `--exclude-caches-under` | FEATURE | exclude everything under directories containing CACHEDIR.TAG | |
| | `--exclude-ignore=FILE` | FEATURE | read exclude patterns for each directory from FILE, if it exists | |
| | `--exclude-ignore-recursive=FILE` | FEATURE | read exclude patterns for each directory and its subdirectories from FILE | |
| | `--exclude-tag=FILE` | FEATURE | exclude contents of directories containing FILE, except for FILE itself | |
| | `--exclude-tag-all=FILE` | FEATURE | exclude directories containing FILE | |
| | `--exclude-tag-under=FILE` | FEATURE | exclude everything under directories containing FILE | |
| | `--exclude-vcs` | FEATURE | exclude version control system directories | |
| | `--exclude-vcs-ignores` | FEATURE | read exclude patterns from the VCS ignore files | |
| | `--no-null` | FEATURE | disable the effect of the previous `--null` option | With `--files-from`. |
| | `--no-recursion` | FEATURE | avoid descending automatically in directories | Default today is recurse full tree. |
| | `--no-unquote` | FILL_ME_IN | do not unquote input file or member names | |
| | `--no-verbatim-files-from` | FEATURE | `-T` treats file names starting with dash as options (default) | |
| | `--null` | FEATURE | `-T` reads null-terminated names; implies `--verbatim-files-from` | |
| | `--recursion` | IMPLEMENTED | recurse into directories (default) | Inventory walks recursively; no flag to disable yet. |
| `-T` | `--files-from=FILE` | FEATURE | get names to extract or create from FILE | |
| | `--unquote` | FILL_ME_IN | unquote input file or member names (default) | |
| | `--verbatim-files-from` | FEATURE | `-T` reads file names verbatim (no escape or option handling) | |
| `-X` | `--exclude-from=FILE` | FEATURE | exclude patterns listed in FILE | |

---

## File name matching options (affect both exclude and include patterns)

| shortopt | longopt | implement | description | comment |
|----------|---------|-----------|-------------|---------|
| | `--anchored` | FEATURE | patterns match file name start | Meaningful once exclude exists. |
| | `--ignore-case` | FEATURE | ignore case | |
| | `--no-anchored` | FEATURE | patterns match after any `/` (default for exclusion) | |
| | `--no-ignore-case` | FEATURE | case sensitive matching (default) | |
| | `--no-wildcards` | FEATURE | verbatim string matching | |
| | `--no-wildcards-match-slash` | FEATURE | wildcards do not match `/` | |
| | `--wildcards` | FEATURE | use wildcards (default for exclusion) | |
| | `--wildcards-match-slash` | FEATURE | wildcards match `/` (default for exclusion) | |

---

## Overwrite control

| shortopt | longopt | implement | description | comment |
|----------|---------|-----------|-------------|---------|
| | `--keep-directory-symlink` | FILL_ME_IN | preserve existing symlinks to directories when extracting | |
| | `--keep-newer-files` | FEATURE | don't replace existing files that are newer than their archive copies | Aligns with extract mtime-only update notes. |
| `-k` | `--keep-old-files` | FEATURE | don't replace existing files when extracting, treat them as errors | |
| | `--no-overwrite-dir` | FILL_ME_IN | preserve metadata of existing directories | |
| | `--one-top-level[=DIR]` | FEATURE | create a subdirectory to avoid having loose files extracted | Related to master/ layout ideas. |
| | `--overwrite` | TO_IMPLEMENT | overwrite existing files when extracting | Place currently copies; policy flags not wired. |
| | `--overwrite-dir` | TO_IMPLEMENT | overwrite metadata of existing directories when extracting (default) | |
| | `--recursive-unlink` | FILL_ME_IN | empty hierarchies prior to extracting directory | |
| | `--remove-files` | DISCARDED | remove files after adding them to the archive | Dangerous; not a tar-dedup goal. |
| | `--skip-old-files` | FEATURE | don't replace existing files when extracting, silently skip over them | |
| `-U` | `--unlink-first` | FILL_ME_IN | remove each file prior to extracting over it | |
| `-W` | `--verify` | FEATURE | attempt to verify the archive after writing it | Hash/rehash on extract is closer to our model. |

---

## Select output stream

| shortopt | longopt | implement | description | comment |
|----------|---------|-----------|-------------|---------|
| | `--ignore-command-error` | DISCARDED | ignore exit codes of children | No external compress-program children yet. |
| | `--no-ignore-command-error` | DISCARDED | treat non-zero exit codes of children as error | |
| `-O` | `--to-stdout` | FEATURE | extract files to standard output | Listed in Reflection specials. |
| | `--to-command=COMMAND` | DISCARDED | pipe extracted files to another program | |

---

## Handling of file attributes

| shortopt | longopt | implement | description | comment |
|----------|---------|-----------|-------------|---------|
| | `--atime-preserve[=METHOD]` | FILL_ME_IN | preserve access times on dumped files | Times captured in inventory; restore policy unclear. |
| | `--clamp-mtime` | DISCARDED | only set time when the file is more recent than what was given with `--mtime` | No `--mtime` force path yet. |
| | `--delay-directory-restore` | TO_IMPLEMENT | delay setting modification times and permissions of extracted directories until the end of extraction | Matches bottom-up permissions plan in TODO. |
| | `--group=NAME` | FEATURE | force NAME as group for added files | |
| | `--group-map=FILE` | FEATURE | use FILE to map file owner GIDs and names | |
| | `--mode=CHANGES` | FEATURE | force (symbolic) mode CHANGES for added files | |
| | `--mtime=DATE-OR-FILE` | FEATURE | set mtime for added files from DATE-OR-FILE | |
| `-m` | `--touch` | FEATURE | don't extract file modified time | |
| | `--no-delay-directory-restore` | TO_IMPLEMENT | cancel the effect of `--delay-directory-restore` | |
| | `--no-same-owner` | IMPLEMENTED | extract files as yourself (default for ordinary users) | Default when `--restore-owner` omitted. |
| | `--no-same-permissions` | FILL_ME_IN | apply the user's umask when extracting permissions from the archive | Permissions stage still stub. |
| | `--numeric-owner` | FEATURE | always use numbers for user/group names | DB stores numeric uid/gid already. |
| | `--owner=NAME` | FEATURE | force NAME as owner for added files | |
| | `--owner-map=FILE` | FEATURE | use FILE to map file owner UIDs and names | |
| `-p` | `--preserve-permissions`, `--same-permissions` | TO_IMPLEMENT | extract information about file permissions | Capture on archive done; apply on extract TODO. |
| | `--same-owner` | TO_IMPLEMENT | try extracting files with the same ownership as exists in the archive | CLI `--restore-owner` exists; full apply still TODO. |
| | `--sort=ORDER` | FILL_ME_IN | directory sorting order: none (default), name or inode | Staging orders by ext/size/id for compression. |
| `-s` | `--preserve-order`, `--same-order` | DISCARDED | member arguments are listed in the same order as the files in the archive | CLI file-list order not used. |

---

## Handling of extended file attributes

| shortopt | longopt | implement | description | comment |
|----------|---------|-----------|-------------|---------|
| | `--acls` | IMPLEMENTED | Enable the POSIX ACLs support | Captured at inventory (`do_posix_acl`); no CLI toggle yet. |
| | `--no-acls` | TO_IMPLEMENT | Disable the POSIX ACLs support | Config knob exists; not on CLI. |
| | `--no-selinux` | TO_IMPLEMENT | Disable the SELinux context support | Same. |
| | `--no-xattrs` | TO_IMPLEMENT | Disable extended attributes support | Same. |
| | `--selinux` | IMPLEMENTED | Enable the SELinux context support | Captured when available. |
| | `--xattrs` | IMPLEMENTED | Enable extended attributes support | Captured at inventory. |
| | `--xattrs-exclude=MASK` | FEATURE | specify the exclude pattern for xattr keys | |
| | `--xattrs-include=MASK` | FEATURE | specify the include pattern for xattr keys | |

---

## Device selection and switching

| shortopt | longopt | implement | description | comment |
|----------|---------|-----------|-------------|---------|
| | `--force-local` | DISCARDED | archive file is local even if it has a colon | Local paths only. |
| `-f` | `--file=ARCHIVE` | IMPLEMENTED | use archive file or device ARCHIVE | Both subcommands. |
| `-F` | `--info-script=NAME`, `--new-volume-script=NAME` | DISCARDED | run script at end of each tape (implies `-M`) | No multi-volume. |
| `-L` | `--tape-length=NUMBER` | DISCARDED | change tape after writing NUMBER x 1024 bytes | |
| `-M` | `--multi-volume` | DISCARDED | create/list/extract multi-volume archive | |
| | `--rmt-command=COMMAND` | DISCARDED | use given rmt COMMAND instead of rmt | |
| | `--rsh-command=COMMAND` | DISCARDED | use remote COMMAND instead of rsh | |
| | `--volno-file=FILE` | DISCARDED | use/update the volume number in FILE | |

---

## Device blocking

| shortopt | longopt | implement | description | comment |
|----------|---------|-----------|-------------|---------|
| `-b` | `--blocking-factor=BLOCKS` | DISCARDED | BLOCKS x 512 bytes per record | rust `tar` crate defaults; not exposed. |
| `-B` | `--read-full-records` | DISCARDED | reblock as we read (for 4.2BSD pipes) | |
| `-i` | `--ignore-zeros` | DISCARDED | ignore zeroed blocks in archive (means EOF) | Conflicts with our footer/EOF model. **Note:** tar-dedup uses `-i` for **input directory** on `archive`, not this GNU meaning. |
| | `--record-size=NUMBER` | DISCARDED | NUMBER of bytes per record, multiple of 512 | |

---

## Archive format selection

| shortopt | longopt | implement | description | comment |
|----------|---------|-----------|-------------|---------|
| `-H` | `--format=FORMAT` | FILL_ME_IN | create archive of the given format | rust `tar` writer format not user-selectable yet. |
| | `--old-archive`, `--portability` | DISCARDED | same as `--format=v7` | |
| | `--pax-option=keyword[[:]=value]...` | FILL_ME_IN | control pax keywords | |
| | `--posix` | FILL_ME_IN | same as `--format=posix` | |
| `-V` | `--label=TEXT` | DISCARDED | create archive with volume name TEXT | |

**FORMAT values (reference only, not flags):** `gnu`, `oldgnu`, `pax`, `posix`, `ustar`, `v7`.

---

## Compression options

| shortopt | longopt | implement | description | comment |
|----------|---------|-----------|-------------|---------|
| `-a` | `--auto-compress` | IMPLEMENTED | use archive suffix to determine the compression program | Default unless `--no-auto-compress`. |
| `-I` | `--use-compress-program=PROG` | FEATURE | filter through PROG (must accept `-d`) | Reflection: will support arbitrary compressors. |
| `-j` | `--bzip2` | IMPLEMENTED | filter the archive through bzip2 | Plus `--bzip-small`, `--level`. |
| `-J` | `--xz` | IMPLEMENTED | filter the archive through xz | Plus `--xz-extreme`, `--memlimit-compress`, `--level`, jobs. |
| | `--lzip` | DISCARDED | filter the archive through lzip | Not in supported crate set (Readme/Reflection). |
| | `--lzma` | DISCARDED | filter the archive through lzma | Use xz. |
| | `--lzop` | DISCARDED | filter the archive through lzop | |
| | `--no-auto-compress` | IMPLEMENTED | do not use archive suffix to determine the compression program | |
| | `--zstd` | IMPLEMENTED | filter the archive through zstd | |
| `-z` | `--gzip`, `--gunzip`, `--ungzip` | IMPLEMENTED | filter the archive through gzip | |
| `-Z` | `--compress`, `--uncompress` | DISCARDED | filter the archive through compress | Legacy; Reflection lists no useful args. |

Uncompressed plain tar is supported by omitting filters / non-compressed suffix (no dedicated short flag).

---

## Local file selection

| shortopt | longopt | implement | description | comment |
|----------|---------|-----------|-------------|---------|
| | `--backup[=CONTROL]` | DISCARDED | backup before removal, choose version CONTROL | Extract overwrite policy TBD without GNU backups. |
| | `--hard-dereference` | FILL_ME_IN | follow hard links; archive and dump the files they refer to | Dedup already collapses identical content. |
| `-h` | `--dereference` | FILL_ME_IN | follow symlinks; archive and dump the files they point to | Stage uses symlinks then tar follows (`follow_symlinks`); inventory stores link targets. GNU `-h` semantics may differ. |
| `-K` | `--starting-file=MEMBER-NAME` | DISCARDED | begin at member MEMBER-NAME when reading the archive | |
| | `--newer-mtime=DATE` | FEATURE | compare date and time when data changed only | |
| `-N` | `--newer=DATE-OR-FILE`, `--after-date=DATE-OR-FILE` | FEATURE | only store files newer than DATE-OR-FILE | |
| | `--one-file-system` | FEATURE | stay in local file system when creating archive | |
| `-P` | `--absolute-names` | FILL_ME_IN | don't strip leading `/`s from file names | Rel-paths from input root today. |
| | `--suffix=STRING` | DISCARDED | backup before removal, override usual suffix | With `--backup`. |

---

## File name transformations

| shortopt | longopt | implement | description | comment |
|----------|---------|-----------|-------------|---------|
| | `--strip-components=NUMBER` | FEATURE | strip NUMBER leading components from file names on extraction | |
| | `--transform=EXPRESSION`, `--xform=EXPRESSION` | FEATURE | use sed replace EXPRESSION to transform file names | |

---

## Informative output

| shortopt | longopt | implement | description | comment |
|----------|---------|-----------|-------------|---------|
| | `--checkpoint[=NUMBER]` | FEATURE | display progress messages every NUMBERth record (default 10) | We use indicatif progress instead. |
| | `--checkpoint-action=ACTION` | DISCARDED | execute ACTION on each checkpoint | |
| | `--full-time` | FILL_ME_IN | print file time to its full resolution | |
| | `--index-file=FILE` | FEATURE | send verbose output to FILE | TODO: good logging / separate log stream. |
| `-l` | `--check-links` | FILL_ME_IN | print a message if not all links are dumped | |
| | `--no-quote-chars=STRING` | DISCARDED | disable quoting for characters from STRING | |
| | `--quote-chars=STRING` | DISCARDED | additionally quote characters from STRING | |
| | `--quoting-style=STYLE` | DISCARDED | set name quoting style | |
| `-R` | `--block-number` | DISCARDED | show block number within archive with each message | |
| | `--show-defaults` | FEATURE | show tar defaults | Could dump Config defaults. |
| | `--show-omitted-dirs` | FILL_ME_IN | when listing or extracting, list each directory that does not match search criteria | |
| | `--show-snapshot-field-ranges` | DISCARDED | show valid ranges for snapshot-file fields | GNU incremental snapshots. |
| | `--show-transformed-names`, `--show-stored-names` | FILL_ME_IN | show file or archive names after transformation | |
| | `--totals[=SIGNAL]` | FEATURE | print total bytes after processing the archive | Meta tallies exist internally. |
| | `--utc` | FILL_ME_IN | print file modification times in UTC | DB stores UTC timestamps. |
| `-v` | `--verbose` | TO_IMPLEMENT | verbosely list files processed | TODO: different verbosities. |
| | `--warning=KEYWORD` | FEATURE | warning control | |
| `-w` | `--interactive`, `--confirmation` | DISCARDED | ask for confirmation for every action | Non-interactive pipeline tool. |

---

## Compatibility options

| shortopt | longopt | implement | description | comment |
|----------|---------|-----------|-------------|---------|
| `-o` | | DISCARDED | when creating, same as `--old-archive`; when extracting, same as `--no-same-owner` | Ambiguous GNU legacy short opt. |

---

## Other options

| shortopt | longopt | implement | description | comment |
|----------|---------|-----------|-------------|---------|
| `-?` | `--help` | IMPLEMENTED | give this help list | clap `--help` on tar-dedup CLI. |
| | `--restrict` | DISCARDED | disable use of some potentially harmful options | |
| | `--usage` | IMPLEMENTED | give a short usage message | clap usage. |
| | `--version` | TO_IMPLEMENT | print program version | TODO: add tool version to metadata; clap version may be partial. |

---

## Footer (GNU help notes)

Mandatory or optional arguments to long options are also mandatory or optional for any corresponding short options.

The backup suffix is `~`, unless set with `--suffix` or `SIMPLE_BACKUP_SUFFIX`.
The version control may be set with `--backup` or `VERSION_CONTROL`, values are:

| Value | Meaning |
|-------|---------|
| `none`, `off` | never make backups |
| `t`, `numbered` | make numbered backups |
| `nil`, `existing` | numbered if numbered backups exist, simple otherwise |
| `never`, `simple` | always make simple backups |

Valid arguments for the `--quoting-style` option are: `literal`, `shell`, `shell-always`, `shell-escape`, `shell-escape-always`, `c`, `c-maybe`, `escape`, `locale`, `clocale`.

**This GNU tar’s defaults** (from the captured help, not tar-dedup):

```
--format=gnu -f- -b20 --quoting-style=escape
--rmt-command=…/gnutar-1.35/libexec/rmt
```

---

## tar-dedup options without a GNU tar equivalent

Not from `tar --help`; listed so the matrix stays honest about our CLI.

| shortopt | longopt | implement | description | comment |
|----------|---------|-----------|-------------|---------|
| `-i` | | IMPLEMENTED | input directory to snapshot (`archive`) | Collides with GNU `-i` `--ignore-zeros` (DISCARDED above). |
| | `--jobs` | IMPLEMENTED | max concurrent workers | |
| | `--level` | IMPLEMENTED | compression level | |
| | `--xz-extreme` | IMPLEMENTED | xz extreme preset bit | |
| | `--bzip-small` | IMPLEMENTED | bzip2 small/100k blocks | |
| | `--memlimit-compress` | IMPLEMENTED | xz encoder RAM cap | |
| | `--page-size` | IMPLEMENTED | sparse/hash zero-page size | |
| | `--min-pages` | IMPLEMENTED | min empty pages before sparsify rewrite | |
| | `--resume` / `--fresh` | IMPLEMENTED | start policy | |
| | `--keep-db` / `--keep-stage` | IMPLEMENTED | post-success cleanup keeps | |
| | `--exit-after-stage` | IMPLEMENTED | stop after named pipeline phase | |
| | `--restore-owner` | TO_IMPLEMENT | extract uid/gid | Flag present; apply path incomplete. |
| | `--bridge` | FEATURE | dedup + link back into place | Reflection. |
| | `--link-in-place` | FEATURE | extract tree as links into stage | Reflection. |
| | `--retry-missing-sha` | FEATURE | archive files missing sha | Config knob / Reflection. |
