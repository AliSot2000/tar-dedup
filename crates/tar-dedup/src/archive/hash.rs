use crate::archive::ArchiveRTArgs;
use crate::common::files::warn_if_times_changed;
use crate::common::io_buffer;
use crate::common::send_receive_loop;
use crate::db::Recorder;
use crate::db::flags::ErrorFlags;
use crate::db::hash::{HashError, HashSuccess, HashingOutcome};
use crate::db::types::StrippedRecord;
use crate::error::{Error, Result};
use crate::progress::BarKind;
use crate::shutdown::Shutdown;
use crossbeam_channel::{Receiver, Sender, bounded};
use indicatif::ProgressBar;
use sha1::{Digest, Sha1};
use std::fs::File;
use std::io::Read;
use std::mem::take;
use std::path::Path;
use std::thread;

pub fn run(rt: &ArchiveRTArgs) -> Result<()> {
    debug_assert!(rt.config.sparse.page_size > 0, "page_size == 0");
    let detect_hardlinks = !rt.config.indexing.no_hardlink_detection;
    let eager_filter = rt.config.filter.eager_filter;

    let total_entries = rt.db.count_entries()?;
    let hash_needed = rt.db.count_all_hashable_files(eager_filter, detect_hardlinks)?;
    let pending = rt.db.count_pending_hashable_files(eager_filter, detect_hardlinks)?;
    let already_hashed = hash_needed.saturating_sub(pending);
    tracing::info!(
        "starting hash: total {}, processing {}, pending {}",
        total_entries,
        hash_needed,
        pending,
    );

    rt.progress.set_phase_total(hash_needed);
    rt.progress.set_phase_position(already_hashed);
    let promoted_entries = rt.db.promote_unhasheable_files(eager_filter, detect_hardlinks)?;
    tracing::info!("Promoted {promoted_entries} entries which cannot be hashed.");
    rt.progress.inc_global(total_entries - hash_needed);

    if pending == 0 {
        return Ok(());
    }

    let progress = rt.progress;
    let page_size = rt.config.sparse.page_size;
    let jobs = rt.config.process.effective_jobs();

    // Ordering table: encodes size-DESC order (position column) so the feed
    // can pull pending rows in that order without a long-lived SQL cursor.
    // Populate is idempotent (INSERT OR IGNORE); dropped only on success.
    rt.db.create_hash_queue()?;
    rt.db.populate_hash_queue(eager_filter, detect_hardlinks)?;

    // One bar per worker, materialized now so each worker can take its bar by
    // value; workers never touch `ProgressBarSet`.
    progress.create_thread_bars(BarKind::Bytes, jobs);
    let mut bars = Vec::<ProgressBar>::new();
    for i in 0..jobs {
        bars.push(progress.thread_bar(i));
    }

    let (work_s, work_r) = bounded::<StrippedRecord>(rt.config.process.work_capacity);
    let (out_s, out_r) = bounded::<Option<HashingOutcome>>(rt.config.process.out_capacity);
    let mut thread_handles = Vec::with_capacity(jobs);

    for i in 0..jobs {
        let bar = bars[i].clone();
        let wr = work_r.clone();
        let os = out_s.clone();
        let sh = rt.shutdown.clone();
        let ps = page_size;
        let res = thread::Builder::new()
            .name(format!("hash-worker-{i}").into())
            .spawn(move || hash_worker(bar, ps, sh, wr, os))
            .expect("spawn hash worker");
        thread_handles.push(res);
    }
    drop(work_r);
    drop(out_s);

    let completed = handle_send_receive_loop(&rt, work_s, out_r, thread_handles)?;
    progress.drop_thread_bars();

    let double_canonical = rt.db.count_double_canonical_dev_inode_group()?;
    if double_canonical > 0 {
        panic!("Encountered {double_canonical} Hard Link files. \
            which have two different hashes. Assuming files modified while hashing.");
    }

    match rt.shutdown.is_interrupted() {
        true => {
            let msg = if rt.shutdown.is_force() {
                "hashing force-aborted; in-flight progress discarded"
            } else {
                "hashing stopped; completed files saved"
            };
            tracing::warn!(saved = completed, "{msg}");
            Err(Error::Interrupted)
        }
        false => {
            tracing::info!(
                "hash done: processed {} of {}, {} error(s)",
                already_hashed + completed,
                hash_needed,
                rt.db.count_errors(Some("hash"))?,
            );
            rt.db.drop_hash_queue()?;
            Ok(())
        }
    }
}

