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
