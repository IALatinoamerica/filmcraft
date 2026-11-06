# Spec 01 — Decoding

**Stage:** reading pictures out of the source recordings.
**Why first:** today it is the export's bottleneck. Rendering and encoding gains can't show
until decoding gets faster.
**Read first:** [README](README.md) (machine, reference project, baseline, measurement protocol,
ground rules).
**Implementation plan:** [PLAN-01-decoding.md](PLAN-01-decoding.md) (stages, tasks, checks, status).

---

## 1. Summary

FilmCraft decodes the OBS recordings (HEVC 1080p60, a keyframe every 250 frames) with VA-API on the
RX 6700 XT, but uses the hardware poorly during export:

1. **Every decoded picture is copied back to CPU memory, one at a time, synchronously.** The decoder
   waits for the GPU to finish each picture, then copies it, before submitting the next one.
2. **Half the pictures it decodes and copies are never used.** In the baseline, 268,681 pictures were
   decoded for 133,703 frame requests, and 251,736 were evicted from the cache.
3. **Decoders restart often.** There were 1,042 restarts and 115 decoder sessions in one export. Each
   restart decodes forward from the last keyframe, up to 249 pictures.
4. **Requests for the same source are serialized behind one lock** while that source decodes.

The goal of this spec is to make decoding cheap enough that it stops being the export's limit,
without changing a single decoded pixel.

---

## 2. How it works today

### 2.1 Call path during export

```
job.rs  Exporter::step_batch            renders a batch (≤16 frames) in parallel on the rayon pool
  └ pipeline.rs Pipeline::frame         one output frame
      └ render (CPU or GPU) → asks the SourceProvider for each visible layer's picture
          └ codecs/src/gop.rs  GopCache::frame / frame_late_unturned
              ├ cache hit → return Arc<VideoFrame>
              └ miss → lock the per-source state, decode_until(...)
                  ├ seek: reset decoder, start at the sync sample before the target
                  └ loop: decoder.decode(sample) → store_output() into the cache, until target cached
                      └ platform/src/vaapi.rs  VaDecoder::decode
                          └ vaapi/hevc.rs (or h264.rs)  Front::decode
                              ├ va.rs decode_picture → vaBeginPicture/vaRenderPicture/vaEndPicture
                              └ read_out(): for every picture leaving the DPB, accel.read(index)
                                  └ va.rs get_image(): vaSyncSurface + vaGetImage   (waits for GPU)
                                     va.rs read_image(): vaMapBuffer, copy rows into Yuv8 planes
                                     (biplanar::to_frame deinterleaves NV12 chroma), vaUnmapBuffer
```

### 2.2 Code facts (fork `main` 0b780f6f, identical to `upstream/main` on 2026-11-05)

| Fact | Where |
|---|---|
| A picture is read back as soon as the DPB outputs it ("a surface leaving the DPB is reused") | `crates/platform/src/vaapi/hevc.rs:148` `read_out`, same in `h264.rs:141` |
| Readback = `vaSyncSurface` then `vaGetImage` into one session-wide `VAImage`, then `vaMapBuffer` and a row-by-row copy | `crates/platform/src/vaapi/va.rs:421` `get_image`, `:472` `read_image` |
| NV12 → planar `Yuv8` copy, with chroma deinterleave | `crates/platform/src/biplanar.rs:40` `to_frame` |
| Surfaces per session = DPB size + 2 (max 18) | `crates/platform/src/vaapi.rs` `VaDecoder::new` |
| Every output picture goes into the cache, needed or not | `crates/codecs/src/gop.rs` `decode_until` (~line 848) → `store_output` |
| The per-source state lock is held for the whole decode | `gop.rs` `frame_late_unturned`: `self.shared.state.lock()` then `decode_until` |
| A running decoder continues through at most 48 samples into the next GOP; otherwise it restarts | `gop.rs:532` `CONTINUE_THROUGH` |
| Live decoders: 4 kept when idle, up to 8 for recently used caches | `gop.rs:295` `MAX_LIVE_DECODERS`, `:316` `HARD_DECODERS` |
| Per-source cache ceiling 384 MB (~120 1080p frames); 1 GB shared budget | `gop.rs:528` `FRAME_CACHE_CEILING`, `FRAME_BUDGET` |
| Non-reference samples are skipped only when the request is "late" (playback) | `gop.rs` `decode_until`, `is_disposable` |
| Decoding runs on a dedicated `decode-N` rayon pool sized to all hardware threads (24) | `gop.rs:536` `decode_pool` |
| The `VideoDecoder` trait has no way to say "decode this but don't output it" | `crates/codecs/src/video.rs:19` |

