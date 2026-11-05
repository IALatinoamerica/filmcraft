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
        let active = self.active.fetch_add(1, Ordering::Relaxed) + 1;
        self.peak.fetch_max(active, Ordering::Relaxed);
        for &request in requests {
            if cancel::cancelled() {
                break;
            }
            self.prefetched.lock().unwrap().push(request);
            let _ = self.inner.video_frame(request);
            if let Some(started) = self.started.lock().unwrap().take() {
                started.send(()).unwrap();
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
    // Distinct project items resolving to the very same source must share one scheduler job.
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
        assert_eq!(output(p.clone(), seq, &map, false, format), output(p.clone(), seq, &map, true, format));
    }
}

#[test]
fn scheduler_prerolls_a_cut_and_does_not_resubmit_its_first_request() {
    let (p, seq, mut map, recorded) = fixture();
    let mut p = (*p).clone();
    let later = Recording::new(Arc::new(GeneratorSource::new(Generator::BlackVideo, 16, 16, FrameRate::FPS_24, FrameRate::FPS_24.tick_of(200))));
    let item = add(&mut p, &mut map, later.clone());
    place(&mut p, seq, 0, item, 90, 10, 17);
    let mut scheduler = new_scheduler(&p, seq);
    advance(&mut scheduler, 0, 100, &map).unwrap();
    // A second call while work is outstanding must never submit another job for a source.
    advance(&mut scheduler, 0, 100, &map).unwrap();
    scheduler.stop(false).unwrap();
    assert_eq!(*later.prefetched.lock().unwrap(), [FrameRequest::full(FrameRate::FPS_24.tick_of(17))]);
    assert_eq!(later.peak.load(Ordering::Relaxed), 1);
    for source in recorded {
        assert_eq!(source.peak.load(Ordering::Relaxed), 1);
    }
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
            sources.push(source);
        }
        let mut scheduler = new_scheduler(&p, seq);
        advance(&mut scheduler, 0, 64, &map).unwrap();
        scheduler.stop(false).unwrap();
        let counts: Vec<_> = sources.iter().map(|s| s.prefetched.lock().unwrap().len() as u64).collect();
        assert!(counts.iter().filter(|&&n| n > 0).count() <= 8);
        assert!(counts.iter().sum::<u64>() * u64::from(size) * u64::from(size) * 4 <= 1_500_000_000);
        if size == 16 {
            assert_eq!(counts, [32, 32, 32, 32, 32, 32, 32, 32, 0, 0]);
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
    place(&mut p, seq, 0, id, 0, 24, 0);
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
    started.recv_timeout(Duration::from_secs(10)).unwrap();
    progress.cancel.store(true, Ordering::Relaxed);
    assert!(matches!(exporter.step(&map, &progress), Err(ExportError::Cancelled)));
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
        let (p, seq, map, recorded) = fixture();
        recorded[0].fail.store(fails, Ordering::Relaxed);
        let mut scheduler = new_scheduler(&p, seq);
        advance(&mut scheduler, 0, 24, &map).unwrap();
        if fails {
            assert!(scheduler.stop(false).is_err());
        } else {
            drop(scheduler);
        }
        for source in &recorded {
            assert!(!source.pinned.load(Ordering::Relaxed));
            assert_eq!(source.active.load(Ordering::Relaxed), 0);
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
