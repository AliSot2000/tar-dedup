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

- CTime
- Mode
- Archived session finished at empty

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




# Compression Algo Overview
```bash
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

=> LZMA = XZ

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