### 2.3 Evidence (baseline, README §5.1)

Full timeline, GPU rendering Off:

- 268,681 pictures decoded for 62,857 output frames (4.27 per frame), against ~2.15 visible media
  layers per frame.
- 133,703 frame requests (97,801 cache hits, 35,902 misses). So **at least half the decoded
  pictures were never requested.**
- 1,042 decoder restarts and 115 hardware sessions created. With 250-frame GOPs, a restart costs
  ~125 extra pictures on average. 1,042 × ~100–125 ≈ the ~105k–130k excess pictures.
- `decodeMsPerSample` ≈ 8.0 ms, i.e. ~125 pictures/s per decoder, including GPU wait, readback and
  copy.
- The encoder waited for rendered frames 61% of the export (74% with GPU rendering on).

Reference points (measured by the research agent on 2026-11-05, on one camera recording; re-measure
in §4.0):
- ffmpeg VA-API decode **plus download to CPU**: ~1.6 ms per picture (~637 fps).
- ffmpeg single-threaded software HEVC decode: ~2.8 ms per picture.

So the same GPU can decode and deliver these pictures about 5× faster than FilmCraft does per
picture, before counting the pictures FilmCraft decodes needlessly.

### 2.4 Hypotheses to confirm in §4.0

- **H1:** most of the 8 ms is readback, not GPU decode. The CPU waits on `vaSyncSurface`, then
  `vaGetImage` blits, then a CPU copy reads the mapped buffer. On Mesa that buffer may be
  uncached or write-combined memory, which is very slow to read row by row.
- **H2:** the synchronous decode → wait → copy loop leaves the GPU's decode engine idle most of
  the time.
- **H3:** restarts come from three causes:
  - (a) cutting into a clip mid-GOP: unavoidable decoding, but its readbacks are avoidable;
  - (b) decoders recycled by the live-decoder limits while still in use;
  - (c) out-of-order requests from frames rendered in parallel, so the running decoder can't
    simply continue.
  How the 1,042 restarts split between these decides which fixes matter.
- **H4:** frames rendered in parallel that need the same source wait on that source's lock, so
  parallelism doesn't help the dominant sources (camera, screen).

---

## 3. Goals and non-goals

**Goals**
1. Fewer pictures read back: only pictures that will be used.
2. Cheaper readback per picture.
3. GPU decode overlapped with readback and with other work.
4. No avoidable decoder restarts during export.
5. Render workers rarely wait for a decoder.

**Non-goals**
- Changing any decoded pixel. Output must stay bit-identical (§6).
- Changing the timeline or the source files.
- Zero-copy decode → GPU compositor (DMA-BUF import). That changes rendering's input and belongs
  to a later phase of spec 02 (§2.6 there).
- Software decoder (`crates/hevc`, `crates/h264`) optimisation. The hardware path is the one used.
- Playback behaviour, beyond not breaking it.

---

## 4. Work items

Do them in order. Measure after each (README §5.3) and log the result. Skip an item if §4.0
shows its cause isn't significant.

### 4.0 Measurement harness and decode instrumentation (prerequisite)

**Harness.** Already in place: the test projects and `test-projects/bench.py` ([TESTING.md](TESTING.md)).
On 2026-11-05 it was checked with the installed 0.17.1 CLI: headless export works, and `perf.stats`
is available headless with the `decode`, `export`, `jobs` and `media` sections. What remains:
**Done (PLAN-01 Stages 0–3, 2026-11-05):** the fork's baseline is in TESTING.md §6, the reference
outputs are in `exports/reference/`, the counters below exist and `bench.py` prints them, and the
profile is in `results/01-decode-profile.md`. Every later decode change must reproduce the reference
files byte for byte (§6).

**New counters** (process-wide atomics like the existing `GopStats`, exposed under `decode` in
`perf.stats`)

