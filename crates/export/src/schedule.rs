//! Bounded, best-effort source decode hints for a stepped export.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;

use filmcraft_media::{FrameRequest, SharedSource, cancel};
use filmcraft_project::ItemId;
use filmcraft_render::SourceProvider;

use crate::{ExportError, Result, pipeline::Pipeline};

const LOOK_AHEAD: i64 = 32;
const PRE_ROLL: i64 = 120;
const MAX_SOURCES: usize = 8;
const DECODE_BUDGET: u64 = 1_500_000_000;

#[derive(Clone, Copy)]
struct Use {
    request: FrameRequest,
    first: i64,
    last: i64,
    pre_roll: bool,
}

struct SourceWork {
    source: SharedSource,
    requests: Vec<Use>,
    cut: Option<i64>,
}

struct ScheduledSource {
    source: SharedSource,
    submitted: Vec<Use>,
    last: i64,
    jobs: Vec<Job>,
}

struct Job {
    requests: Vec<Use>,
    pre_roll: bool,
    handle: JoinHandle<bool>,
}

pub(crate) struct Scheduler {
    pipe: Arc<Pipeline>,
    sources: Vec<ScheduledSource>,
    cancel: Arc<AtomicBool>,
    progress: i64,
}

impl Scheduler {
    pub(crate) fn new(pipe: Arc<Pipeline>) -> Self {
        Self { pipe, sources: Vec::new(), cancel: Arc::new(AtomicBool::new(false)), progress: 0 }
    }

    pub(crate) fn advance(&mut self, next: i64, end: i64, sources: &dyn SourceProvider, cancelled: bool) -> Result<()> {
        self.progress = next;
        self.cancel.store(cancelled, Ordering::Relaxed);
        self.reap()?;
        if self.cancel.load(Ordering::Relaxed) {
            self.stop(true)?;
            return Err(ExportError::Cancelled);
        }
        let work = self.window(next, end, sources);
        // Extend repeated holds/stills before discarding uses behind the output position.
        for state in &mut self.sources {
            if let Some(w) = work.iter().find(|w| Arc::ptr_eq(&state.source, &w.source)) {
                for old in state.submitted.iter_mut().chain(state.jobs.iter_mut().flat_map(|job| &mut job.requests)) {
                    if let Some(r) = w.requests.iter().find(|r| r.request == old.request) {
                        old.last = old.last.max(r.last);
                    }
                }
                if let Some(last) = w.requests.iter().map(|r| r.last).max() {
                    state.last = state.last.max(last);
                }
            }
        }
        self.release_before(next);
        // Source PTS need not increase with output time (reverse, remap, shared aliases).
        // Include all still-needed uses, not the newest request decoded ahead of the renderer.
        for state in &self.sources {
            state.source.set_export_work(true, next);
            let upcoming = work.iter().find(|w| Arc::ptr_eq(&state.source, &w.source)).into_iter().flat_map(|w| &w.requests);
            if let Some(time) = state
                .submitted
                .iter()
                .chain(state.jobs.iter().flat_map(|job| &job.requests))
                .chain(upcoming)
                .filter(|r| r.last >= next)
                .map(|r| r.request.time)
                .min()
            {
                state.source.set_export_position(time);
            }
        }
        let (bytes, capacity, mut remaining) = self.budget(&work);
        for w in work {
            if self.cancel.load(Ordering::Relaxed) {
                self.stop(true)?;
                return Err(ExportError::Cancelled);
            }
            if let Err(error) = self.submit(w, next, bytes, capacity, &mut remaining) {
                let _ = self.stop(true);
                return Err(error);
            }
        }
        Ok(())
    }

    fn window(&self, next: i64, end: i64, sources: &dyn SourceProvider) -> Vec<SourceWork> {
        let mut work = Vec::new();
        let ahead_end = next.saturating_add(LOOK_AHEAD).min(end);
        for f in next..ahead_end {
            self.collect(&mut work, f, None, sources);
        }
        // First output sample at or after each media clip cut.
        let mut starts = Vec::new();
        if let Some(sequence) = self.pipe.project.sequence(self.pipe.seq) {
            let limit = next.saturating_add(PRE_ROLL).min(end);
            for track in sequence.video_tracks.iter().filter(|track| track.enabled) {
                for clip in track.items.iter().filter(|clip| clip.enabled) {
                    let mut f = self.pipe.rate.frame_at(clip.start);
                    if self.pipe.rate.tick_of(f) < clip.start {
                        f = f.saturating_add(1);
                    }
                    if f >= ahead_end && f >= next && f < limit {
                        starts.push((f, clip.item));
                    }
                }
            }
        }
        starts.sort_unstable();
        starts.dedup();
        for (f, item) in starts {
            self.collect(&mut work, f, Some(item), sources);
        }
        work
    }