/// Perform the full enqueue / dequeue loop in batches to improve performance.
/// The state machine for the enqueue / dequeue process is quite involved and pollutes the name
/// space of the function, which is why it is moved to a separate function. Owns the worker
/// handles: the shared loop joins them before the final drain (a trailing `None` can never race
/// `drop(recv)`).
fn handle_send_receive_loop(
    rt: &ArchiveRTArgs,
    send: Sender<StrippedRecord>,
    recv: Receiver<Option<HashingOutcome>>,
    thread_handles: Vec<thread::JoinHandle<()>>)
    -> Result<u64> {
    // Feed cursor over `hash_queue`: `queue_index` is the last consumed queue
    // position. The pull filters to still-pending rows, so a file already
    // handed to a worker (or hashed on a previous run) is never re-pulled.
    let mut recorder = Recorder::new(rt.db, !rt.config.process.no_errors, Some(rt.config.process.auto_flush_limit));
    let mut queue_index = 0u64;

    let pull = || {
        let rows = rt.db.pull_pending_hash_rows::<StrippedRecord>(queue_index, rt.config.process.feed_chunk as u64)?;
        if !rows.is_empty() {
            queue_index = rows[rows.len() - 1].0;
        }
        Ok(rows.into_iter().map(|(_q, row)| row).collect::<Vec<_>>())
    };
    let apply = |pending: &mut Vec<HashingOutcome>| -> Result<()> {
        let items = take(pending);
        if items.is_empty() {
            return Ok(());
        }
        let n = items.len() as u64;
        rt.db.ingest_hash_outcome(&items, !rt.config.indexing.no_hardlink_detection)?;
        for res in items {
            match res {
                Ok(_) => (),
                Err(e) => match &e.err {
                    Error::Interrupted => (),
                    Error::FileStat(fse) => recorder.record_file(
                        e.id,
                        crate::db::ErrorPhase::Pipeline(crate::config::PipelinePhase::Hash),
                        fse.recreate(),
                        ErrorFlags::default(),
                    ),
                    other => panic!(
                        "Invariant Error. Only FileStatError and Interrupted expected. Got: {other}"
                    ),
                } ,
            }
        }
        rt.progress.inc_both(n);
        Ok(())
    };

    let completed = send_receive_loop(
        rt.shutdown, send, recv, thread_handles,
        rt.config.process.drain_chunk, false,
        pull, |_| Ok(()), || Ok(true), apply)?;
    recorder.flush()?;
    Ok(completed)
}
/// Worker thread: pulls one file at a time, hashes it, forwards the outcome.
/// Owns its bar and read buffer; only touches the channels and `tracing`.
fn hash_worker(
    bar: ProgressBar,
    page_size: usize,
    shutdown: Shutdown,
    work: Receiver<StrippedRecord>,
    out: Sender<Option<HashingOutcome>>) -> () {
    let mut buf = io_buffer();
    loop {
        // INFO: Destroy channel to exit the worker!
        match work.recv() {
            Ok(row) => {
                if shutdown.check_between_files().is_err() {
                    break;
                }
                bar.reset();
                bar.set_length(row.size);
                bar.set_message(format!("Hashing {}", row.abs_path.display()));
                let modified = warn_if_times_changed(
                    row.fs_path(), row.mtime, row.atime, row.ctime
                );
                let res = match hash_one(
                    &mut buf, row.fs_path(), page_size, &shutdown, Some(&bar)) {
                    Ok((hash, zero_pages)) => {
                        tracing::debug!("hashed {}", row.abs_path.display());
                        Ok(HashSuccess {
                            id: row.id, hash, zero_pages, modified
                        })
                    }
                    Err(err) => Err(HashError {
                        id: row.id, err, modified
                    })
                };
                out.send(Some(res)).expect("hash worker: result channel closed");
            }
            Err(_) => break
        }
    }
    out.send(None).expect("hash worker: result channel closed");
}

