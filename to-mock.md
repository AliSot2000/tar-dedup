# To Mock

We record instances we observed that require mocking (or a test seam) to improve
our test coverage — i.e. branches of production code that are currently
unreachable from a unit test because they depend on OS behaviour we cannot
reliably provoke (or would rather not provoke) on a live filesystem. Items are
actionable: add a seam/injection point, then write the test that exercises it.

- [ ] `files::get_file_times` error arm in `check_path` (`unarchive/place.rs`).
      `check_path` calls `files::get_file_times(&file_metadata)` directly; its
      `Err` arm ("could not retrieve file mtime") is effectively untriggerable
      from a test because the `Metadata` is already fetched and `file_mtime`
      only fails on implausible timestamps we cannot create. To cover the
      silent / non-silent sub-arms we need a way to inject a failing time
      read (e.g. a `time_fn: fn(&Metadata) -> io::Result<DateTime<Utc>>`
      parameter, or a small trait), then assert the `(false, true, false)` /
      `Err(Io)` divergence.
- [ ] `check_path`'s `symlink_metadata` error arm (`unarchive/place.rs:1068`).
      Unreachable from a test: `tgt.exists()` short-circuits to
      `(true,false,false)` for any path whose `metadata()` fails, and
      `symlink_metadata` fails on the same paths. Only a TOCTOU race (target
      vanishing between the two stats) reaches the arm — not something a unit
      test can provoke deterministically.
- [ ] `check_path`'s `determine_file_type` error arm (`unarchive/place.rs:1149`).
      The same `tgt.exists()` short-circuit means any symlink chain hitting a
      non-`NotFound` error (e.g. EACCES mid-chain) also fails `exists()` first.
      `determine_file_type`'s own error arm is reachable in isolation (via
      `files::determine_file_type` unit tests); through `check_path` it is not.
