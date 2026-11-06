# Spec 02 — Rendering

**Stage:** turning the decoded pictures into the output frame. That covers compositing screen,
camera, inserts and titles, applying effects, adjustment layers and transitions, and converting
the result into the encoder's input.
**Starts after:** spec 01's exit gate. The quick items (§4.1–4.2) may be measured earlier, but their
real effect only shows once decoding is fast.
**Read first:** [README](README.md).

---

## 1. Summary

With GPU rendering on, FilmCraft's export barely uses the GPU compositor for this project:

1. **48.7% of frames are rendered entirely on the CPU.** An adjustment layer anywhere in a frame
   makes the planner hand the whole frame to the CPU renderer. V2's zoom and pan layers cover
   30,062 frames and V4's "Barrido" layers 594. The effects they use (`transform`, `offset`,
   `directional_blur`) already have GPU implementations, but only as clip effects.
2. **GPU rendering caps a batch at 4 frames** (CPU rendering uses 16), to avoid a deadlock between
   the 4 pooled renderers and nested rayon work.
3. **The GPU result is read back as 32-bit floats** (~33 MB per 1080p frame). The CPU then turns it
   into 8-bit RGBA and then into YUV 4:2:0 for the encoder.
4. **Three 24-thread pools share 24 hardware threads**: the global rayon pool (render), the
   `decode-N` pool and the H.264 encoder's pool.

The goal of this spec is to keep as much of each frame on the GPU as possible, cut what crosses
between GPU and CPU, and stop the stages from starving each other of threads.

---

## 2. How it works today

### 2.1 Per-frame path (`crates/export/src/pipeline.rs:247` `Pipeline::frame`)

```
is_cpu_frame(project, seq, t)?          render/src/plan.rs:264 — HDR, an adjustment layer, or a
                                        non-simple transition anywhere at t → whole frame on CPU
 ├─ GPU (pool of 4 renderers, pipeline.rs:66 RendererPool, :213 count = clamp(threads, 2, 4))
 │    gpu/src/export_renderer.rs:148 ExportRenderer::render
 │      plan_frame → prepare (layer conversion to f16) → compositor.render_export_prepared
 │      gpu/src/lib.rs:986 → map_accumulator (:1094): Rgba16Float accumulator read back as Vec<f32>
 └─ CPU  render::render_sequence (premultiplied linear f32 RGBA, rayon-parallel inside)
place()            pipeline.rs:305 — no-op at 1080p→1080p (verified: scale 1.0, offset 0)
overlays()         none in Bruno's settings
over_black_rgba8() f32 → straight sRGB RGBA8 (CPU)
prepare_ahead      encoders.rs:471 — RGBA8 → YUV 4:2:0 BT.709 on the render thread (h264_input, :376)
```

### 2.2 Batching (`crates/export/src/job.rs`)

- Requested batch = `rayon::current_num_threads().clamp(2, 16)` = 16 on this machine (`:294`).
- Lowered by `Pipeline::render_batch_limit` to the renderer pool size (4) when GPU rendering is
  on (`pipeline.rs:178`). The deadlock reasoning is in the doc comment of `RendererPool::with`.
- Lowered again by the overlap memory budget: `2 × batch × frame_bytes ≤ 512 MiB`, so 32 frames
  at 1080p, which doesn't bind.
- Batch *k+1* renders on the pool while the calling thread encodes batch *k*. The two batches then
  meet at a barrier (`step_batch`, `:549`).

### 2.3 Adjustment layers on the CPU (`crates/render/src/lib.rs:341` `adjustment_layer`)

For an adjustment layer item over `below` (the composite of the tracks under it):
1. `adj = below.clone()`, then each non-intrinsic effect is applied with `mask::apply_effect`
   (effect masks honoured);
2. `weight = opacity × opacity masks × coverage of the layer's Motion-transformed frame`
   (`adjustment_region`);
