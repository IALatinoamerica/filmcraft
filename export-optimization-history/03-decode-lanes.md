# Spec 03 — More than one reader per video file (decode lanes)

**Stage:** reading pictures out of the source recordings, for export (follows spec 01).
**Why now:** after spec 02, render workers spend **39% of their time waiting in line for a video
file's single decoder**. It is the largest remaining wait, and the compressor sits idle a third of
the export because of it.
**Read first:** [README](README.md) (§0 how the team works, §6 ground rules),
[TESTING.md](TESTING.md), [spec 01](01-decoding.md) (how decoding and the cache work).
**Implementation plan:** `PLAN-03-decode-lanes.md`, written by the supervisor before work starts.

**In plain words (for Bruno):** each video file is read by exactly one reader, and it makes
pictures one at a time. When several workers need pictures from the same file, especially right
after a cut, they wait in line. This spec gives each busy file a few readers that work at the same
time on different parts of it.

---

## 1. Evidence (bench-A-2min, GPU Auto, final build `ae807b56`, 108.9 s)

Source: `results/runs/2026-11-06T01-16-41--02-5-exit--bench-A-2min--gpu-auto.json`.

**Render tasks:** 546.5 s accumulated over all render tasks.

| Where render tasks spend time | s | share |
|---|---:|---:|
| `decoderQueueMs`: waiting for a source's decoder lock | 212.3 | 38.9% |
| `otherMs`: mostly waiting on the GPU (`gpuReadbackMs` 222.5 s overlaps it) | 301.2 | 55.1% |
| `effectsMs` | 30.1 | 5.5% |
| `decodeWaitMs`, `frameMapWaitMs`, `compositeMs`, rest | < 4 | < 1% |

**Encoder and decoder:**
- **Encoder:** `encodeMs` 64.5 s, and the encoder waits for frames (`waitMs`) 37.2 s, 34% of the
  export. If frames arrived as fast as the encoder can take them, bench-A would take about
  65–70 s. That is this spec's ceiling; spec 04 (encoding) takes over from there.
- **Decoder:**
  - 13,553 pictures for 7,200 frames: 1.9 per frame, against 4.3 at the start of spec 01;
  - 73 restarts (52 cut-ins, 21 out-of-order);
  - 22,490 input samples decoded;
  - `decodeMs` 143.2 s, which already includes `copyMs` 104.7 s, both done while holding the
    decoder lock;
  - `decodeMsPerSample` 6.4 ms per input sample;
  - `liveDecodersPeak` 8;
  - 17 hardware sessions.

