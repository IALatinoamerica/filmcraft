use super::*;
use crate::schedule::Scheduler;
use filmcraft_frame::{AudioBuffer, VideoFrame};
use filmcraft_media::generators::GeneratorSource;
use filmcraft_media::{FrameRequest, Generator, MediaInfo, MediaSource, SharedSource, cancel};
use filmcraft_project::{ItemKind, Label, MediaClip, MediaRef, ParamValue, SequenceSettings, TrackKind};
use filmcraft_render::{RenderOptions, SourceMap};
use filmcraft_time::{Tick, TimeRange};
use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;
use std::sync::mpsc;
use std::time::Duration;

struct Recording {
    inner: SharedSource,
    fetched: Mutex<Vec<FrameRequest>>,
    prefetched: Mutex<Vec<FrameRequest>>,
    batches: Mutex<Vec<Vec<FrameRequest>>>,
    positions: Mutex<Vec<Tick>>,
    work: Mutex<Vec<(bool, i64)>>,
    pinned: AtomicBool,
    active: AtomicUsize,
    peak: AtomicUsize,
    block: AtomicBool,
    fail: AtomicBool,
    started: Mutex<Option<mpsc::Sender<()>>>,
}

impl Recording {
    fn new(inner: SharedSource) -> Arc<Self> {
        Arc::new(Self {
            inner,
            fetched: Default::default(),
            prefetched: Default::default(),
            batches: Default::default(),
            positions: Default::default(),
            work: Default::default(),
            pinned: AtomicBool::new(false),
            active: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
            block: AtomicBool::new(false),
            fail: AtomicBool::new(false),
            started: Default::default(),
        })
    }
}

impl MediaSource for Recording {
    fn info(&self) -> &MediaInfo {
        self.inner.info()
    }
    fn video_frame(&self, req: FrameRequest) -> filmcraft_media::Result<Arc<VideoFrame>> {
        self.fetched.lock().unwrap().push(req);
        self.inner.video_frame(req)
    }
    fn audio(&self, start: i64, frames: usize, rate: u32) -> filmcraft_media::Result<AudioBuffer> {
        self.inner.audio(start, frames, rate)
    }
    fn prefetch(&self, requests: &[FrameRequest]) {
        assert!(cancel::export());
        assert!(self.pinned.load(Ordering::Relaxed));
        assert!(self.work.lock().unwrap().last().is_some_and(|&(scheduled, _)| scheduled));
        assert!(!self.positions.lock().unwrap().is_empty(), "position published before launch");
        self.batches.lock().unwrap().push(requests.to_vec());
        let active = self.active.fetch_add(1, Ordering::Relaxed) + 1;
        self.peak.fetch_max(active, Ordering::Relaxed);
        for &request in requests {
            if cancel::cancelled() {
                break;
            }
            self.prefetched.lock().unwrap().push(request);
            let _ = self.inner.video_frame(request);
            if let Some(started) = self.started.lock().unwrap().as_ref() {
                let _ = started.send(());
            }
            while self.block.load(Ordering::Relaxed) && !cancel::cancelled() {
                std::thread::yield_now();
            }
        }
        self.active.fetch_sub(1, Ordering::Relaxed);
        assert!(!self.fail.load(Ordering::Relaxed), "synthetic prefetch panic");
    }
    fn set_export_pinned(&self, pinned: bool) {
        if !pinned {
            assert_eq!(self.active.load(Ordering::Relaxed), 0);
        }
        self.pinned.store(pinned, Ordering::Relaxed);
        self.inner.set_export_pinned(pinned);
    }
    fn set_export_work(&self, scheduled: bool, output_frame: i64) {
        if !scheduled {
            assert_eq!(self.active.load(Ordering::Relaxed), 0, "join before removing membership");
        }
        self.work.lock().unwrap().push((scheduled, output_frame));
        self.inner.set_export_work(scheduled, output_frame);
    }
    fn set_export_position(&self, time: Tick) {
        assert!(self.pinned.load(Ordering::Relaxed));
        self.positions.lock().unwrap().push(time);
        self.inner.set_export_position(time);
    }
}

fn add(p: &mut Project, map: &mut SourceMap, src: SharedSource) -> ItemId {
    let id = p.add_item(
        "synthetic",
        Label::Iris,
        ItemKind::Media(Box::new(MediaClip {
            media: MediaRef::File { path: "synthetic.mp4".into() },
            info: src.info().clone(),
            interpret: Default::default(),
            mark_in: None,
            mark_out: None,
            markers: vec![],
            offline: false,
            proxy: None,
            identity: None,
        })),
        None,
    );
    map.0.insert(id, src);
    id
}