    fn budget(&self, work: &[SourceWork]) -> (u64, usize, u64) {
        let count = self.sources.len() + work.iter().filter(|w| !self.sources.iter().any(|s| Arc::ptr_eq(&s.source, &w.source))).count();
        // Source info does not expose bit depth; estimate four bytes per source pixel.
        let bytes = work
            .iter()
            .map(|w| {
                w.source
                    .info()
                    .video
                    .as_ref()
                    .map_or(0, |v| u64::from(v.width).checked_mul(u64::from(v.height)).and_then(|n| n.checked_mul(4)).unwrap_or(u64::MAX))
            })
            .chain(self.sources.iter().map(|s| {
                s.source
                    .info()
                    .video
                    .as_ref()
                    .map_or(0, |v| u64::from(v.width).checked_mul(u64::from(v.height)).and_then(|n| n.checked_mul(4)).unwrap_or(u64::MAX))
            }))
            .max()
            .unwrap_or(1)
            .max(1);
        let capacity = usize::try_from(DECODE_BUDGET / bytes / u64::try_from(count.max(1)).unwrap_or(u64::MAX)).unwrap_or(0);
        let retained = self.sources.iter().fold(0u64, |sum, state| {
            let count = state.submitted.len().saturating_add(state.jobs.iter().map(|job| job.requests.len()).sum::<usize>());
            sum.saturating_add(bytes.saturating_mul(u64::try_from(count).unwrap_or(u64::MAX)))
        });
        (bytes, capacity, DECODE_BUDGET.saturating_sub(retained))
    }