| Counter | Meaning |
|---|---|
| `submitMs` | time in `vaBeginPicture`…`vaEndPicture` |
| `syncMs` | time blocked in `vaSyncSurface` |
| `getImageMs` | time in `vaGetImage` |
| `copyMs` | time copying out of the mapped buffer (`read_image` / `to_frame`) |
| `readbacks` | pictures read back |
| `usedFrames` | cached pictures later returned to a caller at least once (count on first hit or direct return) |
| `seeksBy.{cutIn, evictedFrame, decoderRecycled, outOfOrder, other}` | why `decode_until` restarted |
| `lockWaitMs` | time callers waited for a busy source's state lock |
| `liveDecodersPeak` | peak live decoders during the export |

Instrument both the H.264 and HEVC fronts.

**Isolated decode benchmark.** Use `filmcraft-cli bench-decode <media> --frames N` on one OBS
recording, compared with ffmpeg as an oracle:

```
ffmpeg -hwaccel vaapi -hwaccel_device /dev/dri/renderD128 \
       -hwaccel_output_format vaapi -i f.mp4 -vf hwdownload,format=nv12 -f null -
```

This separates "raw decode speed" from "how export uses it".

**Deliverable (done):** `results/01-decode-profile.md` answers H1–H4 with numbers, with a
`perf record -g` profile of one test-project export naming the top 10 functions.

### 4.1 Don't read back pictures nobody will use

**Problem.** When a decoder restarts at a keyframe to reach frame *k* (a cut into a clip at its
in-point, or a seek), every picture between the keyframe and *k* is decoded, which is necessary
for references. It is also **read back and cached**, which is not necessary.

**Design**
- Add to `VideoDecoder` (`crates/codecs/src/video.rs`) a default-no-op method, for example:
  ```rust
  /// Pictures whose pts is below `pts` are decoded (references) but not returned.
  fn set_output_floor(&mut self, pts: Option<i64>) {}
  ```
- In `GopCache::decode_until`, when starting a run (seek) to `want_pts`, set the floor to the
  smallest pts that can still be requested soon. In export that is `want_pts`; with leading
  pictures and reordering, use the earliest pts at or after the request the caller may still need.
  Clear it on the next seek, flush or reset.
- In the VA-API fronts (`vaapi/hevc.rs`, `vaapi/h264.rs` `read_out`): for outputs below the floor,
  skip `accel.read(...)` and just release the surface reference (the DPB still holds it for
  reference as long as needed).
- The software decoders may implement it too (skip the final copy or colour conversion), but that
  isn't required.

**Care points**
- The surface must stay valid while the DPB references it. Only the readback is skipped; DPB
  bookkeeping doesn't change.
- `at_or_before` fallbacks in `decode_until` ("nearest decoded frame at or before the wanted pts")
  must still find the wanted frame. Never floor above `want_pts`.
- Playback scrubbing backwards and thumbnails must not regress. Keep the floor export-only at first
  (enable it through an export flag in `filmcraft_media::cancel::Context`, set like
  `with_background`; `isolated()` in `gop.rs` already carries that context onto the `decode-N`
  threads), then consider playback separately.

**Expected effect:** removes most of the "decoded but never used" readbacks (≥50% of readbacks in
the baseline), if H1 holds.

### 4.2 Faster readback per picture

Choose by measurement (§4.0 `syncMs` / `getImageMs` / `copyMs`). Candidates, cheapest first:

1. **One bulk copy, then convert from cached memory.** Copy the whole mapped buffer with a single
   `copy_from_slice` into a pooled CPU buffer, then deinterleave NV12 chroma from there. This
   avoids strided reads from uncached or write-combined memory. Keep `biplanar::to_frame`
   semantics: crop, 8/10-bit and stride handling must stay exactly the same.
2. **Per-session images in a ring** instead of one shared `VAImage`, so `vaGetImage` for picture
   *n+1* can be issued while picture *n* is being copied (see §4.3).