fn place(p: &mut Project, seq: ItemId, track: usize, media: ItemId, start: i64, len: i64, source_in: i64) -> filmcraft_project::ClipId {
    let r = FrameRate::FPS_24;
    let clip = p.make_track_item(media, TrackKind::Video, r.tick_of(start), TimeRange::new(r.tick_of(source_in), r.tick_of(len)), r).unwrap();
    let id = clip.id;
    p.sequence_mut(seq).unwrap().video_tracks[track].items.push(clip);
    id
}

fn fixture() -> (Arc<Project>, ItemId, SourceMap, Vec<Arc<Recording>>) {
    let r = FrameRate::FPS_24;
    let mut p = Project::new("scheduler");
    let mut map = SourceMap::default();
    let movie = Recording::new(Arc::new(GeneratorSource::new(Generator::Demo(filmcraft_media::generators::DemoScene::OceanSunset), 16, 16, r, r.tick_of(200))));
    let still = Recording::new(Arc::new(filmcraft_media::still::StillSource::from_rgba("still.png", 16, 16, vec![128; 16 * 16 * 4], false, 1024)));
    let media = add(&mut p, &mut map, movie.clone());
    // Distinct project items resolving to the same source share membership and coverage.
    let alias = add(&mut p, &mut map, movie.clone());
    let still_id = add(&mut p, &mut map, still.clone());
    let seq = p.new_sequence("timeline", SequenceSettings { width: 16, height: 16, frame_rate: r, ..Default::default() }, 3, 0, None);
    for (start, source_in, speed, reverse) in [(0, 5, 1.0, false), (6, 17, 1.5, false), (12, 29, 1.0, true), (18, 41, 1.0, false)] {
        let id = place(&mut p, seq, 0, media, start, 6, source_in);
        let clip = p.sequence_mut(seq).unwrap().find_item_mut(id).unwrap().1;
        clip.speed = speed;
        clip.reverse = reverse;
        if start == 18 {
            clip.effect_mut("time_remap").unwrap().params.get_mut("speed").unwrap().keyframes = vec![
                filmcraft_project::Keyframe::new(Tick::ZERO, ParamValue::Float(50.0)),
                filmcraft_project::Keyframe::new(r.tick_of(6), ParamValue::Float(250.0)),
            ];
        }
    }
    let id = place(&mut p, seq, 1, alias, 4, 18, 71);
    let clip = p.sequence_mut(seq).unwrap().find_item_mut(id).unwrap().1;
    clip.speed = 0.75;
    clip.time_interpolation = filmcraft_project::TimeInterpolation::FrameBlending;
    place(&mut p, seq, 2, still_id, 0, 24, 0);
    (Arc::new(p), seq, map, vec![movie, still])
}

fn advance(s: &mut Scheduler, next: i64, end: i64, map: &SourceMap) -> Result<()> {
    s.advance(next, end, map, false)
}

fn new_scheduler(p: &Project, seq: ItemId) -> Scheduler {
    let settings = ExportSettings { gpu_rendering: crate::GpuRendering::Off, ..Default::default() };
    let pipe = Arc::new(Pipeline::new(Arc::new(p.clone()), seq, &settings, false).unwrap());
    Scheduler::new(pipe)
}

#[test]
fn scheduler_eviction_position_tracks_needed_requests_while_prefetch_runs_ahead() {
    let (p, seq, map, recorded) = fixture();
    let movie = &recorded[0];
    movie.block.store(true, Ordering::Relaxed);
    let (send, started) = mpsc::channel();
    *movie.started.lock().unwrap() = Some(send);
    let mut scheduler = new_scheduler(&p, seq);
    advance(&mut scheduler, 0, 24, &map).unwrap();
    let started = started.recv_timeout(Duration::from_secs(5));
    let mut observations = Vec::new();
    for next in [0, 6, 12, 18] {
        advance(&mut scheduler, next, 24, &map).unwrap();
        let source: SharedSource = movie.clone();
        let expected = (next..24)
            .flat_map(|f| filmcraft_render::source_frame_requests(&p, seq, FrameRate::FPS_24.tick_of(f), RenderOptions::default(), &map).unwrap())
            .filter(|(item, _)| map.source(*item).is_some_and(|s| Arc::ptr_eq(&source, &s)))
            .map(|(_, r)| r.time)
            .min()
            .unwrap();
        observations.push((next, expected, movie.positions.lock().unwrap().last().copied(), movie.work.lock().unwrap().last().copied()));
    }
    scheduler.stop(true).unwrap();
    assert!(started.is_ok());
    for (next, expected, position, work) in observations {
        assert_eq!(position, Some(expected), "outstanding job at output frame {next}");
        assert_eq!(work, Some((true, next)));
    }
    assert!(!movie.pinned.load(Ordering::Relaxed));
}

