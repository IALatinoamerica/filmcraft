# Spec 04 — Encoding

**Stage:** compressing each output frame into H.264 and writing the MP4.
**Starts after:** spec 03 (done 2026-11-06, tag `fast-export-03-2026-11-06`), when Bruno says go.
**Read first:** [README](README.md) (§0 how the team works), [TESTING.md](TESTING.md),
[GPU-EFFECTS.md](GPU-EFFECTS.md).
**Implementation plan:** [PLAN-04-encoding.md](PLAN-04-encoding.md).
**Refreshed 2026-11-06** with the numbers measured after specs 01–03.

**In plain words (for Bruno):** the compressor that turns finished frames into the H.264 file is
now busy about 70% of every export. This spec makes it faster in two ways: a faster setting for
today's compressor, and using the graphics card's own built-in H.264 compressor.

---

## 1. Summary

The export uses FilmCraft's own pure-Rust H.264 encoder (`crates/h264enc`), always on its
**Balanced** preset. After specs 01–03 it's the busiest stage of the export:

| Run (GPU Auto, tag `fast-export-03-2026-11-06`) | Export | `encodeMs` (share) | `waitMs`: encoder idle (share) | `renderMs` (share) |
|---|---:|---:|---:|---:|
| bench-A-2min, 7,200 frames | 92.8 s | 64.3 s (69%) | 22.8 s (25%) | 73.5 s (79%) |
| Full video, 62,857 frames | 697.1 s | 491.5 s (70.5%) | 161.2 s (23%) | 608.8 s (87%) |

- **Encoder cost:** about 7.8–8.9 ms of wall time per frame, which caps the export at about
  110–130 fps if nothing else were in the way.
- **But the render side is busy most of the export too.** `renderMs` overlaps encoding.
  - If encoding took no time at all, the export would be bounded by rendering: about **−21% on
    bench-A and −13% on the full video**, unless freeing the CPU also speeds rendering up.
  - It may: the software encoder uses all 24 hardware threads, and rendering still does CPU work
    (decode copies, effects, submission).
  - Stage 0 of the plan measures CPU and GPU load to inform expectations; it doesn't gate the
    work.
- **Expected range:** these are conditional estimates, not promises. About 13–21% if rendering is
  unaffected, possibly more if a freed CPU speeds rendering up. Utilisation numbers alone don't
  prove the critical path; only the stage exports do.

The RX 6700 XT has a hardware H.264 encoder (VCN), reachable through VA-API. FilmCraft's only
Linux hardware encoder is NVENC, so "Hardware encoding: Auto" silently falls back to software on
this machine.

This spec adds two things:
1. an exposed **encoder speed setting** for the software encoder (quick);
2. a **native VA-API H.264 encoder** for the AMD GPU (big).

---

## 2. How it works today

### 2.1 Encoder selection

- `job.rs` `make_venc` → `pick_encoder`: the first registered factory that accepts the export wins.
  A factory that fails doesn't stop the walk, and the software encoder is the fallback.
- On Linux, `crates/platform/src/lib.rs` `register()` registers only `nvenc::export::factory`
  (64-bit Linux). Without `libnvidia-encode.so.1` it declines, and the software `h264_factory`
  (`crates/export/src/encoders.rs:492`) takes the export.
- Test 4 of the baseline confirmed this: "NVENC declined because libnvidia-encode.so.1 was missing".

### 2.2 Software encoder (`crates/h264enc`)

| Fact | Where |
|---|---|
| Presets `Speed`, `Balanced` (default), `Quality` | `h264enc/src/lib.rs:93` |
| What they change: Speed has no sub-partitions, no i8×8, ME range 8, no extra sub-pel rounds, no adaptive 8×8 transform. Balanced has partitions, i8×8, ME range 16, 1 sub-pel round. Quality adds RD and ME range 24. | `lib.rs:436–493` |
| The export never sets the preset, so it is always Balanced | `encoders.rs:492–541` (`h264_factory`) |
| 2 B-frames by default; `bframes` not set by the export | `lib.rs` `EncoderConfig` default |
| Slices = one per 4 macroblock rows → 17 slices at 1080p (fixed for determinism) | `encoders.rs:488` `h264_slices` |
| Slice-parallel on its own rayon pool, `threads = 0` → all 24 hardware threads | `lib.rs:565` |
| RGBA → YUV 4:2:0 conversion is done ahead on the render threads (`prepare_ahead`) | `encoders.rs:471` |
| VBR 1-pass = target 12,000 kbps, VBV max 18,000 kbps; keyframe every ≤ 120 frames | `encoders.rs:503–509`, `settings.rs:558` |

