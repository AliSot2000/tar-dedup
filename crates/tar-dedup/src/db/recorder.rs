//! Batching recorder for the persistent error log (`errors` table).

use crate::common::defaults::DEFAULT_AUTO_FLUSH_LIMIT;
use crate::db::flags;
use crate::db::types::{FileId, OutTreeId};
use crate::db::{Database, ErrorPhase, RecordDraft};
use crate::error::{FileStatError, Result};

/// Batching recorder for the persistent error log. Accumulates drafts and flushes
/// them in one transaction. When `enabled` is `false` (the `--no-errors` flag),
/// drafts are dropped.
///
/// The database is optional so the recorder can be *speculative*: errors may be
/// added before any database exists (e.g. while a scan is still locating the
/// catalog). Use [`Recorder::new`] when a database is already in hand, or
/// [`Recorder::speculative`] followed by [`Recorder::bind`] once one appears.
/// A [`Recorder::flush`] with no attached database logs the loss instead.
///
/// Auto-flush (on by default): once the buffered draft count reaches
/// [`crate::common::defaults::DEFAULT_AUTO_FLUSH_LIMIT`] a flush is attempted so bursts of
/// errors (e.g. an unreliable filesystem) don't grow the buffer without bound.
/// Auto-flush never propagates errors and retains the drafts for a later retry.
pub struct Recorder<'a> {
    db: Option<&'a Database>,
    buf: Vec<RecordDraft>,
    enabled: bool,
    auto_flush: bool,
    flush_limit: u64,
}

impl<'a> Recorder<'a> {
    /// Recorder tied to an already-open database. `flush_limit` overrides the
    /// auto-flush threshold; `None` uses the default from `defaults.rs`.
    pub fn new(db: &'a Database, enabled: bool, flush_limit: Option<u64>) -> Self {
        Self {
            db: Some(db),
            buf: Vec::new(),
            enabled,
            auto_flush: true,
            flush_limit: flush_limit
                .unwrap_or(DEFAULT_AUTO_FLUSH_LIMIT),
        }
    }

    /// Speculative recorder: buffers errors without a database. Attach one later
    /// with [`bind`]; until then a [`flush`] only reports that it could not store.
    /// `flush_limit` overrides the auto-flush threshold; `None` uses the default.
    pub fn speculative(enabled: bool, flush_limit: Option<u64>) -> Self {
        Self {
            db: None,
            buf: Vec::new(),
            enabled,
            auto_flush: true,
            flush_limit: flush_limit
                .unwrap_or(DEFAULT_AUTO_FLUSH_LIMIT),
        }
    }

    /// Return if the function contains a database, to enable smart flushing.
    pub fn is_speculative(&self) -> bool {
        self.db.is_none()
    }

    /// Attach (or replace) the database reference. Buffered drafts stay buffered;
    /// call [`flush`] to persist them. The referenced database must outlive the
    /// recorder for the remainder of its use.
    pub fn bind(&mut self, db: &'a Database) {
        self.db = Some(db);
    }

    /// Drop the database reference, going back to speculative. Buffered drafts are
    /// preserved; a later [`bind`] + [`flush`] can still persist them.
    pub fn detach(&mut self) {
        self.db = None;
    }

    /// Disable/enable automatic flushing once the buffer exceeds the default limit.
    pub fn set_auto_flush(&mut self, on: bool) {
        self.auto_flush = on;
    }

    /// Override the auto-flush threshold (draft count at which a flush is tried).
    /// Useful once a fresh-from-disk database is attached to the process and its
    /// stored config carries the authoritative limit (e.g. extract/resume).
    pub fn set_flush_limit(&mut self, limit: u64) {
        self.flush_limit = limit;
    }

    pub fn record(
        &mut self,
        file_id: Option<FileId>,
        out_tree_id: Option<OutTreeId>,
        phase: ErrorPhase,
        error: FileStatError,
        flags: flags::ErrorFlags) {

        if !self.enabled {
            return;
        }

        // INFO: auto_flush in push!
        self.push(RecordDraft {
            file_id,
            out_tree_id,
            phase,
            error,
            flags,
        });
    }

