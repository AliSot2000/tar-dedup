# Tar-Dedup
A `tar` "wrapper" to deduplicate the file-tree prior to archiving to save extra space. Adds capability to interrupt the 
creation of large tar archives at the cost of lower compression ratios for compressed archives.

Importantly: **THIS IS UNSTABLE CODE!!!**  
Any archives created with this tool prior to the v1.0.0 release have **no guarantees** that future commits will still 
decompress them correctly and successfully! If you need this tool done soon, create an issue or star the project to 
show interest.