#[test]
fn scheduler_prefetch_matches_distinct_renderer_requests() {
    let (p, seq, map, recorded) = fixture();
    let mut scheduler = new_scheduler(&p, seq);
    advance(&mut scheduler, 0, 24, &map).unwrap();
    scheduler.stop(false).unwrap();
    for f in 0..24 {
        filmcraft_render::render_sequence(&p, seq, FrameRate::FPS_24.tick_of(f), RenderOptions::default(), &map);
    }
    for source in recorded {
        let mut distinct = Vec::new();
        for &req in source.fetched.lock().unwrap().iter() {
            if !distinct.contains(&req) {
                distinct.push(req);
            }
        }
        assert_eq!(*source.prefetched.lock().unwrap(), distinct);
        assert_eq!(source.peak.load(Ordering::Relaxed), 1);
        assert!(!source.pinned.load(Ordering::Relaxed));
    }
}

fn output(p: Arc<Project>, seq: ItemId, map: &SourceMap, scheduled: bool, format: Format) -> Vec<u8> {
    let bytes = Arc::new(Mutex::new(Vec::new()));
    let out = bytes.clone();
    let settings = ExportSettings {
        format,
        path: "synthetic.mov".into(),
        include_audio: false,
        gpu_rendering: crate::GpuRendering::Off,
        sink: Some(crate::OutputSink(Arc::new(move |_, data| {
            *out.lock().unwrap() = data;
            Ok(())
        }))),
        ..Default::default()
    };
    let progress = Progress::default();
    let mut exporter = Exporter::new(p, seq, &settings, &progress).unwrap();
    exporter.schedule = scheduled.then(|| Scheduler::new(exporter.pipe.clone()));
    exporter.set_batch(3);
    while !matches!(exporter.step(map, &progress).unwrap(), Step::Done(_)) {}
    bytes.lock().unwrap().clone()
}

#[test]
fn scheduler_on_and_off_produce_identical_bytes() {
    let (p, seq, map, _) = fixture();
    for format in [Format::Mjpeg, Format::H264] {
        let bytes = output(p.clone(), seq, &map, false, format);
        let source = Recording::new(filmcraft_codecs::open_bytes("tiny.mov", bytes.into()).unwrap());
        let mut p = Project::new("codec cuts");
        let mut map = SourceMap::default();
        let item = add(&mut p, &mut map, source.clone());
        let alias = add(&mut p, &mut map, source.clone());
        let seq = p.new_sequence("s", SequenceSettings { width: 16, height: 16, frame_rate: FrameRate::FPS_24, ..Default::default() }, 1, 0, None);
        place(&mut p, seq, 0, item, 0, 8, 0);
        place(&mut p, seq, 0, alias, 40, 8, 16);
        let p = Arc::new(p);
        let off = output(p.clone(), seq, &map, false, format);
        let on = output(p, seq, &map, true, format);
        assert_eq!(off, on, "{format:?}");
        let cut = FrameRequest::full(FrameRate::FPS_24.tick_of(16));
        assert!(source.batches.lock().unwrap().iter().any(|batch| batch == &[cut]), "upcoming cut has its own batch");
    }
}

