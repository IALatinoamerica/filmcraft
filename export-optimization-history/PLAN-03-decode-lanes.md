# Implementation plan — Spec 03: Decode lanes

This plan turns [spec 03](03-decode-lanes.md) into stages. The spec explains the design; this plan
says what to do, in which order, who does it, and how we know it's finished.

Read first: [README](README.md) §0 (how the team works), [BRIEF-REVIEW.md](BRIEF-REVIEW.md) (the
owner's rules), [TESTING.md](TESTING.md), [spec 03](03-decode-lanes.md).

## Goal (Bruno's request)

"More than one reader per video file." After spec 02, render workers wait 39% of their time in line
for a file's single decoder (bench-A, `decoderQueueMs` 212.3 s of 546.5 s). The goal is to cut that
waiting and make the export clearly faster. The ceiling is the encoder, at about 65–70 s on bench-A
against 108.9 s today.

**Out of scope:** the encoder (spec 04), playback, software decoders, zero-copy, and any GPU-side
tuning.

## How this plan is executed

- **Roles.** The supervisor starts one orchestrator per stage. **From 2026-11-06, until Bruno says
  otherwise, orchestrators run on GPT-6.1 Sol at high effort.**
  - The orchestrator plans, briefs, reviews and decides. GPT-6.1 Sol sub-agents (high effort) do
    all the hands-on work.
  - Every outgoing brief, from the supervisor or an orchestrator, is first reviewed by a Sol
    reviewer (high effort) against `BRIEF-REVIEW.md`.
  - Briefs live in `briefs/`. The shared rules are `briefs/common-03.md`, based on
    `common-02.md` with this plan's numbers.
- **Builds.**
  - One shared, warm `target/` in `/media/bruno/SSD 500 2 Linux/Software/filmcraft-perf`; never
    per-agent target dirs.
  - `cargo check` and the touched crates' tests while working.
  - Gates once per stage: `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D
    warnings`, `cargo test --no-fail-fast` for the touched crates, `cargo xtask layers`.
  - Release builds always `cargo build --release -p filmcraft-cli -p filmcraft`, and only when
    the stage exports.
- **Exports.** Only the ones each stage lists.
  - Wait 30 s after a build, run in the foreground with a 20 min timeout, nothing heavy alongside.
  - Delete the outputs once they're checked and recorded.
  - **Never export the full video.**
  - **Decisions use `bench-A-2min` `gpu-auto`** (about 2 min), not the quick project: two quick
    runs of the same build differed by about 9% (PLAN-02 Notes).
- **Correctness**, per spec 03 §5:
  - `bench-quick-1min` `gpu-off`, `cmp` against
    `exports/reference/bench-quick-1min--gpu-off--46763a85.mp4`: it must be byte-identical.
  - GPU Auto PSNR (`ffmpeg -i new.mp4 -i ref.mp4 -lavfi psnr=stats_file=<unique tmp> -f null -`,
    one line per frame): minimum `psnr_y` ≥ 32.49 dB on bench-A and ≥ 33.42 dB on quick.
  - Frame count, duration and audio length match the reference (`ffprobe`), and there's no
    offline media.
- **Git and keep or reject.**
  - Stages 1 and 2 work on **one feature branch**, `perf/03-lanes`, from `perf/export-amd`
    (`ae807b56`, tagged `fast-export-2026-11-06`).
  - Nothing is merged into `perf/export-amd` until Stage 3 accepts it.
  - **Accept or reject on speed** (Bruno, 2026-11-06), comparing bench-A `gpu-auto` with
    108.9 s:
    - more than **10%** faster: keep;
    - **5–10%** faster: run one more bench-A `gpu-auto` export to confirm, and keep only if both
      runs are faster than 5%;
    - under 5%: reject.
  - **Also required:** **total decoder-related waiting** (`decoderQueueMs` + `decodeWaitMs` + any
    new lane-wait counter) is lower than 213.9 s, and correctness passes.
  - **Commit before exporting:** commit the code on the branch first. Each export records the
    commit it tested. Stage 3 reuses a stage's exports if the final head differs from that
    commit only in docs or records, or by an exact revert of a later change.
  - **Otherwise** the branch is kept as `rejected: …` and `perf/export-amd` stays at the tag.
- **Records.** A sub-agent keeps `results/progress/03-stage-<N>.md`, updates this plan's boxes,
  status and Notes, and adds one row to `results/log.md` per export.
- **Bruno** isn't needed during the stages. Decoded pictures must stay bit-identical, so no visual
  check is planned. He gets a short plain summary at the end.

## Status

| Stage | What | Status |
|---|---|---|
| 0 | Who waits for what: counters + one diagnosis export, then go/no-go | Diagnosis complete; provisional GO, supervisor confirmation pending |
| 1 | Decode lanes in the frame cache | Measurement/decision complete: C, pending supervisor confirmation; tested/final 26a60d6fa9d5e54f24cbdab2e210c4cb2304aeff, clean; quick cmp and bench-A PSNR/frame/audio PASS;99.690642s,waiting160.408550s,notcovered24.830300%;8.456711% speed gain is confirmation case; no next-stage work |
| 2 | Scheduler feeds the second lane (only if Stage 1's counters call for it) | Complete — orchestrator KEEP; tested/final ae841c93fdd4120c0d147365cca73403c8c81044, clean; gates/Opus/correctness PASS; bench-A92.792066298s<94.706109590s,waiting137.680255s<160.408550s;2.1–2.4 complete;no merge/confirmation/Stage3 |
| 3 | Exit: CI, final check, merge, summary | Implementation/exit tasks 3.1–3.5 complete; accepted/released ae841c93, local tag fast-export-03-2026-11-06; Stage 2 checks reused, one full export recorded; [summary](results/03-lanes-summary.md); formal closure awaits supervisor relay to Bruno |

All stages run one at a time: they touch the same files (`gop.rs`, `schedule.rs`).

---

## Stage 0 — Who waits for what (spec 03 §2, §4.3)

**Goal:** know whether a second lane would help before building it.

**Tasks**
- [x] 0.1 Branch `perf/03-lanes` from `perf/export-amd`.
- [x] 0.2 **Counters**, exactly as in spec 03 §4.3: cheap atomics, timed on render tasks only,
  in the existing `render_timing` / `perf.stats` pattern.
  - Split `decoderQueueMs` into `decoderQueueCoveredMs` and `decoderQueueNotCoveredMs`.
  - Add `decoderQueueBehindPrefetchMs`, which overlaps the two.
  - **Method:** the holder publishes its run descriptor (start and target pts, floor, kind) as one
    coherent lock-free snapshot, a seqlock-style version counter or an atomically swapped `Arc`,
    as in spec 03 §4.3. Waiters read it without taking the busy decoder lock. The first sample at
    wait start decides.
  - **Tests:** the two parts add up to `decoderQueueMs`; the counters only grow; the keys exist in
    `perf.stats`.
- [x] 0.3 Gates, release build, **one** `bench-quick-1min` `gpu-auto` export. Its time isn't used
  for decisions, only the counters, so the shortest project is enough. Record the three counters
  as shares of `decoderQueueMs` and of render-task time.
- [x] 0.4 **Go/no-go** (the orchestrator reports; the supervisor confirms):
  - **Go to Stage 1** if `decoderQueueNotCoveredMs` ≥ 50% of `decoderQueueMs` **and**
    `decoderQueueMs` ≥ 20% of render-task time.
  - **Otherwise stop.** The supervisor revises this plan before any lane code is written. For
    example, if most waiting is behind prefetch, the fix is in the scheduler instead.
- [x] 0.5 Commit the counters on `perf/03-lanes`. The diagnosis is useful either way. Update Notes
  and the log row.

---

## Stage 1 — Decode lanes in the frame cache (spec 03 §4.1)

**Goal:** a request for a different GOP of a busy file is served by a second decoder instead of
waiting.

**Tasks** (`crates/codecs/src/gop.rs`, `gop/tests.rs`, plus the small source/scheduler hook in
`crates/media` and `crates/export/src/schedule.rs`.)

First, a Sol scout maps the current single decoder state at `ae807b56`:
- the locks, `output_pin`, `export_position`, the floors and the pending queue;
- the `decode_until` continuation predicate, quoted verbatim.

Tasks 1.1–1.6:
- [x] 1.1 **Lane state.** Move the per-decoder state into a `Lane`: session, position (`next`,
  `start`, `out_max`), output floor, pending queue and its own lock. The `GopCache` keeps the frame
  map and accounting.
  - With *L* = 1, behaviour and output are identical to today. The existing gop tests must pass
    unchanged.
- [x] 1.2 **Lane choice**, exactly as in spec 03 §4.1: continue → covered → idle → new (if
  admitted) → lowest index. Add the atomic **GOP claims**, so no GOP is decoded by two lanes at
  once, including when a continuing run crosses a GOP boundary. Default *L* = 2, an export-only constant. `FILMCRAFT_DECODE_LANES`
  is read for a bounded experiment only.
- [x] 1.3 **Shared-cache protection:**
  - `output_pin` becomes a set of per-run reservations, each with a target and a floor;
  - eviction uses the earliest outstanding export position;
  - coverage counts only runs whose reservation includes the pts and whose floor is at or below
    it;
  - waiters recheck after every wakeup.
- [x] 1.4 **Admission, hook and release**, as in spec 03 §4.1:
  - first lanes are always admitted under today's rules;
  - extra lanes are created only while the count of all live export decoders is below 16. That's
    a cap on extra lanes, not a new limit on first lanes. When the count is at the cap, the
    request falls back to queueing;
  - the source/scheduler hook publishes scheduled-work membership, outstanding positions and
    export progress;
  - an extra lane is released after 120 output frames idle, when its source has no scheduled
    work, or when the export ends;
  - lanes exist only under the export flag.
- [x] 1.5 **Tests** (deterministic, bounded by timeouts, in 8.3's stress-test style):
  - two lanes serving a cut-in and a continuing run at the same time;
  - each lane-choice rule;
  - no two lanes decoding the same GOP, including a continuing run that reaches a GOP another lane
    has claimed;
  - a frame suppressed by another lane's floor isn't counted as covered;
  - admission at the cap still makes progress;
  - release;
  - cancellation and errors wake only that lane's waiters, and other lanes continue;
  - no leaked sessions after an export (`live_decoders()` back to idle).
- [x] 1.6 Counters `decode.lanes.created`, `decode.lanes.peakPerSource`, `decode.lanes.released`.
- [x] 1.7 Gates, release build, then **two** exports:
  - `bench-quick-1min` `gpu-off`, `cmp` against its reference. It must be byte-identical.
  - `bench-A-2min` `gpu-auto`:
    - time against 108.9 s;
    - PSNR ≥ 32.49 dB;
    - frame, duration and audio checks;
    - `decoderQueueMs` and its split;
    - the lane counters, and `liveDecodersPeak`.
- [x] 1.8 Commit on `perf/03-lanes`, not merged. Notes and log rows.

**Decision after Stage 1** (the orchestrator reports; the supervisor confirms). Exactly one of:
- **A. Stop.** Correctness fails, bench-A isn't faster than 108.9 s, or total decoder-related
  waiting isn't lower than 213.9 s. The branch is marked rejected, and the supervisor reports to
  Bruno with the numbers.
- **B. Stage 3.** Not A, and `decoderQueueNotCoveredMs` < 10% of render-task time. Stage 2 can't
  add much. Stage 3 then applies the speed rule above (keep > 10%, confirm 5–10%, reject < 5%).
- **C. Stage 2.** Not A, and `decoderQueueNotCoveredMs` ≥ 10% of render-task time.

---

## Stage 2 — Scheduler feeds the second lane (spec 03 §4.2; conditional)

**Goal:** cut-in runs start on a second lane *before* render workers ask for them.

**Tasks** (`crates/export/src/schedule.rs` and its tests)
- [x] 2.1 Allow up to *L* prefetch jobs per source: one for the continuing position, one for the
  next cut's pre-roll (*P* = 120).
  - The ≤ 8 active sources limit and the 1.5 GB budget stay shared across all jobs.
  - Requests are deduplicated, so no job asks for frames another job covers.
  - Every job is joined on stop, and its pins are released.
- [x] 2.2 **Tests:**
  - the pre-roll goes to a different lane than the continuing job;
  - the budget and the source limit hold with two jobs per source;
  - no duplicate requests;
  - cancellation joins all jobs and leaks nothing;
  - output is byte-identical with the scheduler on and off.
- [x] 2.3 Gates, release build, then the same two exports as 1.7: quick `gpu-off` `cmp`, and
  bench-A `gpu-auto` with all its checks and counters.
- [x] 2.4 Commit on `perf/03-lanes`. Notes and log rows.

**Stage 2 is kept** only if all of these hold:
- correctness passes;
- total decoder-related waiting is lower than Stage 1's;
- its bench-A time is better than Stage 1's by more than ~5%.

Otherwise revert it on the branch with an exact revert, record why, and carry Stage 1 forward.
Stage 1's exports stay valid under the reuse rule.

---

## Stage 3 — Exit

- [x] 3.1 `cargo xtask ci` once on the final branch head. Only the known failures are allowed:
  PLAN-01 Notes, plus the flaky `perf_ui::program_monitor_plays_on_while_a_value_is_dragged` if it
  passes when re-run on its own.
- [x] 3.2 **Final check.** Reuse the accepted stage's recorded exports (quick `gpu-off` `cmp`,
  bench-A `gpu-auto` with all checks) under the reuse rule above. Export again only if the final
  head's code differs from the tested commit.
- [x] 3.3 **Accept or reject** with the rule above.
  - **Accept:** fast-forward `perf/export-amd`, archive the CLI as
    `builds/03-<commit>/filmcraft-cli`, and add the local tag `fast-export-03-<date>`.
  - **Reject:** mark the branch `rejected: …`. `perf/export-amd` stays at
    `fast-export-2026-11-06`.
- [x] 3.4 Write `results/03-lanes-summary.md`:
  - a first paragraph in plain words for Bruno;
  - before/after numbers;
  - what each stage did;
  - what limits the export next (GPU readback, encoder, or something else).
- [x] 3.5 Update the README status row for spec 03.

**Implementation/exit tasks complete (2026-11-06), formal closure pending.** Accepted/released
`ae841c93fdd4120c0d147365cca73403c8c81044`, local annotated tag
`fast-export-03-2026-11-06`; [summary](results/03-lanes-summary.md).
CI ran once: fmt/Clippy PASS, nextest 4912 pass/6 known allowed failures/52 skipped,
UI flake passed without retry, translations report PASS; xtask exit 1 from nextest.
Later phases (including doctests/layers) were not run or separately supplemented.
Stage 2 exact-SHA correctness/acceptance was reused without test-project re-export or confirmation.
The owner's Stage 3 exception authorized exactly one retained full-video-03 GPU Auto export:
697.056966386 s, 90.175 fps, checks PASS; no retry. No Stage 3 code change or new Opus review.

**Done when:** all of the above, and the supervisor has relayed the summary to Bruno.

The supervisor relay has not been recorded by this implementation task; that condition remains open.

---

## Notes

- Stage3 release/documentation (2026-11-06): tasks3.1–3.5 complete, formal closure awaits supervisor relay to Bruno. Accepted/released exact tested/final `ae841c93fdd4120c0d147365cca73403c8c81044`; perf/export-amd fast-forwarded, WT clean, local annotated tag `fast-export-03-2026-11-06`, nothing pushed. Stage2 quick cmp0 and bench-A PSNR/frame/audio/offline checks reused at the same SHA; bench-A92.792066298s/77.593fps vs108.9s (14.791491003% less), waiting137.680255s vs213.9s; no test-project re-export/confirmation. CI once283.223197180s:fmt/ClippyPASS,nextest4912pass/6knownallowedfail/52skip,UIflakePASS/no retry,translation reportPASS,xtaskexit1; phases after early return (including doctests/layers) not run or supplemented. Release once195.271706455s; archived CLI builds/03-ae841c93/filmcraft-cli, SHA256 eee80abe62e16298868361324baf0af9e9f02a90bc1a1eee98c763f7ae91ff6f. Owner-authorized single full-video-03 GPUAuto697.056966386s/90.175fps vs794.201249914s/79.145fps:97.144283528s saved,12.231696127% less time; queue/renderTask32.138793737%→24.851725429%. Full62857frames,AVdelta20.333ms<=21.333333ms,offline[],exit0,timeout3600/no retry or cap activation; MP4 retained1259413306bytes with report/JSON in exports/reference/full/fast-export-03-2026-11-06/. Source before/after SHA2562832a6b044557b2022677c001152bd460262ac75c2d40bd85a65555ae676b539 unchanged,only own scratch cleaned. No Stage3 code edits/new Opus review per explicit exception. Heavy CI+build+foreground export1176.534402142s;deliberate idle30.000073913s;heavy+checks/integrity/idle1206.618730963s excludes administration. [Summary](results/03-lanes-summary.md) records final scalar bottleneck evidence/inference, counter limits, accounting and exact artifacts; [progress](results/progress/03-stage-3.md) preserves commands and coverage limits. This docs executor sends no messages to Bruno; relay remains a supervisor action.

- Stage2 final KEEP (2026-11-06): original mandatory Opus APPROVE plus latest incremental semantic-saturation APPROVE, all3LOW dispositions retained. No fresh gates/tests/check/code edits; prior431top+5child/3ignored/layersPASS and correction fmt/Clippy/11scheduler/diffPASS reused. Commit ae841c93fdd4120c0d147365cca73403c8c81044 exactly3authorized files, clean; perf/export-amd unchanged ae807b569a0db327e6d0bd4490bfb96afb6653c9. One release build190.318750225s/one30s cooldown/exactly2sequential exports same clean full SHA: quick149.230122858s/3600/cmp0;bench-A92.792066298s/7200/PSNR7200rows,min32.49dB@n5828/ffmpeg0/probe0,H2647200,120s video-AAC-container,48000Hz5626AAC,AVdiff0/offline[]. Queue135666.900ms=covered25499.904+notcovered110166.996,behindPrefetch128202.967overlap,decodeWait2013.355,total137.680255s;lanes31/absolutepeak2/released31,livepeak10,no final teardown gauge. Orchestrator explicitly KEEP; Stage2 complete:137.680255s<160.408550s waiting and92.792066298s<94.706109590s speed threshold. Bench-A is6.919982919% faster than Stage1(99.690641674s),14.791491003% faster than accepted108.9s;waiting also<213.9s. No code/Git mutation,revert,confirmation,merge or Stage3. Boxes2.3/2.4 complete;one log row/export;owned outputs/PSNR deleted after checks/recording,cleanup verified. Exact records/review links/dispositions/all counters/commands/accounting: results/progress/03-stage-2.md and results/progress/03-stage2-export-captures/.

- Stage2 narrow Clippy correction (2026-11-06): schedule.rs166 replaces checked_add/nested checked_mul fallback with saturating_add/saturating_mul;243 replaces checked_mul fallback with saturating_mul, preserving u64 operands/conversions and saturation. Only2lines/3expressions changed; tests/gop/Opus LOW observations unchanged. Pre-diff briefs/03-stage2-pre-clippy-fix.patch 37187bytes; incremental briefs/03-stage2-clippy-fix.patch 1414bytes; own baseline removed. Once each/foreground/shared target: fmtPASS4.599074095s; workspaceClippyPASS24.528939913s; scheduler11pass/0fail/0ignoredPASS4.493517110s; diff-checkPASS0.005005307s; all exits0. Full prior suite431top-level+5child/3ignored12.521270801s and layers0.187739254s reused, no full-suite/layers/GOP repetition. HEAD26a60d6fa9d5e54f24cbdab2e210c4cb2304aeff unchanged/exactly same3dirty paths/index empty. Stage2 unfinished; incremental Opus review, commit/release/exports2.3/2.4/decision pending; no measurement/log row. Machine bruno-main prescribed checks33.626536425s; idle0; no agents/GUI. Exact corrected authorization, paths/counts/exit/times/captures/accounting: briefs/03-stage2-clippy-correction.md and results/progress/03-stage-2.md.

- Stage2 validation (2026-11-06): mandatory Opus5.5 medium APPROVE confirmed from dispatch for exact3-file diff; all3LOW observations retained per orchestrator, no fixes requested. Each gate once/foreground/shared target: fmtPASS4.615512316s; workspace ClippyFAIL exit101/8.480448560s, three manual_saturating_arithmetic errors at schedule.rs166:13/166:29/243:26; codecs/export testsPASS12.521270801s,431top-level/0fail/3ignored plus5nested passes (GOP1/renderer4), all11scheduler/counter/GPU regressions pass; layersPASS0.187739254s; diff-checkPASS0.005164435s. Unexpected Clippy blocker means no commit/build/cooldown/exports/correctness measurements/log rows/keep-revert/Stage3;2.3/2.4 remain unchecked. Full HEAD/base26a60d6fa9d5e54f24cbdab2e210c4cb2304aeff still original3dirty paths/+434-96, index empty; perf/export-amd ae807b569a0db327e6d0bd4490bfb96afb6653c9 unchanged. Measured25.810135366s incl diff-check; idle0, no owned MP4/PSNR artifacts to clean; no fixes/reruns. Review dispositions, exact counts/commands/exit/elapsed/captures and accounting: results/progress/03-stage-2.md and results/progress/03-stage2-validation-captures/. Stage2 blocked, not finished or accepted.

- Stage 2 implementation 2.1/2.2 (2026-11-06): supervisor confirmed C→Stage2, no confirmation export now. Uncommitted three-file patch on perf/03-lanes at 26a60d6fa9d5e54f24cbdab2e210c4cb2304aeff; perf/export-amd remains ae807b569a0db327e6d0bd4490bfb96afb6653c9. Scheduler keeps at most min(configured L,2) actual JoinHandle<bool> workers/source, separate continuing/nearest-cut batches, source-wide dedup/coverage/budget/pins, and drains every worker before clearing any source on stop/error. gop.rs only extracts default2/clamp1..16 helper; lane choice/claims/floors unchanged. Focused check PASS4.67s; scheduler first10pass/1fail9.03s (new mock incorrectly panicked on expected cleanup cancellation), corrected fixture then11/11PASS8.84s; GOP62/62PASS7.98s plus exact isolated child1/1. Decoder1 returned sample220 while decoder0 was gated at64; shared limits, aliases/dedup, cancellation/error/drop/session cleanup and MJPEG/H264 in-memory byte identity passed. fmt/diff-check PASS; original GOP/counter/GPU tests and five unrelated scheduler tests unchanged. Tasks2.3/2.4, mandatory Opus review, gates/commit/exports/acceptance remain pending; Stage2 NOT finished, no measured speed/waiting or results/log row. Details/method/accounting: results/progress/03-stage-2.md; exact reviewed brief already present at briefs/03-stage2-implementation.md.

- Stage1 final measurement (2026-11-06): actual short GPU Opus APPROVE before separately authorized one-line expectation4→8 commit `26a60d6fa9d5e54f24cbdab2e210c4cb2304aeff` (parent WIP31546c98cd4301372a7a2b9178e98362b4c794f5); adjacent stale comment(4) retained/nonblocking per supervisor; original four findings fixed/re-review APPROVE and counter isolation APPROVE. Gates reused unchanged (430 main+5 child pass,3ignored; prior four known media/engine oracles), no repeats. One release build196.978539s, one30s cooldown, exactly two sequential exports on full tested SHA/dirty=false: quick GPUoff150.132421867s/3600frames/cmp0; bench-A GPUauto99.690641674s/7200frames/PSNR7200lines,min32.49dB@n5828,ffmpeg0,ffprobe2x0,video/audio/container120s,AAC48000Hz5626frames,AVdiff0. No offline/errors. Bench-A renderTask522063.662ms,queue157399.449ms=covered27769.474+notcovered129629.975;behindPrefetch142650.491ms overlaps;decodeWait3009.101ms;no additional lane-wait;total160.408550s<213.9.99.690641674<108.9s and notcovered/render24.830300%>=10% => **C**, supervisor confirmation pending. Gain8.456711% is5–10% confirmation case; report only,no additional export or next stage. Lanes created/absolute peak/released quick13/2/13,bench-A24/2/24,live peak8 each; final teardown gauge unavailable in saved JSON, existing leak tests reused. Tasks1.7/1.8 measured/recorded; no merge or extra commit. Exact paths/commands/reviews/counters/accounting and subsequent owned-output deletion confirmation: results/progress/03-stage-1.md; two exact runJSON and two log rows retained.

- Stage 1 GPU expectation correction (2026-11-06): accepted-tag actual8-panic failure plus existing Pipeline::new pool clamp(2,8) authorizes ONLY tests.rs:454 upper expectation4→8; all other bytes unchanged, no production change. WIPbase31546c98cd4301372a7a2b9178e98362b4c794f5, perf/03-lanes, exactly one dirty file. Four gates PASS: fmt4.634205s, export Clippy9.352787s, full codecs/export tests36.435149s (430 top-level pass/0fail/3ignored; isolated counter child1 and renderer children4 pass separately), first Stage1 layers0.194619s; git diff --check PASS. Prior workspace/codecs Clippy and media/engine results with four known oracle failures retained without rerun. Patch briefs/03-stage1-gpu-expectation.patch826bytes, SHA2564c993f965b226f42406f8a26725bdb677c76d73d14594ff7625baad4ba9292e9; one insertion/one deletion. No commit: short Opus review of new diff then separate validated commit/exports task pending. Stage1 unfinished;1.7/1.8 unchecked; no results/log row, A/B/C or release/exports. Gates50.616760s, idle0, active command none; own TMPDIR removed/shared target preserved. Evidence/accounting: results/progress/03-stage-1.md; approved brief and bounded captures saved.

- Stage 1 warm-tag proof (2026-11-06): supervisor-authorized existing-checkout method supersedes prior temporary-worktree blocker and green-before-WIP assumption. Exactly nine authorized paths committed as `wip: stage 1 lanes (pre-export)`, full WIP `31546c98cd4301372a7a2b9178e98362b4c794f5`. Detached accepted tag `fast-export-2026-11-06`, full HEAD `ae807b569a0db327e6d0bd4490bfb96afb6653c9`, clean. Exactly once foreground/shared warm target: `timeout 10m cargo test -p filmcraft-export gpu_hook_scenarios --no-fail-fast`; exit101, 0 passed/1 failed/0 ignored,158 filtered, body0.02s, command16.032616s; nonmatching integration target0 tests/2 filtered. First intentional panic `GPU renderer failure (test)` at tests.rs:501:9, eight occurrences; root failing assertion tests.rs:454:5 `renderer reused after a panic: 8`. Same relevant TEST FAILURE, not compilation/infrastructure/timeout/zero-test failure: selects supervisor expectation-only fix branch; no code/test expectation change or second commit yet. Finally returned to perf/03-lanes at exact WIP HEAD, clean. Exclusive TMPDIR fixture respects std::env::temp_dir; sampled every250ms, final/peak47070 bytes, removed after recording; output3332 bytes continuously drained/last256KiB bounded. Shared target retained, no override/profile change, no additional gates/build/export/delegation. Stage1 unfinished;1.7/1.8 unchecked; no A/B/C or results/log row. Evidence: results/progress/03-stage1-warm-tag-gpu.log; approved brief: briefs/03-stage1-warm-tag-test.md; accounting and four-step record: results/progress/03-stage-1.md.

- Stage 1 narrow resume (2026-11-06): supplied short Opus5.5 medium re-review APPROVE for reviewed deltas only; original four findings accepted/fixed, mechanical decode_until tuple and subprocess isolation accepted. Low nonblocking hardcoded --exact future-rename/zero-test risk recorded; orchestrator declines an extra hypothetical guard because current child explicitly ran1/pass. No GPU gate/Stage1/commit acceptance. Affected checks only: timeout5m cargo fmt --check PASS4.552502s; timeout10m cargo clippy -p filmcraft-codecs --all-targets -- -D warnings PASS5.005429s; no test repeated. Counter-agent parent default-parallel62/62 plus exact isolated child1/1 retained; exact created/released==2, full concurrent/cleanup body and49 original GOP tests unchanged. Same nine dirty files/full HEAD dc66a9468aa59dd3708c154aa172da6218ee9774, all nine source SHA256 unchanged during resume. Required tag GPU baseline NEVER RAN: cross-project task rejected even with current environment; provisioning errors “A local task runs in this thread’s project” and “Unable to resolve the parent checkout commit”; parent projectroot /media/bruno/NVMe 500/Current Project/IA LATAM/276 News Nov 01/emerg is media, not Git. No temporary worktree/target or baseline test; unused temporary T3 filmcraft-perf project registration removed without deleting files, no projectroot changed. No platform-authorized shell checkout fallback/new top-level thread. Supervisor requires actual same-failure tag evidence before GPU expectation versus Stage1-code decision; no inherited/causal/allowed-failure assumption or GPU expectation/code fix authorized yet. Full gates/layers/commit/release/two exports/A-B-C deferred; Stage1 unfinished,1.7/1.8 unchecked. No results/log row. Details and measured accounting: results/progress/03-stage-1.md; approved briefs saved in briefs/03-stage1-resume-records.md and briefs/03-stage1-opus-rereview.md.


- Stage 1 counter isolation (2026-11-06): only the new cut-in test wrapper changed (+31 lines); current test executable runs only that exact test with FILMCRAFT_LANES_COUNTER_TEST_CHILD=1, inherited output, 15 s Instant watchdog, try_wait and <=10 ms sleep; timeout/wait error kills and reaps before failure, nonzero child exit fails parent. Entire original concurrent body and exact created/released==2, factories==2, timing partition, release120 and private-pool cleanup assertions unchanged; all49 original GOP tests byte-identical, other eight dirty files unchanged. cargo fmt --all PASS 4.534543s; timeout 5m cargo test --no-fail-fast -p filmcraft-codecs gop::tests PASS 6.410551s: parent62/62 (2.06s), isolated child1/1 (0.01s), counts separate; parent default parallel scheduling, shared warm target. git diff --check PASS. Prior >=2 proposal rejected by supervisor. GPU test untouched; no tag reproduction ran: target.projectId rejected “A local task runs in this thread’s project”, omission rejected “Unable to resolve the parent checkout commit”; parent project is a real media directory, not Git. Only reproduction of the SAME >4 panic-count failure at fast-export-2026-11-06 permits GPU expectation change; otherwise Stage1-code investigation, with PASS alone not proof of causality and different/build/infrastructure failure inconclusive. No inherited-failure or allowance claim authorized. Approved tag brief saved, not executed; no shell provisioning. Original full gate1447pass/6fail/6ignored preserved. Original Opus CHANGES four fixes retained; short final re-review waits on baseline blocker. No full gates/release/export/commit; Stage1 unfinished,1.7/1.8 unchecked. Details: results/progress/03-stage-1.md; approved briefs: briefs/03-stage1-counter-isolation.md and briefs/03-stage1-tag-gpu.md.


- Stage 1 Clippy continuation (2026-11-06): private decode_until lane/state tuple fix only; fmt repeat PASS 4.690882 s, Clippy repeat PASS 28.048778 s. First full touched-crate tests FAIL (exit 101, 71.627692 s): 1,447 top-level passed / 6 failed / 6 ignored, plus four passing nested subprocess executions. Two unexpected failures: gop::tests::lanes_cut_in_finishes_before_blocked_prefetch_and_same_gop_continues_once (65 != 2 at gop/tests.rs:1968), export tests::gpu_hook_scenarios (renderer reused after a panic: 8 at tests.rs:454); four known PLAN-01 oracle failures also present. Stopped without reruns/fixes, layers, diff-check/commit, release or exports. No A/B/C decision or measured rejection. HEAD dc66a9468aa59dd3708c154aa172da6218ee9774, same nine dirty Stage1 files. Opus CHANGES all four accepted/fixed; no second review. Tasks 1.7/1.8 remain unchecked; acceptance pending. Details: results/progress/03-stage-1.md; captures: results/progress/03-stage1-clippy-captures/; approved continuation brief saved.

- Stage 1 validation (2026-11-06), tasks 1.7–1.8 partial: fmt PASS (4.600374 s); workspace Clippy FAIL (exit 101, 3.402994 s), unexpected `too_many_arguments` in `decode_until`, `crates/codecs/src/gop.rs:1744` (8/7). Stopped without code edits, tests/layers/release/exports or commit; no A/B/C decision, no measured rejection. HEAD remains dc66a9468aa59dd3708c154aa172da6218ee9774, same nine modified Stage1 files. Opus CHANGES findings remain accepted/fixed per handoff; no second review claimed. 1.7/1.8 stay unchecked; acceptance pending. Details and bounded gate tails: results/progress/03-stage-1.md and results/progress/03-stage1-validation-captures/; approved validation brief saved.

- Stage 1 implementation (2026-11-06): 1.1–1.6 corrected on uncommitted perf/03-lanes from dc66a946. Mandatory Opus medium review completed **CHANGES**; all four dispositions recorded in results/progress/03-stage-1.md: active export-run error tickets/cancel retry, scheduler passed-use retirement, stale idle-primary pending retirement with needed delayed output preserved, and export-only any-source fallback/maintenance suppression. Final working checks: cargo check passed; GOP 62/62 (49 original tests byte-identical), scheduler 10/10; cargo fmt --all and git diff --check passed. Stage 1 remains unfinished: gates, final commit and two benchmark exports still pending; 1.7/1.8 unchecked. Stage 0 GO supervisor-confirmed.

- Stage 0 diagnosis (2026-11-06), source commit `dc66a9468aa59dd3708c154aa172da6218ee9774` on clean `perf/03-lanes`;
  archived CLI `builds/03-0-dc66a946/filmcraft-cli`. Mandatory Opus medium review
  **APPROVE** preceded the commit; no fixes requested or made. Existing gates reused, including
  the known pre-existing engine APV ffprobe failure (1177 pass, 6 ignored, 1 known failure).
  One release build and one 30 s cooldown, then exactly one quick GPU Auto diagnosis, command
  exit 0, no offline media; [run](results/runs/2026-11-06T06-42-34--03-0-diagnosis--bench-quick-1min--gpu-auto.json).
  `export.renderTasks.renderTaskMs` 307494.323 ms; `decoderQueueMs` 109102.985 ms
  (35.481301% of render tasks). Covered 28447.292 ms
  (26.073798% of queue / 9.251323% of render tasks);
  not-covered 80655.693 ms (73.926202% / 26.229978%);
  behind-prefetch 95659.421 ms (87.678097% / 31.109329%, overlaps both parts).
  Partition residual 0.000 ms (tolerance 0.002 ms). Formula:
  not-covered/queue >= 0.50 AND queue/renderTaskMs >= 0.20 -> **provisional GO**;
  supervisor confirmation remains pending. Opus caveat: intra-only/spare-decoder-return waits
  count as not-covered but would not benefit from another lane; multicam work outside render
  tasks remains outside existing timing. Existing concurrent-test 2 s timeout left unchanged.
  Output deleted by runner, confirmed by scoped before/after pathnames even with output:null.
  Export elapsed time is provenance only; no speed comparison, cmp, PSNR or Stage 1 work.

- Starting state (2026-11-06): `perf/export-amd` = `ae807b56`, local tag
  `fast-export-2026-11-06`, CLI in `builds/02-4-ae807b56/`.
- Starting numbers (bench-A `gpu-auto`):
  - export 108.930 s;
  - render-task time 546.5 s, of which `decoderQueueMs` 212.3 s (38.9%);
  - GPU readback 222.5 s;
  - encode 64.5 s, encoder waiting 37.2 s;
  - PSNR minimum 32.49 dB at frame 5828.
- The rejected waiting-line fix is kept for reference as `d06a8cdd` on `perf/01-decoder-queue`.
