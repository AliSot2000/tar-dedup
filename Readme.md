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
- [ ] Something else

Compile hints on Debian:  
`sudo apt install libselinux-dev libclang-dev clang`

The current offering of compression algorithms is due to both the availability of rust crates for the algorithms and 
also a thought of recentness of the algorithm. The program will default to the most aggressive compression possible 
(xz, cpu_count threads, extreme compression profile and no regard for RAM consumption) if the options of the command 
exposes du not suite your needs, the command offers to write to a simple tar file you can compress yourself 
(and also subsequently decompress if the format is not supported.)