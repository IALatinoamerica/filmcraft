# Implementation plan — Spec 04: Encoding

This plan turns [spec 04](04-encoding.md) into stages. The spec explains the design; this plan says
what to do, in which order, who does it, and how we know it's finished.

Read first: [README](README.md) §0, [BRIEF-REVIEW.md](BRIEF-REVIEW.md), [TESTING.md](TESTING.md),
[spec 04](04-encoding.md).

**Status: written, not started. Waiting for Bruno's go.**

## Goal (Bruno's request)

Make the compression step faster. It's busy about 70% of every export now (bench-A 92.8 s, full
video 697.1 s, tag `fast-export-03-2026-11-06`). There are two ways:
- a faster preset for the current software encoder;
- the graphics card's own H.264 encoder (VA-API).

Bruno decides the picture-quality question, encoder and bitrate, from numbers and one frame sheet.

**Out of scope:** HEVC/AV1, HDR, two-pass, zero-copy (spec §4.4), and changes to the software
encoder's algorithms.

## How this plan is executed

- **Roles.**
  - The supervisor (Claude Opus) starts one orchestrator per stage. Orchestrators run on GPT-6.1
    Sol at high effort until Bruno says otherwise.
  - GPT-6.1 Sol sub-agents (high effort) do all hands-on work.
  - Every brief is reviewed first by a Sol reviewer (high effort) against `BRIEF-REVIEW.md`.
  - Before a stage that changed code is done, one Claude Opus reviewer (medium effort) reviews its
    diff. This is the switch in the shared rules.
  - Briefs live in `briefs/`; the shared rules are `briefs/common-04.md`, adapted from
    `common-03.md`.
- **Builds.**
  - One shared warm `target/` in `/media/bruno/SSD 500 2 Linux/Software/filmcraft-perf`.
  - `cargo check` and the touched crates' tests while working. Gates once per stage, with
    `cargo test --no-fail-fast`.
  - Release builds always `cargo build --release -p filmcraft-cli -p filmcraft`, and only for a
    stage's exports.