#[test]
fn scheduler_prerolls_a_cut_and_does_not_resubmit_its_first_request() {
    let (p, seq, mut map, recorded) = fixture();
    let mut p = (*p).clone();
    let movie = &recorded[0];
    let alias = add(&mut p, &mut map, movie.clone());
    place(&mut p, seq, 0, alias, 90, 10, 170);
    // Only the earliest upcoming cut is eligible for this source.
    place(&mut p, seq, 0, alias, 110, 10, 190);
    movie.block.store(true, Ordering::Relaxed);
    let (send, started) = mpsc::channel();
    *movie.started.lock().unwrap() = Some(send);
    let mut scheduler = new_scheduler(&p, seq);
    let initial = advance(&mut scheduler, 0, 120, &map);
    let first = started.recv_timeout(Duration::from_secs(5));
    let second = if filmcraft_codecs::gop::decode_lane_limit() > 1 { Some(started.recv_timeout(Duration::from_secs(5))) } else { None };
    let mut advances = Vec::new();
    for next in [0, 1, 1, 59, 60, 89, 90, 90] {
        advances.push(advance(&mut scheduler, next, 120, &map));
    }
    let batches = movie.batches.lock().unwrap().clone();
    movie.block.store(false, Ordering::Relaxed);
    let stopped = scheduler.stop(false);
    assert!(initial.is_ok() && first.is_ok());
    assert!(second.is_none_or(|r| r.is_ok()));
    assert!(advances.iter().all(Result::is_ok) && stopped.is_ok());
    let requests: Vec<_> = batches.iter().flatten().copied().collect();
    for request in &requests {
        assert_eq!(requests.iter().filter(|r| *r == request).count(), 1, "{request:?}");
    }
    if filmcraft_codecs::gop::decode_lane_limit() > 1 {
        assert_eq!(batches.len(), 2);
        assert_eq!(requests.iter().filter(|r| r.time == FrameRate::FPS_24.tick_of(170)).count(), 1);
        assert!(!requests.iter().any(|r| r.time == FrameRate::FPS_24.tick_of(190)));
        assert_eq!(movie.peak.load(Ordering::Relaxed), 2);
    }
    assert_eq!(movie.active.load(Ordering::Relaxed), 0);
    assert!(!movie.pinned.load(Ordering::Relaxed));
}

// A test-local delayed decoder with real public GOP lane selection, not a peak-only mock.
type DecodeGate = (mpsc::Sender<()>, mpsc::Receiver<()>);
struct LaneSamples {
    gate: Arc<Mutex<Option<DecodeGate>>>,
    fed: Arc<Mutex<Vec<(usize, i64)>>>,
    factories: AtomicUsize,
}
struct LaneDec {
    id: usize,
    gate: Arc<Mutex<Option<DecodeGate>>>,
    fed: Arc<Mutex<Vec<(usize, i64)>>>,
    held: std::collections::VecDeque<i64>,
    floor: Option<i64>,
}
impl LaneDec {
    fn picture(&self, pts: i64) -> filmcraft_codecs::video::DecodedFrame {
        let i = pts / 1000;
        filmcraft_codecs::video::DecodedFrame { pts, frame: VideoFrame::rgba8(1, 1, vec![i as u8, (i >> 8) as u8, self.id as u8, 255]), draft: false }
    }
}
impl filmcraft_codecs::video::VideoDecoder for LaneDec {
    fn decode(&mut self, _: &[u8], pts: i64) -> filmcraft_codecs::Result<Vec<filmcraft_codecs::video::DecodedFrame>> {
        if self.id == 0 && pts == 64_000 {
            let gate = self.gate.lock().unwrap().take();
            if let Some((entered, resume)) = gate {
                let _ = entered.send(());
                resume.recv_timeout(Duration::from_secs(5)).map_err(|e| filmcraft_codecs::CodecError::Decode(e.to_string()))?;
            }
        }
        self.fed.lock().unwrap().push((self.id, pts));
        self.held.push_back(pts);
        let mut out = Vec::new();
        while self.held.len() > 8 {
            let pts = self.held.pop_front().unwrap();
            if self.floor.is_none_or(|floor| pts >= floor) {
                out.push(self.picture(pts));
            }
        }
        Ok(out)
    }
    fn flush(&mut self) -> Vec<filmcraft_codecs::video::DecodedFrame> {
        let held: Vec<_> = self.held.drain(..).collect();
        held.into_iter().filter(|&pts| self.floor.is_none_or(|floor| pts >= floor)).map(|pts| self.picture(pts)).collect()
    }
    fn reset(&mut self) {
        self.held.clear();
    }
    fn set_output_floor(&mut self, floor: Option<i64>) {
        self.floor = floor;
    }
    fn name(&self) -> &str {
        "scheduler lane fixture"
    }
}
impl filmcraft_codecs::gop::VideoSamples for LaneSamples {
    fn count(&self) -> usize {
        400
    }
    fn pts(&self, i: usize) -> i64 {
        i as i64 * 1000
    }
    fn sync_before(&self, i: usize) -> usize {
        i / 200 * 200
    }
    fn sample_at(&self, t: i64) -> Option<usize> {
        usize::try_from(t / 1000).ok().filter(|&i| i < 400)
    }
    fn read(&self, _: usize) -> filmcraft_codecs::Result<Vec<u8>> {
        Ok(vec![0])
    }
    fn make_decoder(&self) -> filmcraft_codecs::Result<Box<dyn filmcraft_codecs::video::VideoDecoder>> {
        Ok(Box::new(LaneDec {
            id: self.factories.fetch_add(1, Ordering::SeqCst),
            gate: self.gate.clone(),
            fed: self.fed.clone(),
            held: Default::default(),
            floor: None,
        }))
    }
}
struct LaneSource {
    inner: GeneratorSource,
    cache: filmcraft_codecs::gop::GopCache,
    samples: LaneSamples,
    cut_done: mpsc::Sender<(i64, Vec<u8>)>,
}
impl MediaSource for LaneSource {
    fn info(&self) -> &MediaInfo {
        self.inner.info()
    }
    fn video_frame(&self, req: FrameRequest) -> filmcraft_media::Result<Arc<VideoFrame>> {
        let i = FrameRate::FPS_24.frame_at(req.time);
        let frame = self.cache.frame(&self.samples, i * 1000)?;
        if i == 220 {
            let _ = self.cut_done.send((i, frame.to_rgba8()));
        }
        Ok(frame)
    }
    fn audio(&self, start: i64, n: usize, rate: u32) -> filmcraft_media::Result<AudioBuffer> {
        self.inner.audio(start, n, rate)
    }
    fn prefetch(&self, requests: &[FrameRequest]) {
        for &request in requests {
            if cancel::cancelled() {
                break;
            }
            match self.video_frame(request) {
                Err(filmcraft_media::MediaError::Cancelled) => break,
                result => {
                    result.unwrap();
                }
            }
        }
    }
    fn set_export_pinned(&self, pinned: bool) {
        self.cache.set_export_pinned(pinned);
    }
    fn set_export_work(&self, scheduled: bool, output: i64) {
        self.cache.set_export_work(scheduled, output);
    }
    fn set_export_position(&self, time: Tick) {
        self.cache.set_export_position(&self.samples, FrameRate::FPS_24.frame_at(time) * 1000);
    }
}
struct ResumeGate(Option<mpsc::Sender<()>>);
impl Drop for ResumeGate {
    fn drop(&mut self) {
        if let Some(resume) = self.0.take() {
            let _ = resume.send(());
        }
    }
}