Baseline encode cost: `encodeMs` 15.3 ms/frame (test 1, CPU busy rendering) and 10.4 ms/frame
(test 2).

### 2.3 Hardware encoder pattern to follow

`crates/platform/src/nvenc/export.rs` (NVENC) and `crates/platform/src/hardware_encode.rs`
(VideoToolbox) are the templates:
- a `factory(format, w, h, rate, settings) -> Option<Result<Box<dyn VideoEncoder>>>` that declines
  (returns `None`) for anything it doesn't handle;
- `config_for(...)` listing the decline reasons: hardware encoding Off, MXF, two-pass, CRF,
  intra-only, HDR, odd sizes, non-square pixels;
- `VideoEncoder` impl: `sample_entry()` builds `SampleEntry::avc(AvcConfig::new([sps], [pps], 4), w, h)`
  from the encoder's parameter sets; `encode()` returns `EncodedPacket { data (length-prefixed),
  key, duration, composition_offset }`; `flush()` drains;
- `filmcraft_export::note_hw_encode_frame()` counts hardware frames (the `export.hardware` section
  of `perf.stats`);
- a hardware failure mid-export ends the export with an error that names the encoder. It can't
  switch to another encoder mid-stream.

Note: NVENC doesn't implement `prepare_ahead`. It hands RGBA to the NVENC API, which can convert
on the GPU, so a conversion step sits on the serial encode path. `docs/performance.md` ("VideoToolbox export takes the RGBA pictures as they are,
#3030") documents that this conversion became the critical path on macOS. **The VA-API encoder
must do its conversion ahead, on the render threads** (or take the GPU-converted planes from spec
02 §4.3).

### 2.4 What the hardware offers (`vainfo`, 2026-11-05)

`VAProfileH264High / VAEntrypointEncSlice` advertises:
- RT formats YUV420 (8-bit and 10-bit);
- rate control CBR, VBR, CQP, QVBR;
- packed headers sequence, picture, slice, misc, raw;
- max references l0 = 1, l1 = 1: the size of each reference list, not a limit on how many B
  frames follow each other;
- arbitrary or equal-row slices;
- 32 quality levels;
- maximum 4096×4096;
- ROI (32 regions, QP delta);
- intra refresh;
- max frame size.

---

## 3. Goals and non-goals

**Goals**
1. Let Bruno choose the software encoder's speed preset in export settings and presets.
2. Add a VA-API H.264 encoder that takes exports with Hardware encoding Auto on this machine,
   with the agreed settings (H.264 High, 1080p60, 8-bit SDR BT.709, VBR 1 pass 12/18 Mbps, AAC
   audio, MP4).
3. Keep the encode thread from becoming the bottleneck: conversion ahead, asynchronous hardware
   submission.
4. Measure picture quality against the software encoder, so Bruno can choose with evidence.

**Non-goals**
- HEVC or AV1 export, HDR / 10-bit, interlaced, two-pass, CRF, MXF, intra-only. The VA-API
  factory declines these and the software encoder keeps them.
- Byte-identical output across machines. That isn't possible with hardware encoding, and the UI
  already says so for VideoToolbox/NVENC.
- Changing the software encoder's algorithms.
- Zero-copy GPU render → VA surface (§4.4, future).

---

## 4. Work items

### 4.0 Measure (prerequisite): which resource limits the export now

One `bench-A-2min` `gpu-auto` export with the tagged build. While it runs, sample once per second
into a small CSV (a few KB, no profiler):
- **CPU busy %:** from `/proc/stat`, total and per core;
- **GPU busy %:** from `/sys/class/drm/card*/device/gpu_busy_percent`. Also VCN (video engine)
  activity, if the kernel exposes it (`amdgpu_pm_info` / `gpu_metrics`). If not, mark it unknown.

Together with `encodeMs`, `waitMs` and `renderMs`, this **informs expectations**. It doesn't
decide whether to build anything:
- **CPU near 100%:** freeing the CPU may also help rendering.
- **GPU near 100%:** the gain likely stays near the 13–21% estimate.
- **VCN already busy** with decoding: hardware encoding shares that engine (risk §7).

The same stage also makes the common **high-quality reference** export (§5).

### 4.1 Expose the software encoder speed preset (quick)

**Design**
- `ExportSettings` (`crates/export/src/format.rs`): add
  `h264_preset: H264Preset { Speed, Balanced, Quality }` with `#[serde(default)]` = Balanced, so
  existing projects and presets load unchanged. Validate it like the other enums.