- **Exports.**
  - Only those listed below. Wait 30 s after a build, run in the foreground with a 20 min timeout,
    nothing heavy alongside.
  - Decisions use `bench-A-2min` (the quick project is too noisy for timing).
  - Outputs are deleted once recorded, **except** the files a later stage compares against; each
    stage says which.
  - No full video during stages. **One full-length export at the end**, kept as the reference
    (Bruno's release conventions).
- **Speed rule** (Bruno), against the accepted bench-A `gpu-auto` time of **92.8 s**:
  - more than 10% faster: keep;
  - 5–10%: one confirmation run;
  - under 5%: reject.
- **Git.**
  - One feature branch, `perf/04-encoding`, from `perf/export-amd` (`ae841c93`).
  - Commit before exporting, and record the tested commit with every export.
  - Merge only in the exit stage. The tag goes on the finished spec.
- **Settings files.** New ones are added next to the existing ones in `test-projects/settings/`,
  never edited in place:
  - `youtube-12mbps-gpu-auto-speed.json` (software encoder, Speed preset);
  - `youtube-12mbps-gpu-auto-hwenc-auto.json` (already exists: hardware encoding Auto).
- **Precedence.** Bruno's current rules override any older wording in the shared rules files:
  the full-length reference at the end, tagging finished specs, the speed rule, and necessary
  exports only.
- **Records.** A sub-agent keeps `results/progress/04-stage-<N>.md`, this plan's boxes, status and
  Notes, and one `results/log.md` row per export.

## Status

| Stage | What | Status |
|---|---|---|
| 0 | Which resource limits the export now (CPU / GPU / VCN load), one export | Not started |
| 1 | Encoder speed setting (software Speed preset) | Not started |
| 2 | VA-API hardware H.264 encoder | Not started |
| 3 | Thread budget with the hardware encoder (only if Stage 2 is kept and the counters call for it) | Not started |
| 4 | Exit: Bruno's quality choice, CI, merge, tag, full-length reference | Not started |

Stages run one at a time, 0 → 1 → 2 → (3) → 4. Stage 0 informs expectations only; it doesn't
gate Stage 2.

---

## Stage 0 — Which resource limits the export (spec §4.0)

**Tasks**
- [ ] 0.1 Branch `perf/04-encoding` from `perf/export-amd`. No code changes.
- [ ] 0.2 A small sampler script (Python, run with `-I`) that, once per second, appends to a CSV:
  - total CPU busy %, and per-core busy % from `/proc/stat`;
  - GPU busy % from `/sys/class/drm/card*/device/gpu_busy_percent`;
  - VCN activity if `gpu_metrics` exposes it, otherwise blank.

  It runs only during the export, so the CSV stays a few KB.
- [ ] 0.3 **One export:** `bench-A-2min` `gpu-auto` with the archived CLI
  `builds/03-ae841c93/filmcraft-cli`, with the sampler running alongside.
  - Record export time, `encodeMs`, `waitMs`, `renderMs`, and the sampler's averages and 95th
    percentiles.
  - **Keep this export's MP4** (the Balanced test export) for the quality comparisons.
- [ ] 0.4 **One more export: the common high-quality reference** (spec §5). Same CLI, `gpu-auto`,
  software encoder at Quality preset, VBR 80/100 Mbps, settings file
  `youtube-ref-hq-gpu-auto.json`. **Keep it.** Measure Balanced's PSNR-Y and SSIM against it.
  - The Quality preset isn't exposed yet (Stage 1 adds it). If the settings can't select it, use
    Balanced at 80/100 Mbps, and say so.
- [ ] 0.5 **No go/no-go.** The numbers inform expectations, which the supervisor reports to Bruno.
  Stages 1 and 2 follow. Utilisation doesn't prove the critical path; the stage exports do.
- [ ] 0.6 Records: Notes, log rows, and the CSV kept in `results/progress/`.

---

## Stage 1 — Encoder speed setting (spec §4.1)

**Tasks**
- [ ] 1.1 `ExportSettings.h264_preset { Speed, Balanced, Quality }` (in `crates/export/src/format.rs`), `#[serde(default)]` =
  Balanced, validated like the other enums.
  - `h264_factory` passes it through.
  - CLI through `--settings` (camelCase `h264Preset`).
  - The settings summary text mentions the preset.
  - A UI dropdown "Encoding speed" next to Profile/Level, saved in presets.
  - **No B-frame option** in this stage.
- [ ] 1.2 Tests:
  - settings round-trip and validation;
  - old projects and presets load as Balanced;
  - the factory maps each preset.
- [ ] 1.3 Gates, release build, then **two** exports:
  - `bench-quick-1min` `gpu-off`, `cmp` against its reference. Balanced must stay byte-identical.
  - `bench-A-2min` `gpu-auto` with `youtube-12mbps-gpu-auto-speed.json`:
    - time against 92.8 s, `encodeMs`;
    - PSNR-Y and SSIM against the common high-quality reference (0.4), next to Balanced's score;
    - VMAF only if the local ffmpeg has libvmaf; don't install anything.
    - **Keep this export** until Bruno has chosen at the exit.
- [ ] 1.4 **Extract frames** for Bruno's sheet, the same frames from every kept export: tweet text,
  the camera, a zoom. Keep the PNGs.
- [ ] 1.5 Opus code review of the diff. Commit on the branch, then the records.

**Keep or reject:** the speed rule applies to the Speed export against 92.8 s. A 5–10% gain allows
one confirmation export. If Speed isn't at least 5% faster, revert Stage 1 on the branch: the
setting would add a choice with no benefit. Whether Speed becomes Bruno's default is his choice
at the exit.

---

## Stage 2 — VA-API hardware H.264 encoder (spec §4.2)

**Tasks** (the parallel split is the orchestrator's choice; these are disjoint files)
- [ ] 2.1 **FFI and ABI** (`crates/platform/src/vaapi/ffi.rs`, a new `va_enc.rs`, `abi_tests.rs`):
  the encode entry points and structs listed in spec §4.2, declarations from the public libva
  headers only. ABI tests check sizes and offsets.
- [ ] 2.2 **Encoder** (`crates/platform/src/vaapi/encode.rs`):
  - probe;
  - decline rules (VBR 1 pass only, progressive only, as in spec §4.2);
  - the first IDR is encoded and synced immediately, so the SPS/PPS exist before the muxer
    starts;
  - GOP: IDR every `keyint`, P frames, no B;
  - VBR 18 Mbps max with ≈66% target, frame rate, HRD;
  - quality level: the driver default only in this spec;
  - headers: the driver's own SPS/PPS if the VUI is correct, else packed headers from
    `filmcraft_h264enc::nal`;
  - Annex B → length-prefixed output, `avcC`;
  - async depth *D* = 3;
  - accounting;
  - error handling and release on drop and cancel.
- [ ] 2.3 **Input** (`crates/export`): GPU frames hand their YUV 4:2:0 planes (spec 02 Stage 2) to
  the encoder as NV12, with chroma interleaved on the render threads. CPU frames use
  `prepare_ahead` RGBA8 → NV12. Register the factory in `platform::register()` (Linux), ahead of
  the software encoder.
- [ ] 2.4 **Tests:**
  - unit tests: parameter buffers, Annex B conversion, `avcC`, each decline rule;
  - hardware tests (skipped without VA-API encode): 120 frames of an ffmpeg `testsrc2` fixture in
    `target/fixtures/`, decoded by FilmCraft and by ffmpeg. Check frame count, keyframes, pts/dts,
    VUI colour tags, average bitrate within ±10% of target, a PSNR floor;
  - cancel mid-encode leaks nothing.
- [ ] 2.5 Gates, release build, then **one** export: `bench-A-2min` with
  `youtube-12mbps-gpu-auto-hwenc-auto.json`.
  - Check `export.hardware.frames` = 7,200, and frame count, duration and audio.
  - **VBV:** the leaky-bucket simulation from spec §5, with that export's configured max rate, a
    2 s buffer (36 Mbit at 18 Mbps, 48 Mbit at 24 Mbps), and the initial fullness the encoder
    configured (recorded in the run), with no underflow.
  - PSNR-Y and SSIM against the common high-quality reference, next to Balanced's and Speed's
    scores.
  - Time against 92.8 s.
  - Decode counters next to encode (VCN sharing).
- [ ] 2.6 **If its score against the reference is clearly below Balanced's** (PSNR-Y more than about
  1 dB lower, or a visibly lower SSIM): **one** more bench-A export at a higher target bitrate
  (16/24 Mbps), only to give Bruno the option. Raising the bitrate costs no export time.
- [ ] 2.7 Extract the same sheet frames as 1.4. **Keep the exports** until Bruno has chosen.
- [ ] 2.8 Opus code review of the diff. Commit on the branch, then the records.

**Keep:** apply the speed rule against 92.8 s, with the confirmation export allowed for 5–10%. The
quality choice is Bruno's, at the exit.

---

## Stage 3 — Thread budget with the hardware encoder (spec §4.3; conditional)

**Do it only if** Stage 2 is kept and Stage 0 or Stage 2 shows the CPU still saturated, with
render CPU work waiting. Otherwise mark it **Skipped (reason)**.

- [ ] 3.1 One bounded experiment: the render, decode and encode pool sizes from spec 02 §4.1, at
  most two combinations, one bench-A export each. Keep a combination only under the speed rule,
  compared with **Stage 2's accepted time**, not 92.8 s. A kept combination joins Bruno's sheet.

---

## Stage 4 — Exit

- [ ] 4.1 **The frame sheet for Bruno**, one image, labelled rows for the **retained** options only:
  software Balanced; software Speed, if Stage 1 was kept; VA-API, if Stage 2 was kept, and at the
  higher bitrate if 2.6 ran. It covers tweet text, the camera
  and a zoom, plus the numbers: time, PSNR-Y, SSIM. The supervisor relays it.
  - **Bruno chooses** the encoder, preset and bitrate for "Youtube Main 11 26".
  - Optional for Bruno: one private YouTube upload to confirm the file plays.
- [ ] 4.2 Apply his choice to the default preset, if it changes. That's a settings change only;
  gates, then commit.
- [ ] 4.2b **bench-B check of the chosen settings:** one `bench-B-2min` export with them.
  - Decode, frame, duration and audio checks; the VBV simulation; `export.hardware.frames` if
    VA-API was chosen.
  - PSNR-Y and SSIM against the bench-B GPU Off reference, as a sanity check. The quality decision
    itself rests on the bench-A comparison against the common high-quality reference.
- [ ] 4.2c After Bruno's choice, delete the kept bench-A test exports and the reference, keeping
  only the PNG sheet and the run records.
- [ ] 4.3 `cargo xtask ci` once. Only the known failures are allowed (PLAN-01 Notes, plus the flaky
  UI test if it passes when re-run on its own).
- [ ] 4.4 Merge: fast-forward `perf/export-amd`, archive the CLI as
  `builds/04-<commit>/filmcraft-cli`, add the tag `fast-export-04-<date>`.
- [ ] 4.5 **One full-length export** with the chosen settings, under Bruno's release conventions:
  a scratch copy of the real project with `sha256` before and after, no retry. Keep it with its
  run record and report in `exports/reference/full/<tag>/`, compared with 697.1 s.
- [ ] 4.6 Write `results/04-encode-summary.md` (a first paragraph in plain words for Bruno), and
  update the README status rows.

---

## Notes

- **Starting state (2026-11-06):** `perf/export-amd` = `ae841c93`, tag
  `fast-export-03-2026-11-06`, CLI `builds/03-ae841c93/`.
- **Starting numbers:**
  - bench-A `gpu-auto`: 92.8 s; encode 64.3 s, encoder idle 22.8 s, render 73.5 s.
  - Full video: 697.1 s; encode 491.5 s, idle 161.2 s, render 608.8 s.
  - The previous full-length reference is in `exports/reference/full/fast-export-03-2026-11-06/`.