#[test]
fn scheduler_preroll_uses_second_lane_while_continuing_is_blocked() {
    // This test's contract needs two configured lanes; no environment mutation or forced
    // lane choice. L=1 remains covered by the sequential recording tests.
    if filmcraft_codecs::gop::decode_lane_limit() < 2 {
        return;
    }
    let r = FrameRate::FPS_24;
    let (entered, seen) = mpsc::channel();
    let (resume, wait) = mpsc::channel();
    let (cut_done, done) = mpsc::channel();
    let source = Arc::new(LaneSource {
        inner: GeneratorSource::new(Generator::BlackVideo, 1, 1, r, r.tick_of(400)),
        cache: filmcraft_codecs::gop::GopCache::new(None),
        samples: LaneSamples { gate: Arc::new(Mutex::new(Some((entered, wait)))), fed: Default::default(), factories: AtomicUsize::new(0) },
        cut_done,
    });
    let primed = source.cache.frame(&source.samples, 31_000);
    let mut p = Project::new("actual decoder identity");
    let mut map = SourceMap::default();
    let item = add(&mut p, &mut map, source.clone());
    let alias = add(&mut p, &mut map, source.clone());
    let seq = p.new_sequence("s", SequenceSettings { width: 16, height: 16, frame_rate: r, ..Default::default() }, 1, 0, None);
    place(&mut p, seq, 0, item, 0, 100, 64);
    place(&mut p, seq, 0, alias, 120, 20, 220);
    let mut scheduler = new_scheduler(&p, seq);
    let release = ResumeGate(Some(resume)); // Drops before scheduler on any unwind.
    let first = advance(&mut scheduler, 0, 180, &map); // Cut at 120 excluded by strict horizon.
    let blocked = seen.recv_timeout(Duration::from_secs(2));
    let before = done.try_recv();
    let second = advance(&mut scheduler, 1, 180, &map);
    let cut_before_release = done.recv_timeout(Duration::from_secs(2));
    let fed_before_release = source.samples.fed.lock().unwrap().clone();
    drop(release); // Unconditionally release decoder 0 before joins or failure assertions.
    let stopped = scheduler.stop(true);
    assert!(primed.is_ok() && first.is_ok() && blocked.is_ok() && second.is_ok() && stopped.is_ok());
    assert!(matches!(before, Err(mpsc::TryRecvError::Empty)));
    assert_eq!(cut_before_release.unwrap(), (220, vec![220, 0, 1, 255]), "sample 220 must finish on decoder 1 before decoder 0 resumes");
    assert!(fed_before_release.contains(&(1, 220_000)));
    assert!(!fed_before_release.contains(&(0, 64_000)));
    assert!(fed_before_release.iter().all(|&(id, pts)| (pts < 200_000) == (id == 0)));
    assert_eq!(source.samples.factories.load(Ordering::SeqCst), 2);
}

