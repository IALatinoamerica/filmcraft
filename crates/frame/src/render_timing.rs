//! Process-wide export frame-task wall timers. Only `task` marks a render origin; scheduler
//! prefetch never enters it. Stages replace, rather than nest inside, the current interval.
//! Row-parallel work is timed at its synchronous caller (including its join), never per row.
//! If Rayon re-enters another frame task on this stack, that task suspends the parent's clock:
//! the same stack time is not counted twice. These are task wall times, not CPU utilisation.

use std::cell::Cell;
use std::marker::PhantomData;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use web_time::Instant;

/// Mutually exclusive parts of a render frame task.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    DecoderQueue,
    DecodeWait,
    FrameMapWait,
    Composite,
    Effects,
    InputConvert,
    OutputConvert,
    Other,
}

const PHASES: [Phase; 6] = [Phase::DecoderQueue, Phase::DecodeWait, Phase::FrameMapWait, Phase::Composite, Phase::Effects, Phase::Other];
static TOTAL: AtomicU64 = AtomicU64::new(0);
static PARTS: [AtomicU64; 6] = [const { AtomicU64::new(0) }; 6];
static CONVERT: AtomicU64 = AtomicU64::new(0);
static GPU_INPUT_CONVERT: AtomicU64 = AtomicU64::new(0);
static GPU_OUTPUT_CONVERT: AtomicU64 = AtomicU64::new(0);
static CPU_INPUT_CONVERT: AtomicU64 = AtomicU64::new(0);
static CPU_OUTPUT_CONVERT: AtomicU64 = AtomicU64::new(0);
static DECODE_RUN: AtomicU64 = AtomicU64::new(0);
static QUEUE_COVERED: AtomicU64 = AtomicU64::new(0);
static QUEUE_NOT_COVERED: AtomicU64 = AtomicU64::new(0);
static QUEUE_PREFETCH: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
struct QueueWait {
    covered: bool,
    behind_prefetch: bool,
}

#[derive(Clone, Copy)]
struct Clock {
    phase: Phase,
    since: Instant,
    ns: [u64; 8],
    gpu_frame: bool,
    queue_wait: QueueWait,
    queue_ns: [u64; 3],
}

impl Clock {
    fn settle(&mut self, now: Instant) {
        let ns = nanos(now.saturating_duration_since(self.since));
        if let Some(part) = self.ns.get_mut(self.phase as usize) {
            let before = *part;
            *part = part.saturating_add(ns);
            if self.phase == Phase::DecoderQueue {
                // Use precisely the time settled to the existing phase, including saturation.
                let elapsed = *part - before;
                let partition = usize::from(!self.queue_wait.covered);
                self.queue_ns[partition] = self.queue_ns[partition].saturating_add(elapsed);
                if self.queue_wait.behind_prefetch {
                    self.queue_ns[2] = self.queue_ns[2].saturating_add(elapsed);
                }
            }
        }
        self.since = now;
    }
}

thread_local! {
    static CLOCK: Cell<Option<Clock>> = const { Cell::new(None) };
}

fn nanos(d: web_time::Duration) -> u64 {
    d.as_nanos().min(u128::from(u64::MAX)) as u64
}

fn add(counter: &AtomicU64, ns: u64) {
    let _ = counter.fetch_update(Relaxed, Relaxed, |current| Some(current.saturating_add(ns)));
}

/// Whether the caller belongs to a render frame, rather than prefetch or playback.
pub fn active() -> bool {
    CLOCK.with(|c| c.get().is_some())
}

/// Attribute the current innermost task's conversions to a GPU frame.
pub fn mark_gpu_frame() {
    CLOCK.with(|c| {
        if let Some(mut clock) = c.get() {
            clock.gpu_frame = true;
            c.set(Some(clock));
        }
    });
}

/// A stage scope, confined to its calling thread; inactive outside render tasks.
pub struct Interval {
    previous: Option<(Phase, QueueWait)>,
    _thread: PhantomData<Rc<()>>,
}

