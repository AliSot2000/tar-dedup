# Plan: extract `--sparsify` / `--page-size` (sparse-aware scan unpack)

Status: **agreed (2026-10-05), implementation round.** Corrections from user review
incorporated (no metadata, no type guard, `Default` + serde default, `page_size > 0`,
config in `ScanOptions`, no member tests this round, docs deferred).

## Goal

While scanning, write archive content members into the extract cache with stepped,
hole-preserving writes when `--sparsify` is set (`--page-size` = hole-detection block),
and stepped plain copies otherwise — replacing the single `entry.unpack()` call.
Three progress bars: global total, phase count (members extracted), per-member bytes.

## Design decisions (user-agreed)

1. **No metadata** on the cache file (mode/times/owner live in the DB; permissions
   phase applies them downstream). `unpack`'s metadata propagation is knowingly dropped.
2. **No file-type guard** — only regular members reach the content arm; anything else
   is a tar-dedup contract violation.
3. `ScanOptions` gets `impl Default` + `#[serde(default)]` so older stored
   `extract_config` JSON still deserializes on resume.
4. `page_size == 0` while `sparsify` → `Error::Config` (mirror `archive/sparsify.rs`).
5. Config fields go in **`ScanOptions`** (scan-phase behavior); CLI flags in `ExtractArgs`.
6. No member-unpack tests this round — deferred to the pipeline test round.
7. Docs folded into the later extract-command docs pass.

## Changes

### `common/files.rs` — reader-based stepped copier
`copy_file_batched` (no production callers; only 2 unit tests) → renamed/re-shaped:
`copy_reader_batched<R: Read>(src: &mut R, dst: &Path, mut on_batch: FnMut(u64) -> Result<()>)`
- loop over `io_buffer()` chunks, `write_all`, then `on_batch(cumulative)?`.
- any `Err` (incl. `Interrupted`) → `fs::remove_file(dst)` (partial) + propagate.
- the 2 unit tests (`…_copies_all_bytes`, `…_interrupt_removes_partial_dst`) reshaped.

### `unarchive/scan.rs` — stepped member write (both paths)
In the content-member arm replace `entry.unpack(&entry_dst)` with a dispatch:
- `ScanRTState` gains `pb: ProgressBar` (a phase-scoped "extracting" `Bytes` sub-bar,
  `push_sub_bar`, created in `run_inner`; reused across members).
- sparse (`config.scan.sparsify`) →
  `sparse_cp::sparse_copy_reader(&mut entry, &entry_dst, entry.size(), &mut vec![0u8; page_size], cb)`
  (`size_in` = `entry.size()`; holes from zero runs; partial removed on any error).
- non-sparse → `copy_reader_batched(&mut entry, &entry_dst, cb)`.
- shared `cb(…, bytes, …)`: `pb.inc(bytes - prev)` + `shutdown.check_in_flight()`.
- caller: `Err(Error::Interrupted)` propagates cleanly (no `record_file_scan_error`,
  no `Config`); other errors keep today's `record_file_scan_error` + `Error::io`.

### `config/extract.rs`
- `ScanOptions { force_scan, rehash, clear_archive_meta, sparsify: bool, page_size: usize }`
- `impl Default for ScanOptions` (false / true / false / **false** / **4096**) +
  `#[serde(default)]` on the struct; dedupe the 3 identical inline constructions to
  `ScanOptions::default()`.
- `build()`: `sparsify: args.sparsify, page_size: args.page_size`; validate
  `page_size > 0` when sparsify.

### `cli.rs` — `ExtractArgs`
New "Sparse Files" section (extract-local copy of the archive flags):
- `--sparsify` (SetTrue, default false)
- `--page-size BYTES` (default `4096`)

## Notes
- Our archives already carry GNU sparse tar headers; reading relies on tar-rs `Entry`
  expanding to the logical stream (`unpack`'s existing contract — unchanged; we read
  the same `Entry`), so hole re-creation sees the zero runs.
- Holes land at the extraction destination regardless of source-filesystem support
  (e.g. exFAT → ext4), because sparse writing happens on the cache/destination write.
- Downstream phases (rehash, place `copy_single_file`) are already sparse-aware; no change.

## Verification
- `cargo build -p tar-dedup -p tar-dedup-cli`
- `cargo test -p tar-dedup --lib` (expect 279 + reshaped helper tests, 0 failures)
- Manual: `extract --sparsify --page-size 4096` → cache files sparse
  (`sparse_page_count` / `ls -ls`), non-sparse path behaves as before.