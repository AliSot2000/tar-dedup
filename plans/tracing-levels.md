# Plan: tracing levels + per-phase start/end reporting

Status: **agreed (2026-10-05), implementation round pending.** All open questions
settled: whole-run semantics (1), cleanup warn (2), interrupt warn (3), ftype in
place traces only — not the errors table (4).

## Global spec

**Levels** (TODO.md):
- `error` — content not restored/archived as intended (file missing, unreadable,
  type unclassified, payload not appended).
- `warn` — metadata, conflicts, cleanup, recording, skip (relax/gate).
- `info` — phase start / finish / progress statements.
- `debug` — one line per entry actually processed; **no** debug when that entry
  errored (the error line is the only output for it).

**Whole-run semantics**: every number refers to the whole run (cumulative),
mirroring a tar process that went to sleep. The only per-session figure is the
explicit `todo` in the start line. On resume, bars/counts keep their
whole-corpus anchor (already how `set_phase_total(workload)` +
`set_phase_position(done)` work everywhere).

**Standard start info** (emitted only when there is work — after the
`pending == 0` early-return exists):
```
starting {phase}: total {total}, processing {processing}, pending {pending}
```
- `total` — literally all rows of the table that defines the phase domain
  (`count_entries()` (files) for archive phases & scan/rehash/filter;
  `count_out_tree_rows()` for placement/prologue/permissions) — "current corpus".
- `processing` — the phase's candidate set (e.g. `canonical_id = id` for
  sparsify/stage/tar-builder; the phase's workload count).
- `pending` — the eligible still to do this phase (per-session).

**Standard end info** (success path, before `Ok`):
```
{phase} done: processed {done} of {total}, {errors} error(s)
```
- `done`/`errors` cumulative over the whole run (`done` is a real processed
  count — never the workload literal copied into both slots).

`errors` comes from the new AUX `count_errors(phase)` (whole-run, so a resume
recounts — accepted).

## Archive

### `archive.rs`
- MOD: `:124`/`:129` interrupt `error` → `warn` (user control flow).
- REM: `:118` `info "archive phase"` — per-phase start info replaces it.
- Keep `:68` resume, `:153/:189` exit-after-stage, `:176` written, `:183` keep-stage.

### `inventory.rs`
- ADD: standard start (`corpus=count_entries`, eligible/pending from walk);
  MOD `:142` end info → standard end (+AUX errors).
- ADD: `debug "inventoried {path}"` on successful insert.
- Keep `:245` walk-fail `error`, `:138` warn, `:324/336/348` timestamp warns,
  `:270/:694` relax warns, `:375` unclassified `error`.

### `hash.rs`
- MOD `:31` → standard start; ADD standard end (+AUX errors).
- DEM: `:250` worker in-flight `info` → `debug`.
- ADD: `debug "hashed {path}"` for `Ok` outcomes in the apply loop.
- Keep `:43` promote info, `:103` interrupt warn.

### `filter.rs` (common + archive + extract)
- ADD: standard start in `archive/filter.rs` and `unarchive/filter.rs`
  (`corpus=count_entries`, eligible/todo from `count_files_in_phase`).
- Keep per-row `debug` (140/143/146), `:62` no-filter info,
  `unarchive/filter.rs:78` promote info.

### `dedup.rs`
- MOD `:75`/`:163` → standard start; MOD `:143`/`:178` → standard end (+AUX errors).
- Keep `:137` interrupt warn, `:247` fail-fast `error`.

### `sparsify.rs`
- MOD `:62` → standard start; MOD `:151` → standard end (ok/err cumulative).
- ADD: `debug "sparsified {path}"` for successes.
- Keep `:76/:83` promote infos, `:145` interrupt warn.

### `stage.rs`
- ADD standard start (`corpus=count_entries`, eligible=`count_all_stage_candidates`);
  `:39` → "nothing to do" info; `:153` → standard end.
- ADD: `debug "staged {path}"`.

### `tar_builder.rs`
- ADD standard start (`eligible=count_files_in_phase(Staged)`, todo bytes)
  + standard end (+AUX errors).
- ADD: `debug "appended {member} ({bytes})"` in the `Ok` arm of `append_path`.
- Keep `:146` append-fail `error`, `:77` warn, `:183` session info, `:232` recovery info.

## Extract

### `unarchive.rs`
- MOD: `:81` resume `error` → `info`; `:155` "extracted to …" `error` → `info`.
- REM: `:96` "unarchive phase", `:100` "extract: scanning archive".

### `scan.rs`
- ADD start (resume: "…from member {n}"); MOD `:321` → standard end (+AUX
  errors); fold `:300` into it.
- DEM: `:235` `info "Index: …"` → `debug` (per-member).
- ADD: `debug "extracted cache payload {name}"` on success.
- Keep `:302/:309` warns, `:498–:569` truncation warns, `:547` force warn,
  `:575/:582` infos, `:531` debug, `:615` temp-db-remove warn, `:226` relax warn.

### `rehash.rs`
- MOD `:39` → standard start; MOD `:116` → standard end (align wording, +AUX errors).
- MOD `:265` `warn "rehash failed"` → `error` (content unverifiable).
- ADD: `debug "rehashed {path} OK"` for matches.
- Keep `:49/:58` promote infos, `:108` interrupt warn, `:124` force-mismatch
  warn, `:142` errors warn.

### `place_prologue.rs`
- Keep `:37/:41/:48` sub-step infos; MOD `:173` → standard end for the phase.

### `place.rs`
- ADD standard start (`corpus=count_out_tree_rows`, eligible/pending from the
  materialize counts); MOD `:91` "Updated File Table" → standard end (one line).
- DEM per-entry infos → `debug` (with `({ftype})` appended): `:631` "Materializing
  to", `:697` socket skip, `:1052/:1062/:1076` dir replacements.
- MOD conflict/type-mismatch cluster `:1108/:1129/:1142/:1160/:1173/:1187/:1197/:1211/:1225`
  → `warn`.
- ADD: `debug "placed {path} ({ftype})"` in materialize_loop success; include
  `ftype` in the per-entry place error **trace** (`:752` keeps `{dev_type}`).
  NOT into the persistent errors table (queryable later via inspector).
- Keep `:76` link-tree info, `:113` cleanup warn, `:144` warn, `:752` `error`,
  `:799` prep status info.

### `permissions.rs`
- MOD `:377` "metadata restore failure" `error` → `warn`.
- Keep `:90`; ADD standard end (+AUX errors).

### `cleanup.rs`
- MOD: work-dir removal failures stop aborting the run → `warn` + continue.
  Keep `kept database` info.

## Cross-cutting
- `db.rs` Recorder (`:1294/:1323/:1359`) unchanged.
- `compression/{xz,rs}`, `tar_writer.rs`, `progress.rs`,
  `common/{files,xattr,perms}.rs` — out of scope.
- `TODO.md` — tick the tracing checklist item; add the final wording:
  ```
  - [x] correct tracing (
              tracing::error - major fail (…)
              tracing::warning - minor fail (…)
              tracing::info - general update (…)
              tracing::debug - file level updates (…))
  ```

## Aux (new DB surface)
- `Database::count_errors(phase: Option<&str>) -> Result<u64>` (impl in
  `db/errors.rs`): rows in `errors` matching `phase` (optional) — the uniform
  `U` for every end line. Whole-run by design.