/// Single-pass SHA-1 and empty-page count.
///
/// `buf` is the worker's reusable read buffer (sized once here); `pb`
/// advances live per buffer so a huge file's bar stays responsive.
///
/// Bytes are hashed as read. Separately, the stream is partitioned into fixed
/// `page_size` windows (independent of the I/O buffer). Only **full**
/// all-zero windows count; a short trailing window does not (same rule as
/// `sparse-cp::sparse_page_count`).
///
/// Zero checks slice `buf` in place. Across a read boundary we only keep
/// `carry_len` / `carry_zero` — never the leftover bytes themselves.
fn hash_one(
    buf: &mut Vec<u8>,
    path: &Path,
    page_size: usize,
    shutdown: &Shutdown,
    pb: Option<&ProgressBar>)
    -> Result<([u8; 20], u64)> {
    let mut file = File::open(path).map_err(|e| Error::io(path, e))?;

    let mut hasher = Sha1::new();
    let mut zero_blocks = 0u64;
    // Incomplete page spanning the previous read: length so far, and whether
    // those bytes were all zero. `carry_len > 0` is the "cut off by buffer" flag.
    let mut carry_len = 0usize;
    let mut carry_zero = true;

    loop {
        match shutdown.check_in_flight() {
            Ok(_) => (),
            Err(e) => {
                tracing::debug!("hash worker got in-flight interrupt");
                return Err(e);
            }
        };
        let n = file.read(buf).map_err(|e| Error::io(path, e))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);

        let mut i = 0usize;

        // Handle segmentation between read_bufs
        if carry_len > 0 {
            let need = page_size - carry_len;
            if n < need {
                carry_zero &= is_all_zero(&buf[..n]);
                carry_len += n;
                continue;
            }
            if carry_zero && is_all_zero(&buf[..need]) {
                zero_blocks += 1;
            }
            carry_len = 0;
            carry_zero = true;
            i = need;
        }

        // Scan contiguous buffer
        while i + page_size <= n {
            if is_all_zero(&buf[i..i + page_size]) {
                zero_blocks += 1;
            }
            i += page_size;
        }

        // Scan remaining page for zeros.
        let rem = n - i;
        if rem > 0 {
            carry_len = rem;
            carry_zero = is_all_zero(&buf[i..n]);
        }
        if let Some(pb) = pb {
            pb.inc(n as u64);
        }
    }

    Ok((hasher.finalize().into(), zero_blocks))
}

