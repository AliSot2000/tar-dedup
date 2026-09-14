//! Unit tests for the persistent-error-log [`Recorder`]: batching, auto-flush
//! at the limit, and retain-when-no-database.

mod common;

use tar_dedup::db::flags::{ErrorFlag, ErrorFlags, ErrorScope, ErrorScopePart};
use tar_dedup::db::{ErrorPhase, RecordDraft, Recorder};
use tar_dedup::error::FileStatError;

/// A session-scoped draft with a distinct message.
fn draft(i: u64) -> RecordDraft {
    RecordDraft {
        file_id: None,
        out_tree_id: None,
        phase: ErrorPhase::Pipeline(tar_dedup::config::PipelinePhase::Hash),
        error: FileStatError::General {
            path: None,
            message: format!("test error {i}"),
        },
        flags: ErrorFlags::default().with(ErrorFlag::SessionError, true),
    }
}

/// `ErrorScope` selecting every partition (empty bitset = all).
fn all_scope() -> ErrorScope {
    ErrorScope::default()
}

#[test]
fn flush_persists_all_drafts_in_one_batch() {
    let (_dir, db) = common::open_temp_db();
    let mut recorder = Recorder::new(&db, true);

    for i in 0..5 {
        recorder.push(draft(i));
    }
    let n = recorder.flush().expect("flush");
    assert_eq!(n, 5);
    assert_eq!(db.count_records(all_scope(), None).expect("count"), 5);
}

#[test]
fn auto_flush_fires_when_buffer_reaches_limit() {
    let (_dir, db) = common::open_temp_db();
    let mut recorder = Recorder::new(&db, true);
    recorder.set_flush_limit(100);

    for i in 0..250 {
        recorder.push(draft(i));
    }
    // Two full batches were flushed automatically (at 100 and 200); the 50-draft
    // tail stays buffered until an explicit flush (or drop).
    assert_eq!(db.count_records(all_scope(), None).expect("count"), 200);
    assert!(!recorder.is_empty());
    let tail = recorder.flush().expect("flush");
    assert_eq!(tail, 50);
    assert_eq!(db.count_records(all_scope(), None).expect("count"), 250);
}

#[test]
fn no_auto_flush_without_db() {
    let (_dir, db) = common::open_temp_db();
    let mut recorder = Recorder::speculative(true);
    recorder.set_flush_limit(10);

    for i in 0..50 {
        recorder.push(draft(i));
    }
    // No db attached: drafts stay buffered, nothing to flush into.
    assert!(!recorder.is_empty());
    assert_eq!(db.count_records(all_scope(), None).expect("count"), 0);
}

#[test]
fn drop_flushes_remaining_drafts() {
    let (_dir, db) = common::open_temp_db();
    {
        let mut recorder = Recorder::new(&db, true);
        recorder.push(draft(1));
        // no explicit flush — the Drop impl must persist.
    }
    assert_eq!(db.count_records(all_scope(), None).expect("count"), 1);
}

#[test]
fn detached_recorder_keeps_drafts_for_later_bind() {
    let (_dir, db) = common::open_temp_db();
    let mut recorder = Recorder::speculative(true);

    recorder.push(draft(1));
    recorder.bind(&db);
    let n = recorder.flush().expect("flush");
    assert_eq!(n, 1);
    assert_eq!(db.count_records(all_scope(), None).expect("count"), 1);
}

/// `session()` must always set `SessionError`, regardless of the passed flags,
/// while still preserving any caller-supplied bits (e.g. `Reemit`).
#[test]
fn session_method_forces_session_flag() {
    let (_dir, db) = common::open_temp_db();
    let mut recorder = Recorder::new(&db, true);

    recorder.record_session(
        ErrorPhase::Pipeline(tar_dedup::config::PipelinePhase::Hash),
        FileStatError::General {
            path: None,
            message: "session err".into(),
        },
        ErrorFlags::default(),
    );
    recorder.record_session(
        ErrorPhase::Pipeline(tar_dedup::config::PipelinePhase::Hash),
        FileStatError::General {
            path: None,
            message: "session err reemit".into(),
        },
        ErrorFlags::default().with(ErrorFlag::Reemit, true),
    );
    recorder.flush().expect("flush");

    // Both rows must be visible under the session partition only.
    let only_session = ErrorScope::default().with(ErrorScopePart::Session, true);
    assert_eq!(db.count_records(only_session, None).expect("count"), 2);

    let only_file = ErrorScope::default().with(ErrorScopePart::File, true);
    assert_eq!(db.count_records(only_file, None).expect("count"), 0);

    // The flag is set unconditionally, and caller bits survive next to it.
    let rows = db.list_records(only_session, None, 0, 100).expect("list");
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(|r| r.flags.get(ErrorFlag::SessionError)));
    assert!(rows
        .iter()
        .any(|r| r.flags.get(ErrorFlag::SessionError) && r.flags.get(ErrorFlag::Reemit)));
}
