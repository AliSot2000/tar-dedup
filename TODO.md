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
- 