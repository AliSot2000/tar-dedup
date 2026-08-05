# Tar-Dedup
A `tar` "wrapper" to deduplicate the file-tree prior to archiving to save extra space. It capability to interrupt the 
creation of large tar archives at the cost of lower compression ratios for compressed archives.

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