#[inline]
fn is_all_zero(chunk: &[u8]) -> bool {
    chunk.iter().all(|&b| b == 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::defaults::IO_BUF_SIZE;
    use crate::db::flags::FileFlag;
    use crate::db::types::{FileId, FilePhase};
    use crate::test_support::TestWorld;
    use chrono::{DateTime, Utc};
    use nix::unistd::geteuid;
    use rusqlite::named_params;
    use std::os::unix::fs::PermissionsExt;

    fn pattern(n: usize, seed: u8) -> Vec<u8> {
        (0..n).map(|i| ((i % 251) as u8) ^ seed).collect()
    }

    fn sha1_of(payload: &[u8]) -> [u8; 20] {
        let mut hasher = Sha1::new();
        hasher.update(payload);
        hasher.finalize().into()
    }

    /// The hash phase sizes its worker pool off `effective_jobs()` =
    /// `min(jobs, io_jobs)`; restore the original 4 io-bound workers that the
    /// shared world's default (io_jobs = 1) would collapse down to one.
    fn test_world() -> TestWorld {
        TestWorld::tuned(|c| c.process.io_jobs = 4)
    }

    impl TestWorld {
        fn sparse_count(&self, id: FileId) -> i64 {
            self.db.with_transaction(|conn| {
                let v: i64 = conn.query_row(
                    "SELECT sparse_count FROM files WHERE id = :id",
                    named_params! { ":id": id.0 },
                    |r| r.get(0),
                ).expect("read sparse_count");
                Ok(v)
            }).expect("sparse_count read")
        }

        fn hashed_digest(&self, id: FileId) -> Option<[u8; 20]> {
            self.db.get_file_by_id::<StrippedRecord>(id)
                .expect("get row")
                .expect("row present")
                .sha1
        }
    }

    #[test]
    fn run_hashes_and_stores_digests() {
        let world = test_world();
        let big = pattern(1024 * 1024, 3);
        let tiny = pattern(7, 9);
        let empty: Vec<u8> = Vec::<u8>::new();
        let mut mixed = Vec::<u8>::new();
        mixed.resize_with(4 * 4096 + 1024, || 0u8);
        for (i, b) in pattern(1024, 5).iter().enumerate() {
            mixed[4 * 4096 + i] = *b;
        }

        let id_big = world.add_file("big.bin", &big);
        let id_dup = world.add_file("dup.bin", &big);
        let id_tiny = world.add_file("tiny.bin", &tiny);
        let id_empty = world.add_file("empty.bin", &empty);
        let id_mixed = world.add_file("mixed.bin", &mixed);

        run(&world.rt()).expect("hash run");

        for (id, payload) in [
            (id_big, &big),
            (id_dup, &big),
            (id_tiny, &tiny),
            (id_empty, &empty),
            (id_mixed, &mixed),
        ] {
            let row = world.db.get_file_by_id::<StrippedRecord>(id)
                .expect("get row").expect("row present");
            assert_eq!(row.phase, FilePhase::Hashed);
            assert_eq!(row.sha1, Some(sha1_of(payload)));
        }
        // the four full zero pages are counted, the short random tail is not.
        assert_eq!(world.sparse_count(id_mixed), 4);
        assert_eq!(world.sparse_count(id_big), 0);
    }

    #[test]
    fn unreadable_file_recorded_not_fatal() {
        // A 0o000 file is still readable by root; the test needs a real
        // permission failure.
        if geteuid().is_root() {
            return;
        }
        let world = test_world();
        let payload = pattern(64 * 1024, 1);
        let id_ok1 = world.add_file("ok1.bin", &payload);
        let id_bad = world.add_file("bad.bin", &payload);
        let id_ok2 = world.add_file("ok2.bin", &payload);

        let bad_path = world.path("bad.bin");
        std::fs::set_permissions(&bad_path, std::fs::Permissions::from_mode(0))
            .expect("chmod 000");

        run(&world.rt()).expect("hash run tolerates an unreadable file");

        assert_eq!(
            world.db.get_file_flag(id_bad, FileFlag::ErrorWhileHash).expect("flag"),
            true
        );
        if let Some(_) = world.hashed_digest(id_bad) {
            panic!("unreadable file must not get a digest");
        }
        assert_ne!(
            world.db.get_records_by_file_id(id_bad).expect("records").len(),
            0
        );
        for id in [id_ok1, id_ok2] {
            assert_eq!(world.hashed_digest(id), Some(sha1_of(&payload)));
            assert_eq!(
                world.db.get_file_by_id::<StrippedRecord>(id)
                    .expect("get row").expect("row").phase,
                FilePhase::Hashed
            );
        }
    }

    #[test]
    fn modified_file_flagged() {
        let world = test_world();
        let now = Utc::now();
        let stale = DateTime::from_timestamp(now.timestamp() - 3600, 0).expect("stale");
        let payload = pattern(128 * 1024, 4);
        let id = world.add_file_recorded("mod.bin", &payload, Some(stale), None, None);

        run(&world.rt()).expect("hash run");

        assert_eq!(
            world.db.get_file_flag(id, FileFlag::Modified).expect("flag"),
            true
        );
        assert_eq!(world.hashed_digest(id), Some(sha1_of(&payload)));
    }

    #[test]
    fn hash_order_is_size_desc() {
        let world = test_world();
        let id_small = world.add_file("s.bin", &pattern(1024 * 1024, 1));
        let id_mid = world.add_file("m.bin", &pattern(4 * 1024 * 1024 + 1, 2));
        let id_big = world.add_file("b.bin", &pattern(8 * 1024 * 1024, 3));

        world.db.create_hash_queue().expect("create queue");
        world.db.populate_hash_queue(false, false).expect("populate queue");

        let ids = world.db.pull_pending_hash_rows::<StrippedRecord>(0, 100)
            .expect("pull")
            .into_iter()
            .map(|pair| pair.1.id)
            .collect::<Vec<FileId>>();
        assert_eq!(ids, [id_big, id_mid, id_small].to_vec());
    }

    #[test]
    fn zero_page_counter_partial_last_page() {
        let world = test_world();
        let path = world.path("z1.bin");
        let payload = vec![0u8; 3 * 4096 + 4089];
        std::fs::write(&path, &payload).expect("write");
        let mut buf = io_buffer();
        let (_hash, sparse) = hash_one(&mut buf, &path, 4096, &Shutdown::detached(), None)
            .expect("hash");
        assert_eq!(sparse, 3);
    }

    #[test]
    fn zero_page_counter_exact_multiple() {
        let world = test_world();
        let path = world.path("z2.bin");
        let payload = vec![0u8; 40 * 4096];
        std::fs::write(&path, &payload).expect("write");
        let mut buf = io_buffer();
        let (_hash, sparse) = hash_one(&mut buf, &path, 4096, &Shutdown::detached(), None)
            .expect("hash");
        assert_eq!(sparse, 40);
    }

    #[test]
    fn zero_page_counter_mixed_content() {
        let world = test_world();
        let path = world.path("z3.bin");
        let mut payload = vec![0u8; 2 * 4096];
        for b in pattern(4096, 7) {
            payload.push(b);
        }
        std::fs::write(&path, &payload).expect("write");
        let mut buf = io_buffer();
        let (_hash, sparse) = hash_one(&mut buf, &path, 4096, &Shutdown::detached(), None)
            .expect("hash");
        assert_eq!(sparse, 2);
    }

    #[test]
    fn zero_page_counter_crosses_buffer_boundary() {
        let world = test_world();
        let path = world.path("z4.bin");
        let payload = vec![0u8; IO_BUF_SIZE + 2 * 4096 + 4089];
        std::fs::write(&path, &payload).expect("write");
        let mut buf = io_buffer();
        let (_hash, sparse) = hash_one(&mut buf, &path, 4096, &Shutdown::detached(), None)
            .expect("hash");
        assert_eq!(sparse, ((IO_BUF_SIZE + 2 * 4096) / 4096) as u64);
    }

    #[test]
    fn batch_size_parity_stub() {
        // Invariant for the `feed_chunk` knob: slicing the hash queue with
        // different batch limits must yield the identical set and order as a
        // single unfettered pull. (DB-layer only — independent of the worker
        // threading model; `handle_send_receive_loop`'s `pull` depends on it.)
        let world = test_world();
        for i in 0..5 {
            let payload = pattern(1024, i as u8);
            let name = format!("b{i}.bin");
            world.add_file(name.as_str(), &payload);
        }
        world.db.create_hash_queue().expect("create queue");
        world.db.populate_hash_queue(false, false).expect("populate queue");
        let all = world.db.pull_pending_hash_rows::<StrippedRecord>(0, 100).expect("pull all");
        let mut collected = Vec::<u64>::new();
        let mut pos = 0u64;
        loop {
            let batch = world.db.pull_pending_hash_rows::<StrippedRecord>(pos, 2)
                .expect("pull batch");
            if batch.is_empty() {
                break;
            }
            for pair in batch {
                let (p, _row) = pair;
                collected.push(p);
                pos = p;
            }
        }
        assert_eq!(collected, all.into_iter().map(|pair| pair.0).collect::<Vec<u64>>());
    }
}
