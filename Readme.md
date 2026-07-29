# Tar-Dedup
A `tar` "wrapper" to deduplicate the file-tree prior to archiving to save extra space. Adds capability to interrupt the 
creation of large tar archives at the cost of lower compression ratios for compressed archives.

Importantly: **THIS IS UNSTABLE CODE!!!**  
Any archives created with this tool prior to the v1.0.0 release have **no guarantees** that future commits will still 
decompress them correctly and successfully! If you need this tool done soon, create an issue or star the project to 
show interest.

Features:
- [ ] For debugging purposes, the `force-utf8-encoding` can be used for better readability of the database. However, 
**if at any point a non-utf8 compliant byte is found, the binary will panic**. 

Compile hints on Debian:  
`sudo apt install libselinux-dev libclang-dev clang`

The current offering of compression algorithms is due to both the availability of rust crates for the algorithms and 
also a thought of recentness of the algorithm. The program will default to the most aggressive compression possible 
(xz, cpu_count threads, extreme compression profile and no regard for RAM consumption) if the options of the command 
exposes du not suite your needs, the command offers to write to a simple tar file you can compress yourself 
(and also subsequently decompress if the format is not supported.)

### Returned Archive
Note on the returned file. The file is a valid archive with an appended footer. This looks like this:
```
[ compressed(tar archive) | MAGIC | final-database.sqlite | sha1 of database | MAGIC | u64 offset ]
```
- `MAGIC` is `"Tar-Dedup-SQLite-Footer"` - to make finding the footer by hand analyzing the file with a program like less.
- `final-database.sqlite` is the database after the session was closed and contains the finished session info. 
  Besides that it allows tar-dedup to instantly have the manifest in the correct state rather than having to apply one 
  snapshot after the other.
- `sha1 of database` is the hash of the database to check for corruption.
- `u64 offset` is the offset of the first MAGIC string relative to the beginning of the file

The returned file can be opened by associated compressor or the tar archive. The footer will most likely be ignored 
by the compression algorithm and the tar stream.