//! `perf.stats`: performance counters an agent can read (headless engine part). The desktop UI
//! extends the same query with playback, frame-worker and UI timings (`ui-egui` `perf.rs`).

use serde_json::{Value, json};

use crate::Session;

/// Decoder / GOP-cache counters since the process started (they only grow: diff two readings to
/// measure an interval).
pub fn decode_json() -> Value {
    let g = filmcraft_codecs::gop_stats();
    let hw = filmcraft_codecs::hw::hw_stats();
    json!({
        "requests": g.hits + g.misses,
        "cacheHits": g.hits,
        "cacheMisses": g.misses,
        "cacheHitRate": g.hit_rate(),
        "seeks": g.seeks,
        "lanes": { "created": g.lanes_created, "peakPerSource": g.lanes_peak_per_source, "released": g.lanes_released },
        "samplesDecoded": g.decoded,
        "samplesSkipped": g.skipped,
        "draftFrames": g.draft,
        "h264Threads": filmcraft_codecs::video::h264_threads(),
        "evicted": g.evicted,
        // sources that hold a decoder right now (idle ones give theirs up beyond a cap)
        "liveDecoders": filmcraft_codecs::live_decoders(),
        // decoded frames all sources hold, and the budget idle sources are trimmed to
        "cacheMB": filmcraft_codecs::cached_bytes() as f64 / 1e6,
        "cacheBudgetMB": filmcraft_codecs::FRAME_BUDGET as f64 / 1e6,
        // plane buffers of evicted frames waiting for the next decoded pictures, and how many
        // planes were decoded into a recycled buffer
        "planePoolMB": filmcraft_frame::pool::stats().idle_bytes as f64 / 1e6,
        "planesReused": filmcraft_frame::pool::stats().reused,
        "decodeMs": g.decode_ns as f64 / 1e6,
        "decodeMsPerSample": g.decode_ms_per_sample(),
        "framesDecoded": g.frames,
        // VA-API timings, frame use, seek reasons, lock waits, and peak live decoders (not a sum).
        "submitMs": hw.submit_ns as f64 / 1e6,
        "syncMs": hw.sync_ns as f64 / 1e6,
        "getImageMs": hw.get_image_ns as f64 / 1e6,
        "copyMs": hw.copy_ns as f64 / 1e6,
        "readbacks": hw.readbacks,
        "usedFrames": g.used_frames,
        "seeksBy": {
            "cutIn": g.seeks_by_cut_in,
            "evictedFrame": g.seeks_by_evicted_frame,
            "decoderRecycled": g.seeks_by_decoder_recycled,
            "outOfOrder": g.seeks_by_out_of_order,
            "other": g.seeks_by_other,
        },
        "lockWaitMs": g.lock_wait_ns as f64 / 1e6,
        "liveDecodersPeak": filmcraft_codecs::live_decoders_peak(),
        // Settings ▸ Playback ▸ Hardware decoding: pictures from OS hardware decoders vs ours,
        // hardware decoders created, streams handed to software up front, mid-stream fallbacks.
        "hardware": {
            "enabled": filmcraft_codecs::hw::hardware_decoding(),
            // the OS decoder backend registered at startup (null where there is none)
            "backend": filmcraft_codecs::hw::hw_backend(),
            "frames": hw.frames,
            "softwareFrames": g.frames.saturating_sub(hw.frames),
            "sessions": hw.sessions,
            "declined": hw.declined,
            "fallbacks": hw.fallbacks,
        },
    })
}

/// Export counters: pictures encoded by hardware encoders, sessions, declined requests; and the wall
/// time of each export stage in milliseconds (`stages`: setup, loudness, render, encode with its
/// `convert` part, audio, mux, finish, wait), summed over the exports of this process (diff two readings).
/// Render and encode overlap (the next batch renders while this one is encoded), so their sum can exceed
/// the export's wall time; `waitMs` is the encoding side's idle wait for the render.
pub fn export_json() -> Value {
    let hw = filmcraft_export::hw_encode_stats();
    let gpu = filmcraft_export::gpu_render_stats();
    let gpu_times = filmcraft_frame::gpu_stats::snapshot();
    let stages: serde_json::Map<String, Value> =
        filmcraft_export::stage_times().into_iter().map(|(s, ns)| (format!("{}Ms", s.name()), json!(ns as f64 / 1e6))).collect();
    let render_tasks: serde_json::Map<String, Value> =
        filmcraft_frame::render_timing::times().into_iter().map(|(name, ns)| (name.to_owned(), json!(ns as f64 / 1e6))).collect();
    let outside: serde_json::Map<String, Value> =
        filmcraft_export::outside_times().into_iter().map(|(name, ns)| (name.to_owned(), json!(ns as f64 / 1e6))).collect();
    json!({
        "hardware": {"frames": hw.frames, "sessions": hw.sessions, "declined": hw.declined},
        "gpu": {
            "frames": gpu.frames,
            "fallbacks": gpu.fallbacks,
            "cpuWholeFrames": {
                "adjustment": gpu_times.adjustment,
                "transition": gpu_times.transition,
                "hdr": gpu_times.hdr,
                "planExhausted": gpu_times.plan_exhausted,
            },
            "gpuSubmitMs": gpu_times.submit_ns as f64 / 1e6,
            "gpuReadbackMs": gpu_times.map_wait_ns as f64 / 1e6,
            "gpuReadbackConvertMs": gpu_times.readback_convert_ns as f64 / 1e6,
            "gpuUploadPrepMs": gpu_times.upload_prep_ns as f64 / 1e6,
        },
        "stages": stages,
        "renderTasks": render_tasks,
        "outside": outside,
    })
}