**What this suggests (to be confirmed in the plan's first stage):**
- The export decodes about 22,490 / 108.9 ≈ **206 input samples per second** and returns about
  124 pictures per second, mostly from 2 sources at once (screen and camera).
- At every cut it also runs from the keyframe, up to 249 samples.
- One source's decoder does this work one run at a time, and the CPU copy sits inside the same
  lock, so requests for that file queue while a run is going.
- The queue time proves **contention on the decoder lock**. It doesn't yet prove *why*:
  - restart runs blocking other requests;
  - prefetch holding the lock;
  - decode capacity.

  GPU waiting (`gpuReadbackMs`, 222.5 s accumulated) is also large and may be the real limit
  once the queue is gone.

**Already tried and rejected** (`results/01-time-breakdown.md`, branch `perf/01-decoder-queue`):
letting waiters wait for the frame instead of the lock. Total waiting went up 13%, because the
single decoder still makes the pictures one at a time. **The fix must add decoding capacity per
source, not reorganise the queue.**

**Hardware headroom:**
- ffmpeg decodes these HEVC files at 1.7 ms per picture on this GPU (Spec 01, Stage 3), and
  FilmCraft at about 4.2 ms. So the decode engine isn't saturated by one session.
- Several VA-API sessions run at the same time today: peak 8 live decoders, about 60 MB of VRAM
  each.

## 2. Hypotheses (checked by the plan's first stage, before lanes are built)

- **H1:** most `decoderQueueMs` is spent waiting for a **frame in a different GOP** from the one
  the lock holder is decoding. A second lane could serve those requests at the same time. Time
  spent waiting for a frame inside the holder's own run can't be helped by lanes.
- **H2:** two decoders per busy source remove most of the queue, and the extra restarts they cause
  cost less than the waiting they remove.
- **H3:** the VCN decode engine and the CPU copy keep up with 2 concurrent sessions per busy
  source, and total live sessions stay ≤ 16.
- **Optimistic bound:** if the queue disappears completely and the GPU doesn't become the limit,
  bench-A approaches the encoder limit, about 65–70 s. That's a ceiling, not a target.

## 3. Goals and non-goals

**Goals**
1. Cut `decoderQueueMs` on bench-A by at least half.
2. Bring bench-A GPU Auto export time clearly down, toward the encoder limit (~65–70 s).
3. Leave decoded pictures unchanged: GPU Off output stays byte-identical to the reference.

**Non-goals**
- The encoder (spec 04).
- Playback and preview behaviour: lanes are export-only, behind the existing export flag
  (`cancel::Context` export flag, spec 01 Stage 4).
- Software decoders, other codecs, other machines.
- Zero-copy decode → GPU (spec 02 §4.6). Separate, much bigger.

## 4. Design

### 4.1 Decode lanes per source (`crates/codecs/src/gop.rs`)

Today a `GopCache` per source holds one decoder state behind the decoder lock, plus the frame map
behind its own lock (spec 01 task 8.3).

During export, a source may have up to **L decoder lanes** (default *L* = 2, an export-only
constant; overridable for one bounded experiment with `FILMCRAFT_DECODE_LANES`).
- **Each lane has:** its own decoder session, its own position (`next`, `start`, `out_max`, as
  today's single state), its own output floor and pending queue (spec 01 Stages 4 and 6), and its
  own decoder lock.
- **All lanes share** the source's frame map, accounting and eviction.
  - Lock order: a lane lock first, then the frame-map lock (extends the 8.3 rule).
  - A thread never holds two lane locks at once.

**Lane choice** for a request that misses the cache, checked in this order:
1. **Continue:** a lane that can continue to the wanted sample under **today's `decode_until`
   continuation predicate, unchanged**. The plan's scout quotes it verbatim from `gop.rs`. The
   48-sample `CONTINUE_THROUGH` limit applies to the lane's `next` position, together with
   today's `start` / `out_max` checks.
   - If that lane is busy, queue on it, as today.
2. **Covered:** a lane whose current run will produce the wanted frame. That means its
   reservation includes the pts, and its floor is at or below it. Wait for that lane.
3. **Idle lane:** an idle lane restarts from the keyframe before the wanted sample.
4. **New lane:** if fewer than *L* lanes exist and admission allows, create a lane and restart
   there.

**No GOP is decoded by two lanes at once.**
- Each source keeps a small set of **GOP claims** (sync-sample index → lane), under the frame-map
  lock.
- A lane must claim a GOP before decoding into it, both when it restarts (steps 3–4) and when a
  continuing run crosses into the next GOP (step 1).
- Check and claim happen atomically under that lock. If the GOP is already claimed, the request
  waits for the claiming lane instead, and a continuing run stops at the GOP boundary.
- Claims are released when the lane leaves the GOP, or on reset, error or cancel.
5. **Otherwise:** queue on the lane with the lowest index.

Ties at any step go to the lowest lane index. Nothing is predicted.

This is what lanes buy: a request for a **different GOP**, typically a cut-in or a pre-roll, no
longer waits behind a long run elsewhere in the file. Requests inside one run still wait for that
run, exactly as today.

**Shared-cache protection** (replaces today's single `output_pin` and source-wide
`export_position`):
- **Pins:** each active run holds a **reservation** with its target pts and output floor. Eviction
  never removes a frame reserved by any active run. `output_pin` becomes a set, one per active run.
- **Export position:** eviction behind the export position (spec 01 task 8.2) uses the
  **earliest** outstanding position over all lanes and scheduled jobs of the source.
- **Coverage:** a waiter counts its frame as covered only by a run whose reservation includes that
  pts *and* whose floor is at or below it. A frame suppressed by another lane's floor is never
  counted as covered.
- **Rechecks:** after every wakeup, a waiter rechecks the frame map, then the lane choice.
- **Wakeups:** errors and cancellation wake the waiters of that lane only. An error fails only the
  requests waiting on that run; other lanes continue.

**Session admission:**
- **First lanes** are today's per-source decoders. They're always admitted under today's rules,
  unchanged, so a source can always make progress. Their number is bounded as it is today; this
  spec doesn't change it.
- **Extra lanes** are admitted only while the global count of all live export decoders (first lanes
  included) is below 16. **16 is the cap for creating extra lanes, not a new limit on first
  lanes:**
  - increment before creating a lane, decrement on release;
  - if the cap is reached, no extra lane is created and the request takes step 5. Nothing ever
    waits for admission, so no wakeup is needed.
- Today's soft limits (`MAX_LIVE_DECODERS`, `HARD_DECODERS`, pinned sources excluded) stay for
  playback.
- Extra lanes exist only under the export flag, for sources the scheduler marks as having
  scheduled work. The scheduler publishes that membership, and the outstanding positions, through
  a small hook on the source. That hook is needed even without §4.2.

**Release:** an extra lane is released when it has been idle for 120 output frames of export
progress (2 s of timeline), when its source has no scheduled work left, or when the export ends.
The first lane follows today's rules.

### 4.2 Scheduler feeds lanes (`crates/export/src/schedule.rs`)

The Stage 7 scheduler allows one prefetch job per source. With lanes it may run up to *L*:
- one for the continuing position;
- one for an upcoming cut's pre-roll, so cut-in runs start on a second lane before render workers
  ask for them.

What stays shared, across all jobs:
- the ≤ 8 active sources limit;
- the 1.5 GB budget;
- request deduplication: one job never requests frames another job already covers;
- pin release, and joining every job on stop.

Everything else is unchanged: *L* = 32 look-ahead, *P* = 120 pre-roll, cancellation.

### 4.3 Instrumentation (cheap atomics, in `perf.stats`)

- `decode.lanes.created`, `decode.lanes.peakPerSource`, `decode.lanes.released`.
- Split the existing `export.renderTasks.decoderQueueMs` by the waiter's situation, to test H1:
  - `decoderQueueCoveredMs`: the holder's current run covers the wanted frame (pts inside its
    reservation and at or above its floor), so it will be produced anyway;
  - `decoderQueueNotCoveredMs`: everything else, the part that a second lane could serve.
  - Also record whether the holder is a render task or a prefetch job
    (`decoderQueueBehindPrefetchMs`).
  - **Method:** the holder publishes its run descriptor (start and target pts, floor, kind) as one
    coherent snapshot, so waiters read it **without** taking the busy decoder lock. For example, a
    seqlock-style version counter around atomic fields, or one atomically swapped `Arc` snapshot. A
    reader never sees a mix of two runs. A waiter samples it when it starts waiting. If the holder changes during the wait, the
    first sample decides; that's a documented approximation.
  This goes into the first stage, before lanes are built.

## 5. Correctness

- **Same pictures:** a picture decoded by any lane is bit-identical to one decoded by the single
  decoder. The decoder is deterministic and lanes don't change floors or frames.
  - **Check:** one `bench-quick-1min` `gpu-off` export, `cmp` against
    `exports/reference/bench-quick-1min--gpu-off--46763a85.mp4`. It must be byte-identical.
- **GPU Auto:** the spec 02 rule. Minimum `psnr_y` against the GPU Off reference no lower than the
  last accepted value for that project: 32.49 dB on bench-A, 33.42 dB on the quick project.
- **Unit tests** (`gop/tests.rs`, `schedule.rs` tests, in the style of 8.3's stress test):
  - two lanes serving a cut-in and a continuing run at once;
  - lane choice;
  - no two lanes decoding one GOP;
  - lane release;
  - the session cap;
  - cancellation releases all lanes;
  - errors in one lane reach its waiters and don't poison the others.
- **No leaks:** after an export, live decoders and lanes return to idle.

## 6. Acceptance

- **H1 first.** The plan's first stage measures H1 with the new counters, on the shortest
  instrumented export. If waiting for other GOPs isn't the main part, lanes aren't built and the
  plan is revised.
- **Keeping lanes:** wall time must clearly improve **and** waiting must drop.
  - **Wall time**, against the accepted 108.9 s on `bench-A-2min` `gpu-auto`:
    - more than **10%** faster: keep;
    - **5–10%** faster: one more bench-A run to confirm, and keep only if both are faster than 5%;
    - under 5%: reject. Quick-project runs of the same build have differed by about 9% (PLAN-02
    Notes), so the quick project isn't used for keep decisions in this spec.
  - **Waiting:** **total decoder-related waiting** on that export is lower than the accepted
    213.9 s (`decoderQueueMs` 212.3 + `decodeWaitMs` 1.6, plus any new wait counter added by
    lanes). Moving waiting from one counter to another, as the rejected fix did, doesn't
    count.
- **Correctness** (§5), with existing outputs reused where they're still valid:
  - **GPU Off:** byte-identical on `bench-quick-1min` (`cmp` against the reference).
  - **GPU Auto PSNR:** no lower than the accepted floor for that project.
    - bench-A: minimum `psnr_y` ≥ **32.49 dB**, the spec 02 exit value;
    - quick: ≥ 33.42 dB.
  - **Same length:** frame count, duration and audio length match the reference, and there's no
    offline media.
- **Never export the full video.**

## 7. Risks

| Risk | Mitigation |
|---|---|
| Extra restarts cost more than the waiting they save (each new lane starts at a keyframe) | Lane choice prefers continuing lanes; *L* = 2 by default; a bounded *L* = 3 experiment only if the counters suggest it. |
| The VCN engine or VRAM limits concurrent sessions | Session cap 16; peak sessions recorded; lanes are released when idle. |
| Lock-order or lost-wakeup bugs between lanes and the frame map | Fixed lock order (lane → frame map); stress tests; the 8.3 design notes. |
| Playback changes by accident | Lanes exist only under the export flag; playback keeps one decoder per source. |
| The GPU becomes the next limit (`gpuReadbackMs` 40.7%) | The final export shows it; it would be the next spec, not this one. |

## 8. Files likely to change

- `crates/codecs/src/gop.rs`, `crates/codecs/src/gop/tests.rs`
- `crates/export/src/schedule.rs`
- `crates/frame/src/render_timing.rs`, `crates/engine/src/perf.rs` (counters)