#[test]
fn scheduler_budget_and_source_limit_bound_submissions() {
    let r = FrameRate::FPS_24;
    for size in [16, 3840, 100_000] {
        let mut p = Project::new("budget");
        let mut map = SourceMap::default();
        let seq = p.new_sequence("s", SequenceSettings { width: 16, height: 16, frame_rate: r, ..Default::default() }, 10, 0, None);
        let mut sources = Vec::new();
        for track in 0..10 {
            let inner = GeneratorSource::new(Generator::BlackVideo, 16, 16, r, r.tick_of(100));
            // Advertise large frames without allocating them in this synthetic source.
            let mut info = inner.info().clone();
            let video = info.video.as_mut().unwrap();
            video.width = size;
            video.height = size;
            struct Sized {
                inner: GeneratorSource,
                info: MediaInfo,
            }
            impl MediaSource for Sized {
                fn info(&self) -> &MediaInfo {
                    &self.info
                }
                fn video_frame(&self, req: FrameRequest) -> filmcraft_media::Result<Arc<VideoFrame>> {
                    self.inner.video_frame(req)
                }
                fn audio(&self, start: i64, n: usize, rate: u32) -> filmcraft_media::Result<AudioBuffer> {
                    self.inner.audio(start, n, rate)
                }
            }
            let source = Recording::new(Arc::new(Sized { inner, info }));
            let id = add(&mut p, &mut map, source.clone());
            place(&mut p, seq, track, id, 0, 64, 0);
            place(&mut p, seq, track, id, 90, 10, 70);
            source.block.store(true, Ordering::Relaxed);
            sources.push(source);
        }
        let mut scheduler = new_scheduler(&p, seq);
        let begun = advance(&mut scheduler, 0, 100, &map);
        let expected_jobs = filmcraft_codecs::gop::decode_lane_limit().min(2);
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while size != 100_000 && sources.iter().take(8).any(|s| s.active.load(Ordering::Relaxed) < expected_jobs) && std::time::Instant::now() < deadline {
            std::thread::yield_now();
        }
        let active: Vec<_> = sources.iter().map(|s| s.active.load(Ordering::Relaxed)).collect();
        let batches: Vec<_> = sources.iter().map(|s| s.batches.lock().unwrap().clone()).collect();
        for source in &sources {
            source.block.store(false, Ordering::Relaxed);
        }
        let stopped = scheduler.stop(false);
        assert!(begun.is_ok() && stopped.is_ok());
        assert!(active.iter().filter(|&&n| n > 0).count() <= 8);
        assert!(active.iter().all(|&n| n <= expected_jobs));
        let unique_counts: Vec<_> = batches
            .iter()
            .map(|batches| {
                let requests: Vec<_> = batches.iter().flatten().copied().collect();
                for request in &requests {
                    assert_eq!(requests.iter().filter(|r| *r == request).count(), 1);
                }
                requests.len() as u64
            })
            .collect();
        assert!(unique_counts.iter().sum::<u64>() * u64::from(size) * u64::from(size) * 4 <= 1_500_000_000);
        let counts: Vec<_> = sources.iter().map(|s| s.prefetched.lock().unwrap().len() as u64).collect();
        assert!(counts.iter().filter(|&&n| n > 0).count() <= 8);
        assert!(counts.iter().sum::<u64>() * u64::from(size) * u64::from(size) * 4 <= 1_500_000_000);
        if size == 16 {
            assert_eq!(&active[..8], vec![expected_jobs; 8]);
            let count = if expected_jobs == 2 { 33 } else { 32 };
            assert_eq!(counts, [count, count, count, count, count, count, count, count, 0, 0]);
        }
    }
}

