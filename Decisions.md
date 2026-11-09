# Keeping track of design decisions that are not properly written down otherwise

## Strip common prefix not per source + delete rows
Due to the logic how it is implemented, we don't do it per source to avoid any unnoticed clobbering of files during the 
place phase.    
We delete the rows, to avoid any "escape" of the permissions pass. We are allowed to work inside --extract-to 
(+ --one-top-level) but we don't want to modify the directory itself. So having a file_id attached to a row point now 
effective_extraction_root() would actually violate this notion.

## Governor for IO Ops
A Governor for IO ops was added as a suggestion by the LLM to improve the performance of massively parallel reads on 
hard drives. After preliminary testing and also preliminary impression, it changed the performance in a barely 
noticeable regime and considering that other processes are running on my test machine which I could not isolate, the 
experiment was aborted and the progress stored in `zzz_io_govenor_experiment` for the moment.

## Work-Dir persists after remove with Dir-Not-Empty Error
Result of NTFS file system and loss of sync between the driver and the kernel. Harmless and needs to be accounted for 
by the user

## Slew of ACL/Xattrs/SELinux Errors
This is by design. If you want to avoid these errors, pass a --no-acl, ... flag. The reason why these errors exist, is
to deal with traversals of multiple file systems. If you traverse into e.g. exFat, it's obvious that you'll only get 
errors however. But if you start on a segment of the file system that has e.g. no xattrs and then you enter a segment 
with and encounter an error, then the error should be recorded, not discarded, becasue the tool at the beginning deemed 
the file tree does not support xattrs/acls/SELinux.