/// Switch intervals, restoring the previous interval even on early return or unwind.
pub fn enter(phase: Phase) -> Interval {
    let previous = CLOCK.with(|c| {
        let mut clock = c.get()?;
        clock.settle(Instant::now());
        let previous = (clock.phase, clock.queue_wait);
        clock.phase = phase;
        clock.queue_wait = QueueWait::default();
        c.set(Some(clock));
        Some(previous)
    });
    Interval { previous, _thread: PhantomData }
}

/// Classify acquisition using the holder's first coherent sample. The classification stays
/// fixed for this wait even if the holder changes; this is intentionally an approximation.
/// Like every interval this is inactive outside a render task.
pub fn decoder_queue(covered: bool, behind_prefetch: bool) -> Interval {
    let interval = enter(Phase::DecoderQueue);
    CLOCK.with(|c| {
        if let Some(mut clock) = c.get() {
            clock.queue_wait = QueueWait { covered, behind_prefetch };
            c.set(Some(clock));
        }
    });
    interval
}

/// Placement/blending invoked by an effect belongs to the effect, not compositing.
pub fn composite() -> Interval {
    if CLOCK.with(|c| c.get().is_some_and(|clock| clock.phase == Phase::Other)) {
        enter(Phase::Composite)
    } else {
        Interval { previous: None, _thread: PhantomData }
    }
}

impl Drop for Interval {
    fn drop(&mut self) {
        if let Some((previous, queue_wait)) = self.previous {
            CLOCK.with(|c| {
                if let Some(mut clock) = c.get() {
                    clock.settle(Instant::now());
                    clock.phase = previous;
                    clock.queue_wait = queue_wait;
                    c.set(Some(clock));
                }
            });
        }
    }
}

struct Task(Option<Clock>);

impl Drop for Task {
    fn drop(&mut self) {
        CLOCK.with(|c| {
            let now = Instant::now();
            if let Some(mut clock) = c.take() {
                clock.settle(now);
                add(&TOTAL, clock.ns.iter().copied().fold(0, u64::saturating_add));
                for (phase, counter) in PHASES.into_iter().zip(PARTS.iter()) {
                    add(counter, clock.ns.get(phase as usize).copied().unwrap_or(0));
                }
                let input = clock.ns.get(Phase::InputConvert as usize).copied().unwrap_or(0);
                let output = clock.ns.get(Phase::OutputConvert as usize).copied().unwrap_or(0);
                for (counter, ns) in [&QUEUE_COVERED, &QUEUE_NOT_COVERED, &QUEUE_PREFETCH].into_iter().zip(clock.queue_ns) {
                    add(counter, ns);
                }
                add(&CONVERT, input.saturating_add(output));
                let (input_counter, output_counter) =
                    if clock.gpu_frame { (&GPU_INPUT_CONVERT, &GPU_OUTPUT_CONVERT) } else { (&CPU_INPUT_CONVERT, &CPU_OUTPUT_CONVERT) };
                add(input_counter, input);
                add(output_counter, output);
            }
            c.set(self.0.take().map(|mut parent| {
                parent.since = now;
                parent
            }));
        });
    }
}

/// Time one frame task, including encoder preparation. No atomics in pixel loops; the task
/// publishes its local totals once, after all synchronous nested Rayon work has joined.
pub fn task<T>(f: impl FnOnce() -> T) -> T {
    let now = Instant::now();
    let parent = CLOCK.with(|c| {
        let parent = c.take().map(|mut clock| {
            clock.settle(now);
            clock
        });
        c.set(Some(Clock { phase: Phase::Other, since: now, ns: [0; 8], gpu_frame: false, queue_wait: QueueWait::default(), queue_ns: [0; 3] }));
        parent
    });
    let _task = Task(parent);
    f()
}

/// Own decode execution on `decode-N` (or the existing inline fallback). This overlaps the
/// caller's DecodeWait and is deliberately not a part of the exclusive frame-task sum.
pub fn decode_run<T>(render_origin: bool, f: impl FnOnce() -> T) -> T {
    struct Run(Option<Instant>);
    impl Drop for Run {
        fn drop(&mut self) {
            if let Some(start) = self.0 {
                add(&DECODE_RUN, nanos(start.elapsed()));
            }
        }
    }
    struct Restore(Option<Clock>);
    impl Drop for Restore {
        fn drop(&mut self) {
            CLOCK.with(|c| c.set(self.0.take()));
        }
    }
    let _restore = Restore(CLOCK.with(Cell::take));
    let _run = Run(render_origin.then(Instant::now));
    f()
}

