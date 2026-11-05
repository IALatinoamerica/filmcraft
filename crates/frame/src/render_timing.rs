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
    FrameWait,
    DecodeWait,
    FrameMapWait,
    Composite,
    Effects,
    Convert,
    Other,
}

const PHASES: [Phase; 8] =
    [Phase::DecoderQueue, Phase::FrameWait, Phase::DecodeWait, Phase::FrameMapWait, Phase::Composite, Phase::Effects, Phase::Convert, Phase::Other];
static TOTAL: AtomicU64 = AtomicU64::new(0);
static PARTS: [AtomicU64; 8] = [const { AtomicU64::new(0) }; 8];
static DECODE_RUN: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy)]
struct Clock {
    phase: Phase,
    since: Instant,
    ns: [u64; 8],
}

impl Clock {
    fn settle(&mut self, now: Instant) {
        let ns = nanos(now.saturating_duration_since(self.since));
        if let Some(part) = self.ns.get_mut(self.phase as usize) {
            *part = part.saturating_add(ns);
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

/// Whether the caller belongs to a render frame, rather than prefetch or playback.
pub fn active() -> bool {
    CLOCK.with(|c| c.get().is_some())
}

/// A stage scope, confined to its calling thread; inactive outside render tasks.
pub struct Interval {
    previous: Option<Phase>,
    _thread: PhantomData<Rc<()>>,
}

/// Switch intervals, restoring the previous interval even on early return or unwind.
pub fn enter(phase: Phase) -> Interval {
    let previous = CLOCK.with(|c| {
        let mut clock = c.get()?;
        clock.settle(Instant::now());
        let previous = clock.phase;
        clock.phase = phase;
        c.set(Some(clock));
        Some(previous)
    });
    Interval { previous, _thread: PhantomData }
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
        if let Some(previous) = self.previous {
            CLOCK.with(|c| {
                if let Some(mut clock) = c.get() {
                    clock.settle(Instant::now());
                    clock.phase = previous;
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
                TOTAL.fetch_add(clock.ns.iter().copied().sum::<u64>(), Relaxed);
                for (counter, ns) in PARTS.iter().zip(clock.ns) {
                    counter.fetch_add(ns, Relaxed);
                }
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
        c.set(Some(Clock { phase: Phase::Other, since: now, ns: [0; 8] }));
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
                DECODE_RUN.fetch_add(nanos(start.elapsed()), Relaxed);
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
            Phase::FrameWait => "frameWaitMs",
            Phase::DecodeWait => "decodeWaitMs",
            Phase::FrameMapWait => "frameMapWaitMs",
            Phase::Composite => "compositeMs",
            Phase::Effects => "effectsMs",
            Phase::Convert => "convertMs",
            Phase::Other => "otherMs",
        };
        times.push((name, counter.load(Relaxed)));
    }
    times
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_task_suspends_and_restores_parent_and_intervals() {
        assert!(!active());
        task(|| {
            let _effects = enter(Phase::Effects);
            let parent = CLOCK.with(|c| c.get().unwrap());
            task(|| {
                assert_eq!(CLOCK.with(|c| c.get().unwrap().phase), Phase::Other);
                let _convert = enter(Phase::Convert);
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
                let _convert = enter(Phase::Convert);
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
