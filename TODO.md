# TODO for the current scope of the project.

## Future Features:
- Sparse files (detect them ahead of time)
- Sparsification of files before archive (build holes on os side)
- Filter ACLS,
- [ ] Support for Windows permissions
- [ ] Only report Permission error / ... once (deduplicate the same error.)
- [ ] Integrated logs? (Capture relevant logs / warning to a separate table?)
- [ ] Retry errored files during tar step

## General:
- [ ] Testing
- [ ] Different Verbosities
- [ ] Sequential / Parallel where possible
- [ ] Add Version of Tool to metadata
- [ ] Add Platform to metadata
- [ ] Exclude.
- [ ] Good logging

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
- [ ] Add separate log stream
- [X] Store ln -s target for windows (file/dir (recursively resolve softlinks until cycle or non-softlink file is reached))

### Filter
- [ ] Research Filtering options of tar
- [ ] Implement filtering on top of paths in the database.

### Hash
- [ ] Docker style output (by default)
- [ ] Check file for changes (based on times)
- [X] Added sparse file check. 

### Dedup
Should be done?
- [ ] Better logging?
- [ ] Run in parallel and do so very well

### Sparsified
- [ ] Create sparse files. 

### Staging
- [ ] Basically done?

### Archive
- [X] Finish the FileEntry and ContentID structs
- [ ] Finish the different compression algorithms
- [ ] Finish plane
- [ ] Finish shell-out

### Scan/Extract
- [ ] Live check the files
- [ ] Potentially data driven file extraction

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