3. **`vaDeriveImage`** on the surface (no blit), if radeonsi supports it for decode surfaces
   (tiled surfaces usually don't). Probe once per session and fall back to `vaGetImage`.
4. **`vaExportSurfaceHandle` (DRM PRIME) + mmap, or a GPU-side linearising copy**: only if 1–3
   are insufficient. More complex; also the foundation for the zero-copy work in spec 02.

**Expected effect:** lowers the per-picture cost toward the ~1.6 ms ffmpeg reference.

### 4.3 Pipeline decode and readback

**Problem.** `read_out` reads back a picture as soon as it leaves the DPB, and `vaSyncSurface` makes
the CPU wait for the GPU before the next picture is submitted.

**Design**
- Keep a small queue of output pictures awaiting readback. Submit picture *n+1* (and more, up to a
  depth *D*) before syncing and reading back picture *n*.
- Raise the session surface count from `DPB + 2` to `DPB + 2 + D` (still capped, for example 24) so
  queued pictures keep their surfaces.
- Return frames from `decode()` in the same output order and with the same pts as today. Only the
  timing changes. `flush()` drains the queue.
- *D* = 2–4 to start; measure.

**Care points:** the DPB's "surface leaving the DPB is reused" rule (comment at `hevc.rs:148`). A
queued picture's surface must not be handed to a new picture until it has been read back. Add an
explicit "pending readback" hold on the surface.

### 4.4 Export-aware decode scheduling (look-ahead)

**Problem.** The export knows the whole timeline, but decoding happens on demand, inside render
workers, under a per-source lock. Each cut stalls the batch while the new clip seeks, and
parallel frames on the same source queue behind each other.

**Design**
- A **decode scheduler** owned by the export (`crates/export`, created in `Exporter::set_up`, dropped
  at the end or on cancel) that walks the sequence ahead of the encoder: for each video clip
  visible in frames `[next, next + L)`, it knows the source, the source frame numbers it will need
  (via the clip's speed and time remap) and their order.
- One decode worker per active source (bounded, for example ≤ 8) on the existing `decode-N` pool. It:
  - pre-rolls a clip before its in-point (seeks to the keyframe and decodes forward with the output
    floor from §4.1 set at the first needed frame), so the cut doesn't stall the batch;
  - then decodes forward in order and puts frames into that source's `GopCache` ahead of
    the render workers, up to a bounded number of frames ahead (look-ahead *L*, for example
    2 × batch);
  - releases the decoder when the clip's last needed frame has been delivered.
- The export reaches the caches through the `SourceProvider` it already holds
  (`SourceProvider::source(item)` returns the clip's `MediaSource`). Add an optional hook to
  `MediaSource` (`crates/media/src/lib.rs`) such as `prefetch(&self, requests: &[FrameRequest])`
  (default no-op), implemented by the `GopCache`-backed container sources, rather than making
  `crates/export` reach into `crates/codecs` internals. `cargo xtask layers` must still pass.
- Render workers keep calling the normal `SourceProvider`. They now mostly hit the cache.
- While an export runs, decoders the scheduler is using are **pinned**: they don't count against
  `MAX_LIVE_DECODERS` / `HARD_DECODERS` and aren't recycled. Size the pin set by the timeline:
  usually 3–4 active plus 2–3 upcoming. A 1080p HEVC session is ~20 surfaces × 3 MB ≈ 60 MB of
  VRAM.
- Memory: frames delivered ahead count against the cache budget. Size *L* so
  `active sources × L × 3.1 MB` stays within a fixed export decode budget (for example 1.5 GB;
  the machine has 62 GB, but stay bounded).

**Care points**
- Time remap and speed changes: compute source frames with the same functions the renderer uses
  (find them from `crates/render`'s source lookup); never re-derive with `f64` seconds.
- VFR sources: request by pts the way the renderer does, so cache keys match exactly.
- Cancellation: the scheduler stops at the next sample and releases decoders on cancel or error
  (`progress.cancel`).
- Determinism: the scheduler changes *when* frames are decoded, never *which pixels*. The output
  must stay byte-identical.

### 4.5 Remove remaining restarts

After 4.1–4.4, read `seeksBy.*`. Expected remaining causes and fixes:

| Cause | Fix |
|---|---|
| `outOfOrder` | Within a batch, let the scheduler deliver in order. If render workers still request ahead of the decoder, the decoder continues forward (no restart) as long as the target is ahead of `next` within the GOP. Verify `continuing` in `decode_until` handles it. |
| `evictedFrame` | A frame was decoded, evicted before use and requested again. Raise the per-source budget during export for scheduled sources, or evict only frames behind the export position. |
| `decoderRecycled` | Should be zero with pinning (4.4). |
| `cutIn` | Unavoidable decode, but read back nothing before the in-point (4.1), and pre-roll it (4.4). |

### 4.6 Lock contention (only if `lockWaitMs` is still significant)

With the scheduler feeding the cache, workers should mostly take the cache-hit path. If they still
wait, split the cache's frame map lock from the decoder lock, so a cache hit never waits behind a
decode in progress.

---

## 5. Instrumentation to keep

Everything added in §4.0 stays in the build (cheap atomics) and is shown in `perf.stats`, so later
specs can watch decoding stay healthy.

---

## 6. Correctness

- **Bit-exact output.** With GPU rendering Off and the software H.264 encoder, FilmCraft's export
  is byte-identical across runs and core counts (`job.rs` module docs, `determinism_tests.rs`).
  After every change, the one Range A GPU Off export (which is also the measurement) must be
  **byte-identical** to its reference output. At the exit gate, Range A and B with GPU Off and GPU
  Auto are each checked once the same way (GPU Auto is deterministic, PLAN-01 Notes).
- **Unit tests** in `crates/platform/src/vaapi/tests.rs` and `crates/codecs` for:
  - output floor: frames below the floor are not returned, frames at or above it are identical
    to before;
  - pipelined readback: same frames, same order, same pts, for H.264 and HEVC fixtures with
    B-frames and reordering. Generate fixtures with ffmpeg into `target/fixtures/` and never commit
    media (`docs/testing.md`);
  - scheduler: a synthetic timeline with cuts into mid-GOP in-points, speed changes and
    cancellation delivers exactly the frames the renderer asks for.
- **Hardware tests skip** when no VA-API device is present, as the existing ones do.
- **Robustness:** a VA-API error mid-export keeps today's behaviour (the frame request fails and
  the export reports it); no panics, no leaked surfaces or sessions after cancel (check
  `displays_opened()` and live decoders return to their idle values).

---

## 7. Acceptance and exit gate

An item is done when:
1. its Range A throughput (one run against the previous build's recorded time, TESTING.md §3) is
   higher than before, or it is a prerequisite of an item that is;
2. the outputs are byte-identical (§6);
3. quality gates pass (README §6).

The spec is done when:
- the Range A and B exports, GPU Off and GPU Auto (one run each), are byte-identical to their
  references, complete without errors, and their throughput is logged. The full video is never
  exported;
- `waitMs` and the new decode counters show **decoding is no longer the limit**: decode-side waits
  are small next to render and encode times, and readbacks ≈ used frames;
- `results/01-decode-summary.md` records before/after numbers and the new bottleneck, which is the
  input to spec 02.

---

## 8. Risks

| Risk | Mitigation |
|---|---|
| radeonsi behaves differently from what the hypotheses assume (for example `vaGetImage` is the slow part, not the copy) | §4.0 decides; §4.2 lists alternatives. |
| Holding more surfaces or decoders exhausts VRAM | Bounded counts; log peak; the RX 6700 XT has 12 GB. |
| Pipelining breaks DPB surface reuse, so wrong pictures appear | Explicit "pending readback" hold; byte-identical check catches it immediately. |
| Scheduler and renderer disagree on source frame mapping (speed, remap, VFR) | Use the renderer's own mapping functions; the byte-identical check catches mismatches. |
| Upstream changes `gop.rs` / VA-API files | Keep commits small; rebase only at the exit gate, if worth it (README §3). |

## 9. Files likely to change

- `crates/platform/src/vaapi/va.rs`, `vaapi/hevc.rs`, `vaapi/h264.rs`, `vaapi.rs`,
  `crates/platform/src/biplanar.rs`
- `crates/codecs/src/video.rs` (trait method), `crates/codecs/src/gop.rs` (floor, pinning,
  counters, restart reasons)
- `crates/export/src/job.rs` and a new `crates/export/src/schedule.rs` (decode scheduler)
- `crates/engine/src/perf.rs` (expose counters)
- `apps/filmcraft-cli` (stats output, if needed)
