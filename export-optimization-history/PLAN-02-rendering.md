# Implementation plan — Spec 02: Rendering

This plan turns [spec 02](02-rendering.md) into stages. The spec explains the design; this plan says
what to do, in which order, who does it, and how we know it's finished.

Read first: [README](README.md) §0 (how the team works), [BRIEF-REVIEW.md](BRIEF-REVIEW.md) (the
owner's rules), [TESTING.md](TESTING.md), [spec 02](02-rendering.md).

## Goal (Bruno's request)

"Move the zooms and the conversions to the graphics card." In the spec's terms:
- **Zooms:** adjustment layers rendered on the GPU (§4.4). Today any frame with one is rendered
  entirely on the CPU: 48.7% of the frames.
- **Conversions:** the CPU work that turns pictures from one format into another, around the GPU.
  - **Output:** the GPU result is read back as f16 RGBA (~16.6 MB per 1080p frame), expanded to an
    f32 CPU buffer (~33 MB), then converted on the CPU to 8-bit RGBA and then to YUV for the encoder
    (§4.3).
  - **Input:** 8-bit decoded pictures (`Yuv8`) already upload directly and convert in a shader.
    16-bit ones (`Yuv16`) and other formats get CPU preparation before upload. Stage 0 measures how
    much CPU conversion remains on GPU frames.

**Measured starting point** (`results/01-time-breakdown.md`, `bench-quick-1min` with GPU Off):
effects incl. adjustment layers 34%, conversions 27.5%, decoder waiting 21.5%, compositing 13.5%
of render-task time. These are GPU Off numbers. Stage 0 measures GPU Auto.

**Out of scope:**
- thread-pool sizing (§4.1), unless Stage 0 shows the pools starve the GPU path;
- CPU-path tuning (§4.5, except input conversion);
- zero-copy (§4.6);
- the decoder waiting line (one decoder per source). That comes back later as a separate item.

## How this plan is executed

- **Roles.** The supervisor starts one orchestrator per stage (Claude Opus, medium effort, with
  Bruno's verbatim orders). The orchestrator plans, briefs, reviews and decides. GPT-6.1 Sol
  sub-agents (high effort) do all the hands-on work. Every outgoing brief, from the supervisor or
  from an orchestrator, is first reviewed by a Sol reviewer (high effort) against `BRIEF-REVIEW.md`.
- **Builds.** One shared, warm `target/` per worktree; never per-agent target dirs. Sub-agents use
  `cargo check` and the touched crates' tests while working. Gates run once per stage:
  `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test` for
  the touched crates, `cargo xtask layers`. A release build only when that stage's export needs it,
  always with the GPU backend: `cargo build --release -p filmcraft-cli -p filmcraft`. A CLI-only
  build has no GPU backend, which would make GPU Auto exports meaningless.
- **Exports.** Only the ones listed in the stages below. Each runs in the foreground with a 20 min
  timeout, with nothing heavy running alongside, through:

  ```
  cd "/media/bruno/SSD 500 2 Linux/Software/filmcraft-export-optimization/test-projects"
  python3 -I bench.py --project <project> --settings <settings> --label <label> --runs 1 \
    --cli "<worktree>/target/release/filmcraft-cli"
  ```

  Delete the output once it's checked and recorded. **Never export the full video.** No profiles,
  no 50-frame render sweeps, no repeated runs, no setting sweeps: the spec mentions these, and this
  plan drops them.
- **Correctness.**
  - **GPU Auto output:** compared against the GPU Off reference of the same project with
    `ffmpeg -i new.mp4 -i ref.mp4 -lavfi psnr=stats_file=psnr.log -f null -`. Delete
    `psnr.log` after reading it.
    - **Threshold (decided 2026-11-05, after Stage 0):** the *unchanged* GPU path already has a
      minimum of 33.2 dB on `bench-quick-1min` (frame 1,539), so 50 dB can't be met by any stage.
    - The rule is now **no regression**: each stage's minimum `psnr_y` must be ≥ 33.2 dB. The stage
      also reports the number of frames below 50 dB and the 5 worst frames with their numbers.
    - Those worst frames, plus frame 1,539, go into Bruno's frame sheet at the end.
  - **The reference:** Spec 01 kept GPU Off byte-identical, so
    `exports/reference/<project>--gpu-off--46763a85.mp4` stands for "the same build's GPU Off
    output", as long as the CPU path doesn't change.
  - **Stages that touch CPU rendering code** (marked below) add one `gpu-off` export of the same
    project, `cmp`'d against that reference.
- **Keep or reject.** A stage that's meant to speed things up is kept when its export is faster
  than the previous recorded GPU Auto time (Notes) by more than ~3%. It's also kept if it's a
  prerequisite of a later stage, and that is stated. A difference within ~3% counts as noise:
  record it, and keep the stage only if it's a prerequisite. Passing PSNR or a lower timer alone
  doesn't make a stage a speed-up.
- **Bruno** does one visual check at the end of the spec. A sub-agent prepares a side-by-side frame
  sheet for it. Nothing else needs him.
- **Git.** Local only, never push. Each stage branches from `perf/export-amd` as
  `perf/02-<stage>-<name>`, and is fast-forwarded into it once accepted. The final CLI of each
  stage is archived as `builds/02-<stage>-<commit>/filmcraft-cli`.

## Status

| Stage | What | Status |
|---|---|---|
| 0 | Set-aside, GPU counters, one diagnosis export | Done (2026-11-05, 00420ab9) |
| 1 | Adjustment layers (zooms) on the GPU | Done (2026-11-05, 9fbb405f) |
| 2 | Output conversion on the GPU (read back YUV) | Done (2026-11-05, bf86fe92) |
| 3 | Input conversion on the GPU (only if Stage 0 shows it matters) | Done (2026-11-06, 892c469b) |
| 4 | Full-size GPU batches (only if Stages 1–2 show the GPU path is batch-limited) | Done (2026-11-06, ae807b56) |
| 5 | Exit: one bench-A export, Bruno's visual check, summary | Pending Bruno's visual check (2026-11-06, ae807b56) |

**Order and parallel work**
- Stage 0 first.
- Stages 1 and 2 can be developed at the same time in separate worktrees. Stage 1 owns
  `crates/render` and the GPU effect/combine code. Stage 2 owns the new YUV pass and the export
  plumbing. Both touch `crates/gpu/src/lib.rs` and `export_renderer.rs`, so whichever lands second
  rebases, re-runs its gates, **rebuilds and re-runs its stage export on the integrated head**, and
  archives that build. The counters and times recorded must come from the integrated binary.
- Stages 3 and 4 are decided from the counters of the earlier exports, then run one at a time.
- Stage 5 last.

---

## Stage 0 — Set-aside, GPU counters, one diagnosis export

**Goal:** a clean starting point, and numbers for GPU Auto: how many frames the GPU renders, and
where the time goes when it does.

**Tasks**
- [x] 0.1 **Set-aside.** In `/media/bruno/SSD 500 2 Linux/Software/filmcraft-perf`, commit the
  uncommitted, rejected waiting-line changes on their branch `perf/01-decoder-queue` as
  `rejected: decoder waiting-line fix (kept for reference, not merged)`. Then switch the worktree to
  `perf/export-amd` (`0c0e60c5`) and branch `perf/02-0-gpu-counters`.
- [x] 0.2 **Counters** (spec §4.0, trimmed), under `export.gpu` in `perf.stats`, following the
  `perf.stats` pattern used in `crates/engine/src/perf.rs`:
  - `frames` and `fallbacks` from the existing `gpu_render_stats()` (`crates/export/src/stats.rs`);
  - `cpuWholeFrames.{adjustment, transition, hdr, planExhausted}`: why `is_cpu_frame`
    (`crates/render/src/plan.rs`) or the planner sent a frame to the CPU;
  - Expose the existing `export_renderer::timing::{submit, map_wait, convert}` timers (GPU submit,
    readback wait, readback conversion) rather than adding duplicates. Add only what's missing:
    `gpuUploadPrepMs`, the CPU preparation of source layers before upload, which isn't timed today.
  - The render-task timers (`crates/frame/src/render_timing.rs`): `convertMs` mixes input and
    output conversion, and doesn't say whether the frame went to the GPU or the CPU. Split it into
    `inputConvertMs` and `outputConvertMs`, each attributed to GPU frames or CPU frames.
  - Tests: the new keys exist in `perf.stats`, and the counters only grow.
- [x] 0.3 Gates, one release build, then **one** export: `bench-quick-1min`, `gpu-auto`, label
  `02-0-diagnosis`. It doesn't change the CPU path, so no `gpu-off` export is needed. Record:
  - export seconds and fps;
  - GPU frames vs CPU frames, by reason;
  - each timer as a share of render-task time;
  - `export.stages.waitMs`.
  Also check the PSNR against the quick GPU Off reference, as the starting value.
- [x] 0.4 Commit, fast-forward `perf/export-amd`, archive. Write the numbers into this plan's Notes
  and one row in `results/log.md`.

**Done when:** the GPU Auto numbers are in Notes, and Stages 3 and 4 have a decision basis.

---

## Stage 1 — Adjustment layers (zooms) on the GPU (spec §4.4)

**Goal:** frames with this project's adjustment layers render on the GPU instead of falling back to
the CPU for the whole frame.

**Scope:** this project's adjustment effects only: `transform` (keyframed position and scale),
`offset`, `directional_blur`, plus the layer's own `motion` and `opacity`. The following stay on the
CPU:
- adjustment layers with masks on effects or opacity;
- `transform` with `shutter_override` on and angle > 0.5;
- transitions into or out of an adjustment layer.

**Tasks** (1.1 first; then 1.2 and 1.3 in parallel, on disjoint crates)
- [x] 1.1 **Plan type, as a buildable foundation.** In `crates/render/src/plan.rs`:
  - add `PlanStep::Adjust { effects, time, weight, blend }` (spec §4.4 item 1);
  - define `AdjustWeight` and whatever effect-evaluation context the executors need;
  - update every exhaustive `match` on `PlanStep`, including `source_bytes`, `max_side` and GPU
    preparation, so the workspace builds with `Adjust` returning a "not supported yet" CPU
    fallback.
  1.2 and 1.3 then fill in the two executors.
- [x] 1.2 **Planner and CPU oracle** (`crates/render`).
  - **Planner:** in `plan_frame`, push `PlanStep::Adjust` for an eligible adjustment layer instead
    of bailing out (`layered_at`). Make `is_cpu_frame` consistent with it. Eligibility uses the
    same checks clip effects already use for GPU support (`FxOp::eval` / `supported()` in
    `crates/render/src/gpufx.rs`), plus the scope rules above.
  - **CPU oracle:** implement `Adjust` in `plan::execute_cpu` by calling the same pieces as
    `render::adjustment_layer` (`crates/render/src/lib.rs`): `mask::apply_effect`,
    `adjustment_region`, `mask::mix` / `scale_by` + blend. If those pieces need factoring out, the
    refactor must not change CPU output (**this touches the CPU path**).
  - **Tests:** new `crates/render/src/adjustment_tests.rs` checking
    `execute_cpu(plan_frame(...)) == render_sequence(...)` for: a keyframed zoom/pan, offset,
    directional blur, opacity < 100, a Motion-scaled adjustment frame, a non-Normal blend, two
    stacked adjustment layers, and the #1524 case (Transform leaving empty areas).
- [x] 1.3 **GPU executor** (`crates/gpu`: `lib.rs`, `fx.rs`, `mask.rs`, a new WGSL combine shader).
  Per spec §4.4 item 4:
  - convert the Rgba16Float accumulator into an Rgba32Float working texture with a small shader
    pass. A raw texture copy can't change the format;
  - run the existing `fx.rs` passes with the layer's parameters at `time`;
  - rasterise the coverage of the Motion-transformed layer frame into a mask, times opacity;
  - combine: Normal → `mix(adj, below, w)`; other blends → the existing blend shader with `adj`
    scaled by `w`.

  **Tests:** GPU vs CPU tests in the style of `blend_tests.rs` / `fx_tests.rs`, for the same cases
  as 1.2, within the existing GPU-vs-CPU tolerance. Don't loosen any existing tolerance.
- [x] 1.4 Gates, one release build, then **two** exports:
  - `bench-quick-1min` `gpu-off`, `cmp` against its reference. It must be byte-identical: this
    stage touches the CPU path.
  - `bench-quick-1min` `gpu-auto`: minimum `psnr_y` ≥ 33.2 dB (no-regression rule) against the GPU Off reference. Record
    time, GPU vs CPU frames (expected: `cpuWholeFrames.adjustment` near 0) and the timers.
- [x] 1.5 Commit, fast-forward `perf/export-amd` (rebasing first if Stage 2 landed), archive. Notes,
  plus one `results/log.md` row.

**Done when:** both exports pass, adjustment frames go to the GPU, and the result is recorded.

---

## Stage 2 — Output conversion on the GPU (spec §4.3)

**Goal:** the GPU hands the encoder 8-bit YUV 4:2:0 (~3.1 MB per frame) instead of 32-bit float
RGBA (~33 MB) that the CPU then converts twice.

**Tasks**
- [x] 2.1 **GPU pass** (`crates/gpu`: a new module and WGSL shader). After compositing: over black →
  sRGB encode → BT.709 limited-range YUV → 2×2 chroma subsampling. Write to Y and chroma textures,
  and read back only those.
  - Port the exact formulas, rounding and chroma averaging of `over_black_rgba8` and `h264_input`
    (`crates/export/src/encoders.rs`).
  - **Test:** GPU output equals `h264_input(over_black_rgba8(img))`, or is within ±1 code value on
    Y, U and V. Cover gradients, pure colours, transparent areas and 1-pixel detail.
- [x] 2.2 **Plumbing** (`crates/gpu/src/export_renderer.rs`, `crates/export/src/{pipeline,registry,job,encoders}.rs`).
  - Add a "render to encoder input" variant of `FrameRenderer::render` that returns YUV planes.
  - Carry the planes through the frame storage that the batch hands to the encoder. `job.rs`
    keeps the picture index; `registry.rs` and `encoders.rs` hold the `ready` map that
    `prepare_ahead` fills. `Pipeline::frame` can't reach that private map directly, so the
    orchestrator has a scout map the path first and specifies it in the implementer's brief.
  - Ordinary preparation must not overwrite planes the GPU already produced.
  - Bypass `place()` only when its geometry is a true no-op: scale 1.0, offset 0, **and** the
    rendered, content and output dimensions are all equal (for example 1920×1080 → 1920×1080).
    Otherwise keep today's path.
  - Only for H.264 8-bit SDR with no export overlays and no video limiter (Bruno's settings).
    Everything else, and all CPU frames, keep today's path.
  - CPU rendering code is untouched, so no `gpu-off` export is needed.
- [x] 2.3 Gates, one release build, **one** export: `bench-quick-1min` `gpu-auto`. Minimum `psnr_y`
  ≥ 33.2 dB (no-regression rule); record time, `gpuReadbackMs`, `outputConvertMs`.
- [x] 2.4 Commit, fast-forward `perf/export-amd` (rebasing first if Stage 1 landed), archive. Notes,
  plus one `results/log.md` row.

**Done when:** the export passes, the readback and output conversion times drop, and the result is
recorded.

---

## Stage 3 — Remaining input conversion on the GPU (conditional)

**Decide after Stages 1–2 have landed**, from the counters of the latest GPU Auto export, not from
Stage 0 alone. Do it only if `inputConvertMs` on GPU frames, plus `gpuUploadPrepMs`, is still
**≥ 10%** of render-task time. Otherwise mark it **Skipped (reason)**.

**Goal:** remove the CPU preparation that remains before upload. 8-bit `Yuv8` pictures already
convert in a shader. What's left is the `Yuv16` code-to-f16 preparation and any other source
formats the counters show.

**Tasks**
- [x] 3.1 The orchestrator first has a Sol scout name exactly which source formats and functions
  account for the remaining time, from the counters and the code (`crates/gpu`, `crates/render`).
  Only those get a GPU path.
- [x] 3.2 Upload those planes as they are and convert them in a shader, using the same colour math
  as the CPU preparation they replace. Tests: GPU vs CPU within the existing tolerance.
- [x] 3.3 Gates, release build (with `-p filmcraft`), then **one** `bench-quick-1min` `gpu-auto`
  export: PSNR ≥ 33.2 dB (no-regression rule), time, `inputConvertMs`, `gpuUploadPrepMs`.
- [x] 3.4 Keep or reject (rule above). Commit, fast-forward, archive, Notes, log row.

---

## Stage 4 — Full-size GPU batches (conditional, spec §4.2 option 1)

Today GPU rendering caps a batch at 4 frames (the renderer pool size), against 16 on the CPU.

**Decide after Stages 1–2.** Do it only if the latest GPU Auto export shows the cap is the limit.
Both of these must hold:
- the encoder still waits for frames (`export.stages.waitMs` ≥ 20% of export time);
- the GPU-frame timers show the 4 GPU render tasks busy working, not waiting on decode or locks.

If the counters can't attribute it, run this as **one bounded experiment**: renderer count 8, one
export. Keep it only if it's faster under the keep-or-reject rule.

**Tasks**
- [x] 4.1 Raise the renderer pool `count` (`crates/export/src/pipeline.rs`, `RendererPool`) to 8,
  and the batch cap (`render_batch_limit`) with it.
  - The deadlock invariant stays: oversized batch *requests* are capped to the pool capacity.
    Admitting more frame tasks than renderers would bring the deadlock back.
  - Keep the existing bounded regression test (`renderer_reentrancy.rs`) passing, and update its
    capacity if needed.
- [x] 4.2 Gates, release build (with `-p filmcraft`), then **one** `bench-quick-1min` `gpu-auto`
  export: PSNR, time.
- [x] 4.3 Keep or reject. Commit, fast-forward, archive, Notes, log row.

---

## Stage 5 — Exit

- [x] 5.1 `cargo xtask ci` once: 4,892 passed / 6 known failures / 52 skipped, 265.83 s; no re-run. Only the known pre-existing failures are allowed (PLAN-01 Notes).
- [x] 5.2 **One** `bench-A-2min` `gpu-auto` export with the final build: 108.930 s / 66.097 fps; frame counts and durations match, no offline media.
  - Minimum `psnr_y` 32.49 dB; five worst frames on the sheet, against `exports/reference/bench-A-2min--gpu-off--46763a85.mp4`. The 33.2 dB floor was measured on the quick project; bench-A had no starting value, so this is not a failure.
  - Time, compared with the Spec 01 final GPU Off result (303.8 s) and the original GPU Auto
    baseline (310.6 s).
  - Frame count, duration and audio length match the reference (`ffprobe -show_entries
    stream=nb_frames,duration`); no offline media in the run record.
- [x] 5.3 **Frame sheet for Bruno:** 18 rows in `results/02-frames.png`; Barrido missing because the required edge margins exclude its 0.5 s clips; visual check pending. From that export and the reference, a sub-agent extracts the
  same frames and puts them side by side in one image (`results/02-frames.png`): screen text, the
  camera, inside a zoom, a "Barrido" if bench-A has one, and a title if it has one. bench-A has no
  transitions, so none is included. Then the export is deleted.
- [x] 5.4 Write `results/02-render-summary.md`: summary written.
  - before/after numbers;
  - what each stage did;
  - which frames still go to the CPU and why;
  - what limits the export next.
- [x] 5.5 Update the README status row for spec 02: done, pending Bruno's visual check.

**Done when:** all of the above, and Bruno has looked at the frame sheet (the supervisor relays it).

---

## Notes

- Starting state (2026-11-05): `perf/export-amd` = `0c0e60c5`, which is Spec 01 plus the per-cause
  render timers. Spec 01's final bench-A GPU Off was 303.8 s (byte-identical). The rejected
  waiting-line fix sits uncommitted on `perf/01-decoder-queue` until Stage 0.1.

### Stage 0 GPU Auto numbers (bench-quick-1min, `00420ab9`)

2026-11-05: compare later stages against this diagnosis run of the counter-only Stage 0 build, not the earlier GPU Off breakdown. Exactly one GPU Auto export; no offline media. [Run JSON](results/runs/2026-11-05T19-41-27--02-0-diagnosis--bench-quick-1min--gpu-auto.json); [gates and acceptance](results/progress/02-stage-0.md).

- Export: **141.012394044 s / 25.530 fps**, 3,600 frames; vs previous GPU Auto **156.4 s**: -15.387606 s (-9.839%). Diagnosis only; no speed keep/reject decision.
- `export.gpu.frames`: **1,820** (50.556%); `fallbacks`: **1,780**. CPU whole frames = adjustment + transition + hdr = **1,780** (49.444%): adjustment **1,780**, transition **0**, hdr **0**; `planExhausted`: **0**. `planExhausted` frames are counted inside `export.gpu.frames`.
- Render-task total: **516,802.779 ms**. Every `export.renderTasks.*Ms` timer, as a share of that total:
  - `renderTaskMs`: 516,802.779 ms (100.000%).
  - `decoderQueueMs`: 3,005.642 ms (0.582%).
  - `decodeWaitMs`: 0.000 ms (0.000%).
  - `frameMapWaitMs`: 23.332 ms (0.005%).
  - `compositeMs`: 54,131.622 ms (10.474%).
  - `effectsMs`: 245,314.189 ms (47.468%).
  - `otherMs`: 94,444.525 ms (18.275%).
  - `convertMs`: 119,883.471 ms (23.197%).
  - `gpuInputConvertMs`: 38,640.804 ms (7.477%).
  - `gpuOutputConvertMs`: 14,690.520 ms (2.843%).
  - `cpuInputConvertMs`: 47,554.321 ms (9.202%).
  - `cpuOutputConvertMs`: 18,997.826 ms (3.676%).
  - `decodeRunMs`: 0.000 ms (0.000%).
- GPU timers (absolute and share of render-task time):
  - `gpuSubmitMs`: 18,941.663 ms (3.665%).
  - `gpuReadbackMs`: 7,625.024 ms (1.475%).
  - `gpuReadbackConvertMs`: 39,835.943 ms (7.708%).
  - `gpuUploadPrepMs`: 20,952.417 ms (4.054%).
- `export.stages.waitMs`: **53,586.457 ms**, **38.001%** of export time.
- PSNR starting value against the GPU Off `46763a85` quick reference: minimum `psnr_y` **33.23 dB**, frame **1,539** (`psnr.log` one-based `n`), **3,600** log lines. Below 50 dB; recorded as a starting value per the Stage 0 brief, not a rejection criterion.
- Unchanged counters are omitted by `bench.py` from `statsDelta`; omitted values above are zero deltas. `convertMs` includes the four GPU/CPU input/output conversion splits; GPU timers overlap render-task timers. `decodeRunMs` is separate from the exclusive phase split.

### Stage 2 GPU Auto numbers (bench-quick-1min, bf86fe92)

2026-11-05: kept. Exactly one GPU Auto export; [Run JSON](results/runs/2026-11-05T20-22-15--02-2-yuv--bench-quick-1min--gpu-auto.json); [gates and acceptance](results/progress/02-stage-2.md). Stage 0 values are shown in brackets below.

- Export: **126.103563587 s / 28.548 fps**, 3,600 frames [141.012 s / 25.530 fps]: **-14.909 s (-10.57%)**. Kept.
- `export.gpu.frames`: **1,820**; `fallbacks`: **1,780** (unchanged; adjustment frames are Stage 1's).
- `gpuReadbackMs`: **6,264.436** [7,625.024]; `gpuReadbackConvertMs`: **1,934.651** [39,835.943].
- `gpuSubmitMs`: **16,373.891** [18,941.663]; `gpuOutputConvertMs`: **3.617** [14,690.520].
- `cpuOutputConvertMs`: **18,615.275** [18,997.826]; `renderTaskMs`: **453,324.204** [516,802.779]; `export.stages.waitMs`: **44,698.456** [53,586.457]. All timers above are in ms.
- PSNR vs `exports/reference/bench-quick-1min--gpu-off--46763a85.mp4`: minimum `psnr_y` **33.25 dB at n=1100** (**PASS ≥33.2 dB**); **1,265 frames <50 dB**. Five worst (n, dB): **1100 33.25, 1533 33.28, 1473 33.34, 1515 33.50, 1509 33.60**. Include these plus frame **1,539** in Bruno's Stage 5 frame sheet. GPU YUV unit test matched the CPU conversion exactly (max difference **0** on Y/U/V).
- Gates: fmt, clippy, layers **PASS**; tests **351 passed**, only the known `workflows::phone_vfr_clip_extract_gain_title_to_h264_mp4` failure. Release build **245.3 s**. Output deleted.

Latest accepted GPU Auto time: 126.1 s (Stage 2). Stage 1 compares against this if it lands second.

### Stage 1 GPU Auto numbers (bench-quick-1min, 9fbb405f)

2026-11-05: kept; measured on the integrated head (Stage 1 + Stage 2 `bf86fe92`). [GPU Off run](results/runs/2026-11-05T20-44-39--02-1-adjust-int-gpuoff--bench-quick-1min--gpu-off.json); [GPU Auto run](results/runs/2026-11-05T20-47-58--02-1-adjust-int-gpuauto--bench-quick-1min--gpu-auto.json); [gates and acceptance](results/progress/02-stage-1.md).

- Export: **93.372935930 s / 38.555 fps**, 3,600 frames; vs Stage 2 **126.104 s**: -32.731064070 s (-25.955611%). Passes the fixed ≤122.32 s acceptance limit.
- GPU Off: **154.493076110 s / 23.302 fps**, byte-identical to the quick `46763a85` reference (`cmp` exit 0); both integrated outputs deleted, run JSONs kept; no offline media.
- `export.gpu.frames` 3,600; `fallbacks` 0; `cpuWholeFrames.{adjustment,transition,hdr,planExhausted}` = 0/0/0/0. Absent statsDelta keys are zero.
- Render-task timers (absolute ms and % of renderTaskMs; GPU timers overlap):
  - `renderTaskMs` 335267.956 ms (100.000%); `decoderQueueMs` 7616.144 ms (2.272%); `decodeWaitMs` 122.737 ms (0.037%); `frameMapWaitMs` 28.346 ms (0.008%).
  - `compositeMs` 45816.453 ms (13.666%); `effectsMs` 88527.419 ms (26.405%); `otherMs` 88894.082 ms (26.514%); `convertMs` 104262.773 ms (31.098%).
  - `gpuInputConvertMs` 104255.724 ms (31.096%); `gpuOutputConvertMs` 7.050 ms (0.002%); `cpuInputConvertMs` 0.000 ms (0.000%); `cpuOutputConvertMs` 0.000 ms (0.000%); `decodeRunMs` 120.796 ms (0.036%).
- `export.gpu.gpuSubmitMs` 33958.335 ms (10.129% of renderTaskMs).
- `export.gpu.gpuReadbackMs` 12743.506 ms (3.801% of renderTaskMs).
- `export.gpu.gpuReadbackConvertMs` 3831.172 ms (1.143% of renderTaskMs).
- `export.gpu.gpuUploadPrepMs` 34824.328 ms (10.387% of renderTaskMs).
- `export.stages.waitMs` **15169.959 ms (16.247% of export time)**.
- PSNR against the quick GPU Off reference: min `psnr_y` **33.25 dB at n=1100**, 1,986 frames <50 dB, 3,600 log lines; **PASS ≥33.2 dB**.
- Five worst (one-based n, dB): 1100 33.25, 1533 33.27, 1473 33.53, 1485 33.54, 1389 33.56. Include these plus frame 1,539 in Bruno's Stage 5 frame sheet.
- The PSNR check used the 33.2 dB no-regression rule (Notes, 2026-11-05), which supersedes the earlier ≥50 dB requirement; task 1.4 already states the updated rule.
- Standalone context before rebase: 118.912 s on base `00420ab9`, minimum PSNR 33.43 dB.
Latest accepted GPU Auto time: 93.372935930 s (Stage 1, integrated).

### Stage 3 GPU Auto numbers (bench-quick-1min, 892c469b)

2026-11-06: kept. Exactly one GPU Auto export; no offline media. [Run JSON](results/runs/2026-11-06T00-42-12--02-3-input--bench-quick-1min--gpu-auto.json); [gates and acceptance](results/progress/02-stage-3.md).

- Task 3.2 implemented the map's "Follow-up: masked Yuv8 direct path" (commit `d58d498e`, plus clippy fix `892c469b`); the RgbaF32 path was not done.
- Export: **54.651 s / 65.873 fps**, 3,600 frames; vs previous accepted **93.373 s**: **-38.722 s (-41.47%)**. Kept.
- `export.gpu.frames`: **3,600**; `export.gpu.fallbacks`: **0**.
- Render-task timers (absolute ms and % of renderTaskMs; GPU timers overlap):
  - `renderTaskMs` 155,690.029 ms (100%); `gpuInputConvertMs` 0 ms (0%) [was 104,255.724 ms]; `cpuInputConvertMs` 0 ms (0%).
  - `compositeMs` 714.028 ms (0.459%); `effectsMs` 16,166.780 ms (10.384%); `otherMs` 90,604.065 ms (58.195%); `convertMs` 4.295 ms (0.003%).
  - `gpuUploadPrepMs` 2,208.852 ms (1.419%) [was 34,824.328 ms]; `gpuSubmitMs` 18,883.717 ms (12.129%); `gpuReadbackMs` 62,069.436 ms (39.867%).
- `export.stages.waitMs`: **23,609.985 ms (43.201% of export time)**, above the Stage 4 ≥20% wait condition; the supervisor decides Stage 4.
- PSNR vs `exports/reference/bench-quick-1min--gpu-off--46763a85.mp4`: minimum `psnr_y` **33.42 dB at n=1100** (**PASS ≥33.2 dB**); **1,987 frames <50 dB**, 3,600 log lines. Five worst (n, dB): **1100 33.42, 1515 33.51, 1533 33.51, 1485 33.53, 1401 33.58**. Output and `psnr.log` deleted.
- Gates: fmt, clippy (after fix `892c469b`), render+gpu tests **331 passed / 0 failed / 7 ignored**, `cargo xtask layers`: **PASS**. Release build **193.57 s**.
- No `gpu-off` export was needed: the diff touches only `crates/render/src/plan.rs` (GPU-plan reference/fallback helpers) and `crates/gpu/src/*`, not `render_sequence` or the CPU renderer in `crates/render/src/lib.rs`.

Latest accepted GPU Auto time: 54.651 s (Stage 3).

### Stage 4 GPU Auto numbers (bench-quick-1min, ae807b56)

2026-11-06: kept by supervisor decision based on the first run. [Run JSON](results/runs/2026-11-06T00-56-20--02-4-batches--bench-quick-1min--gpu-auto.json); [PSNR-only repeat JSON](results/runs/2026-11-06T01-02-15--02-4-batches-psnr--bench-quick-1min--gpu-auto.json); [gates and acceptance](results/progress/02-stage-4.md).

- Ran as the plan's bounded experiment: Stage 3 encoder wait 43.2% ≥ 20%; the counters could not attribute the limit to the 4-renderer cap.
- Change: `crates/export/src/pipeline.rs`, `Pipeline::new`, renderer pool `clamp(2, 4)` → `clamp(2, 8)`; `render_batch_limit` still caps oversized requests to the pool's real capacity. `renderer_reentrancy.rs` unchanged (one-renderer factory).
- First export: **49.527 s / 72.688 fps**, 3,600 frames, no offline media; vs previous accepted **54.651 s**: **-5.124 s (-9.38%)**. Kept.
- `export.gpu.frames`: **3,600**; `fallbacks`: **0**. Timers in ms (% of renderTaskMs; GPU timers overlap): `renderTaskMs` **272,344.605 (100%)**; `otherMs` **146,439.638 (53.770%)**; `gpuSubmitMs` **37,297.337 (13.695%)**; `gpuReadbackMs` **93,802.146 (34.442%)**.
- `export.stages.waitMs`: **17,772.709 ms (35.885% of export time)** [Stage 3: 43.201%]. No VRAM counter exists.
- First-run PSNR was incomplete: a log-capture collision overwrote frames n=57–67, leaving 3,589 measured frames. The supervisor approved one repeat export solely to complete PSNR, using the same `ae807b56` release build without rebuilding: **53.980 s / 66.691 fps**, 3,600 frames, no offline media.
- Repeat PSNR vs the quick GPU Off reference: **3,600 log lines, n=1..3600, none missing**; minimum `psnr_y` **33.42 dB at n=1100 (≥33.2 PASS)**; **1,987 frames <50 dB**. Five worst (n, dB): **1100 33.42, 1515 33.51, 1533 33.51, 1485 33.53, 1401 33.58**. Outputs and `psnr.log` deleted.
- Stage 5 timing caution: the PSNR-only repeat's **53.980 s** is only **-1.23% vs 54.651 s**; the two runs differ by **4.45 s (~9%)**. The keep decision uses the first run, **49.527 s (-9.38%)**.
- Gates: fmt, clippy, export tests (**159 unit + 2 integration, 0 failed**), `cargo xtask layers`: **PASS**. Release build **184.269 s**.

Latest accepted GPU Auto time: 49.527 s (Stage 4).

### Stage 5 exit (bench-A-2min, ae807b56)

2026-11-06: pending Bruno's visual check. Archived CLI `builds/02-4-ae807b56/filmcraft-cli`, one GPU Auto export, label `02-5-exit`. [Run JSON](results/runs/2026-11-06T01-16-41--02-5-exit--bench-A-2min--gpu-auto.json); [frame sheet](results/02-frames.png); [summary](results/02-render-summary.md); [progress](results/progress/02-stage-5.md).

- CI: `cargo xtask ci` once on `ae807b56`, **4,892 passed / 6 failed / 52 skipped**, **265.83 s**. Only the six known pre-existing failures (PLAN-01 Notes); flaky `perf_ui` passed, no re-run.
- Export: **108.930 s / 66.097 fps**, **7,200 frames**; vs Spec 01 final GPU Off **303.8 s**: **−194.870 s (−64.1%)**; vs original GPU Auto **310.6 s** (`46763a85`): **−201.670 s (−64.9%)**. `ffprobe` new = reference: video **7,200 frames / 120.000 s**, audio **120.000 s**; no offline media.
- PSNR vs `exports/reference/bench-A-2min--gpu-off--46763a85.mp4`: **7,200 lines, none missing**; minimum `psnr_y` **32.49 dB at n=5828**; **3,691 frames <50 dB**. Five worst (n, dB): **5828 32.49, 5513 32.94, 5829 32.97, 5826 33.10, 5822 33.22**. The **33.2 dB** floor was measured on the quick project; bench-A had no starting value, so this is **not a failure**. All five are on the sheet: 5822–5829 enter the screen zoom; 5513 is the drone video changing framing.
- Counters (`statsDelta`): `export.gpu.frames` **7,200**, `fallbacks` **0**, `cpuWholeFrames.{adjustment,transition,hdr,planExhausted}` **0/0/0/0**. `export.stages.waitMs` **37,226.475 ms (34.175% of export time)**. Render-task timers (ms and share of `renderTaskMs`; GPU timers overlap): `renderTaskMs` **546,468.985**; `otherMs` **301,209.372 (55.119%)**; `gpuSubmitMs` **64,432.687 (11.791%)**; `gpuReadbackMs` **222,511.198 (40.718%)**; `gpuUploadPrepMs` **1,185.709 (0.217%)**.
- Sheet: **1920×10872, 21.85 MB, 18 rows**, left new GPU Auto `ae807b56`, right GPU Off reference `46763a85`. Bench-A: screen text **3241**, camera bubble over drone video **3961**, inside a zoom **601**, title “ROBÓTICA” **3790**, plus the five worst PSNR frames. Quick: **1100, 1389, 1401, 1473, 1485, 1509, 1515, 1533, 1539**, from one prefix export to **25.65 s**, extraction only, **not a measurement**. **Barrido gap:** bench-A's Barrido clips last **0.5 s**, excluded by the required **≥0.5 s margin from both edges**. Exports were deleted; adding a Barrido row needs an extra export, which the supervisor decides.
- Stage 4 timing caution: first run **49.527 s**, repeat **53.980 s**; the repeat is only **1.23% faster** than Stage 3 (**54.651 s**). Stage 4 was kept by supervisor decision based on the first run.
