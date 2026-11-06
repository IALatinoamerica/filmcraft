//! GOP-aware random access shared by the container sources (MP4/MOV, Matroska/WebM).
//!
//! Seeking: find the sample whose presentation interval covers the requested time, decode forward
//! from the preceding sync sample, and cache every decoded frame (keyed by pts). Playback requests
//! for the next frames therefore hit the cache or continue the running decoder without re-seeking.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, TryLockError, Weak};
use std::time::Duration;

use web_time::Instant;

use filmcraft_color::ColorInfo;
use filmcraft_frame::{Region, VideoFrame};

use crate::CodecError;
use crate::video::VideoDecoder;

/// Configured export decode lanes per source (default two, bounded to 1–16).
pub fn decode_lane_limit() -> usize {
    std::env::var("FILMCRAFT_DECODE_LANES").ok().and_then(|s| s.parse::<usize>().ok()).unwrap_or(2).clamp(1, 16)
}

/// Process-wide decode counters of every [`GopCache`] (benchmarks and diagnostics).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GopStats {
    /// Requests answered from the decoded-frame cache.
    pub hits: u64,
    /// Requests that had to decode.
    pub misses: u64,
    /// Decoder restarts at a sync sample (seeks).
    pub seeks: u64,
    /// Samples fed to decoders.
    pub decoded: u64,
    /// Decoded frames evicted from the cache.
    pub evicted: u64,
    /// Wall time spent inside decoders (`decode` / `flush`), nanoseconds.
    pub decode_ns: u64,
    /// Non-reference samples left out while catching up (frames already late).
    pub skipped: u64,
    /// Frames decoded in draft mode (reduced-resolution playback, [`VideoDecoder::set_draft`]).
    pub draft: u64,
    /// Pictures decoders output (hardware and software; [`crate::hw::hw_stats`] counts the
    /// hardware ones).
    pub frames: u64,
    /// Decoded pictures returned to callers at least once.
    pub used_frames: u64,
    /// Restarts after this cache's decoder was released and recreated.
    pub seeks_by_decoder_recycled: u64,
    /// Restarts for previously decoded, evicted pictures.
    pub seeks_by_evicted_frame: u64,
    /// Restarts for samples behind the running decoder.
    pub seeks_by_out_of_order: u64,
    /// Restarts for a cache's first request or a far forward jump.
    pub seeks_by_cut_in: u64,
    /// Restarts not covered by the other seek causes.
    pub seeks_by_other: u64,
    /// Wall time waiting for request-path cache state locks, nanoseconds.
    pub lock_wait_ns: u64,
    /// Lane sessions created (including primary sessions).
    pub lanes_created: u64,
    /// Highest simultaneous lane count for one source (absolute peak).
    pub lanes_peak_per_source: u64,
    /// Lane sessions retired.
    pub lanes_released: u64,
}

impl GopStats {
    /// Share of requests answered from the decoded-frame cache (0 when there were none).
    pub fn hit_rate(&self) -> f64 {
        let n = self.hits + self.misses;
        if n == 0 { 0.0 } else { self.hits as f64 / n as f64 }
    }

    /// Mean decoder wall time per sample fed (ms).
    pub fn decode_ms_per_sample(&self) -> f64 {
        if self.decoded == 0 { 0.0 } else { self.decode_ns as f64 / 1e6 / self.decoded as f64 }
    }
}

static HITS: AtomicU64 = AtomicU64::new(0);
static MISSES: AtomicU64 = AtomicU64::new(0);
static SEEKS: AtomicU64 = AtomicU64::new(0);
static DECODED: AtomicU64 = AtomicU64::new(0);
static EVICTED: AtomicU64 = AtomicU64::new(0);
static DECODE_NS: AtomicU64 = AtomicU64::new(0);
static SKIPPED: AtomicU64 = AtomicU64::new(0);
static DRAFT: AtomicU64 = AtomicU64::new(0);
static FRAMES: AtomicU64 = AtomicU64::new(0);
static USED_FRAMES: AtomicU64 = AtomicU64::new(0);
static SEEKS_BY_DECODER_RECYCLED: AtomicU64 = AtomicU64::new(0);
static SEEKS_BY_EVICTED_FRAME: AtomicU64 = AtomicU64::new(0);
static SEEKS_BY_OUT_OF_ORDER: AtomicU64 = AtomicU64::new(0);
static SEEKS_BY_CUT_IN: AtomicU64 = AtomicU64::new(0);
static SEEKS_BY_OTHER: AtomicU64 = AtomicU64::new(0);
static LOCK_WAIT_NS: AtomicU64 = AtomicU64::new(0);
static LANES_CREATED: AtomicU64 = AtomicU64::new(0);
static LANES_PEAK: AtomicU64 = AtomicU64::new(0);
static LANES_RELEASED: AtomicU64 = AtomicU64::new(0);
static LIVE_DECODERS_PEAK: AtomicUsize = AtomicUsize::new(0);

/// Count pictures a decoder call output.
fn count_frames<T>(out: &[T]) {
    FRAMES.fetch_add(out.len() as u64, Ordering::Relaxed);
}

/// Run a decoder call, adding its wall time to the process-wide counter.
fn timed<R>(f: impl FnOnce() -> R) -> R {
    let t0 = Instant::now();
    let r = f();
    DECODE_NS.fetch_add(t0.elapsed().as_nanos() as u64, Ordering::Relaxed);
    r
}

/// The counters so far (they only grow; subtract two snapshots to measure an interval).
pub fn gop_stats() -> GopStats {
    GopStats {
        hits: HITS.load(Ordering::Relaxed),
        misses: MISSES.load(Ordering::Relaxed),
        seeks: SEEKS.load(Ordering::Relaxed),
        decoded: DECODED.load(Ordering::Relaxed),
        evicted: EVICTED.load(Ordering::Relaxed),
        decode_ns: DECODE_NS.load(Ordering::Relaxed),
        skipped: SKIPPED.load(Ordering::Relaxed),
        draft: DRAFT.load(Ordering::Relaxed),
        frames: FRAMES.load(Ordering::Relaxed),
        used_frames: USED_FRAMES.load(Ordering::Relaxed),
        seeks_by_decoder_recycled: SEEKS_BY_DECODER_RECYCLED.load(Ordering::Relaxed),
        seeks_by_evicted_frame: SEEKS_BY_EVICTED_FRAME.load(Ordering::Relaxed),
        seeks_by_out_of_order: SEEKS_BY_OUT_OF_ORDER.load(Ordering::Relaxed),
        seeks_by_cut_in: SEEKS_BY_CUT_IN.load(Ordering::Relaxed),
        seeks_by_other: SEEKS_BY_OTHER.load(Ordering::Relaxed),
        lock_wait_ns: LOCK_WAIT_NS.load(Ordering::Relaxed),
        lanes_created: LANES_CREATED.load(Ordering::Relaxed),
        lanes_peak_per_source: LANES_PEAK.load(Ordering::Relaxed),
        lanes_released: LANES_RELEASED.load(Ordering::Relaxed),
    }
}

impl std::ops::Sub for GopStats {
    type Output = GopStats;
    fn sub(self, o: GopStats) -> GopStats {
        GopStats {
            hits: self.hits - o.hits,
            misses: self.misses - o.misses,
            seeks: self.seeks - o.seeks,
            decoded: self.decoded - o.decoded,
            evicted: self.evicted - o.evicted,
            decode_ns: self.decode_ns - o.decode_ns,
            skipped: self.skipped - o.skipped,
            draft: self.draft - o.draft,
            frames: self.frames - o.frames,
            used_frames: self.used_frames - o.used_frames,
            seeks_by_decoder_recycled: self.seeks_by_decoder_recycled - o.seeks_by_decoder_recycled,
            seeks_by_evicted_frame: self.seeks_by_evicted_frame - o.seeks_by_evicted_frame,
            seeks_by_out_of_order: self.seeks_by_out_of_order - o.seeks_by_out_of_order,
            seeks_by_cut_in: self.seeks_by_cut_in - o.seeks_by_cut_in,
            seeks_by_other: self.seeks_by_other - o.seeks_by_other,
            lock_wait_ns: self.lock_wait_ns - o.lock_wait_ns,
            lanes_created: self.lanes_created - o.lanes_created,
            lanes_peak_per_source: self.lanes_peak_per_source,
            lanes_released: self.lanes_released - o.lanes_released,
        }
    }
}

/// A container's video sample table, in decode (file) order. `Sync`: a cache decodes on its own
/// threads ([`isolated`]).
pub trait VideoSamples: Sync {
    fn count(&self) -> usize;
    /// Presentation timestamp of sample `i` (track units).
    fn pts(&self, i: usize) -> i64;
    /// Nearest sync sample at or before `i`.
    fn sync_before(&self, i: usize) -> usize;
    /// The sample presented at `t` (track units), if any.
    fn sample_at(&self, t: i64) -> Option<usize>;
    fn read(&self, i: usize) -> crate::Result<Vec<u8>>;
    fn make_decoder(&self) -> crate::Result<Box<dyn VideoDecoder>>;
}

/// The track time (units of `num / den` s) to look up the frame shown at `t` by, for a track whose
/// nominal rate is `rate`. `snap`: move the request back to the start of its frame first, see
/// [`snap_coarse_units`].
///
/// Rounded to the nearest unit (#73): muxers round frame times to the timescale (Matroska usually
/// to 1 ms), so a stamp can sit up to half a unit after the true time, and flooring missed every
/// frame stamped late.
pub(crate) fn lookup_time(t: filmcraft_time::Tick, num: i64, den: i64, rate: filmcraft_time::FrameRate, snap: bool) -> i64 {
    (if snap { rate.snap(t) } else { t }).to_rational_round(num, den)
}

/// Whether to look frames up by first moving the request back to the start of its frame (nominal
/// rate `rate`), for a track whose samples are stamped `stamps` (units of `num / den` s).
///
/// #73's tolerance of half a unit is only harmless while a unit is short against a frame. With a
/// unit of half a frame or more (an MP4 whose timescale is its frame rate, one unit per frame),
/// half a unit is a quarter of a frame or more, and from its middle on every frame was looked up
/// as the next one. Such a track can't place frames between its units anyway, so the request
/// first moves back to the start of its frame, which then rounds onto that frame's own stamp.
///
/// Only when every stamp is where that lookup lands for its nearest nominal frame: a coarse unit
/// is no constant rate. A variable-rate track with samples 2, 1, 2 units long at two units a frame
/// has a stamp between two nominal frames, and snapping would move a request at it onto the
/// frame before. Such a track keeps #73's lookup by the nearest unit.
pub(crate) fn snap_coarse_units(stamps: impl IntoIterator<Item = i64>, num: i64, den: i64, rate: filmcraft_time::FrameRate) -> bool {
    let unit = filmcraft_time::Tick::from_rational(1, num, den);
    if unit.0.saturating_mul(2) < rate.frame_duration().0 {
        return false;
    }
    stamps.into_iter().all(|p| lookup_time(rate.snap_nearest(filmcraft_time::Tick::from_rational(p, num, den)), num, den, rate, true) == p)
}