    /// Session-scoped error: neither a file nor an out_tree row.
    ///
    /// The `SessionError` flag is always set, regardless of `flags`.
    pub fn record_session(
        &mut self,
        phase: ErrorPhase,
        error: FileStatError,
        flags: flags::ErrorFlags) {

        self.record(
            None,
            None,
            phase,
            error,
            flags.with(flags::ErrorFlag::SessionError, true),
        );
    }

    /// File-scoped error (canonical, duplicate, or any file row).
    pub fn record_file(
        &mut self,
        file_id: FileId,
        phase: ErrorPhase,
        error: FileStatError,
        flags: flags::ErrorFlags,
    ) {
        self.record(Some(file_id), None, phase, error, flags);
    }

    /// Out-tree-scoped error.
    pub fn record_out_tree(
        &mut self,
        out_tree_id: OutTreeId,
        phase: ErrorPhase,
        error: FileStatError,
        flags: flags::ErrorFlags,
    ) {
        self.record(None, Some(out_tree_id), phase, error, flags);
    }

    pub fn push(&mut self, draft: RecordDraft) {
        if self.enabled {
            self.buf.push(draft);
            self.try_auto_flush();
        }
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Best-effort flush once the buffer has grown past the auto-flush limit.
    /// No-op while no database is attached (keeps buffering speculatively) and
    /// never propagates an error: on failure the drafts stay buffered for retry.
    fn try_auto_flush(&mut self) {
        if !self.auto_flush || self.db.is_none() {
            return;
        }
        if (self.buf.len() as u64) < self.flush_limit {
            return;
        }
        match self.flush() {
            Ok(_) => (),
            Err(e) => {
                // Retained by fill of flush() on failure; report once.
                tracing::error!(
                    error = %e,
                    pending = self.buf.len(),
                    "auto-flush failed; errors retained for retry"
                );
            }
        }
    }

    /// Only flush, if a database is present. Otherwise, hold on to the entries.
    pub fn try_flush(&mut self) -> Result<u64> {
        if self.is_speculative() {
            return Ok(0);
        };
        self.flush()
    }

    // TODO: Can we assume that try_flush is called and if this function is called, all messages
    //  are emitted to std::out at i.g. info ot err to be able to suppress this.
    /// Flush buffered drafts in a single transaction. Clears the buffer on
    /// success and retains it on failure (so a retry can persist the same rows).
    /// Without an attached database, buffered drafts are kept and the failure to
    /// store is logged.
    pub fn flush(&mut self) -> Result<u64> {
        if self.buf.is_empty() {
            return Ok(0);
        }
        match self.db {
            None => {
                tracing::error!(
                    pending = self.buf.len(),
                    "Could not store the error, no db present"
                );
                Ok(0)
            }
            Some(db) => {
                let n = db.insert_errors(&self.buf)?;
                self.buf.clear();
                Ok(n)
            }
        }
    }

    /// Flush buffered drafts against a one-off database, without attaching it to
    /// the recorder. Useful when a reference would not live long enough to store
    /// (e.g. a database that is created and dropped within one frame).
    pub fn flush_into(&mut self, db: &Database) -> Result<u64> {
        if self.buf.is_empty() {
            return Ok(0);
        }
        let n = db.insert_errors(&self.buf)?;
        self.buf.clear();
        Ok(n)
    }
}

impl Drop for Recorder<'_> {
    fn drop(&mut self) {
        if !self.buf.is_empty() {
            // Best-effort flush so errors are not lost if a phase forgets to flush.
            // Log-only: a failing flush reports the count but never aborts.
            tracing::info!("flushing remaining errors to db");
            match self.flush() {
                Ok(n) if n > 0 => tracing::info!(flushed = n, "errors flushed"),
                Ok(_) => {}
                Err(e) => tracing::error!(error = %e, "failed to flush remaining errors"),
            }
        }
    }
}