- `h264_factory`: `cfg.preset = match s.h264_preset { … }`.
- Optional, same pattern: `h264_bframes: Option<u8>` (0–3, default `None` = encoder default 2) to
  test whether B-frames are worth their cost here.
- UI: a "Encoding speed" dropdown in the H.264 video settings next to Profile/Level, saved in
  presets. CLI: through `--settings '{"h264Preset":"speed"}'` (check the serde name casing used by
  `fixed-settings.json`: camelCase).
- Settings summary text (`settings.rs:701` area): mention the preset.

**Measure** on bench-A at 12/18 Mbps, one run:
- throughput and `encodeMs` per frame for **Speed**. Balanced is measured in §4.0; Quality is slower
  by design and isn't measured in this spec;
- quality against the common high-quality reference (§5): **PSNR-Y, SSIM**, and **VMAF** if the
  local ffmpeg has libvmaf. These are oracle measurements only;
- one side-by-side frame sheet (small tweet text, camera, zooms), prepared by a sub-agent, for
  Bruno's visual check. Range B only for the chosen setting, at the exit gate.

**Decide** which preset goes into "Youtube Main 11 26". YouTube re-encodes the upload anyway, so
what matters is that small text stays clean before YouTube's encode.

### 4.2 Native VA-API H.264 encoder (big; the main item of this spec)

**Location and layering**
- `crates/platform/src/vaapi/encode.rs` for the encoder logic (safe code); libva calls in
  `vaapi/va.rs` or a new `vaapi/va_enc.rs` (the only place with `unsafe`); FFI declarations
  added to `vaapi/ffi.rs`. Linux only (`#[cfg(target_os = "linux")]`).
- Register in `platform::register()`, Linux branch:
  `filmcraft_export::register_encoder(vaapi::encode::factory)`, in front of the software encoder.
  NVENC stays registered; on this machine it declines.
- Probe once (cached, like `va::probe()`): `VAProfileH264High` with `VAEntrypointEncSlice`,
  `VA_RT_FORMAT_YUV420`, `VA_RC_VBR`. No probe → the factory returns `None`.

**FFI additions** (from the public libva headers `va.h`, `va_enc_h264.h`; clean-room, declarations
only)
- Entry points: `vaQueryConfigEntrypoints`, `vaQueryConfigAttributes` (or reuse
  `vaGetConfigAttributes`), `vaCreateConfig` / `vaCreateContext` with the encode entrypoint,
  `vaCreateSurfaces`, `vaCreateBuffer` (`VAEncCodedBufferType`, parameter buffers),
  `vaMapBuffer` on the coded buffer (`VACodedBufferSegment` list), `vaSyncSurface` or
  `vaSyncBuffer` where available, `vaDeriveImage` / `vaPutImage` / `vaCreateImage` for input
  upload.
- Structs: `VAEncSequenceParameterBufferH264`, `VAEncPictureParameterBufferH264`,
  `VAEncSliceParameterBufferH264`, `VAEncMiscParameterBuffer` with
  `VAEncMiscParameterRateControl`, `VAEncMiscParameterFrameRate`, `VAEncMiscParameterHRD`,
  `VAEncMiscParameterBufferQualityLevel`; `VAEncPackedHeaderParameterBuffer` (if packing our own
  headers); `VACodedBufferSegment`.
- Add ABI tests next to the existing `vaapi/abi_tests.rs` (sizes and field offsets against the
  header values).

**Factory decline rules** (mirror `nvenc` / `hardware_encode::config_for`). Take the export only
when all of these hold:
- `format == H264` and `hardware_encoding == Auto`, container MP4 or MOV (not MXF);
- bitrate mode **VBR 1 pass only**: decline CBR, VBR 2-pass and CRF (Bruno uses VBR 1 pass);
- 8-bit SDR (`signal` not HDR), profile High or Main, not intra-only, **progressive only**
  (decline interlaced);
- even width and height ≤ 4096, square pixels.

Decline (`None`) with a logged reason otherwise; the software encoder takes over.

**Encoding design**
- **GOP:** IDR every `keyint` frames (`settings.keyframe_distance`, 120 at 60 fps), P frames
  between. **No B-frames in this spec.** l1 = 1 means one backward reference per B frame; B-frames
  are a later experiment.
- **Rate control:** `VA_RC_VBR`, `bits_per_second` = max (18,000,000),
  `target_percentage` = target / max × 100 (≈ 66), `window_size` ~1000 ms, HRD buffer size and
  initial fullness from the max rate; frame rate 60/1 via `VAEncMiscParameterFrameRate`. Compare
  QVBR if VBR quality at 12 Mbps is poor.
