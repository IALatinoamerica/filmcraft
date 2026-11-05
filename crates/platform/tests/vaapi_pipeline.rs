//! Pipelined VA-API readback has the same pixels, PTS and output order as depth zero.
#![cfg(target_os = "linux")]

#[path = "it/common/mod.rs"]
mod common;

use common::{Stream, assert_same, decode_all, named, read_stream};
use filmcraft_codecs::VideoDecoder;
use filmcraft_codecs::hw::NalStreamInfo;
use filmcraft_platform::vaapi::VaDecoder;

fn hardware(s: &Stream, depth: usize) -> Option<VaDecoder> {
    let info = NalStreamInfo::from_entry(&s.entry).unwrap().unwrap();
    match VaDecoder::new_with_depth(info, depth) {
        Ok(decoder) => Some(decoder),
        Err(why) => {
            eprintln!("SKIPPED: no VA-API decoder for {}: {why}", s.entry.codec.name());
            None
        }
    }
}

fn pipeline(name: &str) {
    let ff = filmcraft_testkit::require_ffmpeg!();
    let Some(path) = named(&ff, name) else { return };
    let s = read_stream(&path);
    assert!(s.samples.windows(2).any(|w| w[0].1 > w[1].1), "fixture has reordered pictures");
    for depth in [2, 3, 4] {
        let Some(mut immediate) = hardware(&s, 0) else { return };
        let Some(mut delayed) = hardware(&s, depth) else { return };
        let (mut a, mut b) = (Vec::new(), Vec::new());
        for (sample, pts) in &s.samples {
            a.extend(immediate.decode(sample, *pts).unwrap());
            b.extend(delayed.decode(sample, *pts).unwrap());
            assert_same(name, &b, &a[..b.len()]);
        }
        assert_eq!(a.len() - b.len(), depth, "{name}: EOS has queued pictures");
        a.extend(immediate.flush());
        b.extend(delayed.flush());
        assert_eq!(a.len(), s.samples.len(), "{name}: every frame returned after EOS flush");
        assert_same(name, &b, &a);
        assert!(delayed.flush().is_empty(), "{name}: flush drains the queue fully");

        // Reset with outputs still queued, then seek to a later sync sample.
        immediate.reset();
        delayed.reset();
        a.clear();
        b.clear();
        let mid = 16;
        for (sample, pts) in &s.samples[..mid] {
            a.extend(immediate.decode(sample, *pts).unwrap());
            b.extend(delayed.decode(sample, *pts).unwrap());
        }
        assert_eq!(a.len() - b.len(), depth, "{name}: reset starts with a nonempty queue");
        immediate.reset();
        delayed.reset();
        let start = (mid..s.samples.len()).find(|&i| s.sync[i]).unwrap();
        let a = decode_all(&mut immediate, &s.samples[start..]);
        let b = decode_all(&mut delayed, &s.samples[start..]);
        assert!(!b.is_empty());
        assert_same(&format!("{name}: reset and seek with depth {depth}"), &b, &a);

        immediate.reset();
        delayed.reset();
        assert_same(&format!("{name}: restart with depth {depth}"), &decode_all(&mut delayed, &s.samples), &decode_all(&mut immediate, &s.samples));
    }
}

#[test]
fn h264_pipeline_pixels_order_pts_flush_and_reset() {
    pipeline("h264_high.mp4");
}

#[test]
fn hevc_pipeline_pixels_order_pts_flush_and_reset() {
    pipeline("hevc_main.mp4");
    pipeline("hevc_main10.mp4");
}