/// The engine's `perf.stats`.
pub fn stats(s: &Session) -> Value {
    let running = s.jobs.iter().filter(|j| j.result.lock().map(|r| r.is_none()).unwrap_or(false)).count();
    json!({
        "decode": decode_json(),
        "export": export_json(),
        "media": {"openSources": s.media.open_sources()},
        "jobs": {"total": s.jobs.len(), "running": running},
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::Session;

    #[test]
    fn perf_stats_query_reports_decode_counters() {
        let mut s = Session::default();
        assert!(s.execute("perf.stats", json!({})).is_ok(), "available without a project");
        s.execute("file.openDemoProject", json!({})).unwrap();
        let undo = s.history.undo.len();
        let v = s.execute("perf.stats", json!({})).unwrap();
        for k in [
            "requests",
            "cacheHitRate",
            "seeks",
            "samplesDecoded",
            "samplesSkipped",
            "draftFrames",
            "h264Threads",
            "decodeMs",
            "decodeMsPerSample",
            "framesDecoded",
            "submitMs",
            "syncMs",
            "getImageMs",
            "copyMs",
            "readbacks",
            "usedFrames",
            "lockWaitMs",
            "liveDecodersPeak",
            "liveDecoders",
            "cacheMB",
            "cacheBudgetMB",
            "planePoolMB",
            "planesReused",
        ] {
            assert!(v["decode"][k].is_number(), "decode.{k} in {v}");
        }
        for k in ["created", "peakPerSource", "released"] {
            assert!(v["decode"]["lanes"][k].is_number(), "decode.lanes.{k} in {v}");
        }
        for k in ["cutIn", "evictedFrame", "decoderRecycled", "outOfOrder", "other"] {
            assert!(v["decode"]["seeksBy"][k].is_number(), "decode.seeksBy.{k} in {v}");
        }
        for k in ["frames", "softwareFrames", "sessions", "declined", "fallbacks"] {
            assert!(v["decode"]["hardware"][k].is_number(), "decode.hardware.{k} in {v}");
        }
        assert!(v["decode"]["hardware"]["enabled"].is_boolean());
        assert!(v["decode"]["hardware"]["backend"].is_null() || v["decode"]["hardware"]["backend"].is_string());
        for k in ["frames", "sessions", "declined"] {
            assert!(v["export"]["hardware"][k].is_number(), "export.hardware.{k} in {v}");
        }
        for k in ["setupMs", "loudnessMs", "renderMs", "encodeMs", "convertMs", "audioMs", "muxMs", "finishMs", "waitMs"] {
            assert!(v["export"]["stages"][k].is_number(), "export.stages.{k} in {v}");
        }
        for (name, _) in filmcraft_frame::render_timing::times() {
            assert!(v["export"]["renderTasks"][name].is_number(), "renderTasks.{name}");
        }
        for name in [
            "gpuInputConvertMs",
            "gpuOutputConvertMs",
            "cpuInputConvertMs",
            "cpuOutputConvertMs",
            "decoderQueueCoveredMs",
            "decoderQueueNotCoveredMs",
            "decoderQueueBehindPrefetchMs",
        ] {
            assert!(v["export"]["renderTasks"][name].is_number(), "renderTasks.{name}");
        }
        for (name, _) in filmcraft_export::outside_times() {
            assert!(v["export"]["outside"][name].is_number(), "outside.{name}");
        }
        assert!(v["media"]["openSources"].is_number());
        assert_eq!(v["jobs"]["running"], json!(0));
        assert_eq!(s.history.undo.len(), undo, "a query adds no undo step");
    }

    #[test]
    fn perf_stats_gpu_counters_are_cumulative() {
        use filmcraft_frame::gpu_stats::{self, CpuWholeFrame};
        use std::time::Duration;

        let mut s = Session::default();
        let before = s.execute("perf.stats", json!({})).unwrap();
        let keys = ["frames", "fallbacks", "gpuSubmitMs", "gpuReadbackMs", "gpuReadbackConvertMs", "gpuUploadPrepMs"];
        let reasons = ["adjustment", "transition", "hdr", "planExhausted"];
        assert!(before["export"]["gpu"]["cpuWholeFrames"].is_object());
        for key in keys {
            assert!(before["export"]["gpu"][key].is_number(), "export.gpu.{key}");
        }
        for reason in reasons {
            assert!(before["export"]["gpu"]["cpuWholeFrames"][reason].is_number(), "export.gpu.cpuWholeFrames.{reason}");
        }

        let elapsed = Duration::from_millis(1);
        gpu_stats::add_submit(elapsed);
        gpu_stats::add_map_wait(elapsed);
        gpu_stats::add_readback_convert(elapsed);
        gpu_stats::add_upload_prep(elapsed);
        for reason in [CpuWholeFrame::Adjustment, CpuWholeFrame::Transition, CpuWholeFrame::Hdr, CpuWholeFrame::PlanExhausted] {
            gpu_stats::note_cpu_whole_frame(reason);
        }

        let after = s.execute("perf.stats", json!({})).unwrap();
        for key in keys {
            assert!(after["export"]["gpu"][key].as_f64().unwrap() >= before["export"]["gpu"][key].as_f64().unwrap(), "export.gpu.{key}");
        }
        for reason in reasons {
            assert!(
                after["export"]["gpu"]["cpuWholeFrames"][reason].as_f64().unwrap() >= before["export"]["gpu"]["cpuWholeFrames"][reason].as_f64().unwrap(),
                "export.gpu.cpuWholeFrames.{reason}"
            );
        }
    }
}
