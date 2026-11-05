use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::Duration;

/// Why an entire frame was rendered on the CPU.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CpuWholeFrame {
    Adjustment,
    Transition,
    Hdr,
    PlanExhausted,
}

static SUBMIT: AtomicU64 = AtomicU64::new(0);
static MAP_WAIT: AtomicU64 = AtomicU64::new(0);
static READBACK_CONVERT: AtomicU64 = AtomicU64::new(0);
static UPLOAD_PREP: AtomicU64 = AtomicU64::new(0);
static ADJUSTMENT: AtomicU64 = AtomicU64::new(0);
static TRANSITION: AtomicU64 = AtomicU64::new(0);
static HDR: AtomicU64 = AtomicU64::new(0);
static PLAN_EXHAUSTED: AtomicU64 = AtomicU64::new(0);

fn add(counter: &AtomicU64, value: u64) {
    let _ = counter.fetch_update(Relaxed, Relaxed, |current| Some(current.saturating_add(value)));
}

fn nanos(d: Duration) -> u64 {
    d.as_nanos().min(u128::from(u64::MAX)) as u64
}

/// Count one whole-frame CPU decision for the given reason.
pub fn note_cpu_whole_frame(reason: CpuWholeFrame) {
    let counter = match reason {
        CpuWholeFrame::Adjustment => &ADJUSTMENT,
        CpuWholeFrame::Transition => &TRANSITION,
        CpuWholeFrame::Hdr => &HDR,
        CpuWholeFrame::PlanExhausted => &PLAN_EXHAUSTED,
    };
    add(counter, 1);
}

/// Accumulate GPU submission wall time.
pub fn add_submit(d: Duration) {
    add(&SUBMIT, nanos(d));
}

/// Accumulate GPU readback mapping wait time.
pub fn add_map_wait(d: Duration) {
    add(&MAP_WAIT, nanos(d));
}

/// Accumulate CPU conversion time after GPU readback.
pub fn add_readback_convert(d: Duration) {
    add(&READBACK_CONVERT, nanos(d));
}

/// Accumulate CPU source preparation time before GPU upload.
pub fn add_upload_prep(d: Duration) {
    add(&UPLOAD_PREP, nanos(d));
}

/// Cumulative process-wide GPU timing and whole-frame CPU decision counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct GpuStatsSnapshot {
    pub submit_ns: u64,
    pub map_wait_ns: u64,
    pub readback_convert_ns: u64,
    pub upload_prep_ns: u64,
    pub adjustment: u64,
    pub transition: u64,
    pub hdr: u64,
    pub plan_exhausted: u64,
}

/// Read cumulative counters without resetting them.
pub fn snapshot() -> GpuStatsSnapshot {
    GpuStatsSnapshot {
        submit_ns: SUBMIT.load(Relaxed),
        map_wait_ns: MAP_WAIT.load(Relaxed),
        readback_convert_ns: READBACK_CONVERT.load(Relaxed),
        upload_prep_ns: UPLOAD_PREP.load(Relaxed),
        adjustment: ADJUSTMENT.load(Relaxed),
        transition: TRANSITION.load(Relaxed),
        hdr: HDR.load(Relaxed),
        plan_exhausted: PLAN_EXHAUSTED.load(Relaxed),
    }
}
