# Build tar-dedup

**Motivation**   
Given Datasets that people worked on, you cannot assume that the file trees are free of duplicates. However, 
hard deduplicating is an O(n^2) problem. To avid this, we are going to compute an easy hash (sha1) and only compare 
files on a binary level, if they have a matching hash and file size. If they have, we are going to use transitivity. 
I.e. suppose we have three files a, b, c with matching hash: We only check a, b and a, c if they are identical 
(and drop b, c). Of course, suppose we have 5 files with identical hash and file sizes but with different binary data, 
we get something like this: a != b, a = c, a != d, b = d, a = e.

As such, for our first attempt, we shall write a program tha does the following. 

For compression:
1. Create sqlite.db file for the given task.
2. use ls -R to get a full list of the tree we want to archive and add the given information into the sqlite db 
(so paths, file sizes, user/group, modify, create, access time) as well as a timestamp in utc of the current time 
when this snapshot was taken.
3. For all files inside the directory compute a sha1 fingerprint and add this fingerprint to the db
4. Search the db for duplicate sha prints and file sizes. If they exist, for each of the duplicates, start a binary 
comparison of the file
5. For all files in step 4. that passed the check i.e. were identical, add in another column the parent to the first 
file with the matching binary content.
6. create a stage directory where all files are ln -s ed into. The new name is the file hash in base64 and the file 
size in base64.
7. Copy the database also into the folder, making sure to have commited the transactions and detached.
8. Tar (with or without compression) but dereferencing the sym links  to a single file.
9. Done

And in reverse:
1. Untar the sqlite.db
2. Create the subtree of files that was tared, creating empty place-holder files for the moment.
3. Untar file by file into the root directory of the output, and once the file is untared, move it to all necessary locations.
4. Apply the access, modify, create times, if they are not preserved
5. Optionally, set user,group and permissions.

The interface for the program should 
- take -f (like tar for file)
- take -i (for input directory - will change later on)
- take -C (for output directory)

Feat:
- Docker style progress (bottom all files, top each file being hashed)
- Pass compression args to the different compression algos (e.g. -9 for xz) 
- Start compression during dedup.
- Check file times when dedup and hash are performed.
- Create an interruptible tar when compressing dir without any duplicates
- Fix permissions when extracting
- Can we skip the stage directory
- Recreate dir (create empty dirs when they are empty)
- CTime
- Mode
- Archived session finished at empty
- Sparse tar archives. 
  - General
  - Write as Sparse (detect if a long stretch of zeros is present and skip it.)
  - Write Sparse Canonical prior to Tar writer.
- ACL
- SELinux
- XAttrs
- Abort on Change
- Check on extraction that the file hashes are still consistent.
- Progress memory in .cache dir 

- Only update the ETA when we are actively consuming data, not when we are compressing buffer.
- _ separator in staged name
- Detect identical hash with different file extension.
- -I will be supported to support arbitrary compression algos.
- `zstd, gzip, xz`

Compression Algo extractions:
- `bzip2`  
  `-s --small`          use less memory (at most 2500k)  
  `-1 .. -9`            set block size to 100k .. 900k
- `xz`  
  `-0 ... -9`           compression preset; default is 6; take compressor *and* decompressor memory usage into account before using 7-9!  
  `-e, --extreme`       try to improve compression ratio by using more CPU time; does not affect decompressor memory requirements  
  `-T, --threads=NUM`   use at most NUM threads; the default is 0 which uses as many threads as there are processor cores
- `lzip`  
  `-0 .. -9`                       set compression level [default 6]  
  `-m, --match-length=<bytes>`     set match length limit in bytes [36] (more optional)  
  `-s, --dictionary-size=<bytes>`  set dictionary size limit in bytes [8 MiB]
- `lzma`  
   Same as xz from arguments.
- `lzop`  
  `-1     compress faster                   -9    compress better`
- `zstd`  
  `-#`                            Desired compression level, where `#` is a number between 1 and 19; lower numbers provide faster compression, higher numbers yield better compression ratios. [Default: 3]
- `gzip`  
  `-1, --fast`        compress faster  
  `-9, --best`        compress better
- `compress`
  None

# Filesystem Nodes supported
- Regular File
- Directory
- Symlink
- Hardlink (duh)
- FIFO
- ~~Socket~~
- Block Devices (root!)
- Character Devices (root!)
- (Doors) - Solaris possible feature
- (Whiteout) - BSD possible feature.
- Doors and Whiteout are discontinued for the moment.
- However, sockets will be indexed but not created in any capacity.