/// A container's sync sample for `i` when leading pictures need the GOP before: `sync` is the
/// index's own sync sample, `leads(i, key)` says `i` is shown before `key`. One step per GOP, so a
/// loop: a crafted index can chain tens of thousands of GOPs (#2803). A `sync` past `i` stops the
/// walk instead of looping on it.
pub(crate) fn open_gop_sync(mut i: usize, sync: impl Fn(usize) -> usize, leads: impl Fn(usize, usize) -> bool) -> usize {
    loop {
        let key = sync(i);
        if key == 0 || key > i || !leads(i, key) {
            return key;
        }
        i = key - 1;
    }
}

/// The sync sample to start decoding at for sample `i`: its sync sample, or an earlier one when
/// `i` is a leading picture of an open GOP (stored after its random-access picture, shown before
/// it, like the RASL pictures after every HEVC CRA of iPhone video). Those are predicted from the
/// GOP before, so a decoder starting at their random-access picture leaves them out.
fn decode_start(s: &dyn VideoSamples, i: usize) -> usize {
    let mut key = s.sync_before(i);
    while key > 0 && s.pts(i) < s.pts(key) {
        key = s.sync_before(key - 1).min(key - 1);
    }
    key
}

struct State {
    decoder: Option<Session>,
    /// Export pinning shares the decoder lock so trimming cannot race a pin change.
    export_pinned: bool,
    /// A decoder existed before the current fresh one; committed after seek classification.
    had_decoder: bool,
    /// The current decoder has not yet reached seek classification.
    fresh_decoder: bool,
    /// The decoder only produces intra pictures: frames decode independently and in parallel.
    intra: bool,
    /// Idle decoders for parallel intra decoding.
    spare: Vec<Session>,
    /// Next sample (decode order) to feed, and the sync sample the current run started from.
    next: usize,
    start: usize,
    /// Highest pts the running decoder has output since it started (pictures leave in pts order).
    out_max: i64,
    pending_max: i64,
    gop: usize,
}

struct Frames {
    /// Decoded frames by presentation pts (bounded).
    frames: BTreeMap<i64, Arc<VideoFrame>>,
    /// Cached pictures already returned to a caller.
    used: BTreeSet<i64>,
    /// Previously decoded pictures evicted since they were last stored.
    evicted: BTreeSet<i64>,
    /// The cached frames that were decoded in draft mode: served to draft requests only.
    drafts: std::collections::BTreeSet<i64>,
    /// The cached frames still waiting for [`GopCache::finish`]'s crop and rotation: the request
    /// that takes one does it off the lock (#3467).
    raw: std::collections::BTreeSet<i64>,
    bytes: usize,
    claims: BTreeMap<usize, usize>,
    claim_ends: BTreeMap<usize, i64>,
    reservations: BTreeMap<usize, RunSnapshot>,
    pending: BTreeMap<usize, RunSnapshot>,
    /// When a frame was last asked of this cache.
    last_used: Instant,
    /// Presentation pts of the frame the last request wanted: eviction keeps a window around it
    /// (not around the newest decoded picture). Walking a long GOP dumps pictures in decode
    /// order; a window around those would throw away the playhead frame and force another walk.
    pin: i64,
    /// A serialized decode keeps its target protected even when cache hits move `pin`.
    output_pin: Option<i64>,
    /// Scheduler's earliest still-needed source PTS. Set only while export-pinned; request
    /// hits and decode-ahead must never advance it. MIN protects everything before the first hint.
    export_position: Option<i64>,
    /// Only background requests (thumbnails, [`filmcraft_media::cancel::with_background`]) have
    /// used the decoder since it was made: it is given up as soon as another background request
    /// is done, whatever the caches' recency.
    background: bool,
}

impl State {
    /// Give up the decoder (and the spare intra decoders): the next request that has to decode
    /// makes a new one and restarts at a sync sample. Returned so they are dropped off the lock.
    fn release_decoder(&mut self, pool: &Pool) -> Vec<Session> {
        let mut out = std::mem::take(&mut self.spare);
        if let Some(d) = self.decoder.take() {
            self.had_decoder = true;
            LANES_RELEASED.fetch_add(1, Ordering::Relaxed);
            pool.decoders.fetch_sub(1, Ordering::Relaxed);
            if !self.export_pinned {
                pool.unpinned_decoders.fetch_sub(1, Ordering::Relaxed);
            }
            out.push(d);
        }
        self.next = usize::MAX;
        self.pending_max = i64::MIN;
        out
    }

    /// Classify a restart before changing the running decoder's position or random-access key.
    fn seek_cause(&self, frames: &Frames, i: usize, key: usize, want_pts: i64) -> SeekCause {
        if self.fresh_decoder && self.had_decoder {
            SeekCause::DecoderRecycled
        } else if frames.evicted.contains(&want_pts) {
            SeekCause::EvictedFrame
        } else if self.next != usize::MAX && self.next > i {
            SeekCause::OutOfOrder
        } else if (self.fresh_decoder && !self.had_decoder) || (self.next != usize::MAX && self.next <= key && key.saturating_sub(self.next) > CONTINUE_THROUGH)
        {
            SeekCause::CutIn
        } else {
            SeekCause::Other
        }
    }
}

impl Frames {
    /// Evict frames, earliest first, while `more` says so.
    fn release_frames(&mut self, pool: &Pool, more: impl Fn() -> bool) {
        self.retire_idle_pending();
        while more() {
            let next = match self.earliest_position() {
                Some(position) => self.frames.range(..position).find(|&(&pts, _)| self.export_evictable(pts, position)),
                None => self.frames.iter().find(|&(&pts, _)| !self.reserved(pts) && Some(pts) != self.output_pin),
            };
            let Some((&pts, _)) = next else { break };
            let Some(f) = self.frames.remove(&pts) else { break };
            self.drafts.remove(&pts);
            self.raw.remove(&pts);
            self.used.remove(&pts);
            self.evicted.insert(pts);
            self.bytes = self.bytes.saturating_sub(f.byte_size());
            pool.bytes.fetch_sub(f.byte_size(), Ordering::Relaxed);
            EVICTED.fetch_add(1, Ordering::Relaxed);
            filmcraft_frame::pool::recycle(f);
        }
    }

    fn export_evictable(&self, pts: i64, position: i64) -> bool {
        pts < position && pts != self.pin && Some(pts) != self.output_pin && !self.reserved(pts)
    }

    fn release_lane(&mut self, lane: usize) {
        self.claims.retain(|_, owner| *owner != lane);
        let Self { claims, claim_ends, .. } = self;
        claim_ends.retain(|key, _| claims.contains_key(key));
        self.reservations.remove(&lane);
        self.pending.remove(&lane);
    }

    fn reserved(&self, pts: i64) -> bool {
        self.reservations.values().chain(self.pending.values()).any(|r| r.covers(pts))
    }

    fn earliest_position(&self) -> Option<i64> {
        self.export_position
            .map(|position| self.reservations.values().chain(self.pending.values()).filter(|r| r.valid).map(|r| r.start.max(r.floor)).fold(position, i64::min))
    }

    /// Idle readbacks protect only output still needed by the scheduler or an active run.
    /// Active lanes keep their full delayed-output range, including across GOP boundaries.
    fn retire_idle_pending(&mut self) {
        let Some(position) = self.export_position else { return };
        let needed = self.reservations.values().filter(|r| r.valid).map(|r| r.start.max(r.floor)).fold(position, i64::min);
        self.pending.retain(|lane, pending| {
            if !self.reservations.contains_key(lane) {
                pending.start = pending.start.max(needed);
            }
            pending.valid && pending.start.max(pending.floor) <= pending.target
        });
    }

    /// The cached frame at `pts`, unless it is a draft frame and the request wants the exact one.
    fn cached(&self, pts: i64, draft_ok: bool) -> Option<&Arc<VideoFrame>> {
        self.frames.get(&pts).filter(|_| draft_ok || !self.drafts.contains(&pts))
    }

    fn count_used(&mut self, pts: i64) {
        if self.used.insert(pts) {
            USED_FRAMES.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// The nearest usable frame at or before `pts` (robust to decoder pts quirks).
    fn at_or_before(&self, pts: i64, draft_ok: bool) -> Option<Taken> {
        let (pts, frame) = match self.cached(pts, draft_ok) {
            Some(f) => (pts, f),
            None => self.frames.range(..=pts).rev().find(|(p, _)| draft_ok || !self.drafts.contains(p)).map(|(p, f)| (*p, f))?,
        };
        Some(Taken { pts, frame: frame.clone(), raw: self.raw.contains(&pts) })
    }
}

/// Measure only acquisition on a render task's stack. Decode-worker publication is already
/// included in the caller's DecodeWait, so it must not also enter FrameMapWait.
fn lock_frames(frames: &Mutex<Frames>) -> std::sync::MutexGuard<'_, Frames> {
    let _wait = filmcraft_frame::render_timing::enter(filmcraft_frame::render_timing::Phase::FrameMapWait);
    frames.lock().unwrap_or_else(PoisonError::into_inner)
}

fn lock_decoder(shared: &Shared, want_pts: Option<i64>) -> DecoderStateGuard<'_> {
    lock_lane(shared, &shared.primary, want_pts)
}

fn lock_lane<'a>(shared: &'a Shared, lane: &'a Lane, want_pts: Option<i64>) -> DecoderStateGuard<'a> {
    let run = lane.run.snapshot();
    let covered = run.is_some_and(|run| want_pts.is_some_and(|pts| run.covers(pts)));
    let prefetch = run.is_some_and(|run| run.origin == RunOrigin::Prefetch);
    let _wait = filmcraft_frame::render_timing::decoder_queue(covered, prefetch);
    DecoderStateGuard {
        state: lane.state.lock().unwrap_or_else(PoisonError::into_inner),
        shared,
        lane,
        outcome: None,
        completed: false,
        released: RetiredSessions::default(),
    }
}