/// Cumulative nanoseconds; diff snapshots after work completes (publication is per task).
pub fn times() -> Vec<(&'static str, u64)> {
    let mut times = vec![("renderTaskMs", TOTAL.load(Relaxed)), ("decodeRunMs", DECODE_RUN.load(Relaxed))];
    for (phase, counter) in PHASES.into_iter().zip(PARTS.iter()) {
        let name = match phase {
            Phase::DecoderQueue => "decoderQueueMs",
            Phase::DecodeWait => "decodeWaitMs",
            Phase::FrameMapWait => "frameMapWaitMs",
            Phase::Composite => "compositeMs",
            Phase::Effects => "effectsMs",
            Phase::InputConvert | Phase::OutputConvert => continue,
            Phase::Other => "otherMs",
        };
        times.push((name, counter.load(Relaxed)));
    }
    times.extend([
        ("decoderQueueCoveredMs", QUEUE_COVERED.load(Relaxed)),
        ("decoderQueueNotCoveredMs", QUEUE_NOT_COVERED.load(Relaxed)),
        ("decoderQueueBehindPrefetchMs", QUEUE_PREFETCH.load(Relaxed)),
        ("convertMs", CONVERT.load(Relaxed)),
        ("gpuInputConvertMs", GPU_INPUT_CONVERT.load(Relaxed)),
        ("gpuOutputConvertMs", GPU_OUTPUT_CONVERT.load(Relaxed)),
        ("cpuInputConvertMs", CPU_INPUT_CONVERT.load(Relaxed)),
        ("cpuOutputConvertMs", CPU_OUTPUT_CONVERT.load(Relaxed)),
    ]);
    times
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decoder_queue_partitions_settled_time_and_restores_first_sample() {
        let snapshot = || [QUEUE_COVERED.load(Relaxed), QUEUE_NOT_COVERED.load(Relaxed), QUEUE_PREFETCH.load(Relaxed)];
        let before = times();
        let idle = snapshot();
        drop(decoder_queue(true, true));
        assert_eq!(snapshot(), idle, "prefetch/playback without a render clock publishes no wait");
        assert!(!active());
        task(|| {
            let _queue = decoder_queue(true, true);
            let parent = CLOCK.with(|c| c.get().unwrap());
            task(|| {
                let _other_queue = decoder_queue(false, false);
                CLOCK.with(|c| {
                    let mut clock = c.get().unwrap();
                    clock.settle(clock.since + web_time::Duration::from_millis(1));
                    assert_eq!(clock.queue_ns[0], 0);
                    assert_eq!(clock.queue_ns[1], clock.ns[Phase::DecoderQueue as usize]);
                    assert_eq!(clock.queue_ns[2], 0);
                    c.set(Some(clock));
                });
            });
            assert_eq!(CLOCK.with(|c| c.get().unwrap().queue_wait), parent.queue_wait);
            // A phase nested inside the wait and an isolated decode restore its initial sample.
            drop(enter(Phase::FrameMapWait));
            decode_run(true, || {
                assert!(!active());
                let before = snapshot();
                drop(decoder_queue(false, false));
                assert_eq!(snapshot(), before, "decode execution has no render clock");
            });
            assert_eq!(CLOCK.with(|c| c.get().unwrap().queue_wait), parent.queue_wait);
            {
                let _queue = decoder_queue(false, true);
                drop(enter(Phase::Other));
            }
            CLOCK.with(|c| {
                let mut clock = c.get().unwrap();
                clock.settle(Instant::now());
                assert_eq!(clock.queue_ns[0].saturating_add(clock.queue_ns[1]), clock.ns[Phase::DecoderQueue as usize]);
                assert_eq!(clock.queue_ns[2], clock.ns[Phase::DecoderQueue as usize]);
                // Once the existing phase saturates, no classification adds further time.
                clock.ns[Phase::DecoderQueue as usize] = u64::MAX;
                let parts = clock.queue_ns;
                clock.settle(clock.since + web_time::Duration::from_millis(1));
                assert_eq!(clock.queue_ns, parts);
            });
        });
        let after = times();
        let keys = ["decoderQueueMs", "decoderQueueCoveredMs", "decoderQueueNotCoveredMs", "decoderQueueBehindPrefetchMs"];
        let delta = keys.map(|key| {
            let get = |snapshot: &[(&str, u64)]| snapshot.iter().find(|(name, _)| *name == key).unwrap().1;
            assert!(get(&after) >= get(&before), "{key} must be cumulative");
            get(&after) - get(&before)
        });
        assert_eq!(delta[0], delta[1] + delta[2], "completed-task publication preserves the exact partition");
        assert!(delta[2] >= 1_000_000);
        assert!(delta[3] <= delta[0]);
        // Verify first-sample restoration through unwind without publishing a second task.
        task(|| {
            let _queue = decoder_queue(true, false);
            let result = std::panic::catch_unwind(|| {
                let _inner = decoder_queue(false, true);
                panic!("queue unwind");
            });
            assert!(result.is_err());
            assert_eq!(CLOCK.with(|c| c.get().unwrap().queue_wait), QueueWait { covered: true, behind_prefetch: false });
        });
        assert!(!active());
    }

    #[test]
    fn conversion_times_are_attributed_to_gpu_and_cpu_tasks() {
        fn snapshot() -> [u64; 4] {
            let times = times();
            ["gpuInputConvertMs", "gpuOutputConvertMs", "cpuInputConvertMs", "cpuOutputConvertMs"]
                .map(|name| times.iter().find(|(key, _)| *key == name).unwrap().1)
        }

        let convert = || {
            {
                let _input = enter(Phase::InputConvert);
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            {
                let _output = enter(Phase::OutputConvert);
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        };
        mark_gpu_frame();
        assert!(!active());
        let before = snapshot();
        task(|| {
            convert();
            mark_gpu_frame();
        });
        let after_gpu = snapshot();
        assert!(after_gpu[0].saturating_sub(before[0]) >= 1_000_000);
        assert!(after_gpu[1].saturating_sub(before[1]) >= 1_000_000);
        task(convert);
        let after_cpu = snapshot();
        assert!(after_cpu[2].saturating_sub(after_gpu[2]) >= 1_000_000);
        assert!(after_cpu[3].saturating_sub(after_gpu[3]) >= 1_000_000);
        assert_eq!(after_cpu[0], after_gpu[0]);
        assert_eq!(after_cpu[1], after_gpu[1]);
    }

    #[test]
    fn nested_task_suspends_and_restores_parent_and_intervals() {
        assert!(!active());
        task(|| {
            let _effects = enter(Phase::Effects);
            let parent = CLOCK.with(|c| c.get().unwrap());
            task(|| {
                assert_eq!(CLOCK.with(|c| c.get().unwrap().phase), Phase::Other);
                let _convert = enter(Phase::InputConvert);
            });
            let restored = CLOCK.with(|c| c.get().unwrap());
            assert_eq!(restored.phase, Phase::Effects);
            assert!(restored.since >= parent.since);
            let _blend = composite();
            assert_eq!(CLOCK.with(|c| c.get().unwrap().phase), Phase::Effects);
        });
        assert!(!active());
    }

    #[test]
    fn unwind_restores_context_and_completed_snapshot_is_additive() {
        let r = std::panic::catch_unwind(|| {
            task(|| {
                let _convert = enter(Phase::OutputConvert);
                panic!("test unwind");
            })
        });
        assert!(r.is_err());
        assert!(!active());
        // Local accounting is exact independently of concurrent process-wide test snapshots.
        task(|| {
            let _wait = enter(Phase::DecodeWait);
            drop(enter(Phase::FrameMapWait));
            CLOCK.with(|c| {
                let mut clock = c.get().unwrap();
                let end = Instant::now();
                let start_ns: u64 = clock.ns.iter().sum();
                let tail = nanos(end.saturating_duration_since(clock.since));
                clock.settle(end);
                assert_eq!(clock.ns.iter().sum::<u64>(), start_ns + tail);
            });
        });
    }
}