#[test]
fn scheduler_cancel_export_and_export_again_release_sessions() {
    let (p, seq, generator_map, _) = fixture();
    let fixture_bytes = output(p.clone(), seq, &generator_map, false, Format::H264);
    let inner = filmcraft_codecs::open_bytes("tiny.mp4", fixture_bytes.into()).unwrap();
    let source = Recording::new(inner);
    let mut p = Project::new("cancel");
    let mut map = SourceMap::default();
    let id = add(&mut p, &mut map, source.clone());
    let r = FrameRate::FPS_24;
    let seq = p.new_sequence("s", SequenceSettings { width: 16, height: 16, frame_rate: r, ..Default::default() }, 1, 0, None);
    place(&mut p, seq, 0, id, 0, 8, 0);
    let alias = add(&mut p, &mut map, source.clone());
    place(&mut p, seq, 0, alias, 90, 8, 16);
    let p = Arc::new(p);
    filmcraft_codecs::release_idle();
    let idle = filmcraft_codecs::live_decoders();
    let progress = Progress::default();
    let settings = ExportSettings {
        format: Format::Mjpeg,
        path: "cancel.mov".into(),
        include_audio: false,
        sink: Some(crate::OutputSink(Arc::new(|_, _| Ok(())))),
        gpu_rendering: crate::GpuRendering::Off,
        ..Default::default()
    };
    let mut exporter = Exporter::new(p.clone(), seq, &settings, &progress).unwrap();
    exporter.schedule = Some(Scheduler::new(exporter.pipe.clone()));
    exporter.set_batch(1);
    exporter.set_overlap(false);
    source.block.store(true, Ordering::Relaxed);
    let (send, started) = mpsc::channel();
    *source.started.lock().unwrap() = Some(send);
    exporter.step(&map, &progress).unwrap();
    let first = started.recv_timeout(Duration::from_secs(5));
    let second = if filmcraft_codecs::gop::decode_lane_limit() > 1 { Some(started.recv_timeout(Duration::from_secs(5))) } else { None };
    progress.cancel.store(true, Ordering::Relaxed);
    let cancelled = exporter.step(&map, &progress);
    assert!(first.is_ok() && second.is_none_or(|r| r.is_ok()));
    assert!(matches!(cancelled, Err(ExportError::Cancelled)));
    assert_eq!(source.peak.load(Ordering::Relaxed), filmcraft_codecs::gop::decode_lane_limit().min(2));
    assert_eq!(source.active.load(Ordering::Relaxed), 0);
    assert!(!source.pinned.load(Ordering::Relaxed));
    drop(exporter);
    assert_eq!(filmcraft_codecs::live_decoders(), idle);
    source.block.store(false, Ordering::Relaxed);
    assert!(!output(p, seq, &map, true, Format::Mjpeg).is_empty());
    assert_eq!(filmcraft_codecs::live_decoders(), idle);
}

#[test]
fn scheduler_drop_and_prefetch_error_release_pins() {
    for fails in [false, true] {
        let (p, seq, mut map, recorded) = fixture();
        let mut p = (*p).clone();
        let movie = &recorded[0];
        let alias = add(&mut p, &mut map, movie.clone());
        place(&mut p, seq, 0, alias, 90, 10, 170);
        movie.fail.store(fails, Ordering::Relaxed);
        movie.block.store(true, Ordering::Relaxed);
        let (send, started) = mpsc::channel();
        *movie.started.lock().unwrap() = Some(send);
        let mut scheduler = new_scheduler(&p, seq);
        let begun = advance(&mut scheduler, 0, 100, &map);
        let first = started.recv_timeout(Duration::from_secs(5));
        let second = if filmcraft_codecs::gop::decode_lane_limit() > 1 { Some(started.recv_timeout(Duration::from_secs(5))) } else { None };
        let stopped = if fails {
            movie.block.store(false, Ordering::Relaxed);
            Some(scheduler.stop(false))
        } else {
            drop(scheduler); // Cancellation releases both blocked calls.
            None
        };
        assert!(begun.is_ok() && first.is_ok() && second.is_none_or(|r| r.is_ok()));
        if let Some(result) = stopped {
            assert!(result.is_err());
        }
        assert_eq!(movie.peak.load(Ordering::Relaxed), filmcraft_codecs::gop::decode_lane_limit().min(2));
        for source in &recorded {
            assert!(!source.pinned.load(Ordering::Relaxed));
            assert_eq!(source.active.load(Ordering::Relaxed), 0);
            assert_eq!(source.work.lock().unwrap().last().map(|w| w.0), Some(false));
        }
    }
}