    fn submit(&mut self, mut w: SourceWork, next: i64, bytes: u64, capacity: usize, remaining: &mut u64) -> Result<()> {
        // The continuing window and nearest cut share the source's allowance.
        let reserve = usize::from(w.requests.iter().any(|r| r.pre_roll));
        let ahead = capacity.saturating_sub(reserve).min(LOOK_AHEAD as usize) as i64;
        w.requests.retain(|r| r.pre_roll || r.first < next.saturating_add(ahead));
        for r in &mut w.requests {
            if !r.pre_roll {
                r.last = r.last.min(next.saturating_add(ahead).saturating_sub(1));
            }
        }
        w.requests.truncate(capacity);
        if w.requests.is_empty() {
            return Ok(());
        }
        let index = match self.sources.iter().position(|s| Arc::ptr_eq(&s.source, &w.source)) {
            Some(i) => i,
            None if self.sources.len() < MAX_SOURCES && *remaining >= bytes => {
                w.source.set_export_pinned(true);
                w.source.set_export_work(true, next);
                self.sources.push(ScheduledSource { source: w.source, submitted: Vec::new(), last: next, jobs: Vec::new() });
                self.sources.len().saturating_sub(1)
            }
            None => return Ok(()),
        };
        let Some(state) = self.sources.get_mut(index) else { return Ok(()) };
        state.source.set_export_work(true, next);
        if let Some(time) = state
            .submitted
            .iter()
            .chain(state.jobs.iter().flat_map(|job| &job.requests))
            .chain(&w.requests)
            .filter(|r| r.last >= next)
            .map(|r| r.request.time)
            .min()
        {
            state.source.set_export_position(time);
        }
        let limit = filmcraft_codecs::gop::decode_lane_limit().min(2);
        for pre_roll in [false, true] {
            if state.jobs.len() >= limit || state.jobs.iter().any(|job| job.pre_roll == pre_roll) {
                continue;
            }
            let retained = state.submitted.len().saturating_add(state.jobs.iter().map(|job| job.requests.len()).sum::<usize>());
            let available = capacity.saturating_sub(retained).min(usize::try_from(*remaining / bytes).unwrap_or(0));
            let requests: Vec<_> = w
                .requests
                .iter()
                .copied()
                .filter(|r| r.pre_roll == pre_roll)
                .filter(|r| !state.submitted.iter().chain(state.jobs.iter().flat_map(|job| &job.requests)).any(|old| old.request == r.request))
                .take(available)
                .collect();
            if requests.is_empty() {
                continue;
            }
            let batch: Vec<_> = requests.iter().map(|r| r.request).collect();
            let source = state.source.clone();
            let context = cancel::with_cancel(&self.cancel, || cancel::with_export(true, cancel::Context::current));
            // Keep the actual worker handle: completion includes all thread cleanup.
            let handle = std::thread::Builder::new()
                .name("export-prefetch".into())
                .spawn(move || {
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        context.run(|| {
                            if !cancel::cancelled() {
                                source.prefetch(&batch);
                            }
                        })
                    }))
                    .is_ok()
                })
                .map_err(|e| ExportError::Encode(format!("could not spawn source prefetch: {e}")))?;
            let charge = bytes.saturating_mul(u64::try_from(requests.len()).unwrap_or(u64::MAX));
            *remaining = remaining.saturating_sub(charge);
            state.last = state.last.max(requests.iter().map(|r| r.last).max().unwrap_or(next));
            state.jobs.push(Job { requests, pre_roll, handle });
        }
        Ok(())
    }

    fn collect(&self, work: &mut Vec<SourceWork>, f: i64, pre_roll: Option<ItemId>, sources: &dyn SourceProvider) {
        let Some(requests) = filmcraft_render::source_frame_requests(&self.pipe.project, self.pipe.seq, self.pipe.rate.tick_of(f), self.pipe.opts, sources)
        else {
            return;
        };
        for (item, request) in requests {
            if pre_roll.is_some_and(|only| only != item) {
                continue;
            }
            let Some(source) = sources.source(item) else { continue };
            let index = match work.iter().position(|w| Arc::ptr_eq(&w.source, &source)) {
                Some(i) => i,
                None => {
                    if work.len() >= MAX_SOURCES {
                        continue;
                    }
                    work.push(SourceWork { source, requests: Vec::new(), cut: None });
                    work.len().saturating_sub(1)
                }
            };
            if let Some(w) = work.get_mut(index) {
                if pre_roll.is_some() {
                    if w.cut.is_some_and(|cut| cut != f) {
                        continue;
                    }
                    w.cut = Some(f);
                }
                if let Some(old) = w.requests.iter_mut().find(|r| r.request == request) {
                    old.last = old.last.max(f);
                } else {
                    w.requests.push(Use { request, first: f, last: f, pre_roll: pre_roll.is_some() });
                }
            }
        }
    }

    fn reap(&mut self) -> Result<()> {
        let mut failed = false;
        for state in &mut self.sources {
            let mut i = 0;
            while i < state.jobs.len() {
                if state.jobs.get(i).is_some_and(|job| job.handle.is_finished()) {
                    let job = state.jobs.remove(i);
                    if matches!(job.handle.join(), Ok(true)) {
                        state.submitted.extend(job.requests);
                    } else {
                        failed = true;
                        self.cancel.store(true, Ordering::Relaxed);
                    }
                } else {
                    i += 1;
                }
            }
        }
        if failed {
            let _ = self.stop(true);
            Err(ExportError::Encode("internal error while prefetching source frames".into()))
        } else {
            Ok(())
        }
    }

    fn release_before(&mut self, next: i64) {
        self.sources.retain_mut(|state| {
            // Passed uses no longer set the eviction position, but jobs keep their coverage
            // until joined; their GOP reservations independently protect decode work.
            state.submitted.retain(|r| r.last >= next);
            if state.last < next && state.jobs.is_empty() {
                state.source.set_export_work(false, next);
                state.source.set_export_pinned(false);
                false
            } else {
                true
            }
        });
    }

    /// Drain every worker before releasing any source membership or pin.
    pub(crate) fn stop(&mut self, cancel: bool) -> Result<()> {
        let mut failed = false;
        if cancel {
            self.cancel.store(true, Ordering::Relaxed);
        }
        for state in &mut self.sources {
            for job in state.jobs.drain(..) {
                if !matches!(job.handle.join(), Ok(true)) {
                    failed = true;
                    self.cancel.store(true, Ordering::Relaxed);
                }
            }
        }
        for state in &self.sources {
            state.source.set_export_work(false, self.progress);
            state.source.set_export_pinned(false);
        }
        self.sources.clear();
        if failed { Err(ExportError::Encode("internal error while prefetching source frames".into())) } else { Ok(()) }
    }
}

impl Drop for Scheduler {
    fn drop(&mut self) {
        let _ = self.stop(true);
    }
}