3. with Normal blend, `adj` **replaces** `below` by `weight` (`mask::mix`), so a Transform that
   leaves areas empty shows nothing of the untransformed picture (#1524); with other blends, `adj`
   is scaled by `weight` and blended.

Transitions that involve an adjustment layer use it as "the frame below with effects"
(`lib.rs:246–250`).

### 2.4 GPU compositor capabilities (`crates/gpu`)

- Layers are drawn as transformed quads into an Rgba16Float accumulator; 27 blend modes.
- Per-clip effect chains run in an Rgba32Float working image as WGSL compute passes
  (`gpu/src/fx.rs`). The supported list, `render/src/gpufx.rs:225` `GPU_EFFECTS`, includes
  **`transform`, `offset` and `directional_blur`**: everything this project's adjustment layers use.
- `FramePlan` (`render/src/plan.rs:83`) has `Layers`, `Composite { steps }` and `Image`.
  `PlanStep` (`:93`) has `Layers` and `Transition`. **There is no step for "apply effects to
  the canvas so far".**
- The CPU plan executor (`plan::execute_cpu`) is the oracle the GPU tests compare against.

### 2.5 Evidence

| | Test 1 (GPU Off) | Test 2 (GPU Auto) |
|---|---|---|
| Throughput | 21.4 fps | 22.6 fps |
| `convertMs` per frame | 9.7 ms | 2.6 ms |
| Encoder waiting for frames | 61% | 74% |

- Enabling the GPU compositor changed throughput by ~5%, within noise. Decoding was the limit in
  both, so this mostly confirms that rendering improvements must wait for spec 01.
- `perf.stats` doesn't yet show how many frames the GPU actually rendered: `gpu_render_stats()`
  and `export_lock_wait()` exist in `crates/export/src/stats.rs` but aren't exposed.
- Average CPU load was 13–15 busy cores of 24 during export.

---

## 3. Goals and non-goals

**Goals**
1. Render frames with adjustment layers on the GPU (the 48.7%).
2. Let GPU rendering use full-size batches.
3. Read back from the GPU only what the encoder needs: 8-bit YUV 4:2:0 (~3.1 MB) instead of f32
   RGBA (~33 MB).
4. Size thread pools so decoding, rendering and encoding don't oversubscribe the CPU.
5. Keep CPU and GPU pictures matching within the existing tolerance (§6).

**Non-goals**
- Changing what any effect looks like.
- GPU versions of effects this project doesn't use. Add only what Bruno's projects need; check his
  other projects before adding more.
- Complex transitions beyond what the project uses (it has 2 cross dissolves, already GPU-capable).
- HDR, wide gamut and alpha-channel exports.
- Playback rendering (the UI compositor), except that shared code must keep working.

---

## 4. Work items

### 4.0 Instrumentation (prerequisite)

- Expose in `perf.stats` (under `export.gpu`):
  - `frames` and `fallbacks` (`gpu_render_stats()`);
  - `lockWaitMs` (`export_lock_wait()`);
  - and new counters:

| Counter | Meaning |
|---|---|
| `cpuWholeFrames.{adjustment, transition, hdr, planExhausted}` | why `is_cpu_frame` or the planner sent a frame to the CPU |
| `gpuRenderMs`, `gpuReadbackMs` | time inside `render_export_prepared` and in `map_accumulator` |
| `cpuRenderMs` | time in `render_sequence` |
| `toRgba8Ms` | time in `over_black_rgba8` |
| `convertMs` | existing; RGBA8 → YUV |

- Log the result of one Range A export (GPU Auto), run under `perf record -g`, in
  `results/02-render-profile.md`: the new counters and the top functions on the render threads.
  One export gives both.

### 4.1 Thread-pool sizing (quick)

**Problem.** Three pools each default to 24 threads:
- the global rayon pool, which renders frames and runs nested `par_*` work inside the compositor
  and conversions;
- `decode-N` (`codecs/src/gop.rs:536`);
- the H.264 encoder's own pool (`h264enc/src/lib.rs:565`, `threads = 0` → all cores).

They contend once decoding stops being the limit.

**Design**
- Make each pool's size configurable by environment variable for experiments:
  `FILMCRAFT_RENDER_THREADS`, `FILMCRAFT_DECODE_THREADS`, `FILMCRAFT_ENCODE_THREADS`.
  These are local-fork knobs, read once at start-up, ignored if invalid.
- Measure at most three combinations on Range A, one run each, chosen from the §4.0 counters (for
  example render 16 / decode 4 / encode 8 against the recorded 24/24/24 time). Pick defaults for
  this machine and hard-code them in the fork as "auto on 24 threads".

**Expected effect:** small to moderate, and only after spec 01. Cheap, and needed to read the other
items' measurements correctly.

### 4.2 Full-size batches with GPU rendering (quick)

**Problem.** `render_batch_limit` caps the batch at the renderer pool capacity (4). The reason: a
frame task holding a renderer may run nested rayon work, which can steal another frame task. That
task then waits on the pool's condition variable forever if all renderers are held by suspended
outer frames.

**Options** (measure 1 first; implement 2 only if 1 isn't enough):
1. **More renderers.** Raise `count` (`pipeline.rs:213`) to 8 or 16 and the cap with it. The
   original comment measured no gain past 4 on a 16-thread machine "where more only contend for
   memory bandwidth in the read-back". After §4.3 the readback is ~10× smaller, so re-measure.
   Watch VRAM (12 GB) and log peak usage.
2. **A dedicated GPU-frame pool.** Run GPU frame tasks on a separate rayon pool sized to the
   renderer count, so global-pool work can never steal a GPU frame task; whole-frame-CPU frames
   stay on the global pool. The batch can then stay at 16 regardless of renderer count. Keep the
   deadlock invariant documented next to `RendererPool::with` and add a regression test that
   deadlocked before (a batch larger than capacity with nested work).

### 4.3 GPU output conversion: read back YUV 4:2:0, not f32 RGBA (medium)

**Problem.** `map_accumulator` reads back the Rgba16Float accumulator as `Vec<f32>` (~33 MB per
frame). Then the CPU, per frame:
- converts f32 → straight sRGB RGBA8 (`over_black_rgba8`);
- converts RGBA8 → planar YUV 4:2:0 BT.709 limited range (`h264_input`, `encoders.rs:376`);
- and the frame's f32 buffer is recycled.

**Design**
- Add a GPU pass after compositing: over black → sRGB encode → BT.709 limited-range YUV →
  2×2 chroma subsampling, written to an R8 luma texture and an R8/RG8 chroma texture (or one
  packed buffer). Read back ~3.1 MB.
- It must reproduce the CPU path's math: `over_black_rgba8` quantisation then `h264_input` from
  those RGBA8 values, including rounding and the chroma siting/averaging used by `h264_input`.
  Read both functions and port their exact formulas. If exact equality isn't achievable on the GPU,
  the tolerance is ±1 code value on Y, U and V (§6).
- Plumbing: `FrameRenderer::render` gains an optional "render to encoder input" variant that
  returns YUV planes. `Pipeline::frame` then hands them to the encoder through the existing
  `ready` map that `prepare_ahead` fills (`encoders.rs:471`), skipping `over_black_rgba8` and
  `h264_input` for that frame. Frames that fall back to the CPU keep today's path.
- Only for the H.264 8-bit SDR export, and only when no export overlays or video limiter are
  enabled (Bruno's settings). Otherwise use today's path.
- This also prepares spec 04 (encoding): a VA-API encoder wants NV12, which is these planes with interleaved
  chroma.

### 4.4 Adjustment layers on the GPU (big; the main item of this spec)

**Problem.** `layered_at` (`plan.rs:278`) returns false whenever an adjustment layer is visible
at `t`. The planner then hands back `FramePlan::Image` from the CPU renderer, for 48.7% of this
project's frames.

**Design**
1. **Plan representation.** Add a step:
   ```rust
   PlanStep::Adjust {
       effects: Vec<EffectInstance>, // the layer's enabled non-intrinsic effects, GPU-capable
       time: Tick,                   // effect time (item.effect_time_at(t))
       weight: AdjustWeight,         // opacity, plus the Motion-transformed coverage region
       blend: Blend,
   }
   ```
   It means: take the canvas composited so far (the steps before it), apply `effects`, and combine
   with the canvas by `weight` and `blend`, exactly as `render::adjustment_layer` does.
2. **Planner.** In `plan_frame`, when walking tracks bottom-up, push `PlanStep::Adjust` for an
   adjustment layer instead of bailing out. Only do so if every effect is in `GPU_EFFECTS`, none
   has a mask, the layer has no opacity masks, and the effect parameters don't need a CPU-only
   variant. For example, `transform` with `shutter_override` on and angle > 0.5 runs
   `distort::transform_fx` on the CPU (`render/src/vfx.rs:489`): keep that on the CPU at first.
   Otherwise keep the whole-frame CPU fallback. Transitions into or out of an adjustment layer
   stay on the CPU (rare in this project).
3. **CPU oracle.** Implement `PlanStep::Adjust` in `plan::execute_cpu` by calling the same
   functions as `adjustment_layer` (`mask::apply_effect`, `adjustment_region`, `mask::mix` /
   `scale_by` + blend). Then for any timeline,
   `execute_cpu(plan_frame(...)) == render_sequence(...)` within the existing tolerance.
4. **GPU executor** (`crates/gpu`):
   - copy the accumulator (Rgba16Float) into an Rgba32Float working texture;
   - run the existing effect compute passes (`fx.rs`) on it with the layer's effect parameters
     at `time`;
   - compute the weight: a uniform opacity, times the coverage of the Motion-transformed layer
     frame. Coverage is a quad rasterised into an R16F/R32F mask, reusing the `mask` module's
     machinery where possible;
   - combine into the accumulator: Normal → `mix(adj, below, w)`, which replaces rather than
     draws over (#1524); other blends → the existing blend shader with `adj` scaled by `w`;
   - continue with the following steps.
5. **Effect scope for this project:** `transform` (keyframed position/scale with Bezier
   interpolation, evaluated on the CPU at `time` and passed as uniforms, as clip effects already
   are), `offset`, `directional_blur`, plus the layer's intrinsic `motion` and `opacity`.

**Tests**
- New `crates/render/src/adjustment_tests.rs` cases for `execute_cpu(plan) == render_sequence`:
  zoom/pan transform with keyframes, offset, directional blur, opacity < 100, a Motion-scaled
  adjustment frame, non-Normal blend, two stacked adjustment layers, adjustment over a transition.
- GPU vs CPU tests in `crates/gpu` (same style as `blend_tests.rs` / `fx_tests.rs`) for the same
  cases.
- On the real project: render 50 sample frames inside adjustment-layer ranges with GPU and with
  CPU (`filmcraft-cli render --seconds S --out f.png` with GPU on/off) and compare.

### 4.5 CPU path costs (only if still significant after 4.4)

For frames that stay on the CPU, and for the CPU-only variant, profile and fix the top items. Likely
candidates:
- YUV → linear f32 conversion of each full 1080p source layer;
- the camera's 1920×1080 → 51.5% resample;
- the `adj = below.clone()` full-frame copies;
- `over_black_rgba8`.

### 4.6 Future, not part of this spec unless measurements demand it: zero-copy decode → compositor

Export decoded VA surfaces as DMA-BUF (`vaExportSurfaceHandle`) and import them into wgpu/Vulkan
as textures, removing the decode readback (spec 01) and the texture upload. This needs Vulkan
external-memory interop under wgpu (wgpu-hal), DRM format modifiers and synchronisation. Write a
separate spec if, after 4.1–4.4, readback plus upload is still a top cost.

---

## 5. Order and dependencies

1. 4.0 instrumentation, then 4.1 pools.
2. 4.3 YUV readback. It is independent of 4.4, and it makes 4.2 option 1 worth re-testing.
3. 4.2 batches.
4. 4.4 adjustment layers (largest).
5. 4.5 if needed.

---

## 6. Correctness

- **GPU vs CPU tolerance:** what the existing GPU-vs-CPU tests in `crates/gpu` use for the same kind
  of content (e.g. `EPS` = 1e-4 on linear values in `export_renderer.rs` tests; blend and fx tests
  have their own bounds). Don't loosen an existing tolerance to make a new case pass.
- **Encoder-input conversion (4.3):** byte-identical to `h264_input(over_black_rgba8(img))` where
  possible; otherwise ≤ 1 code value per sample, with a test over gradients, pure colours,
  transparent areas and 1-pixel detail.
- **Exported files:** per change, the Range A GPU Auto export (the same run that measures it)
  against the reference GPU Off export: luma PSNR ≥ 50 dB on every frame (ffmpeg oracle). At the
  exit gate, Range B the same way, and one side-by-side frame sheet (zooms, text, camera) prepared
  by a sub-agent for Bruno's visual check.
- **Determinism:** GPU Off exports must stay byte-identical to the spec 01 outputs. This spec
  doesn't change the CPU path, except 4.5, which must also stay byte-identical or be explicitly
  re-baselined.
- **Robustness:** a GPU device loss or a renderer panic keeps today's behaviour: the pool retires
  and the rest of the export renders on the CPU (`RendererPool::with`).

---

## 7. Acceptance and exit gate

- Each item: higher Range A throughput (GPU Auto, one run against the recorded previous time,
  TESTING.md §3), correctness checks pass, quality gates pass.
- Spec done when:
  - `export.gpu.fallbacks` is small: only frames with genuinely CPU-only features. For this project
    the expected remainder is ≲ 1% (V4 transitions, titles if needed);
  - the Range A and B GPU Auto exports (one run each) complete, meet §6, and their throughput is
    logged. The full video is never exported;
  - `results/02-render-summary.md` records before/after numbers and names the new bottleneck,
    which is the input to spec 04 (encoding).

---

## 8. Risks

| Risk | Mitigation |
|---|---|
| GPU effect results differ subtly from CPU on adjustment canvases (edges, premultiplication, empty areas after Transform) | Oracle tests first (`execute_cpu` = `render_sequence`); include #1524's case. |
| More renderers or bigger batches exhaust VRAM | Bounded counts; log peak; fall back to 4. |
| Planner changes alter playback rendering (the UI also plans frames) | Gate new behaviour behind the same code path but test playback frames too; the CPU oracle covers both. |
| GPU YUV conversion rounding differs | Exact port of the CPU formulas, plus a ±1 tolerance test. |

## 9. Files likely to change

- `crates/render/src/plan.rs` (`PlanStep::Adjust`, planner, `execute_cpu`), `crates/render/src/lib.rs`
  (factor `adjustment_layer` pieces for reuse), `crates/render/src/adjustment_tests.rs`
- `crates/gpu/src/lib.rs`, `fx.rs`, `mask.rs`, new WGSL for the combine and YUV passes,
  `export_renderer.rs`
- `crates/export/src/pipeline.rs` (pool size, batch, YUV hand-off), `job.rs`,
  `encoders.rs` (`ready` map), `stats.rs`
- `crates/codecs/src/gop.rs` and `crates/h264enc/src/lib.rs` (pool size knobs)
- `crates/engine/src/perf.rs`