#[test]
fn scheduler_switch_is_read_at_setup() {
    let (p, seq, _, _) = fixture();
    let settings = ExportSettings { format: Format::Mjpeg, sink: Some(crate::OutputSink(Arc::new(|_, _| Ok(())))), ..Default::default() };
    let exporter = Exporter::new(p, seq, &settings, &Progress::default()).unwrap();
    assert_eq!(
        exporter.schedule.is_some(),
        std::env::var_os("FILMCRAFT_EXPORT_SCHEDULER").as_deref() != Some(std::ffi::OsStr::new("0")) && !cfg!(target_arch = "wasm32")
    );
}

#[test]
fn scheduler_sliding_window_deduplicates_holds_and_releases_finished_sources() {
    let (p, seq, map, recorded) = fixture();
    let mut p = (*p).clone();
    let tracks = &mut p.sequence_mut(seq).unwrap().video_tracks;
    tracks.truncate(1);
    tracks[0].items.truncate(1);
    tracks[0].items[0].duration = FrameRate::FPS_24.tick_of(96);
    tracks[0].items[0].frame_hold = Some(FrameRate::FPS_24.tick_of(5));
    rayon::ThreadPoolBuilder::new().num_threads(1).build().unwrap().install(|| {
        let mut scheduler = new_scheduler(&p, seq);
        for next in [0, 16, 32, 64] {
            advance(&mut scheduler, next, 128, &map).unwrap();
        }
        assert!(recorded[0].pinned.load(Ordering::Relaxed));
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while recorded[0].pinned.load(Ordering::Relaxed) {
            advance(&mut scheduler, 96, 128, &map).unwrap();
            assert!(std::time::Instant::now() < deadline, "prefetch did not finish");
            std::thread::yield_now();
        }
        assert_eq!(*recorded[0].prefetched.lock().unwrap(), [FrameRequest::full(FrameRate::FPS_24.tick_of(5))]);
        scheduler.stop(false).unwrap();
    });
}

#[test]
fn scheduler_publishes_output_frame_progress_119_120_and_clears_after_join() {
    let (p, seq, map, recorded) = fixture();
    let mut p = (*p).clone();
    let tracks = &mut p.sequence_mut(seq).unwrap().video_tracks;
    tracks.truncate(1);
    tracks[0].items.truncate(1);
    tracks[0].items[0].duration = FrameRate::FPS_24.tick_of(160);
    tracks[0].items[0].frame_hold = Some(FrameRate::FPS_24.tick_of(5));
    let movie = &recorded[0];
    movie.block.store(true, Ordering::Relaxed);
    let (send, started) = mpsc::channel();
    *movie.started.lock().unwrap() = Some(send);
    let mut scheduler = new_scheduler(&p, seq);
    advance(&mut scheduler, 0, 180, &map).unwrap();
    let begun = started.recv_timeout(Duration::from_secs(2));
    let at119 = advance(&mut scheduler, 119, 180, &map);
    let at120 = advance(&mut scheduler, 120, 180, &map);
    let busy_work = movie.work.lock().unwrap().clone();
    let busy_position = movie.positions.lock().unwrap().last().copied();
    let stopped = scheduler.stop(true); // Releases the mock through its cancellation flag.
    assert!(begun.is_ok());
    assert!(at119.is_ok() && at120.is_ok());
    assert!(stopped.is_ok());
    assert!(busy_work.contains(&(true, 119)) && busy_work.contains(&(true, 120)));
    assert_eq!(busy_position, Some(FrameRate::FPS_24.tick_of(5)), "source PTS holds while output-frame progress advances");
    assert_eq!(movie.work.lock().unwrap().last(), Some(&(false, 120)));
    assert_eq!(movie.active.load(Ordering::SeqCst), 0);
    assert!(!movie.pinned.load(Ordering::SeqCst));
    assert_eq!(movie.peak.load(Ordering::SeqCst), 1, "one prefetch job per source remains unchanged");
}