# Compression Algo Overview
## bzip2
```
bzip2, a block-sorting file compressor.  Version 1.0.8, 13-Jul-2019.

   usage: bzip2 [flags and input files in any order]

   -h --help           print this message
   -d --decompress     force decompression
   -z --compress       force compression
   -k --keep           keep (don't delete) input files
   -f --force          overwrite existing output files
   -t --test           test compressed file integrity
   -c --stdout         output to standard out
   -q --quiet          suppress noncritical error messages
   -v --verbose        be verbose (a 2nd -v gives more)
   -L --license        display software version & license
   -V --version        display software version & license
   -s --small          use less memory (at most 2500k)
   -1 .. -9            set block size to 100k .. 900k
   --fast              alias for -1
   --best              alias for -9

   If invoked as `bzip2', default action is to compress.
              as `bunzip2',  default action is to decompress.
              as `bzcat', default action is to decompress to stdout.

   If no file names are given, bzip2 compresses or decompresses
   from standard input to standard output.  You can combine
   short flags, so `-v -4' means the same as -v4 or -4v, &c.
```
## The xz command
```
Usage: xz [OPTION]... [FILE]...
Compress or decompress FILEs in the .xz format.

Mandatory arguments to long options are mandatory for short options too.

  -z, --compress      force compression
  -d, --decompress    force decompression
  -t, --test          test compressed file integrity
  -l, --list          list information about .xz files
  -k, --keep          keep (don't delete) input files
  -f, --force         force overwrite of output file and (de)compress links
  -c, --stdout        write to standard output and don't delete input files
  -0 ... -9           compression preset; default is 6; take compressor *and*
                      decompressor memory usage into account before using 7-9!
  -e, --extreme       try to improve compression ratio by using more CPU time;
                      does not affect decompressor memory requirements
  -T, --threads=NUM   use at most NUM threads; the default is 0 which uses as
                      many threads as there are processor cores
  -q, --quiet         suppress warnings; specify twice to suppress errors too
  -v, --verbose       be verbose; specify twice for even more verbose
  -h, --help          display this short help and exit
  -H, --long-help     display the long help (lists also the advanced options)
  -V, --version       display the version number and exit

With no FILE, or when FILE is -, read standard input.

Report bugs to <xz@tukaani.org> (in English or Finnish).
XZ Utils home page: <https://tukaani.org/xz/>
```
## The lzip command
```
Lzip is a lossless data compressor.

Usage: lzip [options] [files]

Options:
  -h                             display usage help and exit
      --help                     display full help and exit
  -V, --version                  output version information and exit
  -a, --trailing-error           exit with error status if trailing data
  -b, --member-size=<bytes>      set member size limit of multimember files
  -c, --stdout                   write to standard output, keep input files
  -d, --decompress               decompress, test compressed file integrity
  -f, --force                    overwrite existing output files
  -F, --recompress               force re-compression of compressed files
  -k, --keep                     keep (don't delete) input files
  -l, --list                     print (un)compressed file sizes
  -m, --match-length=<bytes>     set match length limit in bytes [36]
  -o, --output=<file>            write to <file>, keep input files
  -q, --quiet                    suppress all messages
  -s, --dictionary-size=<bytes>  set dictionary size limit in bytes [8 MiB]
  -S, --volume-size=<bytes>      set volume size limit in bytes
  -t, --test                     test compressed file integrity
  -v, --verbose                  be verbose (a 2nd -v gives more)
  -0 .. -9                       set compression level [default 6]
      --loose-trailing           allow trailing data seeming corrupt header

If no file names are given, or if a file is '-', lzip compresses or
decompresses from standard input to standard output.
```
## The lzma command
```
Usage: lzma [OPTION]... [FILE]...
Compress or decompress FILEs in the .xz format.

Mandatory arguments to long options are mandatory for short options too.

  -z, --compress      force compression
  -d, --decompress    force decompression
  -t, --test          test compressed file integrity
  -l, --list          list information about .xz files
  -k, --keep          keep (don't delete) input files
  -f, --force         force overwrite of output file and (de)compress links
  -c, --stdout        write to standard output and don't delete input files
  -0 ... -9           compression preset; default is 6; take compressor *and*
                      decompressor memory usage into account before using 7-9!
  -e, --extreme       try to improve compression ratio by using more CPU time;
                      does not affect decompressor memory requirements
  -T, --threads=NUM   use at most NUM threads; the default is 0 which uses as
                      many threads as there are processor cores
  -q, --quiet         suppress warnings; specify twice to suppress errors too
  -v, --verbose       be verbose; specify twice for even more verbose
  -h, --help          display this short help and exit
  -H, --long-help     display the long help (lists also the advanced options)
  -V, --version       display the version number and exit

With no FILE, or when FILE is -, read standard input.

Report bugs to <xz@tukaani.org> (in English or Finnish).
XZ Utils home page: <https://tukaani.org/xz/>
```
## The lzop command
```
                          Lempel-Ziv-Oberhumer Packer
                           Copyright (C) 1996 - 2017
lzop v1.04         Markus Franz Xaver Johannes Oberhumer         Aug 10th 2017

Usage: lzop [-dxlthIVL19] [-qvcfFnNPkUp] [-o file] [-S suffix] [file..]

Commands:
  -1     compress faster                   -9    compress better
  -d     decompress                        -x    extract (same as -dPp)
  -l     list compressed file              -I    display system information
  -t     test compressed file              -V    display version number
  -h     give this help                    -L    display software license
Options:
  -q     be quiet                          -v       be verbose
  -c     write on standard output          -oFILE   write output to 'FILE'
  -p     write output to current dir       -pDIR    write to path 'DIR'
  -f     force overwrite of output files
  -n     do not restore the original file name (default)
  -N     restore the original file name
  -P     restore or save the original path and file name
  -S.suf use suffix .suf on compressed files
  -U     delete input files after successful operation (like gzip and bzip2)
  file.. files to (de)compress. If none given, try standard input.
```
## The zstd command
```
Compress or decompress the INPUT file(s); reads from STDIN if INPUT is `-` or not provided.

Usage: zstd [OPTIONS...] [INPUT... | -] [-o OUTPUT]

Options:
  -o OUTPUT                     Write output to a single file, OUTPUT.
  -k, --keep                    Preserve INPUT file(s). [Default] 
  --rm                          Remove INPUT file(s) after successful (de)compression.

  -#                            Desired compression level, where `#` is a number between 1 and 19;
                                lower numbers provide faster compression, higher numbers yield
                                better compression ratios. [Default: 3]

  -d, --decompress              Perform decompression.
  -D DICT                       Use DICT as the dictionary for compression or decompression.

  -f, --force                   Disable input and output checks. Allows overwriting existing files,
                                receiving input from the console, printing output to STDOUT, and
                                operating on links, block devices, etc. Unrecognized formats will be
                                passed-through through as-is.

  -h                            Display short usage and exit.
  -H, --help                    Display full help and exit.
  -V, --version                 Display the program version and exit.