/// The mutex is dropped before retired sessions. Only this lane's publication and
/// completion are cleared; another lane can continue producing frames throughout.
struct DecoderStateGuard<'a> {
    state: MutexGuard<'a, State>,
    shared: &'a Shared,
    lane: &'a Lane,
    outcome: Option<RunResult>,
    completed: bool,
    released: RetiredSessions,
}
impl std::ops::Deref for DecoderStateGuard<'_> {
    type Target = State;
    fn deref(&self) -> &State {
        &self.state
    }
}
impl std::ops::DerefMut for DecoderStateGuard<'_> {
    fn deref_mut(&mut self) -> &mut State {
        &mut self.state
    }
}
impl DecoderStateGuard<'_> {
    fn complete<T>(&mut self, result: &crate::Result<T>) {
        if let Some(outcome) = &self.outcome {
            *outcome.lock().unwrap_or_else(PoisonError::into_inner) = result.as_ref().err().map(RunFailure::from_error);
        }
        // The Arc is the run identity. Maintenance/intra holders expose no prior ticket.
        *self.lane.outcome.lock().unwrap_or_else(PoisonError::into_inner) = None;
        self.lane.run.publish_state(None, Some(&self.state));
        self.completed = true;
    }
}
impl Drop for DecoderStateGuard<'_> {
    fn drop(&mut self) {
        if !self.completed
            && let Some(outcome) = &self.outcome
        {
            *outcome.lock().unwrap_or_else(PoisonError::into_inner) = Some(RunFailure::Decode("internal error while decoding".into()));
        }
        let failed = self.outcome.as_ref().is_some_and(|o| o.lock().unwrap_or_else(PoisonError::into_inner).is_some());
        *self.lane.outcome.lock().unwrap_or_else(PoisonError::into_inner) = None;
        let retiring = self.lane.id != 0 && (self.lane.release.load(Ordering::SeqCst) || !self.shared.scheduled.load(Ordering::SeqCst));
        if retiring {
            self.released.extend(self.state.release_decoder(&self.shared.pool));
        }
        {
            let mut frames = lock_frames(&self.shared.frames);
            frames.reservations.remove(&self.lane.id);
            if self.state.next == usize::MAX || retiring || failed {
                frames.release_lane(self.lane.id);
            }
            frames.retire_idle_pending();
        }
        self.lane.live.store(self.state.decoder.is_some(), Ordering::SeqCst);
        self.lane.last_idle.store(self.shared.progress.load(Ordering::SeqCst), Ordering::SeqCst);
        self.lane.run.publish_state(None, Some(&self.state));
    }
}

/// Primary-lane compatibility for the original eviction tests. Production uses reservations.
#[cfg(test)]
struct OutputPin<'a>(&'a Mutex<Frames>);

#[cfg(test)]
impl<'a> OutputPin<'a> {
    fn new(frames: &'a Mutex<Frames>, pts: i64) -> Self {
        lock_frames(frames).output_pin = Some(pts);
        Self(frames)
    }
}

#[cfg(test)]
impl Drop for OutputPin<'_> {
    fn drop(&mut self) {
        lock_frames(self.0).output_pin = None;
    }
}

#[derive(Debug, PartialEq, Eq)]
enum SeekCause {
    DecoderRecycled,
    EvictedFrame,
    OutOfOrder,
    CutIn,
    Other,
}

/// A cached frame handed to a request, `raw` while it still waits for [`GopCache::finish`].
enum DecodeOutcome {
    Taken(Taken),
    Blocked(usize),
}

struct Taken {
    pts: i64,
    frame: Arc<VideoFrame>,
    raw: bool,
}