- **Quality/speed:** this spec uses the **driver's default quality level**. Mapping the §4.1 presets
  to `VAEncMiscParameterBufferQualityLevel` (32 levels) is a later step, done only if Bruno wants
  it after seeing the results.
- **Header timing:** `sample_entry()` is needed before the muxer starts. So the **first IDR is
  encoded and synced immediately**, without async depth, and its SPS/PPS are read before any
  later picture is submitted. That also covers exports shorter than the async depth *D*. With
  packed headers (below), the SPS/PPS are known before encoding starts.
- **Headers:** prefer the driver's own SPS/PPS (no packed headers) if radeonsi writes a correct
  VUI: BT.709 primaries, transfer and matrix, limited range, `timing_info` 60 fps. Read them from
  the first IDR's coded buffer (Annex B) to build `AvcConfig` for `sample_entry()`. If the VUI is
  missing or wrong, send packed SPS/PPS built with `filmcraft_h264enc`'s NAL writer
  (`h264enc/src/nal.rs`: `Sps`, `Pps`, `Vui`, `avcc`; make the needed parts `pub` behind a small
  API) so the stream signals exactly what the software encoder signals.
- **Output:** convert the coded buffer's Annex B NAL units to 4-byte length-prefixed samples, drop
  SPS/PPS/AUD NALs from samples (they live in `avcC`), set `key` on IDR.
- **Input:**
  - **GPU frames** (every frame of Bruno's projects since spec 02): take the YUV 4:2:0 planes the
    GPU already produces for the encoder (spec 02 Stage 2, through the `ready` map) and
    interleave chroma into NV12. That's cheap, and it happens on the render threads;
  - **CPU frames** (fallback): implement `prepare_ahead` so render threads convert RGBA8 → NV12,
    reusing `filmcraft_export::rgba_to_yuv420_8`;
  - the encode thread uploads into a pool of VA input surfaces (`vaCreateImage` + map + write +
    `vaPutImage`, or `vaDeriveImage` if the driver supports writing a derived image) and submits.
- **Asynchrony:** keep up to *D* (2–4) pictures in flight. Submit picture *n*, then sync and read
  the coded buffer of picture *n − D*. `flush()` drains everything. The serial encode thread then
  spends its time only on upload and packet handling.
- **Accounting:** `note_hw_encode_frame()` per frame. Hardware sessions and declined counts in
  `export.hardware` like NVENC. A log line naming the encoder ("VA-API H.264 (radeonsi)") at
  session creation. Make the export status text say which encoder ran, so Bruno never has to guess
  again whether "Auto" used the hardware.

**Failure handling:** any VA error during setup → decline (software encoder takes over). Any error
mid-export → `ExportError::Encode("VA-API: …")` and the export stops (same as NVENC). No panics. All
VA objects released on drop and cancel (surfaces, buffers, context, config); confirm with a test
that cancels mid-export and checks nothing leaks.

**Tests**
- Pure unit tests (no GPU): parameter buffer construction for 1080p60 VBR; Annex B →
  length-prefixed conversion; `avcC` from parsed SPS/PPS; decline rules for each excluded setting.
- Hardware tests (skip without a VA-API H.264 encoder, like the decoder tests):
  - encode 120 frames of an ffmpeg `testsrc2` fixture (`target/fixtures/`, never committed);
  - decode the result with FilmCraft's own H.264 decoder and with ffprobe/ffmpeg as an oracle;
  - check frame count, keyframe positions, pts/dts, VUI colour tags, average bitrate within ±10%
    of target, and PSNR above a floor against the source.
- Export-level: Range A with Hardware encoding Auto produces a valid MP4. Check `ffprobe`, A/V
  duration match, `export.hardware.frames` = frame count, and that the stream signals High
  profile at a level that allows 1080p60 (4.2 or higher).

### 4.3 Thread budget with a hardware encoder

With VA-API encoding, the software encoder's 24-thread pool is unused and the encode thread mostly
waits on the GPU. Re-run spec 02 §4.1's pool sizing with the hardware encoder and pick the
defaults for that combination.

### 4.4 Future, not part of this spec unless measurements demand it: zero-copy render → encode

Composite on the GPU, convert to NV12 on the GPU (spec 02 §4.3), export that texture as a DMA-BUF
and import it as a VA surface for encoding. This removes the readback and the upload. It needs
Vulkan external memory under wgpu, DRM modifiers and cross-API synchronisation. Write a separate
spec if, after §4.2, readback plus upload is a top cost.

---

## 5. Correctness and quality

- **Software encoder with a preset change (§4.1):** Balanced output must stay byte-identical to
  before (default unchanged). Speed and Quality are new streams: valid H.264 (decodes with
  FilmCraft and ffmpeg), the same frame count and timing.
- **Common high-quality reference** (made once, in §4.0):
  - one `bench-A-2min` `gpu-auto` export with the same build and the same rendering path, but the
    software encoder at Quality preset and a very high bitrate (VBR 80/100 Mbps);
  - same frames, size, colour tags and frame rate as the test exports.
  - **Balanced, Speed and VA-API are all measured against it**, so their scores can be compared
    directly.
- **Hardware encoder (§4.2):** not byte-comparable. Accept when, on bench-A and, for the final
  chosen setting, bench-B:
  - every frame decodes; frame count, duration and A/V sync match the software export;
  - **VBV:** a timestamped leaky-bucket simulation over `ffprobe` packet sizes and DTS. It uses
    the HRD values the encoder was configured with: that export's max rate, a 2 s buffer (36 Mbit
    at 18 Mbps, 48 Mbit at 24 Mbps), and the initial fullness the encoder configured, recorded in
    the run. The buffer must never underflow;
  - quality: report PSNR-Y / SSIM / VMAF against the common high-quality reference, next to
    Balanced's own score, on bench-A. The bench-B check of the final setting is a sanity check
    against the bench-B GPU Off reference.
    There is no fixed threshold: Bruno decides with the numbers and a side-by-side visual check of
    tweet text, camera and zooms. If hardware quality at 12 Mbps is visibly worse, test a higher
    target (hardware encoders typically need more bits for the same quality). Raising the bitrate
    costs nothing in export time.
