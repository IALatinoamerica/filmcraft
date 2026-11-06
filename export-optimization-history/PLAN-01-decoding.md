# Implementation plan — Spec 01: Decoding

This plan turns [spec 01](01-decoding.md) into stages that can be worked through in order. Each
stage has a goal, concrete tasks, the checks that must pass, the measurement to take, and a
"done when" condition. The spec explains *why* and the design. This plan says *what to do, in which
order, and how to know it's finished*.

Read first: [README](README.md) (§0: how the agent team works), [TESTING.md](TESTING.md),
[spec 01](01-decoding.md).

**Who does what.** Each stage gets one orchestrator. The orchestrator splits the stage, briefs
GPT-6.1 Sol sub-agents (in parallel where files don't overlap), reviews their reports and decides.
Sub-agents do every hands-on step in this plan: scouting code, implementing, testing, gates,
builds, exports, `cmp`, logs, docs, PLAN updates, archiving and local commits. Where a task below
says "measure", "log" or "update", a sub-agent does it. Every step must be able to change a
decision; agents flag steps that can't instead of doing them.

## Status

| Stage | What | Size | Status |
|---|---|---|---|
| 0 | Working copy, branch, release build | S | Done (2026-11-05) |
| 1 | Baseline measurements and reference outputs | S (≈2 h machine time) | Done (2026-11-05) |
| 2 | Decode instrumentation | M | Done (2026-11-05) |
| 3 | Profile and decide (checkpoint) | S | Done (2026-11-05) |
| 4 | Don't read back unused pictures (spec §4.1) | M | Done (2026-11-05) — ~1.8% (342→336 s), readbacks −51% |
| 5 | Faster readback per picture (spec §4.2) | S–M | Done (2026-11-05) — gates passed, merged into `perf/export-amd` at `4d3fe712` |
| 6 | Pipeline decode and readback (spec §4.3) | M | Done (2026-11-05) — commit `140d702904fe4f9fc0976879d71317d5da88eafc` |
| 7 | Export decode scheduler (spec §4.4) | L | Done (2026-11-05) — gates pass except known test failures, not measured (Stage 9) |
| 8 | Remaining restarts and lock contention (spec §4.5–4.6) | S–M | 8.2 and 8.3 done; 8.1 not started — order 6 |
| 9 | Exit gate | S (+ ≈1 h machine time) | Done (2026-11-05); 9.1/9.5/9.6 skipped, resource observability limit recorded in [Stage 9](results/progress/stage-9.md) |

Sizes are relative (S < M < L) and only help order the work; they aren't schedules. Update the status
column as work happens. Use **Skipped (reason)** when Stage 3's measurements show an item isn't
worth doing.

Stage 3 may reorder Stages 4–8. The order below is the default.
**Decided by Stage 3** (`results/01-decode-profile.md`): 4 → 5 (5.2, 5.1; 5.3 with 6) → 6 → 8.3 → 7 → 8.1–8.2.

---

## Standard procedure for every code change

Every stage from 2 on follows this loop for each change. Sub-agents do every step; the
orchestrator reviews after step 2 and approves step 4.

**Measurements are only done in Stage 9.** Wherever a stage below says "measure", "Milestone …:
measure", "fps higher", "tune" or "keep the best", skip the measurement: use the spec's default
values (for example *D* = 3, *L* = 32, *P* = 120) and move on. Stage 9 measures the result once.

1. **Branch** from the integration branch `perf/export-amd`: `perf/01-<stage>-<short-name>`.
2. **Implement** with tests (spec §6), split across parallel sub-agents by file. Follow the ground
   rules (README §6): no panics, `unsafe` only in `crates/platform` with `// SAFETY:` comments, no
   GPL code. Step 3 can go to one sub-agent that runs it in the foreground.
3. **Gates:** `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`,
   `cargo test` for the touched crates, `cargo xtask layers`.
4. **No export, no benchmark per change** (Bruno, 2026-11-05: get it done). The change is accepted
   when the gates pass and the orchestrator approves the review. Commit it on `perf/export-amd`.
5. **Export only at the end of the spec** (Stage 9): one release build, one `bench-A-2min` `gpu-off`
   export, `cmp` against the reference and the time against the baseline. If that output differs,
   bisect the stage commits to find the change that did it.

**Build archive.** Code changes overwrite `target/release`, so comparisons need copies. Keep every
accepted build's CLI binary:

```
builds/<stage>-<short-commit>/filmcraft-cli
```

`builds/` sits in this folder next to `exports/`. The first entry is `builds/baseline-<commit>/`
(Stage 1). `bench.py --cli builds/<…>/filmcraft-cli` runs an archived build. Delete old archives
only after the spec is finished.

**Rebasing.** Don't rebase during this spec. Rebase `perf/export-amd` onto `upstream/main` only at
the exit gate (9.1), and only if upstream has changes worth having. Upstream changes can change
output bytes, so after a rebase:
- rebuild;
- re-create the baseline archive and `exports/reference/` from the rebased branch *without* this
  spec's changes, or accept the rebased build as the new reference once it reproduces the previous
  references;
- re-measure the last accepted build.

Never compare bytes across different upstream bases.

---

## Stage 0 — Working copy, branch, release build

**Goal:** a separate place to develop that doesn't disturb Bruno's checkout, and a release build.

**Tasks**
- [x] 0.1 `git fetch upstream origin`. Local `main` is 0 commits ahead and 20 behind `upstream/main`
  (2026-11-05), so fast-forward it: `git branch -f main upstream/main`. Ask Bruno before pushing
  `main` to `origin`.
- [x] 0.2 Create the integration branch `perf/export-amd` from `main`.
- [x] 0.3 Create a worktree so the existing checkout (on `fix/effect-drop-selection`) stays untouched:
  `git worktree add "/media/bruno/SSD 500 2 Linux/Software/filmcraft-perf" perf/export-amd`.
  All spec 01 work happens there.
- [x] 0.4 Release build in the worktree: `cargo build --release -p filmcraft-cli -p filmcraft`. The
  worktree has its own `target/`, so the first build is from scratch; the existing `target/` holds
  23 GB of debug builds. Use the same `CRAFT_FONTS_DIR` setting (unset) for every build.
- [x] 0.5 Run the full test suite once on the untouched branch (`cargo test --workspace`). Record any
  tests that already fail, so they aren't blamed on later changes.
- [x] 0.6 `bench.py --cli <worktree>/target/release/filmcraft-cli --project bench-quick-1min
  --settings gpu-off --label smoke --end 2 --delete-output`. It must work with the fork's CLI. Fix
  `bench.py` if the fork's CLI output differs from 0.17.1's.

**Done when:** the worktree builds in release mode, its CLI runs a smoke export through `bench.py`,
and pre-existing test failures (if any) are noted at the end of this file.

---

## Stage 1 — Baseline and reference outputs

**Goal:** know exactly how fast the unmodified fork is, and have outputs to compare against.

**Tasks**
- [x] 1.1 Archive the build: copy `target/release/filmcraft-cli` to `builds/baseline-<commit>/`.
- [x] 1.2 **Determinism check:** export `bench-quick-1min` twice with `gpu-off` and `cmp` the files,
  then twice with `gpu-auto`.
  - Expected: GPU Off identical.
  - Record whether GPU Auto is identical run to run. If it isn't, the GPU Auto correctness check in
    later stages compares decoded frames instead (spec §6).
- [x] 1.3 **Baseline runs** with the baseline build:

  | Project | Settings | Runs |
  |---|---|---|
  | bench-A-2min | gpu-off, gpu-auto | 3 each |
  | bench-B-2min | gpu-off, gpu-auto | 1 each |
  | bench-quick-1min | gpu-off, gpu-auto | 1 each |

  About 1 h 15 min of machine time at today's speed. Nothing else heavy running.
- [x] 1.4 **Reference outputs:** keep one export per project × settings (6 files) in
  `exports/reference/` as `<project>--<settings>--<commit>.mp4`. Delete the other baseline exports.
- [ ] 1.5 **Skipped (Bruno: short projects are the references)** — **Full-length references** (unattended, ≈50 min each): on a *copy* of the real project,
  export the whole timeline with the baseline build, `gpu-off` (required) and `gpu-auto` (if 1.2
  showed it's deterministic). Keep them in `exports/reference/` for the Stage 9 byte-identical
  check (~1.2 GB each). This can run any time before Stage 9, for example overnight, because the
  baseline binary is archived.
- [x] 1.6 Log everything in `results/log.md`, and fill in TESTING.md §6 (baseline on the test
  projects).

**Done when:** `builds/baseline-*`, the 6 short reference outputs and the baseline rows exist, and
the determinism result is recorded. The full-length references may still be pending.

---

## Stage 2 — Decode instrumentation (spec §4.0)

**Goal:** counters that say where decode time goes and why pictures get decoded.

**Tasks**
- [x] 2.1 **VA-API timing** in `crates/platform/src/vaapi/va.rs`: process-wide atomics for
  `submitMs` (`decode_into`: `vaBeginPicture`…`vaEndPicture`), `syncMs` (`vaSyncSurface` in
  `get_image`), `getImageMs` (`vaGetImage`), `copyMs` (`read_image` including
  `biplanar::to_frame`), and `readbacks`. Follow the existing `hw_stats` pattern in
  `filmcraft_codecs::hw`, so the platform crate reports through codecs without new dependencies.
- [x] 2.2 **Cache counters** in `crates/codecs/src/gop.rs`:
  - `usedFrames`: mark a cached frame used the first time it is returned, and count it once;
  - `seeksBy.{cutIn, evictedFrame, decoderRecycled, outOfOrder, other}`: classify in
    `decode_until` when `!continuing`:
    - `decoderRecycled` = the decoder was just created for a cache that had one before;
    - `evictedFrame` = the wanted pts was decoded before by this cache, then evicted;
    - `outOfOrder` = the running decoder is past the wanted sample;
    - `cutIn` = first request for this cache, or a jump forward beyond `CONTINUE_THROUGH`;
  - `lockWaitMs`: time between requesting the per-source state lock and getting it;
  - `liveDecodersPeak`.
- [x] 2.3 **Expose** all of them under `decode` in `perf.stats` (`crates/engine/src/perf.rs`). Add the
  key ones to `bench.py`'s one-line summary.
- [x] 2.4 **Tests:** unit tests for the seek classification on a synthetic sample list (gop.rs
  already has test pools: `Pool::new(...)` under `cfg(test)`). Check that the counters only grow,
  and that `perf.stats` contains the new keys.
- [x] 2.5 **Replaced (Bruno): one correctness run + first counter readings, no overhead benchmark** — **Overhead check:** `bench-A-2min` `gpu-off`, 3 runs alternating with the baseline. Output
  byte-identical, fps within noise.

**Done when:** the counters appear in `bench.py` run records, tests pass, there is no measurable
slowdown, and output is byte-identical. Archive as `builds/02-instrumentation-<commit>/`. It becomes
the comparison build for Stage 4.

---

## Stage 3 — Profile and decide (checkpoint)

**Goal:** answer the spec's hypotheses H1–H4 with numbers, and fix the order of Stages 4–8.

**Tasks**
- [x] 3.1 Use the instrumented build's `bench-quick-1min` `gpu-off` run record from Stage 2 (no new
  run). Compute from the run record:
  - share of readbacks never used: `(readbacks − usedFrames) / readbacks`;
  - per-readback split: `syncMs`, `getImageMs`, `copyMs` ÷ `readbacks`;
  - restart causes: `seeksBy.*` as shares of `seeks`;
  - `lockWaitMs` as a share of export wall time;
  - `liveDecodersPeak`.
- [x] 3.2 **Profile:** `perf record -g` on the CLI process during **one** `bench-quick-1min` `gpu-off`
  export (~3 min).
  Top 10 functions by self time and the share inside VA-API / libva / Mesa.
- [x] 3.3 **Raw decode speed** for one OBS camera recording, one OBS screen recording and one downloaded
  H.264 clip from `test-projects/media-used.txt`:
  - FilmCraft: `filmcraft-cli bench-decode <file> --frames 3000`;
  - oracle: ffmpeg `-hwaccel vaapi … -vf hwdownload,format=nv12 -f null -` (spec §4.0).
  Record ms per picture for both.
- [x] 3.4 Write `results/01-decode-profile.md` with the numbers, the answers to H1–H4, and the
  decisions below.
- [x] 3.5 **Decide** with these rules, then update the status table:

  | If… | Then… |
  |---|---|
  | unused readbacks ≥ 20% | Stage 4 stays first |
  | unused readbacks < 5% | skip Stage 4 |
  | `copyMs` is the largest part of a readback | Stage 5 task 5.1 (bulk copy) first |
  | `getImageMs` is the largest | Stage 5 tasks 5.2–5.3 (derive image, ring) |
  | `syncMs` is the largest (CPU waiting for the GPU's decode) | do Stage 6 (pipelining) before Stage 5 |
  | `decoderRecycled` ≥ 10% of restarts | move task 7.4 (decoder pinning) ahead as a standalone change right after Stage 4 |
  | `lockWaitMs` ≥ 10% of wall time | move task 8.3 (lock split) ahead of Stage 7 |
  | FilmCraft's raw decode ≈ ffmpeg's | the problem is how export uses decoders: Stages 4, 7, 8 matter most |
  | FilmCraft's raw decode ≫ ffmpeg's | per-picture cost matters: Stages 5, 6 matter most |

**Done when:** `results/01-decode-profile.md` exists and the status table reflects the decided order.

---

## Stage 4 — Don't read back unused pictures (spec §4.1)

**Goal:** pictures decoded only as references on the way to a wanted frame are never copied to the
CPU.

**Tasks**
- [x] 4.1 **Trait:** add a default no-op `fn set_output_floor(&mut self, pts: Option<i64>)` to
  `VideoDecoder` (`crates/codecs/src/video.rs`), documented: pictures whose pts is below the
  floor are decoded but not returned.
- [x] 4.2 **Export-only switch:** add an `export` flag to `filmcraft_media::cancel::Context`, the
  per-thread context that already carries cancel, background and draft, with a
  `with_export(on, f)` helper like `with_background`.
  - `isolated()` in `gop.rs` already moves the `Context` onto the `decode-N` threads, so the flag
    reaches the decoder.
  - The export sets it around frame rendering (`job.rs` `render_batch`, and the prefetch inside
    `step_batch`).
- [x] 4.3 **Cache:** in `GopCache::decode_until`, when a run starts (`!continuing`) and the export flag
  is on, call `set_output_floor(Some(want_pts))`. Clear it (`None`) on reset, on flush and when the
  run ends. Keep the "nearest decoded frame at or before the wanted pts" fallback working: never
  floor above `want_pts`.
- [x] 4.4 **VA-API fronts:** in `vaapi/hevc.rs` and `vaapi/h264.rs` `read_out`, skip
  `accel.read(...)` for outputs below the floor. DPB bookkeeping and surface reuse stay unchanged:
  only the copy is skipped.
- [x] 4.5 **Tests:**
  - decoder-level: generate an H.264 and an HEVC fixture with B-frames and a 250-frame GOP
    (ffmpeg into `target/fixtures/`). Seek to mid-GOP with the floor; frames ≥ floor are identical
    to a run without the floor, and frames < floor are not returned. Hardware tests skip without
    VA-API;
  - cache-level: with the flag off, behaviour is unchanged (existing gop tests pass); with it on,
    `readbacks` drops and the returned frames are identical;
  - leading pictures: an HEVC CRA with RASL pictures, and an open-GOP H.264. The wanted frame must
    still come back.
- [x] 4.6 Standard procedure steps 3–9. Expect `readbacks` to fall toward `usedFrames`.

**Done when:** output byte-identical, `readbacks ≈ usedFrames` (plus cache-fill frames), fps
measurably higher, archived as `builds/04-output-floor-<commit>/`.

---

## Stage 5 — Faster readback per picture (spec §4.2)

**Status:** done (2026-11-05); gates passed, merged into `perf/export-amd` at
`4d3fe712ea5a560f1661457e216f9fbcf25b0a33`. Task 5.3 remains deferred to Stage 6.

**Goal:** lower the cost of each picture that is read back. Try options in order; stop when the
per-readback cost is close to ffmpeg's (Stage 3.3) or the remaining options don't help.

**Tasks**
- [x] 5.1 **Bulk copy:** in `va.rs` `read_image`, copy the whole mapped buffer with one
  `copy_from_slice` into a pooled CPU buffer (`filmcraft_frame::pool`), unmap, then run
  `biplanar::to_frame` on the CPU copy. Measure `copyMs` per readback before and after.
- [x] 5.2 **`vaDeriveImage` probe:** add `vaDeriveImage` to the loaded API (`Api` in `va.rs`;
  declaration in `ffi.rs`; ABI test). Per session, try it once on a decoded surface. If it succeeds
  and gives NV12, use it instead of `vaGetImage` (map the derived image directly). Otherwise
  remember the failure and keep `vaGetImage`.
- [x] 5.3 **Image ring** — done in Stage 6: replace the single
  session `VAImage` with a small ring, so a `vaGetImage` can be issued while the previous
  picture's copy runs.
- [x] 5.4 **Tests:** `biplanar::to_frame` unit tests for crop offsets, odd crops, 8- and 10-bit (P010),
  strides wider than the picture. The output must be identical to the strided path. Hardware tests
  for each readback method.
- [x] 5.5 End-of-stage gates passed and local integration completed; measurements deferred to Stage 9.

**Done when:** each option tried is either kept (faster, byte-identical) or recorded as rejected,
and per-readback cost is logged. Archive as `builds/05-readback-<commit>/`.

---

## Stage 6 — Pipeline decode and readback (spec §4.3)

**Goal:** the GPU decodes the next pictures while the CPU copies earlier ones.

**Tasks**
- [x] 6.1 **Surfaces:** in `VaDecoder::new` (`vaapi.rs`), raise the surface count from `DPB + 2`
  to `DPB + 2 + D` (cap 24). Make *D* a constant, default 3, overridable for experiments by
  environment variable (`FILMCRAFT_VAAPI_DEPTH`, 0 = today's behaviour).
- [x] 6.2 **Pending queue** in the H.264 and HEVC fronts:
  - outputs from the DPB go into a FIFO of `(surface, meta)` instead of being read at once;
  - `decode()` reads back and returns the oldest entries only when the queue exceeds *D*;
  - `flush()` drains it; `reset()` drops it, releasing holds.
- [x] 6.3 **Surface hold:** a surface in the pending queue must not be reused for a new picture. Add an
  explicit "pending readback" flag checked where the front picks a free surface. Picking a surface
  while all are held is an error (`CodecError`), never a panic.
- [x] 6.4 **Interaction with Stage 4:** outputs below the floor never enter the queue.
- [x] 6.5 **Cache:** `decode_until` stops when the wanted frame is cached. With *D* > 0 the wanted frame
  can sit in the queue, so the loop must flush or drain the queue when the stream ends or the
  target is reached but not yet output. Check that `continuing` logic and `st.next` stay consistent
  (comments in `decode_until`). Existing playback tests must pass.
- [x] 6.6 **Tests:** same frames, order and pts as *D* = 0, for H.264 and HEVC fixtures with
  reordering, with seeks mid-queue, flush at end of stream, and reset during a pending queue.
- [ ] 6.7 Measure *D* = 3 on `bench-A-2min` (one run; *D* = 0 is the previous build's recorded time).
  Try *D* = 2 or 4 (one run each) only if `syncMs` per readback suggests the depth is the limit.
  Keep the best as the default. Then the standard procedure steps 3–9.
  **Status:** default D=3; measurement deferred to Stage 9.

**Done when:** byte-identical output, `syncMs` per readback lower, fps higher, archived as
`builds/06-pipelined-<commit>/`.

---

## Stage 7 — Export decode scheduler (spec §4.4)

**Goal:** decoding for export runs ahead, in order, per source, so render workers mostly hit the
cache and cuts don't stall batches. This is the largest stage, so it is delivered in milestones,
each measured.

**Tasks**
- [x] 7.1 **Source frame mapping.** Find the function the renderer uses to turn
  `(clip, sequence time)` into a source request (time remap, speed, reverse, VFR pts lookup):
  start from `filmcraft_render`'s layer/source lookup used by `plan_frame` and `render_sequence`.
  Expose it for the export, or a thin wrapper, so the scheduler computes **exactly** the same
  source pts as the renderer. Test: for every frame of the three test projects, the scheduler's
  predicted requests equal the requests the renderer makes (record them with a test
  `SourceProvider`).
- [x] 7.2 **Prefetch hook.** Add a default no-op method to `MediaSource` (`crates/media/src/lib.rs:216`),
  for example `fn prefetch(&self, requests: &[FrameRequest])`. Implement it for the container
  sources backed by `GopCache` (MP4/MOV, Matroska): decode the requests in order into the cache.
  The export reaches sources through `SourceProvider::source(item)`, so `crates/export` doesn't
  depend on `crates/codecs` internals. `cargo xtask layers` must pass.
- [x] 7.3 **Scheduler skeleton** (`crates/export/src/schedule.rs`):
  - created in `Exporter::set_up`;
  - walks the sequence from `next` to `next + L` (look-ahead *L*, default 2 × batch = 32 frames);
  - groups the requests per media source in output order;
  - submits them through `prefetch` on the `decode-N` pool, at most one job per source at a time
    and ≤ 8 sources at once;
  - runs inside the export `Context` (Stage 4.2), so floors and cancellation apply.
  **Milestone A:** measure. Even without pre-roll, in-order feeding should cut `outOfOrder`
  restarts and render waits.
- [x] 7.4 **Decoder pinning.** While an export runs, caches with scheduled work aren't counted toward
  `MAX_LIVE_DECODERS` / `HARD_DECODERS` and aren't recycled (`gop.rs` `trim_decoders`). Release the
  pin when the clip's last needed frame is delivered or the export ends. Log `liveDecodersPeak`.
  **Milestone B:** measure; `seeksBy.decoderRecycled` should be ~0.
- [x] 7.5 **Pre-roll for cuts.** Extend the look-ahead for clip starts: when a clip begins within the
  next *P* frames (default *P* = 120, two seconds), start its decode early from the keyframe, with
  the output floor at the clip's first needed frame. **Milestone C:** measure; render waits at cuts
  should disappear.
- [x] 7.6 **Budget.** Frames delivered ahead count against the cache budgets. Size *L* and *P* so
  `active sources × (L + pre-roll frames) × frame size` stays under a fixed export decode budget
  (1.5 GB to start). Evict only frames behind the export position.
- [x] 7.7 **Cancellation and errors.** On cancel, error or drop: the scheduler stops at the next sample,
  releases pins, and leaves caches consistent. Test: cancel mid-export and immediately export
  again in the same process; no leaked sessions (`displays_opened()`, `live_decoders()` back to
  idle values).
- [x] 7.8 **Tests:**
  - synthetic timeline with cuts into mid-GOP in-points, a speed-changed clip, a reversed clip,
    a time-remapped clip, two clips from the same file overlapping on different tracks, and a
    still;
  - prefetch delivers exactly the renderer's requests;
  - output byte-identical with the scheduler on and off.
  Keep an environment switch (`FILMCRAFT_EXPORT_SCHEDULER=0`) to compare.
- [ ] 7.9 Tune *L*, *P* and the source limit on `bench-A-2min`: at most three configurations, one
  run each, chosen from the counters of the milestone runs. bench-B is checked once, at the exit
  gate. Then the standard procedure steps 3–9.

**Done when:** milestones A–C measured and logged; byte-identical output; render workers' decode
waits are small (`lockWaitMs`, and cache misses during export near the count of unavoidable cuts);
archived as `builds/07-scheduler-<commit>/`.

---

## Stage 8 — Remaining restarts and lock contention (spec §4.5–4.6)

**Goal:** clean up whatever restarts and waits the earlier stages left.

**Tasks**
- [ ] 8.1 Read `seeksBy.*` from the latest runs. For each remaining cause above ~2% of restarts,
  apply the fix in spec §4.5's table and measure.
- [x] 8.2 `evictedFrame` restarts: raise the per-source budget for scheduled sources during export,
  or evict only frames behind the export position.
- [x] 8.3 **Lock split** (only if `lockWaitMs` is still ≥ 5% of wall time): separate the cache's frame
  map lock from the decoder lock, so a cache hit never waits behind a decode. This needs careful
  review of `decode_until` / `store_output` / eviction ordering. Add a stress test with many threads
  requesting frames while a decode runs.
- [ ] 8.4 Standard procedure steps 3–9 for each kept change.

**Done when:** restarts ≈ unavoidable cuts, lock waits small, each change logged.

---

## Stage 9 — Exit gate

**Goal:** confirm the spec is finished and hand over to spec 02.

**Tasks**
- [x] 9.1 **Skipped:** no rebase, per Bruno.
- [x] 9.2 Full CI run once: 4,862 passed, 7 failed, 52 skipped; six known failures plus the
  UI playback timing flake, already triaged 3/3 in isolation. No reruns; see Notes.
- [x] 9.3 One release CLI build and one `bench-A-2min` `gpu-off` export, label `01-final`:
  303.779 s / 23.701 fps, 1.126× baseline; `cmp` byte-identical. No bisect required.
- [x] 9.4 Same-run resource check: no errors, warnings or offline media; live decoders, frame
  cache and plane pool return to 0. VA `hardware.sessions` is cumulative (17), and no active-session
  gauge is exposed; independent zero-active-VA-session verification is unavailable.
- [x] 9.5 **Skipped:** separate cancel/re-export check covered by the Stage 7.7 unit test.
- [x] 9.6 **Skipped:** Bruno does playback.
- [x] 9.7 Completed `results/01-decode-summary.md` with the final run, all counters, bottleneck,
  deferred items and 8.2 commit `c3ce0655`; CLI archived at `builds/01-final-c3ce0655/filmcraft-cli`.
- [x] 9.8 Updated only spec 01's README status row. **Spec 02 plan skipped per explicit scope.**

**Gate result:** automated work complete with the documented skips and resource observability
limit. Bruno's summary review and playback remain with Bruno. Details: [Stage 9 progress](results/progress/stage-9.md).

---

## Notes

- Stage 9 (2026-11-05): one final `c3ce0655` release build and `bench-A-2min` gpu-off export:
  303.779 s / 23.701 fps, 1.126× baseline, byte-identical (`cmp` exit 0); no bisect. Full CI's single
  run had 4,862 passed, 7 failed, 52 skipped: six known failures plus `filmcraft-ui-egui`
  `perf_ui::program_monitor_plays_on_while_a_value_is_dragged` (`perf_ui.rs:323`, four distinct frames).
  This live-playback test uses 16 ms sleeps; earlier isolation triage on `c3ce0655` passed 3/3
  (~2.7 s each), so the extra failure is timing flakiness under full-CI load, not a regression.
  CI and the test were not rerun. Live decoders/cache/pool returned to 0; VA sessions is a cumulative
  creation counter (17), with no active-session gauge. 9.1/9.5/9.6 and the spec 02 plan skipped as
  instructed. Details: `results/progress/stage-9.md`.

- Stage 7 (2026-11-05): done; commit `b21736ec3e3e004129a2367eeb6380a8e56cca88` on `perf/01-7-scheduler`, rebased onto `140d7029`; fmt, workspace clippy and layers PASS, touched-crate tests have only the four known engine/media failures after minimal gate fixes; 7.1–7.8 checked, 7.9 and milestones skipped per Bruno (Stage 9 measures); behind-position cache eviction deferred; fast-forwarded into `perf/export-amd`, worktree removed; details: `results/progress/stage-7.md`.

- Stage 6 (2026-11-05): done; commit `140d702904fe4f9fc0976879d71317d5da88eafc`, fast-forwarded into `perf/export-amd` without rebase; fmt, workspace clippy and layers PASS; platform tests 149 passed (export suites excluded), codecs tests 244 passed / 3 ignored; 5.3 done in Stage 6; default D=3, measurement deferred to Stage 9; H.264 unused synthetic-reference fix also affects D=0 frame_num-gap streams, so Stage 9 must check the byte comparison; details: `results/progress/stage-6.md`.

- Stage 8.3 (2026-11-05): 8.3 done; commit: c3729afb; gates: `cargo fmt --check` PASS; `cargo clippy --workspace --all-targets -- -D warnings` PASS; `cargo test -p filmcraft-codecs` PASS (243 passed, 0 failed, 3 ignored); `cargo xtask layers` PASS; details: `results/progress/stage-8.md`.

- Stage 5 (2026-11-05): done, all four gates passed; commit `4d3fe712ea5a560f1661457e216f9fbcf25b0a33`, fast-forwarded into `perf/export-amd`; 5.3 deferred to Stage 6; export test suites skipped as instructed; details: `results/progress/stage-5.md`.

- Stage 0 (2026-11-05): `main` fast-forwarded locally 0b780f6f → `46763a85` (= `upstream/main`, not
  pushed); `perf/export-amd` created there; worktree `/media/bruno/SSD 500 2 Linux/Software/filmcraft-perf`.
  Release build from scratch 3m52s; CLI reports `filmcraft-cli 0.17.0`. Smoke via `bench.py` OK.
  `bench.py` fix: it decided the fork commit by `cli.startswith(<original checkout path>)`, which the
  worktree path also matches; it now asks git for the repo containing the CLI binary.
  Details: `results/progress/stage-0.md`.
- Pre-existing test failures (Stage 0.5, `cargo test --workspace --no-fail-fast` on `46763a85`):
  4816 passed, **6 failed**, 52 ignored; log `results/progress/stage-0-tests.log`. Five depend on the
  system ffmpeg 6.1.1 as oracle/fixture generator; (2) touches the export path:
  1. `filmcraft-av1` `fuzz_hostile::hostile_streams_never_panic` (ffmpeg libsvtav1 can't encode the seed)
  2. `filmcraft-cli` `workflows::phone_vfr_clip_extract_gain_title_to_h264_mp4` (frame 20 matches the
     right source frame, but diff 19.04 is over the threshold)
  3. `filmcraft-engine` `export_tests::every_builtin_preset_exports_and_ffprobe_confirms` (ffprobe sees
     no codec for APV 422-10)
  4. `filmcraft-media` `tiff_ycbcr::ffmpeg_ycbcr_tiffs_match_ffmpeg_decode` (yuv420p diff 17)
  5. `filmcraft-media` `exr_oracle::exr_sequence_plays_linear_float_frames` (ffmpeg EXR decode fails)
  6. `filmcraft-media` `exr_oracle::exr_stills_match_ffmpeg` (ffmpeg half-float EXR decode fails)
- GPU Auto determinism (Stage 1.2): **deterministic.** GPU Off and GPU Auto outputs are both
  byte-identical run to run (quick ×2, bench-A ×3 per setting), so the `cmp` check applies to both.
- Stage 1 (2026-11-05): baseline CLI archived at `builds/baseline-46763a85/filmcraft-cli`; 6 references in
  `exports/reference/`; baseline in TESTING.md §6 and `results/log.md`. Decoded-picture and seek counts vary
  run to run with identical output, so compare their medians. Records of archived builds show
  `build.commit: "installed build"` (no git repo around `builds/`). 1.5 skipped by Bruno.
  Details: `results/progress/stage-1.md`.
- Stage 2 (2026-11-05): commit `2ca399b5` (fast-forwarded into `perf/export-amd`), archived at
  `builds/02-instrumentation-2ca399b5/`. New `decode` keys in `perf.stats`: `submitMs`, `syncMs`, `getImageMs`,
  `copyMs`, `readbacks`, `usedFrames`, `seeksBy.*`, `lockWaitMs`, `liveDecodersPeak`; `bench.py` prints them on a
  second summary line. Gates green except the known APV failure (3). 2.5 replaced (Bruno): bench-quick-1min gpu-off
  byte-identical to the reference; no overhead benchmark. Details: `results/progress/stage-2.md`.
- Stage 3 (2026-11-05): `results/01-decode-profile.md`. 62% of readbacks unused; per readback sync 2.5 / getImage 3.4 /
  copy 1.5 ms; restarts evicted 48%, cutIn 36%, outOfOrder 16%, recycled 0%; lockWait 30% per render thread (16);
  raw decode 2.5× ffmpeg (HEVC), 1.5× (H.264); profile: VA/Mesa user space 0.09%, CPU time mostly rendering.
  Order 4 → 5 → 6 → 8.3 → 7 → 8.1–8.2, nothing skipped. Details: `results/progress/stage-3.md`.

- Stage 4 (2026-11-05): commit `e510c586` (fast-forwarded into `perf/export-amd`), archived at
  `builds/04-output-floor-e510c586/`. Output floor kept: ~1.8% faster (342→336 s), readbacks −51%; all outputs
  byte-identical, both new runs below all old runs. Run 1: 337.0 s / 21.39 fps vs 342.0 s baseline;
  readbacks 14,815 / used 12,351, 2.06 decodes/frame, seeks 140 (cutIn 49, evictedFrame 15, outOfOrder 76).
  Old/new pair: 342.2 s / 21.07 fps → 335.6 s / 21.48 fps; readbacks 30,707 / used 12,358 →
  15,190 / used 12,356; 4.26 → 2.11 decodes/frame; seeks 112 (outOfOrder 18) → 185 (cutIn 62,
  evictedFrame 22, outOfOrder 101); VA sync/getImage/copy 73.6/110.8/48.0 s → 43.0/86.7/34.6 s.
  Gain partly eaten by more outOfOrder seeks: frames below the floor aren't cached, so later requests
  re-seek. Follow-up for Stage 7/8 or a later refinement: "floor at the earliest pts the caller may still need".
  Gates green except known media failures (4–6); log `results/progress/stage-4-gates.log`.
  Details: `results/progress/stage-4.md`.

### Preparation checks (2026-11-05, before Stage 0)

- Disk: 119 GB free on `SSD 500 2 Linux`, enough for the worktree's build folder and the
  reference exports.
- Toolchain: Rust/cargo 1.97.1 (the workspace needs ≥ 1.95), clippy, rustfmt, the
  `wasm32-unknown-unknown` target (for `cargo xtask wasm`), and `cargo-nextest` 0.9.151, which
  `cargo xtask ci` uses when present.
- Tools: ffmpeg with libx264 / libx265 (fixtures) and VA-API, ffprobe, vainfo, perf 7.0.14, git, gh
  (logged in as IALatinoamerica).
- All 41 media files of the test projects present.
- Automatic sleep on AC is off (`sleep-inactive-ac-timeout` 0), so unattended benchmark runs
  won't be interrupted.
- `perf` profiling: Bruno set `kernel.perf_event_paranoid=1` on 2026-11-05, and `perf record`
  works unprivileged. **It resets on reboot:** if the machine restarts before Stage 3, ask Bruno to
  run `sudo sysctl kernel.perf_event_paranoid=1` again.
- Git: commit and create branches freely in the fork, locally. Don't push to any remote
  (Bruno, 2026-11-05). Stage 0.1's "ask before pushing `main`" therefore means: update `main`
  locally only.
- Bruno frees the machine before measurement runs start, and the test media won't be touched.
- First data point for Stage 3.3, one OBS camera recording (`2026-10-31 14-22-54.mp4`), 600 frames:
  - installed FilmCraft 0.17.1 `bench-decode`: 228.5 fps (4.4 ms/picture, sequential);
  - ffmpeg VA-API decode + download to NV12: 1.13 s wall including start-up, i.e. ≥ 530 fps.
  Re-measure with the fork build and more frames in Stage 3.3.