```
## The gzip command
```
Usage: gzip [OPTION]... [FILE]...
Compress or uncompress FILEs (by default, compress FILES in-place).

Mandatory arguments to long options are mandatory for short options too.

  -c, --stdout      write on standard output, keep original files unchanged
  -d, --decompress  decompress
  -f, --force       force overwrite of output file and compress links
  -h, --help        give this help
  -k, --keep        keep (don't delete) input files
  -l, --list        list compressed file contents
  -L, --license     display software license
  -n, --no-name     do not save or restore the original name and timestamp
  -N, --name        save or restore the original name and timestamp
  -q, --quiet       suppress all warnings
  -r, --recursive   operate recursively on directories
      --rsyncable   make rsync-friendly archive
  -S, --suffix=SUF  use suffix SUF on compressed files
      --synchronous synchronous output (safer if system crashes, but slower)
  -t, --test        test compressed file integrity
  -v, --verbose     verbose mode
  -V, --version     display version number
  -1, --fast        compress faster
  -9, --best        compress better

With no FILE, or when FILE is -, read standard input.

Report bugs to <bug-gzip@gnu.org>.
```
## The compress command
```
Usage: compress [-dfhvcVr] [-b maxbits] [--] [path ...]
  --   Halt option processing and treat all remaining args as paths.
  -d   If given, decompression is done instead.
  -c   Write output on stdout, don't remove original.
  -k   Keep input files (do not automatically remove).
  -b   Parameter limits the max number of bits/code.
  -f   Forces output file to be generated, even if one already.
       exists, and even if no space is saved by compressing.
       If -f is not used, the user will be prompted if stdin is.
       a tty, otherwise, the output file will not be overwritten.
  -h   This help output.
  -v   Write compression statistics.
  -V   Output version and compile options.
  -r   Recursive. If a path is a directory, compress everything in it.
```

# The Tar command
Classify flag support into: [Done, TODO, Partial, Feature, Omitted]
### Main operation mode:
- `-A, --catenate, --concatenate`: Omitted
- `-c, --create`: Done
- `--delete`: Omitted
- `-d, --diff, --compare`: Feature
- `-r, --append`: Omitted
- `--test-label`: Omitted
- `-t, --list`: TODO
- `-u, --update`: Omitted
- `-x, --extract, --get`: Partial


```bash
Usage: tar [OPTION...] [FILE]...
GNU 'tar' saves many files together into a single tape or disk archive, and can
restore individual files from the archive.

Examples:
  tar -cf archive.tar foo bar  # Create archive.tar from files foo and bar.
  tar -tvf archive.tar         # List all files in archive.tar verbosely.
  tar -xf archive.tar          # Extract all files from archive.tar.

 Main operation mode:
  -A, --catenate, --concatenate   append tar files to an archive
  -c, --create               create a new archive
      --delete               delete from the archive (not on mag tapes!)
  -d, --diff, --compare      find differences between archive and file system
  -r, --append               append files to the end of an archive
      --test-label           test the archive volume label and exit
  -t, --list                 list the contents of an archive
  -u, --update               only append files newer than copy in archive
  -x, --extract, --get       extract files from an archive

 Operation modifiers:

      --check-device         check device numbers when creating incremental
                             archives (default)
  -g, --listed-incremental=FILE   handle new GNU-format incremental backup
  -G, --incremental          handle old GNU-format incremental backup
      --hole-detection=TYPE  technique to detect holes
      --ignore-failed-read   do not exit with nonzero on unreadable files
      --level=NUMBER         dump level for created listed-incremental archive
      --no-check-device      do not check device numbers when creating
                             incremental archives
      --no-seek              archive is not seekable
  -n, --seek                 archive is seekable
      --occurrence[=NUMBER]  process only the NUMBERth occurrence of each file
                             in the archive; this option is valid only in
                             conjunction with one of the subcommands --delete,
                             --diff, --extract or --list and when a list of
                             files is given either on the command line or via
                             the -T option; NUMBER defaults to 1
      --sparse-version=MAJOR[.MINOR]
                             set version of the sparse format to use (implies
                             --sparse)
  -S, --sparse               handle sparse files efficiently

 Local file name selection:
      --add-file=FILE        add given FILE to the archive (useful if its name
                             starts with a dash)
  -C, --directory=DIR        change to directory DIR
      --exclude=PATTERN      exclude files, given as a PATTERN
      --exclude-backups      exclude backup and lock files
      --exclude-caches       exclude contents of directories containing
                             CACHEDIR.TAG, except for the tag file itself
      --exclude-caches-all   exclude directories containing CACHEDIR.TAG
      --exclude-caches-under exclude everything under directories containing
                             CACHEDIR.TAG
      --exclude-ignore=FILE  read exclude patterns for each directory from
                             FILE, if it exists
      --exclude-ignore-recursive=FILE
                             read exclude patterns for each directory and its
                             subdirectories from FILE, if it exists
      --exclude-tag=FILE     exclude contents of directories containing FILE,
                             except for FILE itself
      --exclude-tag-all=FILE exclude directories containing FILE
      --exclude-tag-under=FILE   exclude everything under directories
                             containing FILE
      --exclude-vcs          exclude version control system directories
      --exclude-vcs-ignores  read exclude patterns from the VCS ignore files
      --no-null              disable the effect of the previous --null option
      --no-recursion         avoid descending automatically in directories
      --no-unquote           do not unquote input file or member names
      --no-verbatim-files-from   -T treats file names starting with dash as
                             options (default)
      --null                 -T reads null-terminated names; implies
                             --verbatim-files-from
      --recursion            recurse into directories (default)
  -T, --files-from=FILE      get names to extract or create from FILE
      --unquote              unquote input file or member names (default)
      --verbatim-files-from  -T reads file names verbatim (no escape or option
                             handling)
  -X, --exclude-from=FILE    exclude patterns listed in FILE

 File name matching options (affect both exclude and include patterns):

      --anchored             patterns match file name start
      --ignore-case          ignore case
      --no-anchored          patterns match after any '/' (default for
                             exclusion)
      --no-ignore-case       case sensitive matching (default)
      --no-wildcards         verbatim string matching
      --no-wildcards-match-slash   wildcards do not match '/'
      --wildcards            use wildcards (default for exclusion)
      --wildcards-match-slash   wildcards match '/' (default for exclusion)

 Overwrite control:

      --keep-directory-symlink   preserve existing symlinks to directories when
                             extracting
      --keep-newer-files     don't replace existing files that are newer than
                             their archive copies
  -k, --keep-old-files       don't replace existing files when extracting,
                             treat them as errors
      --no-overwrite-dir     preserve metadata of existing directories
      --one-top-level[=DIR]  create a subdirectory to avoid having loose files
                             extracted
      --overwrite            overwrite existing files when extracting
      --overwrite-dir        overwrite metadata of existing directories when
                             extracting (default)
      --recursive-unlink     empty hierarchies prior to extracting directory
      --remove-files         remove files after adding them to the archive
      --skip-old-files       don't replace existing files when extracting,
                             silently skip over them
  -U, --unlink-first         remove each file prior to extracting over it
  -W, --verify               attempt to verify the archive after writing it

 Select output stream:

      --ignore-command-error ignore exit codes of children
      --no-ignore-command-error   treat non-zero exit codes of children as
                             error
  -O, --to-stdout            extract files to standard output
      --to-command=COMMAND   pipe extracted files to another program

 Handling of file attributes:

      --atime-preserve[=METHOD]   preserve access times on dumped files, either
                             by restoring the times after reading
                             (METHOD='replace'; default) or by not setting the
                             times in the first place (METHOD='system')
      --clamp-mtime          only set time when the file is more recent than
                             what was given with --mtime
      --delay-directory-restore   delay setting modification times and
                             permissions of extracted directories until the end
                             of extraction
      --group=NAME           force NAME as group for added files
      --group-map=FILE       use FILE to map file owner GIDs and names
      --mode=CHANGES         force (symbolic) mode CHANGES for added files
      --mtime=DATE-OR-FILE   set mtime for added files from DATE-OR-FILE
  -m, --touch                don't extract file modified time
      --no-delay-directory-restore
                             cancel the effect of --delay-directory-restore
                             option
      --no-same-owner        extract files as yourself (default for ordinary
                             users)
      --no-same-permissions  apply the user's umask when extracting permissions
                             from the archive (default for ordinary users)
      --numeric-owner        always use numbers for user/group names
      --owner=NAME           force NAME as owner for added files
      --owner-map=FILE       use FILE to map file owner UIDs and names
  -p, --preserve-permissions, --same-permissions
                             extract information about file permissions
                             (default for superuser)
      --same-owner           try extracting files with the same ownership as
                             exists in the archive (default for superuser)
      --sort=ORDER           directory sorting order: none (default), name or
                             inode
  -s, --preserve-order, --same-order
                             member arguments are listed in the same order as
                             the files in the archive

 Handling of extended file attributes:

      --acls                 Enable the POSIX ACLs support
      --no-acls              Disable the POSIX ACLs support
      --no-selinux           Disable the SELinux context support
      --no-xattrs            Disable extended attributes support
      --selinux              Enable the SELinux context support
      --xattrs               Enable extended attributes support
      --xattrs-exclude=MASK  specify the exclude pattern for xattr keys
      --xattrs-include=MASK  specify the include pattern for xattr keys

 Device selection and switching:

      --force-local          archive file is local even if it has a colon
  -f, --file=ARCHIVE         use archive file or device ARCHIVE
  -F, --info-script=NAME, --new-volume-script=NAME
                             run script at end of each tape (implies -M)
  -L, --tape-length=NUMBER   change tape after writing NUMBER x 1024 bytes
  -M, --multi-volume         create/list/extract multi-volume archive
      --rmt-command=COMMAND  use given rmt COMMAND instead of rmt
      --rsh-command=COMMAND  use remote COMMAND instead of rsh
      --volno-file=FILE      use/update the volume number in FILE

 Device blocking:

  -b, --blocking-factor=BLOCKS   BLOCKS x 512 bytes per record
  -B, --read-full-records    reblock as we read (for 4.2BSD pipes)
  -i, --ignore-zeros         ignore zeroed blocks in archive (means EOF)
      --record-size=NUMBER   NUMBER of bytes per record, multiple of 512

 Archive format selection:

  -H, --format=FORMAT        create archive of the given format

 FORMAT is one of the following:
    gnu                      GNU tar 1.13.x format
    oldgnu                   GNU format as per tar <= 1.12
    pax                      POSIX 1003.1-2001 (pax) format
    posix                    same as pax
    ustar                    POSIX 1003.1-1988 (ustar) format
    v7                       old V7 tar format

      --old-archive, --portability
                             same as --format=v7
      --pax-option=keyword[[:]=value][,keyword[[:]=value]]...
                             control pax keywords
      --posix                same as --format=posix
  -V, --label=TEXT           create archive with volume name TEXT; at
                             list/extract time, use TEXT as a globbing pattern
                             for volume name

 Compression options:

  -a, --auto-compress        use archive suffix to determine the compression
                             program
  -I, --use-compress-program=PROG
                             filter through PROG (must accept -d)
  -j, --bzip2                filter the archive through bzip2
  -J, --xz                   filter the archive through xz
      --lzip                 filter the archive through lzip
      --lzma                 filter the archive through xz
      --lzop                 filter the archive through lzop
      --no-auto-compress     do not use archive suffix to determine the
                             compression program
      --zstd                 filter the archive through zstd
  -z, --gzip, --gunzip, --ungzip   filter the archive through gzip
  -Z, --compress, --uncompress   filter the archive through compress

 Local file selection:

      --backup[=CONTROL]     backup before removal, choose version CONTROL
      --hard-dereference     follow hard links; archive and dump the files they
                             refer to
  -h, --dereference          follow symlinks; archive and dump the files they
                             point to
  -K, --starting-file=MEMBER-NAME
                             begin at member MEMBER-NAME when reading the
                             archive
      --newer-mtime=DATE     compare date and time when data changed only
  -N, --newer=DATE-OR-FILE, --after-date=DATE-OR-FILE
                             only store files newer than DATE-OR-FILE
      --one-file-system      stay in local file system when creating archive
  -P, --absolute-names       don't strip leading '/'s from file names
      --suffix=STRING        backup before removal, override usual suffix ('~'
                             unless overridden by environment variable
                             SIMPLE_BACKUP_SUFFIX)

 File name transformations:

      --strip-components=NUMBER   strip NUMBER leading components from file
                             names on extraction
      --transform=EXPRESSION, --xform=EXPRESSION
                             use sed replace EXPRESSION to transform file
                             names

 Informative output:

      --checkpoint[=NUMBER]  display progress messages every NUMBERth record
                             (default 10)
      --checkpoint-action=ACTION   execute ACTION on each checkpoint
      --full-time            print file time to its full resolution
      --index-file=FILE      send verbose output to FILE
  -l, --check-links          print a message if not all links are dumped
      --no-quote-chars=STRING   disable quoting for characters from STRING
      --quote-chars=STRING   additionally quote characters from STRING
      --quoting-style=STYLE  set name quoting style; see below for valid STYLE
                             values
  -R, --block-number         show block number within archive with each message
                            
      --show-defaults        show tar defaults
      --show-omitted-dirs    when listing or extracting, list each directory
                             that does not match search criteria
      --show-snapshot-field-ranges
                             show valid ranges for snapshot-file fields
      --show-transformed-names, --show-stored-names
                             show file or archive names after transformation
      --totals[=SIGNAL]      print total bytes after processing the archive;
                             with an argument - print total bytes when this
                             SIGNAL is delivered; Allowed signals are: SIGHUP,
                             SIGQUIT, SIGINT, SIGUSR1 and SIGUSR2; the names
                             without SIG prefix are also accepted
      --utc                  print file modification times in UTC
  -v, --verbose              verbosely list files processed
      --warning=KEYWORD      warning control
  -w, --interactive, --confirmation
                             ask for confirmation for every action

 Compatibility options:

  -o                         when creating, same as --old-archive; when
                             extracting, same as --no-same-owner

 Other options:

  -?, --help                 give this help list
      --restrict             disable use of some potentially harmful options
      --usage                give a short usage message
      --version              print program version

Mandatory or optional arguments to long options are also mandatory or optional
for any corresponding short options.

The backup suffix is '~', unless set with --suffix or SIMPLE_BACKUP_SUFFIX.
The version control may be set with --backup or VERSION_CONTROL, values are:

  none, off       never make backups
  t, numbered     make numbered backups
  nil, existing   numbered if numbered backups exist, simple otherwise
  never, simple   always make simple backups

Valid arguments for the --quoting-style option are:

  literal
  shell
  shell-always
  shell-escape
  shell-escape-always
  c
  c-maybe
  escape
  locale
  clocale

*This* tar defaults to:
--format=gnu -f- -b20 --quoting-style=escape --rmt-command=/usr/sbin/rmt
--rsh-command=/usr/bin/rsh
```