# 085 — Slurm LZMA benchmark harness (7z vs tar.xz vs tar-dedup orderings)

Status: **approved — implementing (2026-11-10).**

## Motivation

Benchmark tar-dedup against 7-Zip and plain `tar.xz` on a few real target
directories, using **identical LZMA2 parameters** so any size delta is attributable
to tar-dedup's dedup, not to different compression settings. The cluster has
slurm; we run 2–4 scenarios at once (space constrained). An interrupt must not
lose completed work.

## Scenarios (6, per target directory)

| # | id | command (threads = 16) | notes |
|---|----|------------------------|-------|
| 0 | `7z` | `7z a -t7z -mx9 -mmt=$T $OUT/$B.7z $TARGET` | LZMA2 max preset, MT. Default dict 64 MiB (matches xz -9). |
| 1 | `tarxz` | `tar -cf - $TARGET \| xz -9 -T$T > $OUT/$B.tar.xz` | Same preset, MT block framing as tar-dedup (BLOCK_SIZE 0 = 3×dict). |
| 2 | `tdd-id` | `tar-dedup archive -f $OUT/$B-id.tdd.tar.xz -i $TARGET --ordering id --level 9 --jobs $T` | `.tdd.tar.xz` infers xz, level 9 default. |
| 3 | `tdd-size` | same, `--ordering size` | |
| 4 | `tdd-name` | same, `--ordering name` | |
| 5 | `tdd-tar` | `tar-dedup archive -f $OUT/$B-raw.tdd.tar -i $TARGET --ordering size --jobs $T` | `.tdd.tar` = uncompressed baseline. |

- `--ordering id` requires the **`debug-ordering`** feature: build with
  `cargo build --release -p tar-dedup --features tar-dedup-lib/debug-ordering`.
- tar-dedup's xz preset = level 9, CRC64, MT block-based — identical parameter
  set to `xz -9 -T16`. 7z `-mx9` = same 64 MiB LZMA2 preset.

## Slurm layout

- One **array job**, one array task per `(target × scenario)`. Array size =
  `n_targets × 6`; task id decomposes into `target_idx = id / 6`,
  `scenario_idx = id % 6`.
- **Concurrency cap** via array `%` spec: `--array=0-{N-1}%$CONCURRENCY`
  (default 3; 2–4 recommended). Bounds simultaneous disk usage.
- Each task: `--ntasks=1 --cpus-per-task=16` (hardcoded). Partition / account /
  QOS / time-limit passed **as sbatch flags only when set** (env-overridable,
  empty defaults).
- Targets come from a file (`TARGETS_FILE`); archives/results go under `OUT_DIR`
  (one archive per scenario, default work dirs derived from each archive's stem
  are naturally distinct, e.g. `$B-id.astage`, `$B-raw.astage`).

## Interrupt / resume semantics

- **Idempotent per scenario**: each scenario writes one result object to
  `$RESULTS_DIR/$B.$SCENARIO.json` (atomic `mv`). A present result file ⇒ skip.
- **tar-dedup**: mid-run interrupt leaves the `.astage` work dir (never removed
  on failure); rerun with `tar-dedup resume --work-dir $ASTAGE --jobs $T`
  (preserves the stored ordering). A completed archive with cleaned work dir and
  missing result (node died during stat) ⇒ record without recompressing.
- **7z / tar.xz**: no mid-stream resume in those tools — a partial output is
  `rm`'d and that one scenario restarts. Completed scenarios are never redone.
- Recovery: simply re-run `submit.sh`; it is safe to repeat.

## Result format (JSON lines)

One JSON object per scenario run, merged by `collect.sh` into `results.jsonl`:

```json
{"target":"/data/x","scenario":"tdd-id","threads":16,"wall_ms":12345,
 "size_bytes":1048576,"archive":"/out/x-id.tdd.tar.xz","status":"ok"}
```

Wall time measured with `date +%s%N` around the whole scenario; size via
`stat -c %s`. `reused:true` marks a recorded-but-not-recompressed archive.

## Files

- `benchmarks/lzma-compare/run-scenario.sh` — run one (target, scenario), idempotent.
- `benchmarks/lzma-compare/bench.sbatch` — slurm array wrapper (task id → target/scenario).
- `benchmarks/lzma-compare/submit.sh` — builds sbatch flags, submits the array.
- `benchmarks/lzma-compare/collect.sh` — merge per-scenario JSON into `results.jsonl`.
- `benchmarks/lzma-compare/README.md` — usage.
- `docs/plans/085-…` — this plan.

## Order

1. `run-scenario.sh` → 2. `bench.sbatch` → 3. `submit.sh` → 4. `collect.sh` →
   5. README → 6. local smoke (run-scenario directly, no slurm).

## Verification

- Local (this host, no slurm): run each of the 6 `run-scenario.sh` invocations on
  a small tree; assert JSON result written, archive present, sizes sensible, and
  a second run skips (idempotent).
- On cluster: `submit.sh` submits the array with the concurrency cap; interrupt
  (`scancel`) mid-run then re-submit — completed scenarios skip, tar-dedup ones
  resume, only the in-flight 7z/xz restarts.