- **Container:** MP4 plays in a browser and uploads to YouTube (test one real upload as private).

---

## 6. Acceptance and exit gate

- **§4.1 done** when:
  - the setting works in UI, presets and CLI;
  - Balanced is byte-identical (quick `gpu-off` `cmp`);
  - the measurement table exists in `results/04-encoder-presets.md`.
- **§4.2 done** when the bench-A export with Hardware encoding Auto:
  - uses VA-API for every frame (`export.hardware.frames` = the frame count);
  - completes without errors and passes §5;
  - has its throughput logged next to the software-encoder exports.

  bench-B is checked once, for the final chosen setting (plan 4.2b).
- **Keep-or-reject speed rule** (Bruno), compared with the accepted bench-A `gpu-auto` time
  (92.8 s): more than 10% faster, keep; 5–10%, one confirmation run; under 5%, reject.
- **Quality:** Bruno chooses the encoder and bitrate for "Youtube Main 11 26" from the numbers and
  one side-by-side frame sheet.
- **Spec done** when:
  - `results/04-encode-summary.md` records the final numbers and the chosen settings;
  - the version is tagged;
  - **one full-length export** of the final version is kept with its data in
    `exports/reference/full/<tag>/`, under Bruno's release conventions.

## 7. Risks

| Risk | Mitigation |
|---|---|
| radeonsi's driver-generated VUI or headers are incomplete | Packed headers from `filmcraft_h264enc::nal` (§4.2 Headers). |
| Hardware VBR at 12 Mbps looks worse on small text than the software encoder | Measure (§5); raise the bitrate or use QVBR; Bruno decides. |
| Encode thread becomes the bottleneck through uploads | Conversion ahead (`prepare_ahead`), surface pool, asynchronous depth *D*. |
| Mesa updates change behaviour | Log the Mesa version with every result; hardware tests catch regressions. |
| The VCN engine is shared by decoding (VA-API decode, specs 01/03) and encoding, so hardware encoding slows decoding | §4.0 samples VCN load if exposed; the export with the VA-API encoder shows decode counters next to encode. |
| Rendering, not encoding, turns out to be the limit (GPU near 100%) | §4.0 shows it in advance. The stage exports and the speed rule decide whether §4.2 is kept. |
| ABI mistakes in hand-written FFI structs | ABI tests against header sizes and offsets; start from the existing decoder FFI conventions. |

## 8. Files likely to change

- `crates/export/src/format.rs` (the preset field), `settings.rs` (validation, summary text), `encoders.rs` (`h264_factory`),
  `settings_tests.rs`, `presets.rs`; UI export settings in `crates/ui-egui` (dropdown)
- `crates/platform/src/vaapi/{ffi.rs, va.rs or va_enc.rs, encode.rs, abi_tests.rs, tests.rs}`,
  `crates/platform/src/lib.rs` (registration)
- `crates/h264enc/src/nal.rs` / `lib.rs` (public header-writing API, if packed headers are needed)
- `crates/engine/src/perf.rs` (encoder name in stats)
