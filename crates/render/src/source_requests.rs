use filmcraft_media::FrameRequest;
use filmcraft_project::{ItemId, ItemKind, Project, Sequence, Track, TrackItem, Transition};
use filmcraft_time::Tick;

use crate::{RenderOptions, SourceProvider};

#[derive(Clone, Copy)]
pub(crate) enum TrackLayer<'a> {
    Item(&'a TrackItem),
    Transition(&'a Transition, Option<&'a TrackItem>, Option<&'a TrackItem>),
}

pub(crate) fn track_layer_at(track: &Track, t: Tick) -> Option<TrackLayer<'_>> {
    if let Some(tr) = track.transitions.iter().find(|tr| tr.range().contains(t)) {
        let a = tr.from.and_then(|id| track.item(id)).filter(|i| i.enabled);
        let b = tr.to.and_then(|id| track.item(id)).filter(|i| i.enabled);
        Some(TrackLayer::Transition(tr, a, b))
    } else {
        track.item_at(t).filter(|i| i.enabled).map(TrackLayer::Item)
    }
}

/// Predict the successful CPU renderer's ordered `(SourceProvider item, request)` calls at `t`,
/// without fetching frames. Supply the same project snapshot, sequence id, timeline time,
/// render options and source provider as `render_sequence`. An export frame `f` uses its output
/// rate's `tick_of(f)`, which may differ from the sequence rate. Sources are queried only for info.
///
/// Best-effort hint: primary media and interpolation requests are predicted even with effects,
/// nests or captions. Frame-dependent effect lookups and duplicate fallback fetches are omitted.
/// VFR PTS lookup remains inside the source. Time Remapping keys retain the renderer's current
/// behavior: `TrackItem::source_time_at` ignores them. The result is `Some`, possibly empty.
/// Use `plan::source_frame_requests` for the GPU planner's transition order.
pub fn source_frame_requests(
    project: &Project,
    seq_id: ItemId,
    t: Tick,
    opts: RenderOptions,
    sources: &dyn SourceProvider,
) -> Option<Vec<(ItemId, FrameRequest)>> {
    for_frame(project, seq_id, t, opts, sources, false)
}

pub(crate) fn for_frame(
    project: &Project,
    seq_id: ItemId,
    t: Tick,
    opts: RenderOptions,
    sources: &dyn SourceProvider,
    planned: bool,
) -> Option<Vec<(ItemId, FrameRequest)>> {
    let mut collector = RequestCollector { project, sources, out: Vec::new() };
    if let Some(seq) = project.sequence(seq_id) {
        let planned = planned && !crate::plan::is_cpu_frame(project, seq_id, t);
        collector.collect(seq, t, opts, planned, None)?;
    }
    Some(collector.out)
}

struct RequestCollector<'a> {
    project: &'a Project,
    sources: &'a dyn SourceProvider,
    out: Vec<(ItemId, FrameRequest)>,
}

impl RequestCollector<'_> {
    fn collect(&mut self, seq: &Sequence, t: Tick, opts: RenderOptions, planned: bool, only: Option<usize>) -> Option<()> {
        if opts.depth > crate::MAX_NEST_DEPTH {
            return Some(());
        }
        for (ti, track) in seq.video_tracks.iter().enumerate() {
            if only.is_some_and(|o| o != ti) || (!track.enabled && only.is_none()) {
                continue;
            }
            let items = match track_layer_at(track, t) {
                Some(TrackLayer::Item(item)) => [Some(item), None],
                Some(TrackLayer::Transition(tr, a, b)) => {
                    if planned && matches!(tr.effect.effect.as_str(), "dip_to_black" | "dip_to_white") {
                        [if tr.progress(t) < 0.5 { a } else { b }, None]
                    } else {
                        [a, b]
                    }
                }
                None => continue,
            };
            for item in items.into_iter().flatten() {
                self.collect_item(seq, item, t, opts)?;
            }
        }
        Some(())
    }

    fn collect_item(&mut self, seq: &Sequence, item: &TrackItem, t: Tick, opts: RenderOptions) -> Option<()> {
        let project = self.project;
        let sources = self.sources;
        let Some(size) = crate::source_size(project, item.item) else { return Some(()) };
        let motion = crate::motion_matrix(seq, item, size, crate::source_par(project, item.item), item.effect_time_at(t));
        let want = crate::source_scale(&motion, opts.scale);
        match project.item(item.item).map(|p| &p.kind) {
            Some(ItemKind::Media(_) | ItemKind::Subclip { .. }) => {
                let Some(src) = sources.source(item.item) else { return Some(()) };
                let request = crate::video_frame_request(item, t, src.info().frame_rate(), want);
                let next = crate::media_interpolation(item, t, &src, seq.settings.frame_rate);
                self.out.push((item.item, request));
                if let Some((time, _)) = next {
                    self.out.push((item.item, FrameRequest { time, scale: want }));
                }
            }
            Some(ItemKind::Sequence(nested)) => {
                let sub = RenderOptions { scale: want, depth: opts.depth + 1, captions: false, working_output: true, ..opts };
                let only = match item.multicam_angle(nested) {
                    Some(angle) => match nested.angle_video_track_index(angle) {
                        Some(i) => Some(i),
                        None => return Some(()),
                    },
                    None => None,
                };
                self.collect(nested, item.source_time_at(t), sub, false, only)?;
            }
            _ => {}
        }
        Some(())
    }
}