thread_local! {
    /// Caches whose lock this thread holds while decoding (see [`GopCache::frame`]).
    static DECODING: std::cell::RefCell<Vec<usize>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Marks a cache as decoding on this thread until dropped (also on early return / `?`).
struct DecodingGuard(usize);

impl DecodingGuard {
    fn enter(cache: usize) -> Self {
        DECODING.with(|d| d.borrow_mut().push(cache));
        Self(cache)
    }
    fn active(cache: usize) -> bool {
        DECODING.with(|d| {
            let d = d.borrow();
            if filmcraft_media::cancel::export() { !d.is_empty() } else { d.contains(&cache) }
        })
    }
}

impl Drop for DecodingGuard {
    fn drop(&mut self) {
        DECODING.with(|d| {
            let mut d = d.borrow_mut();
            if let Some(k) = d.iter().rposition(|&c| c == self.0) {
                d.remove(k);
            }
        });
    }
}

/// A decoder keeps its reference and in-flight pictures (hundreds of MB for a frame-threaded
/// 4K decoder), so only this many caches keep theirs once idle: a timeline's other clips make a
/// new one when the playhead reaches them, which costs what a seek costs.
const MAX_LIVE_DECODERS: usize = 4;

/// A cache asked for a frame this recently may be needed again soon (the next clip being
/// prefetched, a clip the user scrubs back to): it keeps its decoder up to [`HARD_DECODERS`], and
/// beyond [`MAX_LIVE_DECODERS`] only until it has been idle this long. Its frames are only kept
/// within [`FRAME_BUDGET`] once it has been idle for [`RECENT`]: going back to a clip left (or
/// paused) more than that long ago may cost a re-decode from its keyframe when other clips have
/// filled the budget meanwhile. Frames go least recently used cache first, earliest frames first.
const IDLE: Duration = Duration::from_secs(3);

/// A cache asked for a frame this recently is in use (a layer of the frame being composited): it
/// keeps its decoder and frames whatever the count. Caches idle for less than [`IDLE`] but longer
/// than this are a burst of navigation (thumbnails of every clip, the playhead jumping from clip
/// to clip) that the playhead has already left: they give up decoders beyond [`HARD_DECODERS`]
/// and frames beyond [`FRAME_BUDGET`]. Without this, everything touched within [`IDLE`] counted
/// as in use, so a burst held every clip's decoder (42 live hardware sessions, 6.5 GB of VRAM)
/// and hundreds of MB of frames per clip.
const RECENT: Duration = Duration::from_millis(500);

/// Most decoders caches idle for [`RECENT`] to [`IDLE`] keep together (a composite of a few
/// layers plus the clips just left behind).
const HARD_DECODERS: usize = 8;

/// Decoded frames all caches may hold together before idle ones give theirs up. Each cache also
/// has its own budget, which alone let every clip on a timeline keep hundreds of MB of frames
/// nobody was looking at.
pub const FRAME_BUDGET: usize = 1 << 30;

/// What the caches of a process share: the cap on live decoders and the frame budget.
struct Pool {
    caches: Mutex<Vec<Weak<Shared>>>,
    /// Caches that hold a decoder.
    decoders: AtomicUsize,
    /// Live decoders subject to the limits; `decoders` still includes export-pinned caches.
    unpinned_decoders: AtomicUsize,
    export_decoders: AtomicUsize,
    max_decoders: usize,
    idle: Duration,
    /// Caches idle at least this long are no longer in use (see [`RECENT`]).
    recent: Duration,
    /// Decoders the caches idle for [`Self::recent`] but not [`Self::idle`] may keep.
    hard_decoders: usize,
    /// Bytes of the frames the caches hold.
    bytes: AtomicUsize,
    budget: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
enum RunOrigin {
    Other,
    Render,
    Prefetch,
}

impl RunOrigin {
    fn current() -> Self {
        if filmcraft_frame::render_timing::active() {
            Self::Render
        } else if filmcraft_media::cancel::export() {
            Self::Prefetch
        } else {
            Self::Other
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RunSnapshot {
    valid: bool,
    start: i64,
    target: i64,
    floor: i64,
    origin: RunOrigin,
}

impl RunSnapshot {
    fn covers(self, pts: i64) -> bool {
        self.valid && pts >= self.start && pts <= self.target && pts >= self.floor
    }
}

/// Single writer, serialized by Shared.state. All operations are SeqCst so an accepted
/// even version brackets exactly one complete publication, including inactive states.
#[derive(Default)]
struct RunDescriptor {
    version: AtomicU64,
    active: AtomicBool,
    valid: AtomicBool,
    start: AtomicI64,
    target: AtomicI64,
    floor: AtomicI64,
    origin: AtomicU8,
    next: AtomicUsize,
    key: AtomicUsize,
    out_max: AtomicI64,
}

impl RunDescriptor {
    #[cfg(test)]
    fn publish(&self, run: Option<RunSnapshot>) {
        self.publish_state(run, None);
    }

    fn publish_state(&self, run: Option<RunSnapshot>, st: Option<&State>) {
        self.version.fetch_add(1, Ordering::SeqCst);
        self.active.store(run.is_some(), Ordering::SeqCst);
        if let Some(st) = st {
            self.next.store(st.next, Ordering::SeqCst);
            self.key.store(st.start, Ordering::SeqCst);
            self.out_max.store(st.out_max, Ordering::SeqCst);
        }
        if let Some(run) = run {
            self.valid.store(run.valid, Ordering::SeqCst);
            self.start.store(run.start, Ordering::SeqCst);
            self.target.store(run.target, Ordering::SeqCst);
            self.floor.store(run.floor, Ordering::SeqCst);
            self.origin.store(run.origin as u8, Ordering::SeqCst);
        }
        self.version.fetch_add(1, Ordering::SeqCst);
    }

    fn snapshot(&self) -> Option<RunSnapshot> {
        self.selection_snapshot().0
    }

    fn selection_snapshot(&self) -> (Option<RunSnapshot>, usize, usize, i64) {
        loop {
            let version = self.version.load(Ordering::SeqCst);
            if !version.is_multiple_of(2) {
                std::hint::spin_loop();
                continue;
            }
            let active = self.active.load(Ordering::SeqCst);
            let run = RunSnapshot {
                valid: self.valid.load(Ordering::SeqCst),
                start: self.start.load(Ordering::SeqCst),
                target: self.target.load(Ordering::SeqCst),
                floor: self.floor.load(Ordering::SeqCst),
                origin: match self.origin.load(Ordering::SeqCst) {
                    1 => RunOrigin::Render,
                    2 => RunOrigin::Prefetch,
                    _ => RunOrigin::Other,
                },
            };
            let next = self.next.load(Ordering::SeqCst);
            let key = self.key.load(Ordering::SeqCst);
            let out_max = self.out_max.load(Ordering::SeqCst);
            if version == self.version.load(Ordering::SeqCst) {
                return (active.then_some(run), next, key, out_max);
            }
            std::hint::spin_loop();
        }
    }

    fn reserve(&self, st: &State, s: &dyn VideoSamples, target: i64, floor: Option<i64>, origin: RunOrigin) {
        // Keep pending pictures between out_max and next in the reservation. Starting at
        // pts(next) would incorrectly exclude delayed output. Never promise already-output
        // (possibly evicted) pictures, or speculative pictures beyond the requested target.
        let mut valid = st.next <= s.count() && st.start < s.count();
        let mut start = if valid { s.pts(st.start) } else { target };
        if st.out_max >= start {
            if let Some(after) = st.out_max.checked_add(1) {
                start = after;
            } else {
                valid = false;
            }
        }
        self.publish_state(Some(RunSnapshot { valid: valid && start <= target, start, target, floor: floor.unwrap_or(i64::MIN), origin }), Some(st));
    }
}

/// An export session ticket counts primary, spare and private decoders alike.
struct ExportSession(Arc<Pool>);

impl ExportSession {
    fn first(pool: &Arc<Pool>) -> Self {
        pool.export_decoders.fetch_add(1, Ordering::SeqCst);
        Self(pool.clone())
    }
    fn extra(pool: &Arc<Pool>) -> Option<Self> {
        pool.export_decoders.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| (n < 16).then(|| n + 1)).ok()?;
        Some(Self(pool.clone()))
    }
}
impl Drop for ExportSession {
    fn drop(&mut self) {
        self.0.export_decoders.fetch_sub(1, Ordering::SeqCst);
    }
}

#[derive(Default)]
struct RetiredSessions(Vec<Session>);
impl std::ops::Deref for RetiredSessions {
    type Target = Vec<Session>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl std::ops::DerefMut for RetiredSessions {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}
impl Drop for RetiredSessions {
    fn drop(&mut self) {
        if !self.0.is_empty() {
            let _wait = filmcraft_frame::render_timing::enter(filmcraft_frame::render_timing::Phase::DecodeWait);
            drop(std::mem::take(&mut self.0));
        }
    }
}

struct Session {
    inner: Box<dyn VideoDecoder>,
    export: Option<ExportSession>,
}
impl Session {
    fn new(s: &dyn VideoSamples, export: Option<ExportSession>) -> crate::Result<Self> {
        Ok(Self { inner: s.make_decoder()?, export })
    }
    fn as_mut(&mut self) -> &mut dyn VideoDecoder {
        self.inner.as_mut()
    }
}
impl std::ops::Deref for Session {
    type Target = dyn VideoDecoder;
    fn deref(&self) -> &Self::Target {
        self.inner.as_ref()
    }
}
impl std::ops::DerefMut for Session {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.inner.as_mut()
    }
}

#[derive(Clone)]
enum RunFailure {
    Cancelled,
    Decode(String),
    Unsupported(String),
    Container(String),
}
impl RunFailure {
    fn from_error(e: &CodecError) -> Self {
        match e {
            CodecError::Cancelled => Self::Cancelled,
            CodecError::Decode(s) => Self::Decode(s.clone()),
            CodecError::Unsupported(s) => Self::Unsupported(s.clone()),
            CodecError::Container(s) => Self::Container(s.clone()),
        }
    }
    fn error(self) -> CodecError {
        match self {
            Self::Cancelled => CodecError::Cancelled,
            Self::Decode(s) => CodecError::Decode(s),
            Self::Unsupported(s) => CodecError::Unsupported(s),
            Self::Container(s) => CodecError::Container(s),
        }
    }
}
type RunResult = Arc<Mutex<Option<RunFailure>>>;

struct Lane {
    state: Mutex<State>,
    run: RunDescriptor,
    id: usize,
    live: AtomicBool,
    intra: AtomicBool,
    last_idle: AtomicI64,
    release: AtomicBool,
    outcome: Mutex<Option<RunResult>>,
    pool: Arc<Pool>,
}
impl Lane {
    fn new(id: usize, pool: Arc<Pool>) -> Self {
        Self {
            state: Mutex::new(State {
                decoder: None,
                export_pinned: id != 0,
                had_decoder: false,
                fresh_decoder: false,
                intra: false,
                spare: Vec::new(),
                next: usize::MAX,
                start: 0,
                out_max: i64::MIN,
                pending_max: i64::MIN,
                gop: 0,
            }),
            run: RunDescriptor::default(),
            id,
            live: AtomicBool::new(false),
            intra: AtomicBool::new(false),
            last_idle: AtomicI64::new(0),
            release: AtomicBool::new(false),
            outcome: Mutex::new(None),
            pool,
        }
    }
}
impl Drop for Lane {
    fn drop(&mut self) {
        let st = self.state.get_mut().unwrap_or_else(PoisonError::into_inner);
        drop(st.release_decoder(&self.pool));
    }
}

/// The primary lane stays directly accessible for single-lane callers and tests.
struct Shared {
    primary: Arc<Lane>,
    extra: Mutex<Vec<Arc<Lane>>>,
    scheduled: AtomicBool,
    progress: AtomicI64,
    #[cfg(test)]
    wait_observer: Mutex<Option<std::sync::mpsc::Sender<usize>>>,
    frames: Mutex<Frames>,
    pool: Arc<Pool>,
}
impl std::ops::Deref for Shared {
    type Target = Lane;
    fn deref(&self) -> &Lane {
        &self.primary
    }
}
impl Shared {
    fn retire_primary(&self, st: &mut State, frames: &mut Frames) -> Vec<Session> {
        let released = st.release_decoder(&self.pool);
        frames.release_lane(0);
        self.primary.live.store(false, Ordering::SeqCst);
        self.primary.run.publish_state(None, Some(st));
        released
    }
}

impl Drop for Shared {
    fn drop(&mut self) {
        self.pool.bytes.fetch_sub(self.frames.get_mut().unwrap_or_else(PoisonError::into_inner).bytes, Ordering::Relaxed);
    }
}

/// A cache's state unless another thread is using it (it is then decoding: not idle).
fn try_state(c: &Shared) -> Option<MutexGuard<'_, State>> {
    match c.state.try_lock() {
        Ok(g) => Some(g),
        Err(TryLockError::Poisoned(e)) => Some(e.into_inner()),
        Err(TryLockError::WouldBlock) => None,
    }
}

/// Frame maintenance also skips a cache whose frames another request is using.
fn try_frames(c: &Shared) -> Option<MutexGuard<'_, Frames>> {
    match c.frames.try_lock() {
        Ok(g) => Some(g),
        Err(TryLockError::Poisoned(e)) => Some(e.into_inner()),
        Err(TryLockError::WouldBlock) => None,
    }
}

impl Pool {
    fn new(max_decoders: usize, idle: Duration, budget: usize) -> Self {
        Self {
            caches: Mutex::new(Vec::new()),
            decoders: AtomicUsize::new(0),
            unpinned_decoders: AtomicUsize::new(0),
            export_decoders: AtomicUsize::new(0),
            max_decoders,
            idle,
            recent: idle,
            hard_decoders: max_decoders,
            bytes: AtomicUsize::new(0),
            budget,
        }
    }

    /// Caches idle for `recent` (< the idle time) are still trimmed, down to `hard_decoders`.
    fn tiered(mut self, recent: Duration, hard_decoders: usize) -> Self {
        self.recent = recent.min(self.idle);
        self.hard_decoders = hard_decoders.max(self.max_decoders);
        self
    }

    /// The pool of every cache in the process.
    fn global() -> Arc<Pool> {
        static POOL: std::sync::OnceLock<Arc<Pool>> = std::sync::OnceLock::new();
        POOL.get_or_init(|| Arc::new(Pool::new(MAX_LIVE_DECODERS, IDLE, FRAME_BUDGET).tiered(RECENT, HARD_DECODERS))).clone()
    }

    fn register(&self, c: &Arc<Shared>) {
        let mut g = self.caches.lock().unwrap_or_else(PoisonError::into_inner);
        g.retain(|w| w.strong_count() > 0);
        g.push(Arc::downgrade(c));
    }

    /// The caches other than `me` idle for at least `idle` that `keep` selects, least recently used first. Other
    /// caches are only ever try-locked, so two caches trimming each other cannot deadlock.
    fn idle_others(&self, me: &Shared, idle: Duration, keep: impl Fn(&State, &Frames) -> bool) -> Vec<Arc<Shared>> {
        let all: Vec<Arc<Shared>> = self.caches.lock().unwrap_or_else(PoisonError::into_inner).iter().filter_map(Weak::upgrade).collect();
        let mut idle: Vec<(Instant, Arc<Shared>)> = all
            .into_iter()
            .filter(|c| !std::ptr::eq(Arc::as_ptr(c), me))
            .filter_map(|c| {
                let used = {
                    let st = try_state(&c)?;
                    let frames = try_frames(&c)?;
                    (keep(&st, &frames) && frames.last_used.elapsed() >= idle).then_some(frames.last_used)?
                };
                Some((used, c))
            })
            .collect();
        idle.sort_by_key(|(used, _)| *used);
        idle.into_iter().map(|(_, c)| c).collect()
    }

    /// Take the decoders of idle caches, least recently used first, down to the cap. The caller
    /// drops them with no cache lock held: dropping a decoder can block (a hardware session waits
    /// for its frames in flight).
    #[must_use = "drop the released decoders with no cache lock held"]
    fn trim_decoders(&self, me: &Shared) -> Vec<Session> {
        let mut released = Vec::new();
        // first the caches idle for long, down to the cap; then those a burst has left behind
        for (idle, cap) in [(self.idle, self.max_decoders), (self.recent, self.hard_decoders)] {
            if self.unpinned_decoders.load(Ordering::Relaxed) <= cap {
                continue;
            }
            for c in self.idle_others(me, idle, |st, _| st.decoder.is_some() && !st.export_pinned) {
                if self.unpinned_decoders.load(Ordering::Relaxed) <= cap {
                    break;
                }
                if let Some(mut st) = try_state(&c)
                    && !st.export_pinned
                    && let Some(mut frames) = try_frames(&c)
                {
                    released.extend(c.retire_primary(&mut st, &mut frames));
                }
            }
        }
        released
    }

    /// Take the decoders of the caches other than `me` that only background requests have used
    /// (see [`Frames::background`]) and that are not decoding now. A burst of thumbnails (an import
    /// of many clips) touches every clip within moments, so the recency rules treat them all as
    /// in use; each software 4K decoder holds ~0.5 GB. The cache `me` just served keeps its
    /// decoder (hovering a clip asks for more of its frames). Drop the result with no cache lock
    /// held.
    #[must_use = "drop the released decoders with no cache lock held"]
    fn release_background(&self, me: &Shared) -> Vec<Session> {
        let all: Vec<Arc<Shared>> = self.caches.lock().unwrap_or_else(PoisonError::into_inner).iter().filter_map(Weak::upgrade).collect();
        let mut released = Vec::new();
        for c in all.iter().filter(|c| !std::ptr::eq(Arc::as_ptr(c), me)) {
            if let Some(mut st) = try_state(c)
                && !st.export_pinned
                && let Some(mut frames) = try_frames(c)
                && frames.background
                && st.decoder.is_some()
            {
                released.extend(c.retire_primary(&mut st, &mut frames));
            }
        }
        released
    }

    /// Empty every cache whose lock is free (see [`release_idle`]). Decoders are dropped with no
    /// cache lock held, as for [`Self::trim_decoders`].
    fn release_idle(&self) {
        let all: Vec<Arc<Shared>> = self.caches.lock().unwrap_or_else(PoisonError::into_inner).iter().filter_map(Weak::upgrade).collect();
        let mut released = Vec::new();
        for c in &all {
            // try-locked: a cache another thread holds is decoding, so it is in use
            if let Some(mut st) = try_state(c)
                && !st.export_pinned
                && let Some(mut frames) = try_frames(c)
            {
                released.extend(c.retire_primary(&mut st, &mut frames));
                frames.release_frames(self, || true);
            }
        }
        drop(released);
    }

    /// Evict the frames of caches idle for [`RECENT`], least recently used first, down to the
    /// budget. Caches in use keep theirs (each within its own budget): a frame evicted before it is shown costs a
    /// re-decode from the keyframe.
    fn trim_frames(&self, me: &Shared) {
        let over = || self.bytes.load(Ordering::Relaxed) > self.budget;
        if !over() {
            return;
        }
        for c in self.idle_others(me, self.recent, |_, frames| !frames.frames.is_empty()) {
            if !over() {
                break;
            }
            if let Some(_st) = try_state(&c)
                && let Some(mut frames) = try_frames(&c)
            {
                frames.release_frames(self, over);
            }
        }
    }
}

/// Free the decoded frames and the decoders of every cache that nobody is using right now,
/// process-wide. The frame budget only trims caches that were idle for [`RECENT`] while another
/// cache is asking for frames, so after a long job (an export) every source keeps up to
/// [`FRAME_CACHE_CEILING`] of frames and its decoders until the session ends. Whoever finishes
/// such a job calls this. A cache a thread is decoding on (its lock is held) is skipped, so
/// concurrent users lose nothing; a cache emptied here only re-decodes from a sync sample when
/// asked again.
pub fn release_idle() {
    Pool::global().release_idle();
}

/// Caches that hold a decoder right now, process-wide (`perf.stats`).
pub fn live_decoders() -> usize {
    Pool::global().decoders.load(Ordering::Relaxed)
}

/// Highest observed live decoder count, process-wide.
pub fn live_decoders_peak() -> usize {
    LIVE_DECODERS_PEAK.load(Ordering::Relaxed)
}

/// Bytes of decoded frames the caches hold right now, process-wide (`perf.stats`).
pub fn cached_bytes() -> usize {
    Pool::global().bytes.load(Ordering::Relaxed)
}

/// Decoder + decoded-frame cache for one video track.
pub struct GopCache {
    shared: Arc<Shared>,
    /// Colour signalled by the container, which wins over the bitstream's.
    explicit_color: Option<ColorInfo>,
    /// Clockwise quarter turns from the container's display matrix, applied to every frame.
    rotation: u8,
    /// The container's clean aperture, cut from every frame before the rotation.
    crop: Option<Region>,
    budget: usize,
}

/// The cache reserves room for this many small frames: a frame-threaded decoder runs up to ~2x
/// its thread count ahead of the frame it returns, and playback prefetches ahead of the playhead.
const MIN_FRAMES: usize = 64;
/// Ordinary per-source ceiling, regardless of frame dimensions. In particular, untrusted VP9
/// `show_existing_frame` samples must not enlarge the small-frame reserve above. Scheduled exports
/// may exceed this soft budget to retain frames at/ahead of their explicit eviction boundary.
const FRAME_CACHE_CEILING: usize = 384 << 20;

/// A running decoder up to this many samples before the wanted sample's sync sample keeps going
/// rather than restarting at the sync sample.
const CONTINUE_THROUGH: usize = 48;

/// Threads that run the caches' decoding (see [`isolated`]); `None` where none can be made.
#[cfg(not(target_arch = "wasm32"))]
fn decode_pool() -> Option<&'static rayon::ThreadPool> {
    static POOL: std::sync::OnceLock<Option<rayon::ThreadPool>> = std::sync::OnceLock::new();
    POOL.get_or_init(|| {
        let n = std::thread::available_parallelism().map(std::num::NonZeroUsize::get).unwrap_or(1);
        rayon::ThreadPoolBuilder::new().num_threads(n).thread_name(|i| format!("decode-{i}")).build().ok()
    })
    .as_ref()
}

/// Run a cache's decoding, its lock held by the caller, on [`decode_pool`], the caller waiting
/// without doing other rayon work meanwhile (#480).
///
/// That decoding starts rayon work (a decoder's slices, cropping and rotating the frames). On the
/// pool of the render jobs that ask the cache for frames, a worker that took part of it could,
/// waiting inside it, pick up another of those jobs and block on the lock: the lock holder then
/// waits for its work on the blocked worker's stack, and that worker for the lock, for good (an
/// export of rotated iPhone clips stopped making frames). On the decode pool that work only ever
/// runs beside other caches' decoding, which never waits for a cache's lock. A caller that is a
/// rayon worker would also run other jobs of its pool while it waits for the decode pool, some
/// of them asking for a cache's lock while it holds this one: it waits on a plain thread instead.
fn isolated<T: Send>(f: impl FnOnce() -> crate::Result<T> + Send) -> crate::Result<T> {
    let render_origin = filmcraft_frame::render_timing::active();
    let _wait = filmcraft_frame::render_timing::enter(filmcraft_frame::render_timing::Phase::DecodeWait);
    let ctx = filmcraft_media::cancel::Context::current();
    let job = move || filmcraft_frame::render_timing::decode_run(render_origin, || ctx.run(f));
    #[cfg(not(target_arch = "wasm32"))]
    if let Some(pool) = decode_pool() {
        if rayon::current_thread_index().is_none() {
            return pool.install(job);
        }
        let slot = Mutex::new(Some(job));
        let waited = std::thread::scope(|scope| {
            let spawned = std::thread::Builder::new().name("decode-wait".into()).spawn_scoped(scope, || {
                let job = slot.lock().unwrap_or_else(PoisonError::into_inner).take();
                job.map(|job| pool.install(job))
            });
            spawned.map(std::thread::ScopedJoinHandle::join)
        });
        return match waited {
            Ok(Ok(Some(r))) => r,
            Ok(Err(_)) => Err(CodecError::Decode("internal error while decoding".into())),
            // no thread to wait on: decode here, as before
            Ok(Ok(None)) | Err(_) => match slot.into_inner().unwrap_or_else(PoisonError::into_inner) {
                Some(job) => job(),
                None => Err(CodecError::Decode("internal error while decoding".into())),
            },
        };
    }
    job()
}

impl GopCache {
    pub fn new(explicit_color: Option<ColorInfo>) -> Self {
        // unit tests count decoder restarts: each of their caches gets a pool of its own
        let pool = if cfg!(test) { Arc::new(Pool::new(MAX_LIVE_DECODERS, IDLE, FRAME_BUDGET).tiered(RECENT, HARD_DECODERS)) } else { Pool::global() };
        Self::in_pool(explicit_color, pool)
    }

    /// An empty cache whose decoders and frame memory count against `pool`.
    fn in_pool(explicit_color: Option<ColorInfo>, pool: Arc<Pool>) -> Self {
        let shared = Arc::new(Shared {
            primary: Arc::new(Lane::new(0, pool.clone())),
            extra: Mutex::new(Vec::new()),
            scheduled: AtomicBool::new(false),
            progress: AtomicI64::new(0),
            #[cfg(test)]
            wait_observer: Mutex::new(None),
            frames: Mutex::new(Frames {
                frames: BTreeMap::new(),
                used: BTreeSet::new(),
                evicted: BTreeSet::new(),
                drafts: Default::default(),
                raw: Default::default(),
                bytes: 0,
                claims: BTreeMap::new(),
                claim_ends: BTreeMap::new(),
                reservations: BTreeMap::new(),
                pending: BTreeMap::new(),
                last_used: Instant::now(),
                pin: i64::MIN,
                output_pin: None,
                export_position: None,
                background: false,
            }),
            pool: pool.clone(),
        });
        pool.register(&shared);
        Self { shared, explicit_color, rotation: 0, crop: None, budget: FRAME_CACHE_CEILING }
    }

    /// Exclude this cache's decoder from the limits and recycling while export work needs it.
    /// A boolean pin: repeated pin or unpin calls are idempotent.
    pub fn set_export_pinned(&self, pinned: bool) {
        let mut st = lock_decoder(&self.shared, None);
        if st.export_pinned == pinned {
            return;
        }
        if st.decoder.is_some() {
            if pinned {
                self.shared.pool.unpinned_decoders.fetch_sub(1, Ordering::Relaxed);
            } else {
                self.shared.pool.unpinned_decoders.fetch_add(1, Ordering::Relaxed);
            }
        }
        let State { decoder, spare, .. } = &mut *st;
        for session in decoder.iter_mut().chain(spare.iter_mut()) {
            if pinned && session.export.is_none() {
                session.export = Some(ExportSession::first(&self.shared.pool));
            }
            if !pinned {
                session.export = None;
            }
        }
        st.export_pinned = pinned;
        lock_frames(&self.shared.frames).export_position = pinned.then_some(i64::MIN);
    }

    /// Set the scheduler's eviction boundary using the same sample lookup as frame requests,
    /// so a VFR frame spanning `target` is retained. This takes only the frame-map lock.
    pub fn set_export_position(&self, s: &dyn VideoSamples, target: i64) {
        let n = s.count();
        let Some(i) = s.sample_at(target.max(0)).or_else(|| (n > 0).then(|| n - 1)) else { return };
        let mut frames = lock_frames(&self.shared.frames);
        if frames.export_position.is_some() {
            frames.export_position = Some(s.pts(i));
            frames.retire_idle_pending();
        }
    }

    /// Turn every decoded frame clockwise by `quarter_turns` × 90° (the container's display
    /// rotation, e.g. portrait phone video stored landscape).
    pub fn with_rotation(mut self, quarter_turns: u8) -> Self {
        self.rotation = quarter_turns % 4;
        self
    }

    /// Cut every decoded frame to `region` (the container's clean aperture, in stored picture
    /// coordinates) before the rotation, as cropping and the display matrix compose.
    pub fn with_crop(mut self, region: Option<Region>) -> Self {
        self.crop = region;
        self
    }

    /// Colour signalled by the container wins over the bitstream's (YUV frames only); the
    /// container's clean aperture and then its display rotation are applied.
    fn finish(&self, f: VideoFrame) -> VideoFrame {
        let mut f = self.colored(f);
        if let Some(r) = self.crop {
            f = f.cropped(r);
        }
        if self.rotation != 0 { f.rotated(self.rotation) } else { f }
    }

    /// `f` with the container's colour, which costs nothing (raw frames carry it too).
    fn colored(&self, mut f: VideoFrame) -> VideoFrame {
        if let Some(c) = self.explicit_color
            && !matches!(f.data, filmcraft_frame::PixelData::Rgba8(_) | filmcraft_frame::PixelData::RgbaF32(_))
        {
            f.color = c;
        }
        f
    }

    /// Cache decoder output. Cropping and rotating a 4K frame takes longer than decoding it: with
    /// the lock held, every frame worker and the decoder itself would wait for it, and a rotated
    /// phone clip played at 2 fps (#3467). The frame is cached raw instead and [`Self::finished`]
    /// does that work off the lock, for the frames requests actually take.
    fn store(&self, st: &mut Frames, pts: i64, f: VideoFrame, draft: bool) {
        st.used.remove(&pts);
        st.evicted.remove(&pts);
        if self.crop.is_none() && self.rotation == 0 {
            return self.store_finished(st, pts, self.finish(f), draft);
        }
        self.store_finished(st, pts, self.colored(f), draft);
        st.raw.insert(pts);
    }

    /// `t`'s frame with [`Self::finish`] applied, which must run without the cache's lock. The
    /// finished frame replaces the raw one in the cache unless that was evicted or replaced meanwhile.
    fn finished(&self, t: Taken) -> Arc<VideoFrame> {
        if !t.raw {
            return t.frame;
        }
        let done = Arc::new(self.finish(VideoFrame::clone(&t.frame)));
        let wait = Instant::now();
        let mut st = lock_frames(&self.shared.frames);
        LOCK_WAIT_NS.fetch_add(wait.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64, Ordering::Relaxed);
        let stored = st.frames.get(&t.pts).is_some_and(|f| Arc::ptr_eq(f, &t.frame));
        if stored {
            let draft = st.drafts.contains(&t.pts);
            self.store_finished(&mut st, t.pts, done.clone(), draft);
        }
        drop(st);
        if stored {
            self.shared.pool.trim_frames(&self.shared);
        }
        // the raw planes serve the next decoded pictures once nothing else holds them
        filmcraft_frame::pool::recycle(t.frame);
        done
    }

    /// The small-frame reserve may use the configured cache budget, but may never enlarge it.
    fn cache_budget(&self, frame_bytes: usize) -> usize {
        let reserve = MIN_FRAMES.saturating_mul(frame_bytes).min(FRAME_CACHE_CEILING);
        self.budget.min(FRAME_CACHE_CEILING).max(reserve)
    }

    /// Cache a frame [`Self::finish`] has been applied to ([`Self::store`] marks a raw one after).
    fn store_finished(&self, st: &mut Frames, pts: i64, f: impl Into<Arc<VideoFrame>>, draft: bool) {
        st.retire_idle_pending();
        let f = f.into();
        st.last_used = Instant::now();
        if draft {
            st.drafts.insert(pts);
            DRAFT.fetch_add(1, Ordering::Relaxed);
        } else {
            st.drafts.remove(&pts);
        }
        let budget = self.cache_budget(f.byte_size());
        let before = st.bytes;
        st.bytes += f.byte_size();
        st.raw.remove(&pts);
        if let Some(old) = st.frames.insert(pts, f) {
            st.bytes -= old.byte_size();
            filmcraft_frame::pool::recycle(old);
        }
        // Evict frames far from the request (pin), not the newest decode. A frame-threaded
        // decoder can emit a burst past the playhead; pivoting on those pts used to drop the
        // wanted picture and walk the GOP again.
        let pivot = st.output_pin.or_else(|| st.reservations.values().map(|r| r.target).min()).unwrap_or(if st.pin != i64::MIN { st.pin } else { pts });
        while st.bytes > budget && st.frames.len() > 2 {
            let (Some(&first), Some(&last)) = (st.frames.keys().next(), st.frames.keys().next_back()) else { break };
            let mut victim = if let Some(position) = st.earliest_position() {
                let Some((&victim, _)) = st.frames.range(..position).find(|&(&p, _)| st.export_evictable(p, position) && p != pts) else { break };
                victim
            } else if pivot - first > last - pivot {
                first
            } else {
                last
            };
            if victim == pivot || victim == pts {
                victim = if victim == first { last } else { first };
                if victim == pivot || victim == pts {
                    break;
                }
            }
            if st.reserved(victim) {
                break;
            }
            if let Some(v) = st.frames.remove(&victim) {
                st.drafts.remove(&victim);
                st.raw.remove(&victim);
                st.used.remove(&victim);
                st.evicted.insert(victim);
                st.bytes -= v.byte_size();
                EVICTED.fetch_add(1, Ordering::Relaxed);
                // its planes serve the next decoded pictures instead of going back to the allocator
                filmcraft_frame::pool::recycle(v);
            }
        }
        let pool = &self.shared.pool;
        if st.bytes >= before {
            pool.bytes.fetch_add(st.bytes - before, Ordering::Relaxed);
        } else {
            pool.bytes.fetch_sub(before - st.bytes, Ordering::Relaxed);
        }
    }

    /// Store decoder output (in presentation order) and advance `out_max`.
    fn store_output(&self, st: &mut State, out: Vec<crate::video::DecodedFrame>) {
        count_frames(&out);
        for d in out {
            st.out_max = st.out_max.max(d.pts);
            let mut frames = lock_frames(&self.shared.frames);
            self.store(&mut frames, d.pts, d.frame, d.draft);
            drop(frames);
        }
    }

    /// The frame presented at `target` (track units, clamped to the stream).
    ///
    /// Decoders may run slices on rayon, and a rayon thread waiting inside a decode can pick up
    /// another render job that asks this same source for a frame — on the thread that holds this
    /// cache's lock. A nested request uses a private decoder ([`Self::private_frame`]): it must
    /// neither lock this cache again nor hold a second cache's lane at the same time.
    pub fn frame(&self, s: &dyn VideoSamples, target: i64) -> crate::Result<Arc<VideoFrame>> {
        self.frame_late(s, target, None)
    }

    /// [`Self::frame`] while catching up: frames shown before `late_before` (track units) are
    /// late. Non-reference samples of late frames are left out on the way to the wanted frame
    /// (no other picture depends on them, so the wanted frame decodes exactly as it would
    /// otherwise; a later request for a skipped frame re-seeks).
    ///
    /// Inside [`filmcraft_media::cancel::with_draft`]`(true, …)` the decoder runs in draft mode
    /// ([`VideoDecoder::set_draft`]); its draft frames are cached for draft requests only, and an
    /// exact request for one re-decodes it.
    pub fn frame_late(&self, s: &dyn VideoSamples, target: i64, late_before: Option<i64>) -> crate::Result<Arc<VideoFrame>> {
        let r = self.frame_late_locked(s, target, late_before).map(|t| self.finished(t));
        self.after_request();
        r
    }

    /// Run once a request is answered, outside the cache's lock (see `release_background`). A
    /// thumbnail leaves the decoders of earlier thumbnails no reason to stay: each costs hundreds
    /// of MB at 4K.
    fn after_request(&self) {
        // A reentrant request used a private session precisely because its caller
        // still owns a lane. Pool maintenance must not acquire a second lane either.
        if filmcraft_media::cancel::export() && DecodingGuard::active(self as *const Self as usize) {
            return;
        }
        drop(self.shared.pool.trim_decoders(&self.shared));
        self.shared.pool.trim_frames(&self.shared);
        if filmcraft_media::cancel::background() {
            drop(self.shared.pool.release_background(&self.shared));
        }
    }

    /// [`Self::frame_late`] leaving the display rotation to the caller: the frame comes with the
    /// clockwise quarter turns still to apply (0 when it is already upright). The GPU compositor
    /// folds them into the layer's placement for free, where turning a 4K frame on the CPU at
    /// every frame of playback is too slow (#3467).
    pub fn frame_late_unturned(&self, s: &dyn VideoSamples, target: i64, late_before: Option<i64>) -> crate::Result<(Arc<VideoFrame>, u8)> {
        let r = self.frame_late_locked(s, target, late_before).map(|t| {
            // shortcut: a clean aperture is still cut and turned on the CPU; rare outside camera files
            if !t.raw || self.crop.is_some() { (self.finished(t), 0) } else { (t.frame, self.rotation) }
        });
        self.after_request();
        r
    }

    /// The cached or decoded frame for [`Self::frame_late`] and [`Self::frame_late_unturned`], raw
    /// when it still waits for [`Self::finish`]. The cache's lock is released when this returns.
    fn frame_late_locked(&self, s: &dyn VideoSamples, target: i64, late_before: Option<i64>) -> crate::Result<Taken> {
        let n = s.count();
        let i = s.sample_at(target.max(0)).or_else(|| (n > 0).then(|| n - 1)).ok_or_else(|| CodecError::Decode("empty track".into()))?;
        let want_pts = s.pts(i);
        let me = self as *const Self as usize;
        if DecodingGuard::active(me) {
            return Ok(Taken { pts: want_pts, frame: self.private_frame(s, i, want_pts, n)?, raw: false });
        }
        let draft = filmcraft_media::cancel::draft();
        let background = filmcraft_media::cancel::background();
        if let Some(taken) = self.cached(want_pts, draft, background) {
            return Ok(taken);
        }
        let mut forced = None;
        let mut allow_extra = true;
        let mut counted_miss = false;
        loop {
            // Every unlock/wakeup returns through the frame map and selection again.
            if let Some(taken) = self.cached(want_pts, draft, background) {
                return Ok(taken);
            }
            let lane = forced.take().unwrap_or_else(|| self.choose_lane(s, i, want_pts, allow_extra));
            let mut released = Vec::new();
            let state = match lane.state.try_lock() {
                Ok(state) => state,
                Err(TryLockError::Poisoned(e)) => e.into_inner(),
                Err(TryLockError::WouldBlock) => {
                    // Observe only the active run at wait start. Never hold metadata while
                    // acquiring a lane; maintenance and intra holders have no run ticket.
                    let (run, outcome) = {
                        let sampled = lane.run.snapshot();
                        let _wait = filmcraft_frame::render_timing::decoder_queue(
                            sampled.is_some_and(|r| r.covers(want_pts)),
                            sampled.is_some_and(|r| r.origin == RunOrigin::Prefetch),
                        );
                        let metadata = lane.outcome.lock().unwrap_or_else(PoisonError::into_inner);
                        (lane.run.snapshot(), filmcraft_media::cancel::export().then(|| metadata.clone()).flatten())
                    };
                    #[cfg(test)]
                    if let Some(observer) = self.shared.wait_observer.lock().unwrap_or_else(PoisonError::into_inner).as_ref() {
                        let _ = observer.send(lane.id);
                    }
                    let _wait = filmcraft_frame::render_timing::decoder_queue(
                        run.is_some_and(|r| r.covers(want_pts)),
                        run.is_some_and(|r| r.origin == RunOrigin::Prefetch),
                    );
                    let wait = Instant::now();
                    let state = lane.state.lock().unwrap_or_else(PoisonError::into_inner);
                    LOCK_WAIT_NS.fetch_add(wait.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64, Ordering::Relaxed);
                    drop(_wait);
                    if !filmcraft_media::cancel::export() {
                        // Playback keeps the original lock-and-decode-own-request behavior.
                        state
                    } else {
                        let failure = outcome.and_then(|o| o.lock().unwrap_or_else(PoisonError::into_inner).clone());
                        drop(state);
                        if filmcraft_media::cancel::cancelled() {
                            return Err(CodecError::Cancelled);
                        }
                        if let Some(taken) = self.cached(want_pts, draft, background) {
                            return Ok(taken);
                        }
                        if let Some(failure) = failure
                            && !matches!(failure, RunFailure::Cancelled)
                        {
                            return Err(failure.error());
                        }
                        allow_extra = true;
                        continue;
                    }
                }
            };
            let outcome = Arc::new(Mutex::new(None));
            {
                let _wait = filmcraft_frame::render_timing::decoder_queue(false, false);
                let mut metadata = lane.outcome.lock().unwrap_or_else(PoisonError::into_inner);
                lane.run.publish_state(
                    Some(RunSnapshot { valid: false, start: want_pts, target: want_pts, floor: i64::MIN, origin: RunOrigin::current() }),
                    Some(&state),
                );
                *metadata = filmcraft_media::cancel::export().then(|| outcome.clone());
            }
            let mut st =
                DecoderStateGuard { state, shared: &self.shared, lane: &lane, outcome: Some(outcome), completed: false, released: RetiredSessions::default() };
            if let Some(taken) = self.cached(want_pts, draft, background) {
                st.complete(&Ok(()));
                return Ok(taken);
            }
            if !counted_miss {
                MISSES.fetch_add(1, Ordering::Relaxed);
                counted_miss = true;
            }
            if filmcraft_media::cancel::export() {
                let State { decoder, spare, .. } = &mut *st;
                for session in decoder.iter_mut().chain(spare.iter_mut()) {
                    if session.export.is_none() {
                        session.export = Some(ExportSession::first(&self.shared.pool));
                    }
                }
            }
            let was_missing = st.decoder.is_none();
            if !background && st.decoder.as_ref().is_some_and(|d| d.thread_limited()) {
                released.extend(st.release_decoder(&self.shared.pool));
            }
            if st.decoder.is_none() {
                let ticket = if lane.id != 0 {
                    if !filmcraft_media::cancel::export() || !self.shared.scheduled.load(Ordering::SeqCst) {
                        st.complete(&Ok(()));
                        drop(st);
                        allow_extra = false;
                        continue;
                    }
                    let Some(ticket) = ExportSession::extra(&self.shared.pool) else {
                        st.complete(&Ok(()));
                        drop(st);
                        allow_extra = false;
                        continue;
                    };
                    Some(ticket)
                } else if st.export_pinned || filmcraft_media::cancel::export() {
                    Some(ExportSession::first(&self.shared.pool))
                } else {
                    None
                };
                let creation_wait = (lane.id != 0).then(|| filmcraft_frame::render_timing::enter(filmcraft_frame::render_timing::Phase::DecodeWait));
                let decoder = Session::new(s, ticket);
                drop(creation_wait);
                if decoder.is_err() {
                    st.complete(&decoder);
                }
                let d = decoder?;
                st.intra = d.intra_only();
                lane.intra.store(st.intra, Ordering::SeqCst);
                st.decoder = Some(d);
                st.fresh_decoder = true;
                st.next = usize::MAX;
                lane.live.store(true, Ordering::SeqCst);
                lane.release.store(false, Ordering::SeqCst);
                if !st.export_pinned {
                    self.shared.pool.unpinned_decoders.fetch_add(1, Ordering::Relaxed);
                }
                let live = self.shared.pool.decoders.fetch_add(1, Ordering::Relaxed).saturating_add(1);
                LIVE_DECODERS_PEAK.fetch_max(live, Ordering::Relaxed);
                LANES_CREATED.fetch_add(1, Ordering::Relaxed);
                let lanes = self.lanes().iter().filter(|l| l.live.load(Ordering::SeqCst)).count();
                LANES_PEAK.fetch_max(lanes as u64, Ordering::Relaxed);
            }
            let origin = RunOrigin::current();
            lane.run.publish_state(Some(RunSnapshot { valid: false, start: want_pts, target: want_pts, floor: i64::MIN, origin }), Some(&st));
            let _decoding = DecodingGuard::enter(me);
            {
                let mut frames = lock_frames(&self.shared.frames);
                frames.pin = want_pts;
                if !background {
                    frames.background = false;
                } else if was_missing {
                    frames.background = true;
                }
                frames.reservations.insert(lane.id, RunSnapshot { valid: true, start: want_pts, target: want_pts, floor: i64::MIN, origin });
            }
            if filmcraft_media::cancel::cancelled() {
                let result: crate::Result<()> = Err(CodecError::Cancelled);
                st.complete(&result);
                return Err(CodecError::Cancelled);
            }
            if st.intra {
                st.complete(&Ok(()));
                return self.intra_frame(st, s, i, want_pts);
            }
            let state = &mut *st;
            let result = isolated(|| self.decode_until((&lane, state), s, i, want_pts, late_before, draft));
            st.complete(&result);
            match result {
                Ok(DecodeOutcome::Taken(taken)) => {
                    lock_frames(&self.shared.frames).count_used(taken.pts);
                    drop(st);
                    drop(released);
                    return Ok(taken);
                }
                Ok(DecodeOutcome::Blocked(owner)) => {
                    drop(st);
                    drop(released);
                    forced = self.lanes().into_iter().find(|l| l.id == owner);
                }
                Err(e) => {
                    // decode_until clears the floor and invalidates suppressed runs. Keep
                    // the original unsuppressed continuation behavior on cancellation/errors.
                    return Err(e);
                }
            }
        }
    }

    fn lanes(&self) -> Vec<Arc<Lane>> {
        let _wait = filmcraft_frame::render_timing::decoder_queue(false, false);
        let mut lanes = vec![self.shared.primary.clone()];
        lanes.extend(self.shared.extra.lock().unwrap_or_else(PoisonError::into_inner).iter().cloned());
        lanes
    }

    fn choose_lane(&self, s: &dyn VideoSamples, i: usize, want_pts: i64, allow_extra: bool) -> Arc<Lane> {
        if !filmcraft_media::cancel::export() || !self.shared.scheduled.load(Ordering::SeqCst) || self.shared.primary.intra.load(Ordering::SeqCst) {
            return self.shared.primary.clone();
        }
        let limit = decode_lane_limit();
        self.choose_lane_limit(s, i, want_pts, allow_extra, limit)
    }

    fn choose_lane_limit(&self, s: &dyn VideoSamples, i: usize, want_pts: i64, allow_extra: bool, limit: usize) -> Arc<Lane> {
        let lanes: Vec<_> = self.lanes().into_iter().filter(|l| l.id < limit.max(1)).collect();
        let key = decode_start(s, i);
        let n = s.count();
        let snapshots: Vec<_> = lanes.iter().map(|l| l.run.selection_snapshot()).collect();
        for (lane, &(_, next, start, out_max)) in lanes.iter().zip(&snapshots) {
            let running = lane.live.load(Ordering::SeqCst) && next != usize::MAX && next <= n && start <= key;
            let near = next <= key && key - next <= CONTINUE_THROUGH;
            let continuing = running && (next > key || near) && (i >= next || (start <= i && want_pts > out_max));
            if continuing {
                return lane.clone();
            }
        }
        for (lane, (run, ..)) in lanes.iter().zip(&snapshots) {
            if run.is_some_and(|r| r.covers(want_pts)) {
                return lane.clone();
            }
        }
        for (lane, (run, ..)) in lanes.iter().zip(&snapshots) {
            if run.is_none() && (lane.id == 0 || lane.live.load(Ordering::SeqCst)) {
                return lane.clone();
            }
        }
        if allow_extra && self.shared.primary.live.load(Ordering::SeqCst) && self.shared.pool.export_decoders.load(Ordering::SeqCst) < 16 {
            for (lane, (run, ..)) in lanes.iter().zip(&snapshots) {
                if lane.id != 0 && lane.id < limit && run.is_none() && !lane.live.load(Ordering::SeqCst) {
                    return lane.clone();
                }
            }
            let admission_wait = filmcraft_frame::render_timing::decoder_queue(false, false);
            let mut extra = self.shared.extra.lock().unwrap_or_else(PoisonError::into_inner);
            drop(admission_wait);
            if extra.len().saturating_add(1) < limit {
                let lane = Arc::new(Lane::new(extra.len().saturating_add(1), self.shared.pool.clone()));
                extra.push(lane.clone());
                return lane;
            }
        }
        self.shared.primary.clone()
    }

    /// Output-frame progress, independent of source PTS and decoder feed progress.
    pub fn set_export_work(&self, scheduled: bool, output_frame: i64) {
        self.shared.progress.store(output_frame, Ordering::SeqCst);
        self.shared.scheduled.store(scheduled, Ordering::SeqCst);
        lock_frames(&self.shared.frames).retire_idle_pending();
        let mut released = Vec::new();
        for lane in self.lanes().into_iter().skip(1) {
            if !scheduled {
                lane.release.store(true, Ordering::SeqCst);
            }
            let idle = lane.run.snapshot().is_none() && output_frame.saturating_sub(lane.last_idle.load(Ordering::SeqCst)) >= 120;
            if (!scheduled || idle)
                && let Ok(mut st) = lane.state.try_lock()
            {
                if scheduled && output_frame.saturating_sub(lane.last_idle.load(Ordering::SeqCst)) < 120 {
                    continue;
                }
                released.extend(st.release_decoder(&self.shared.pool));
                lane.live.store(false, Ordering::SeqCst);
                lane.run.publish_state(None, Some(&st));
                let mut frames = lock_frames(&self.shared.frames);
                frames.release_lane(lane.id);
            }
        }
        drop(released);
    }

    /// Take an exact cache hit without acquiring the decoder lock.
    fn cached(&self, want_pts: i64, draft: bool, background: bool) -> Option<Taken> {
        let wait = Instant::now();
        let mut frames = lock_frames(&self.shared.frames);
        LOCK_WAIT_NS.fetch_add(wait.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64, Ordering::Relaxed);
        frames.last_used = Instant::now();
        frames.pin = want_pts;
        let frame = frames.cached(want_pts, draft)?.clone();
        HITS.fetch_add(1, Ordering::Relaxed);
        frames.count_used(want_pts);
        // A monitor hit makes a thumbnail's decoder one in use.
        frames.background &= background;
        Some(Taken { pts: want_pts, frame, raw: frames.raw.contains(&want_pts) })
    }

    /// Decode (the lock held) until the frame with `want_pts` (sample `i`) is cached; return it.
    fn decode_until(
        &self,
        lane_state: (&Lane, &mut State),
        s: &dyn VideoSamples,
        i: usize,
        want_pts: i64,
        late_before: Option<i64>,
        draft: bool,
    ) -> crate::Result<DecodeOutcome> {
        let (lane, st) = lane_state;
        // Only this mutex holder writes the descriptor; its origin was captured by the caller.
        let origin = lane.run.snapshot().map_or(RunOrigin::Other, |run| run.origin);
        let n = s.count();
        let mut allow_floor = filmcraft_media::cancel::export();
        loop {
            let mut key = decode_start(s, i);
            // Continue the running decoder when it has passed the wanted sample's sync sample and
            // either has not reached the sample yet or has been fed it without outputting it yet
            // (a frame-threaded decoder holds many pictures in flight). Otherwise the frame was
            // evicted or lies in another GOP: restart at the sync sample. A run that started after
            // `key` (at the random-access picture of a wanted leading picture) left the picture out.
            let running = st.next != usize::MAX && st.next <= n && st.start <= key;
            // Decoding on through a short stretch into the next GOP is cheaper than a restart, and
            // playback wants those frames anyway.
            let near = st.next <= key && key - st.next <= CONTINUE_THROUGH;
            let continuing = running && (st.next > key || near) && (i >= st.next || (st.start <= i && want_pts > st.out_max));
            let floored = !continuing && allow_floor;
            if !continuing {
                let cause = st.seek_cause(&lock_frames(&self.shared.frames), i, key, want_pts);
                // The container's sync flags may be wrong for the codec (an MP4 without `stss` marks
                // every sample): step back to a sample the decoder can start from.
                while key > 0 {
                    let data = s.read(key)?;
                    if st.decoder.as_ref().and_then(|d| d.is_random_access(&data)) != Some(false) {
                        break;
                    }
                    key = s.sync_before(key - 1);
                }
                {
                    let mut frames = lock_frames(&self.shared.frames);
                    if let Some(&owner) = frames.claims.get(&key)
                        && owner != lane.id
                    {
                        return Ok(DecodeOutcome::Blocked(owner));
                    }
                    frames.release_lane(lane.id);
                    frames.claims.insert(key, lane.id);
                    frames.claim_ends.insert(key, i64::MIN);
                    frames.pending.remove(&lane.id);
                }
                // A conflicting claim is a yield, not a decoder restart.
                SEEKS.fetch_add(1, Ordering::Relaxed);
                let counter = match cause {
                    SeekCause::DecoderRecycled => &SEEKS_BY_DECODER_RECYCLED,
                    SeekCause::EvictedFrame => &SEEKS_BY_EVICTED_FRAME,
                    SeekCause::OutOfOrder => &SEEKS_BY_OUT_OF_ORDER,
                    SeekCause::CutIn => &SEEKS_BY_CUT_IN,
                    SeekCause::Other => &SEEKS_BY_OTHER,
                };
                counter.fetch_add(1, Ordering::Relaxed);
                st.had_decoder = true;
                st.fresh_decoder = false;
                st.pending_max = i64::MIN;
                st.gop = key;
                if let Some(d) = st.decoder.as_mut() {
                    d.set_output_floor(None);
                    d.reset();
                    if floored {
                        d.set_output_floor(Some(want_pts));
                    }
                }
                st.next = key;
                st.start = key;
                st.out_max = i64::MIN;
            }
            let mut floor = floored.then_some(want_pts);
            self.reserve_run(lane, st, s, want_pts, floor, origin);
            let mut blocked = None;
            let decoded: crate::Result<()> = (|| {
                let limit = (i.max(st.next) + 64).min(n);
                let late = late_before.unwrap_or(i64::MIN).min(want_pts);
                let Some(decoder) = st.decoder.as_mut() else {
                    return Err(CodecError::Decode("no video decoder".into()));
                };
                decoder.set_draft(draft);
                while st.next < limit {
                    if filmcraft_media::cancel::cancelled() {
                        // A run without suppression can continue on a later request.
                        return Err(CodecError::Cancelled);
                    }
                    let k = st.next;
                    // Claim every encountered sync sample before feeding it, including a
                    // continuing run. Retain older claims until their delayed output is drained.
                    let data = s.read(k)?;
                    let candidate = s.sync_before(k);
                    let gop = if candidate > st.gop && st.decoder.as_ref().and_then(|d| d.is_random_access(&data)) != Some(false) { candidate } else { st.gop };
                    {
                        let mut frames = lock_frames(&self.shared.frames);
                        if let Some(&owner) = frames.claims.get(&gop)
                            && owner != lane.id
                        {
                            blocked = Some(owner);
                            return Ok(());
                        }
                        let done: Vec<_> = frames
                            .claims
                            .iter()
                            .filter(|(key, owner)| **owner == lane.id && **key != gop && frames.claim_ends.get(key).is_some_and(|end| *end <= st.out_max))
                            .map(|(&key, _)| key)
                            .collect();
                        for key in done {
                            frames.claims.remove(&key);
                            frames.claim_ends.remove(&key);
                        }
                        frames.claims.insert(gop, lane.id);
                        let end = frames.claim_ends.entry(gop).or_insert(i64::MIN);
                        *end = (*end).max(s.pts(k));
                    }
                    st.gop = gop;
                    st.pending_max = st.pending_max.max(s.pts(k));
                    self.reserve_run(lane, st, s, want_pts, floor, origin);
                    if k != i && s.pts(k) < late && st.decoder.as_ref().is_some_and(|d| d.is_disposable(&data)) {
                        st.next += 1;
                        SKIPPED.fetch_add(1, Ordering::Relaxed);
                        continue;
                    }
                    let Some(decoder) = st.decoder.as_mut() else {
                        return Err(CodecError::Decode("no video decoder".into()));
                    };
                    let out = timed(|| decoder.decode(&data, s.pts(k)))?;
                    st.next += 1;
                    DECODED.fetch_add(1, Ordering::Relaxed);
                    self.store_output(st, out);
                    self.reserve_run(lane, st, s, want_pts, floor, origin);
                    if lock_frames(&self.shared.frames).cached(want_pts, draft).is_some() {
                        break;
                    }
                }
                if lock_frames(&self.shared.frames).cached(want_pts, draft).is_none() {
                    floor = None;
                    if let Some(d) = st.decoder.as_mut() {
                        d.set_output_floor(None);
                    }
                    self.reserve_run(lane, st, s, want_pts, floor, origin);
                    let out = st.decoder.as_mut().map(|d| timed(|| d.flush())).unwrap_or_default();
                    self.store_output(st, out);
                    st.next = usize::MAX;
                    self.reserve_run(lane, st, s, want_pts, floor, origin);
                }
                Ok(())
            })();
            // Also clear on cancellation, sample-read failure and decode errors. A later
            // request may continue this decoder outside export.
            if let Some(d) = st.decoder.as_mut() {
                d.set_output_floor(None);
            }
            self.reserve_run(lane, st, s, want_pts, None, origin);
            if floored && decoded.is_err() {
                // Earlier pictures may have been suppressed before an interrupted cut-in.
                // A later request for one must restart, rather than continue past it.
                st.next = usize::MAX;
            }
            decoded?;
            if let Some(owner) = blocked {
                // The old GOP can still own delayed readbacks. Drain them before yielding
                // its claim, without feeding even one sample into the other lane's GOP.
                self.reserve_run(lane, st, s, want_pts, None, origin);
                let out = st.decoder.as_mut().map(|d| timed(|| d.flush())).unwrap_or_default();
                self.store_output(st, out);
                st.next = usize::MAX;
                self.reserve_run(lane, st, s, want_pts, None, origin);
                return Ok(DecodeOutcome::Blocked(owner));
            }
            if floored && lock_frames(&self.shared.frames).cached(want_pts, draft).is_none() {
                // Container and decoder PTS can differ, or the wanted picture can be absent.
                // Replay once without suppression so at_or_before sees the old path's candidates.
                allow_floor = false;
                st.next = usize::MAX;
                self.reserve_run(lane, st, s, want_pts, None, origin);
                continue;
            }
            // nearest decoded frame at or before the wanted pts (robust to decoder pts quirks)
            return lock_frames(&self.shared.frames)
                .at_or_before(want_pts, draft)
                .map(DecodeOutcome::Taken)
                .ok_or_else(|| CodecError::Decode("frame not produced".into()));
        }
    }

    fn reserve_run(&self, lane: &Lane, st: &State, s: &dyn VideoSamples, target: i64, floor: Option<i64>, origin: RunOrigin) {
        lane.run.reserve(st, s, target, floor, origin);
        let mut frames = lock_frames(&self.shared.frames);
        if let Some(run) = lane.run.snapshot() {
            frames.reservations.insert(lane.id, run);
            let pending = RunSnapshot { valid: st.next != usize::MAX && run.start <= st.pending_max, target: st.pending_max, ..run };
            if pending.valid && pending.start <= pending.target {
                frames.pending.insert(lane.id, pending);
            } else {
                frames.pending.remove(&lane.id);
            }
        }
    }

    /// A nested request (see [`Self::frame`]): decode from the sync sample with a fresh decoder,
    /// without touching the shared state.
    fn private_frame(&self, s: &dyn VideoSamples, i: usize, want_pts: i64, n: usize) -> crate::Result<Arc<VideoFrame>> {
        let ticket = filmcraft_media::cancel::export().then(|| ExportSession::first(&self.shared.pool));
        let mut d = Session::new(s, ticket)?;
        let mut next = usize::MAX;
        let out = Self::decode_to(s, d.as_mut(), &mut next, i, want_pts, n)?;
        let f = out
            .into_iter()
            .filter(|p| p.pts <= want_pts)
            .max_by_key(|p| p.pts)
            .map(|p| p.frame)
            .ok_or_else(|| CodecError::Decode("frame not produced".into()))?;
        USED_FRAMES.fetch_add(1, Ordering::Relaxed);
        Ok(Arc::new(self.finish(f)))
    }

    /// Decode with `d` (positioned at `next`) until the picture with `want_pts` (sample `i`) comes out.
    fn decode_to(
        s: &dyn VideoSamples,
        d: &mut dyn VideoDecoder,
        next: &mut usize,
        i: usize,
        want_pts: i64,
        n: usize,
    ) -> crate::Result<Vec<crate::video::DecodedFrame>> {
        let mut key = decode_start(s, i);
        // Continue the running decoder when the wanted sample is ahead within this GOP run.
        let continuing = *next != usize::MAX && *next > key && *next <= i + 16 && *next <= n;
        if !continuing {
            // The container's sync flags may be wrong for the codec (an MP4 without `stss` marks
            // every sample): step back to a sample the decoder can start from.
            while key > 0 {
                let data = s.read(key)?;
                if d.is_random_access(&data) != Some(false) {
                    break;
                }
                key = s.sync_before(key - 1);
            }
            d.reset();
            *next = key;
        }
        let limit = (i + 64).min(n);
        let mut out = Vec::new();
        let mut found = false;
        while *next < limit {
            let k = *next;
            let data = s.read(k)?;
            let pics = timed(|| d.decode(&data, s.pts(k)))?;
            count_frames(&pics);
            *next += 1;
            found |= pics.iter().any(|p| p.pts == want_pts);
            out.extend(pics);
            if found {
                break;
            }
        }
        if !found {
            let pics = timed(|| d.flush());
            count_frames(&pics);
            out.extend(pics);
            *next = usize::MAX;
        }
        Ok(out)
    }

    /// Intra-only streams: decode the one sample outside the lock, so several frame workers
    /// decode different frames of the same source at once.
    fn intra_frame(&self, st: DecoderStateGuard<'_>, s: &dyn VideoSamples, i: usize, want_pts: i64) -> crate::Result<Taken> {
        let mut st = st;
        let spare = st.spare.pop();
        drop(st);
        let mut dec = match spare {
            Some(d) => d,
            None => Session::new(s, filmcraft_media::cancel::export().then(|| ExportSession::first(&self.shared.pool)))?,
        };
        if filmcraft_media::cancel::export() && dec.export.is_none() {
            dec.export = Some(ExportSession::first(&self.shared.pool));
        }
        let res = s.read(i).and_then(|data| {
            DECODED.fetch_add(1, Ordering::Relaxed);
            timed(|| dec.decode(&data, want_pts))
        });
        let res = res.inspect(|out| count_frames(out));
        let wait = Instant::now();
        let mut st = lock_decoder(&self.shared, Some(want_pts));
        LOCK_WAIT_NS.fetch_add(wait.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64, Ordering::Relaxed);
        if !st.export_pinned {
            dec.export = None;
        }
        if st.spare.len() < 16 {
            st.spare.push(dec);
        }
        // cached raw: the request that takes a frame crops and turns it off the lock, or leaves
        // the turn to the GPU (`frame_late_unturned`)
        for d in res? {
            let mut frames = lock_frames(&self.shared.frames);
            self.store(&mut frames, d.pts, d.frame, d.draft);
            drop(frames);
        }
        let mut frames = lock_frames(&self.shared.frames);
        let taken = frames.at_or_before(want_pts, true).ok_or_else(|| CodecError::Decode("frame not produced".into()))?;
        frames.count_used(taken.pts);
        Ok(taken)
    }
}

#[cfg(test)]
mod tests;
