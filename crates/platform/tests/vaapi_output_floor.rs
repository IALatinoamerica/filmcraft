//! Output floors through the hybrid and VA-API dispatchers: reference decoding and output
//! pixels survive skipped readbacks, including long GOPs and reordered leading pictures.
#![cfg(target_os = "linux")]

#[path = "it/common/mod.rs"]
mod common;

use std::path::Path;
use std::sync::Mutex;

use common::{Stream, assert_same, decode_all, fixture, read_stream};
use filmcraft_codecs::hw::{NalCodec, NalStreamInfo, hw_stats};
use filmcraft_codecs::{DecodedFrame, VideoDecoder};
use filmcraft_platform::HybridDecoder;
use filmcraft_platform::vaapi::VaDecoder;

// Readback counters are process-wide; this binary's hardware runs must not overlap.
static SERIAL: Mutex<()> = Mutex::new(());

fn floor_stream(ff: &Path, codec: NalCodec, leading: bool) -> Option<Stream> {
    let (encoder, name, param_flag) = match codec {
        NalCodec::H264 => ("libx264", "h264", "-x264-params"),
        NalCodec::Hevc => ("libx265", "hevc", "-x265-params"),
    };
    let keyint = if leading { 24 } else { 250 };
    let frames = if leading { "76" } else { "520" };
    let params = format!(
        "keyint={keyint}:min-keyint={keyint}:scenecut=0:bframes=3:b-adapt=0:open-gop={}{}",
        u8::from(leading),
        if codec == NalCodec::Hevc { ":log-level=error:pools=1:frame-threads=1" } else { "" }
    );
    let mut args = vec![
        "-f",
        "lavfi",
        "-i",
        "testsrc2=s=128x96:r=60",
        "-frames:v",
        frames,
        "-c:v",
        encoder,
        "-preset",
        "fast",
        param_flag,
        &params,
        "-pix_fmt",
        "yuv420p",
    ];
    if codec == NalCodec::Hevc {
        args.extend(["-tag:v", "hvc1"]);
    }
    let name = format!("va_floor_{name}_{}.mp4", if leading { "leading" } else { "gop250" });
    let path = fixture(ff, &name, &args)?;
    Some(read_stream(&path))
}

fn hardware(s: &Stream) -> Option<HybridDecoder> {
    let info = NalStreamInfo::from_entry(&s.entry).unwrap().unwrap();
    match VaDecoder::new(info.clone()) {
        Ok(va) => Some(HybridDecoder::new(Box::new(va), s.entry.clone(), info)),
        Err(why) => {
            eprintln!("SKIPPED: no VA-API decoder for {}: {why}", s.entry.codec.name());
            None
        }
    }
}

fn run(d: &mut HybridDecoder, samples: &[(Vec<u8>, i64)]) -> Vec<DecodedFrame> {
    let before = hw_stats();
    let out = decode_all(d, samples);
    let after = hw_stats();
    assert!(d.is_hardware(), "stayed in VA-API");
    assert_eq!(after.readbacks - before.readbacks, out.len() as u64, "only returned pictures are read back");
    assert_eq!(after.frames - before.frames, out.len() as u64, "only returned pictures count as hardware output");
    out
}

fn compare_floor(d: &mut HybridDecoder, samples: &[(Vec<u8>, i64)], floor: i64) {
    d.set_output_floor(None);
    d.reset();
    let reference = run(d, samples);
    assert!(reference.iter().any(|f| f.pts < floor), "reference has pre-roll pictures");
    assert!(reference.iter().any(|f| f.pts == floor), "wanted frame exists without the floor");

    d.reset();
    d.set_output_floor(Some(floor));
    let out = run(d, samples);
    assert!(out.iter().all(|f| f.pts >= floor), "nothing below the floor is returned");
    assert!(out.iter().any(|f| f.pts == floor), "wanted frame survives");
    let wanted: Vec<_> = reference.into_iter().filter(|f| f.pts >= floor).collect();
    assert_same("output floor", &out, &wanted);

    // Clearing the floor restores all output after a seek on the same decoder instance.
    d.set_output_floor(None);
    d.reset();
    let restored = run(d, samples);
    assert!(restored.iter().any(|f| f.pts < floor));
    let wanted_again: Vec<_> = restored.into_iter().filter(|f| f.pts >= floor).collect();
    assert_same("cleared output floor", &wanted_again, &wanted);
}

fn mid_gop(codec: NalCodec) {
    let ff = filmcraft_testkit::require_ffmpeg!();
    let Some(s) = floor_stream(&ff, codec, false) else { return };
    let _serial = SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(mut d) = hardware(&s) else { return };
    let syncs: Vec<_> = s.sync.iter().enumerate().filter_map(|(i, &sync)| sync.then_some(i)).collect();
    assert_eq!(&syncs[..3], &[0, 250, 500], "250-frame GOPs");
    assert!(s.samples.windows(2).any(|w| w[0].1 > w[1].1), "B-frame reordering");
    let start = syncs[1];
    let mut presentation: Vec<_> = s.samples[start..syncs[2]].iter().map(|s| s.1).collect();
    presentation.sort_unstable();
    // Seek to the previous keyframe, then decode the pre-roll to the middle of this GOP.
    compare_floor(&mut d, &s.samples[start..], presentation[125]);
}

fn leading_pictures(codec: NalCodec) {
    let ff = filmcraft_testkit::require_ffmpeg!();
    let Some(s) = floor_stream(&ff, codec, true) else { return };
    let _serial = SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(mut d) = hardware(&s) else { return };
    let info = NalStreamInfo::from_entry(&s.entry).unwrap().unwrap();
    let start = (1..s.samples.len()).find(|&i| s.sync[i]).unwrap();
    let sync_pts = s.samples[start].1;
    let types = info.nal_types(&s.samples[start].0);
    match codec {
        NalCodec::H264 => assert!(types.contains(&1) && !types.contains(&5), "open GOP starts with a non-IDR I picture"),
        NalCodec::Hevc => assert!(types.contains(&21), "open GOP starts with a CRA"),
    }
    let leading = s.samples[start + 1..]
        .iter()
        .find(|(sample, pts)| *pts < sync_pts && (codec == NalCodec::H264 || info.nal_types(sample).iter().any(|&ty| ty == 8 || ty == 9)))
        .expect("reordered leading picture (HEVC RASL)")
        .1;
    // Starting before the open GOP must preserve a wanted leading picture, even though its
    // access unit follows the I / CRA picture and its PTS precedes that picture's PTS.
    compare_floor(&mut d, &s.samples, leading);
    // Seeking directly to the I / CRA must preserve the first wanted picture after pre-roll.
    // CRA-associated RASLs are suppressed by the existing first-picture logic in both runs.
    let mut wanted: Vec<_> = s.samples[start..].iter().map(|s| s.1).filter(|&pts| pts >= sync_pts).collect();
    wanted.sort_unstable();
    compare_floor(&mut d, &s.samples[start..], wanted[1]);
}

#[test]
fn h264_output_floor_mid_gop_250() {
    mid_gop(NalCodec::H264);
}

#[test]
fn hevc_output_floor_mid_gop_250() {
    mid_gop(NalCodec::Hevc);
}

#[test]
fn h264_output_floor_open_gop_leading_pictures() {
    leading_pictures(NalCodec::H264);
}

#[test]
fn hevc_output_floor_cra_rasl() {
    leading_pictures(NalCodec::Hevc);
}
