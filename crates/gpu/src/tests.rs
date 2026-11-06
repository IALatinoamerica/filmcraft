use super::*;
use filmcraft_geom::{Affine, Vec2};
use filmcraft_render::Blend;
use filmcraft_render::plan::execute_cpu;

fn device() -> Option<(wgpu::Device, wgpu::Queue)> {
    let instance = crate::adapter::native_instance();
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default())).ok()?;
    pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default())).ok()
}

#[test]
fn yuv_code_range_uniforms() {
    let mut mismatches = Vec::new();
    for bits in [8, 10, 12, 16] {
        let levels = (1u32 << bits) as f32;
        for range in [Range::Full, Range::Limited] {
            let src = SrcInfo {
                kind: 2,
                code_scale: if bits == 8 { 255.0 } else { levels },
                code_levels: levels,
                alpha: 0.0,
                chroma: (1, 1),
                size: (1, 1),
                color: filmcraft_color::ColorInfo { range, ..filmcraft_color::ColorInfo::REC709 },
                input: None,
            };
            let u = GpuCompositor::uniforms(&src, &Affine::IDENTITY, 1.0, Blend::Normal, (1, 1));
            // Full: 0..2^bits-1, neutral 2^(bits-1). Limited: BT.709 8-bit codes * 2^(bits-8).
            let expected = match range {
                Range::Full => [0.0, levels - 1.0, levels / 2.0, levels - 1.0],
                Range::Limited => [16.0, 219.0, 128.0, 224.0].map(|v| v * (1u32 << (bits - 8)) as f32),
            };
            if u[16..20] != expected {
                mismatches.push(format!("{bits}-bit {range:?}: {:?} != {expected:?}", &u[16..20]));
            }
        }
    }
    assert!(mismatches.is_empty(), "{}", mismatches.join("\n"));
}

#[test]
fn yuv_code_range_gpu_matches_cpu() {
    let Some((dev, q)) = device() else {
        eprintln!("no GPU adapter; skipping");
        return;
    };
    let mut c = GpuCompositor::new(&dev, &q);
    let mut prepared_compositor = GpuCompositor::new(&dev, &q);
    for bits in [8, 10, 12, 16] {
        let scale = 1u32 << (bits - 8);
        for range in [Range::Full, Range::Limited] {
            // Constant chroma isolates range normalization from CPU/GPU chroma reconstruction.
            for (u, v) in [(128, 128), (64, 192), (192, 64)] {
                let y = (0..=255).map(|code| (code * scale) as u16).collect::<Vec<_>>();
                let planes = [Arc::new(y), Arc::new(vec![(u * scale) as u16; 256]), Arc::new(vec![(v * scale) as u16; 256])];
                let data = if bits == 8 {
                    PixelData::Yuv8 { planes: planes.map(|p| Arc::new(p.iter().map(|v| *v as u8).collect())), chroma: Chroma::C444, alpha: None }
                } else {
                    PixelData::Yuv16 { planes, chroma: Chroma::C444, bits, alpha: None }
                };
                let frame = Arc::new(VideoFrame {
                    width: 256,
                    height: 1,
                    data,
                    color: filmcraft_color::ColorInfo { range, ..filmcraft_color::ColorInfo::REC709 },
                    par: (1, 1),
                    pts: Default::default(),
                });
                let cpu = frame.to_rgba8();
                let plan = FramePlan::Layers { width: 256, height: 1, layers: vec![PlanLayer::new(frame, Affine::IDENTITY, 1.0, Blend::Normal)] };
                c.composite(&plan);
                let (w, h, gpu) = c.read_output().expect("readback");
                assert_eq!((w, h, gpu.len()), (256, 1, cpu.len()));
                let error = cpu.iter().zip(&gpu).map(|(a, b)| a.abs_diff(*b)).max().expect("pixels");
                // One output-code tolerance: CPU f32 vs GPU f16 accumulator quantization.
                assert!(error <= 1, "{bits}-bit {range:?} U={u} V={v}: max error {error}");
                if u == 128 && v == 128 {
                    assert!(gpu.as_chunks::<4>().0.iter().all(|p| p[0] == p[1] && p[1] == p[2]), "neutral tint: {bits}-bit {range:?}");
                }
                let prepared = prepare(&plan);
                prepared_compositor.composite_prepared(&plan, Some(&prepared));
                assert_eq!(prepared_compositor.read_output().expect("prepared readback").2, gpu, "prepared: {bits}-bit {range:?}");
                eprintln!("range gate: {bits}-bit {range:?} U={u} V={v}, max error={error}, prepared=inline");
            }
        }
    }
}

fn yuv_frame(w: u32, h: u32) -> Arc<VideoFrame> {
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    let y: Vec<u8> = (0..w * h).map(|i| (16 + ((i % w) * 219 / w)) as u8).collect();
    let u: Vec<u8> = (0..cw * ch).map(|i| (64 + (i / cw) * 128 / ch) as u8).collect();
    let v: Vec<u8> = (0..cw * ch).map(|i| (200 - (i % cw) * 100 / cw) as u8).collect();
    Arc::new(VideoFrame {
        width: w,
        height: h,
        data: PixelData::Yuv8 { planes: [Arc::new(y), Arc::new(u), Arc::new(v)], chroma: Chroma::C420, alpha: None },
        color: filmcraft_color::ColorInfo::REC709,
        par: (1, 1),
        pts: Default::default(),
    })
}

#[test]
fn gpu_matches_cpu_plan() {
    let Some((dev, q)) = device() else {
        eprintln!("no GPU adapter; skipping");
        return;
    };
    let mut c = GpuCompositor::new(&dev, &q);
    let pixels = filmcraft_media::generators::render(
        &filmcraft_media::Generator::Demo(filmcraft_media::DemoScene::OceanSunset),
        320,
        180,
        1.0,
        24,
        filmcraft_time::FrameRate::FPS_24,
    )
    .unwrap();
    let rgba = Arc::new(VideoFrame::rgba8(320, 180, pixels));
    let (w, h) = (320usize, 180usize);
    let plan = FramePlan::Layers {
        width: w,
        height: h,
        layers: vec![
            PlanLayer { frame: yuv_frame(640, 360), matrix: Affine::scale(0.5, 0.5), opacity: 1.0, blend: Blend::Normal, fx: None, input: None },
            PlanLayer {
                frame: rgba,
                matrix: Affine::motion(Vec2::new(200.0, 100.0), Vec2::new(0.4, 0.4), 12.0, Vec2::new(160.0, 90.0)),
                opacity: 0.7,
                blend: Blend::Normal,
                fx: None,
                input: None,
            },
        ],
    };
    let cpu = execute_cpu(&plan).over_black_rgba8();
    c.composite(&plan);
    let (gw, gh, gpu) = c.read_output().expect("readback");
    assert_eq!((gw as usize, gh as usize), (w, h));
    // compare away from antialiased edges: mean abs error and 99th percentile
    let mut diffs: Vec<u32> =
        cpu.chunks(4).zip(gpu.chunks(4)).map(|(a, b)| (0..3).map(|k| (a[k] as i32 - b[k] as i32).unsigned_abs()).max().unwrap_or(0)).collect();
    diffs.sort_unstable();
    let p99 = diffs[diffs.len() * 99 / 100];
    let mean = diffs.iter().sum::<u32>() as f64 / diffs.len() as f64;
    assert!(p99 <= 6 && mean < 1.5, "p99 {p99}, mean {mean}");
    // cache: compositing the same plan again uploads nothing
    let before = c.uploaded_bytes;
    c.composite(&plan);
    assert_eq!(c.uploaded_bytes, before);
}

#[test]
fn half_float_conversion() {
    for v in [0.0f32, 1.0, 0.5, 0.123, 65504.0, -2.0, 1e-5] {
        let h = f32_to_f16(v);
        // decode
        let s = if h & 0x8000 != 0 { -1.0 } else { 1.0 };
        let e = ((h >> 10) & 0x1f) as i32;
        let m = (h & 0x3ff) as f32;
        let d = if e == 0 { s * m * 2f32.powi(-24) } else { s * (1.0 + m / 1024.0) * 2f32.powi(e - 15) };
        assert!((d - v).abs() <= v.abs() * 1e-3 + 1e-6, "{v} → {d}");
    }
}

#[test]
fn gpu_push_matches_cpu_with_alpha_and_surrounding_blends() {
    use filmcraft_project::ParamValue;
    let Some((dev, q)) = device() else {
        eprintln!("no GPU adapter; skipping");
        return;
    };
    let mut c = GpuCompositor::new(&dev, &q);
    let (w, h) = (128usize, 72usize);
    let layer = |side: usize| {
        let px = (0..w * h)
            .flat_map(|i| {
                let u = (i % w % 2) as f32;
                let v = (i / w % 2) as f32;
                let a = if side == 0 { (u + v) / 2.0 } else { 1.0 - (u + v) / 2.0 };
                [u * a, v * a, 0.5 * a, a]
            })
            .collect();
        PlanLayer::new(Arc::new(VideoFrame::rgba_f32(w as u32, h as u32, px)), Affine::IDENTITY, 0.7, Blend::Normal)
    };
    let mut worst = (0, 0.0f64);
    for direction in 0..4 {
        for blur in [0.0, 50.0, 100.0] {
            for progress in [0.0, 0.25, 0.37, 0.5, 0.75, 1.0] {
                let mut effect = filmcraft_project::find_effect("push").unwrap().instance();
                effect.param_mut("direction").unwrap().value = ParamValue::Choice(direction);
                effect.param_mut("motion_blur").unwrap().value = ParamValue::Float(blur);
                let mut above = layer(1);
                above.blend = Blend::Multiply;
                let plan = FramePlan::Composite {
                    width: w,
                    height: h,
                    steps: vec![
                        PlanStep::Layers(vec![layer(1)]),
                        PlanStep::Transition { inputs: [vec![layer(0)], vec![layer(1)]], effect, progress, scale: 1.0 },
                        PlanStep::Layers(vec![above]),
                    ],
                };
                let cpu = execute_cpu(&plan).over_black_rgba8();
                c.composite_prepared(&plan, Some(&prepare(&plan)));
                let (_, _, gpu) = c.read_output().expect("readback");
                let mut diffs: Vec<u32> =
                    cpu.as_chunks::<4>().0.iter().zip(gpu.as_chunks::<4>().0).map(|(a, b)| (0..3).map(|k| a[k].abs_diff(b[k]) as u32).max().unwrap()).collect();
                diffs.sort_unstable();
                let p99 = diffs[diffs.len() * 99 / 100];
                let mean = diffs.iter().sum::<u32>() as f64 / diffs.len() as f64;
                worst = (worst.0.max(p99), worst.1.max(mean));
                // Existing compositor acceptance band; moving edges included.
                assert!(p99 <= 6 && mean < 1.5, "direction {direction}, blur {blur}, progress {progress}: p99 {p99}, mean {mean}");
            }
        }
    }
    assert_eq!(c.gpu_transitions, 4 * 3 * 6);
    assert_eq!(c.cpu_transitions, 0);
    eprintln!("Push 72 cases: worst p99 {}, mean {}", worst.0, worst.1);
    let fallback = FramePlan::Composite {
        width: w,
        height: h,
        steps: vec![PlanStep::Transition {
            inputs: [vec![layer(0)], vec![layer(1)]],
            effect: filmcraft_project::find_effect("cross_dissolve").unwrap().instance(),
            progress: 0.37,
            scale: 1.0,
        }],
    };
    c.composite(&fallback);
    let (_, _, got) = c.read_output().expect("fallback readback");
    let expected = execute_cpu(&fallback).over_black_rgba8();
    assert!(got.iter().zip(expected).all(|(a, b)| a.abs_diff(b) <= 1));
    assert_eq!(c.cpu_transitions, 1);
    assert_eq!(c.gpu_transitions, 4 * 3 * 6);

    // Reuse both input targets across ordered transitions; preparation advances past intervening layers.
    let transition = |progress| PlanStep::Transition {
        inputs: [vec![layer(0)], vec![layer(1)]],
        effect: filmcraft_project::find_effect("push").unwrap().instance(),
        progress,
        scale: 1.0,
    };
    let mut middle = layer(0);
    middle.blend = Blend::Screen;
    let stacked = FramePlan::Composite {
        width: w,
        height: h,
        steps: vec![PlanStep::Layers(vec![layer(1)]), transition(0.25), PlanStep::Layers(vec![middle]), transition(0.75)],
    };
    c.composite_prepared(&stacked, Some(&prepare(&stacked)));
    let (_, _, gpu) = c.read_output().expect("stacked readback");
    let cpu = execute_cpu(&stacked).over_black_rgba8();
    // One byte level tolerates half-float resolve rounding.
    assert!(gpu.iter().zip(cpu).all(|(a, b)| a.abs_diff(b) <= 1), "stacked transitions exceed one byte level");
    assert_eq!(c.gpu_transitions, 4 * 3 * 6 + 2);
    assert_eq!(c.cpu_transitions, 1);
}

#[test]
fn prepared_upload_matches_inline_conversion() {
    let Some((dev, q)) = device() else {
        eprintln!("no GPU adapter; skipping");
        return;
    };
    let (w, h) = (64u32, 36u32);
    let yuv16 = Arc::new(VideoFrame {
        width: w,
        height: h,
        data: PixelData::Yuv16 {
            planes: [
                Arc::new((0..w * h).map(|i| (64 + (i * 7) % 876) as u16).collect()),
                Arc::new((0..w * h / 2).map(|i| (64 + (i * 13) % 896) as u16).collect()),
                Arc::new((0..w * h / 2).map(|i| (960 - (i * 5) % 896) as u16).collect()),
            ],
            chroma: Chroma::C422,
            bits: 10,
            alpha: None,
        },
        color: filmcraft_color::ColorInfo::REC709,
        par: (1, 1),
        pts: Default::default(),
    });
    let f32_layer: Vec<f32> = (0..w * h * 4).map(|i| ((i * 37) % 1000) as f32 / 1000.0 * if i % 4 == 3 { 1.0 } else { 0.6 }).collect();
    let rgbaf = Arc::new(VideoFrame::rgba_f32(w, h, f32_layer.clone()));
    let layers = FramePlan::Layers {
        width: w as usize,
        height: h as usize,
        layers: vec![
            PlanLayer { frame: yuv16, matrix: Affine::IDENTITY, opacity: 1.0, blend: Blend::Normal, fx: None, input: None },
            PlanLayer { frame: rgbaf, matrix: Affine::scale(0.5, 0.5), opacity: 0.8, blend: Blend::Normal, fx: None, input: None },
        ],
    };
    let image = FramePlan::Image(filmcraft_render::Image { w: w as usize, h: h as usize, px: f32_layer });
    for plan in [layers, image] {
        let mut a = GpuCompositor::new(&dev, &q);
        a.composite(&plan);
        let inline = a.read_output().expect("readback");
        let mut b = GpuCompositor::new(&dev, &q);
        let prep = prepare(&plan);
        assert!(prep.bytes() > 0);
        b.composite_prepared(&plan, Some(&prep));
        let prepared = b.read_output().expect("readback");
        assert!(inline == prepared, "prepared upload differs");
    }
}

/// The upload cache is keyed by pixel-buffer address, so it must keep the buffer alive: otherwise
/// a new frame allocated at a freed frame's address is drawn with the stale texture (seen as
/// whole frames from a previous composite in `crates/golden` GPU parity).
#[test]
fn upload_cache_keeps_buffers_alive() {
    let Some((dev, q)) = device() else {
        eprintln!("no GPU adapter; skipping");
        return;
    };
    let mut c = GpuCompositor::new(&dev, &q);
    let px = Arc::new(vec![200u8; 16 * 8 * 4]);
    let frame = Arc::new(VideoFrame { width: 16, height: 8, data: PixelData::Rgba8(px.clone()), ..(*yuv_frame(16, 8)).clone() });
    let plan = FramePlan::Layers {
        width: 16,
        height: 8,
        layers: vec![PlanLayer { frame, matrix: Affine::IDENTITY, opacity: 1.0, blend: Blend::Normal, fx: None, input: None }],
    };
    c.composite(&plan);
    drop(plan);
    assert!(Arc::strong_count(&px) > 1, "cached upload must own its pixel buffer");
}

/// The WGSL tetrahedral LUT matches `Lut3d::apply` on the CPU.
#[test]
fn gpu_lut_matches_cpu_tetrahedral() {
    let Some((dev, q)) = device() else {
        eprintln!("no GPU adapter; skipping");
        return;
    };
    let g = GpuLut::new(&dev, &q);
    for size in [2usize, 17, 33, 65] {
        let mut lut = filmcraft_color::Lut3d::from_fn(size, |c| {
            [(c[0] * c[1] + c[2] * c[2]).sin(), c[1].powf(0.45) * (1.0 - 0.3 * c[0]), (c[0] - c[2]).abs() + 0.1 * c[1]]
        });
        if size == 17 {
            lut.domain_min = [-0.1, 0.0, 0.0];
            lut.domain_max = [1.2, 1.0, 2.0];
        }
        let mut s = 99u64;
        let mut px = Vec::new();
        for _ in 0..70_000 {
            for k in 0..4 {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                px.push(if k == 3 { 0.5 } else { (s % 10_001) as f32 / 10_000.0 * 1.2 - 0.1 });
            }
        }
        let gpu = g.apply(&lut, &px).expect("gpu lut");
        let mut worst = 0f32;
        for (i, p) in px.as_chunks::<4>().0.iter().enumerate() {
            let c = lut.apply([p[0], p[1], p[2]]);
            for k in 0..3 {
                worst = worst.max((c[k] - gpu[i * 4 + k]).abs());
            }
            assert_eq!(gpu[i * 4 + 3], 0.5);
        }
        eprintln!("LUT {size}³: max |cpu − gpu| = {worst:e}");
        assert!(worst < 1e-4, "size {size}: max |cpu - gpu| = {worst}");
    }
}

fn test_masks(scale: f32) -> Vec<filmcraft_render::mask::FlatMask> {
    use filmcraft_project::{Mask, MaskMode, MaskPath, ParamValue};
    let mut a = Mask::new("a", MaskPath::ellipse(Vec2::new(150.0, 90.0), Vec2::new(90.0, 55.0)));
    a.feather.value = ParamValue::Float(24.0);
    a.expansion.value = ParamValue::Float(6.0);
    let mut b = Mask::new("b", MaskPath::polygon(&[Vec2::new(40.0, 20.0), Vec2::new(260.0, 50.0), Vec2::new(120.0, 170.0)]));
    b.mode = MaskMode::Subtract;
    b.feather.value = ParamValue::Float(0.0);
    b.opacity.value = ParamValue::Float(70.0);
    let mut c = Mask::new("c", MaskPath::rect(200.0, 100.0, 300.0, 175.0));
    c.mode = MaskMode::Difference;
    c.inverted = true;
    c.feather.value = ParamValue::Float(3.5);
    filmcraft_render::mask::prepare(&[a, b, c], filmcraft_time::Tick(0), scale)
}

#[test]
fn gpu_mask_coverage_matches_cpu() {
    let Some((dev, q)) = device() else {
        eprintln!("no GPU adapter; skipping");
        return;
    };
    let g = crate::GpuMask::new(&dev, &q);
    for scale in [1.0f32, 0.5] {
        let masks = test_masks(scale);
        let (w, h) = ((320.0 * scale) as usize, (180.0 * scale) as usize);
        let cpu = filmcraft_render::mask::coverage(&masks, w, h).unwrap();
        let gpu = g.coverage(&masks, w, h).expect("gpu coverage");
        let worst = cpu.iter().zip(&gpu).map(|(a, b)| (a - b).abs()).fold(0f32, f32::max);
        eprintln!("mask coverage @{scale}: max |cpu − gpu| = {worst:e}");
        assert!(worst < 2e-4, "scale {scale}: {worst}");
        assert!(cpu.iter().any(|v| *v > 0.99) && cpu.iter().any(|v| *v < 0.01) && cpu.iter().any(|v| *v > 0.2 && *v < 0.8));
    }
}

#[test]
fn gpu_masked_mix_matches_cpu() {
    let Some((dev, q)) = device() else {
        eprintln!("no GPU adapter; skipping");
        return;
    };
    let g = crate::GpuMask::new(&dev, &q);
    let masks = test_masks(1.0);
    let (w, h) = (320usize, 180usize);
    let original = filmcraft_render::Image { w, h, px: (0..w * h * 4).map(|i| ((i * 37) % 101) as f32 / 100.0).collect() };
    let mut effected = original.clone();
    effected.px.iter_mut().for_each(|v| *v = 1.0 - *v * 0.5);
    let gpu = g.mix(&masks, w, h, &original.px, &effected.px).expect("gpu mix");
    let cov = filmcraft_render::mask::coverage(&masks, w, h).unwrap();
    let mut cpu = effected.clone();
    filmcraft_render::mask::mix(&mut cpu, &original, &cov);
    let worst = cpu.px.iter().zip(&gpu).map(|(a, b)| (a - b).abs()).fold(0f32, f32::max);
    eprintln!("masked mix: max |cpu − gpu| = {worst:e}");
    assert!(worst < 2e-4, "{worst}");
}

/// A 4:4:4 grey picture with an alpha plane that ramps from transparent (left) to opaque (right).
fn yuv_alpha_frame(w: u32, h: u32, bits: u32) -> Arc<VideoFrame> {
    let n = (w * h) as usize;
    let max = (1u32 << bits) - 1;
    let ramp = |i: usize| ((i % w as usize) as u32 * max / (w - 1)) as u16;
    let data = if bits == 8 {
        PixelData::Yuv8 {
            planes: [Arc::new(vec![180u8; n]), Arc::new(vec![100u8; n]), Arc::new(vec![170u8; n])],
            chroma: Chroma::C444,
            alpha: Some(Arc::new((0..n).map(|i| ramp(i) as u8).collect())),
        }
    } else {
        let s = |v: u32| (v << (bits - 8)) as u16;
        PixelData::Yuv16 {
            planes: [Arc::new(vec![s(180); n]), Arc::new(vec![s(100); n]), Arc::new(vec![s(170); n])],
            chroma: Chroma::C444,
            bits,
            alpha: Some(Arc::new((0..n).map(ramp).collect())),
        }
    };
    Arc::new(VideoFrame { width: w, height: h, data, color: filmcraft_color::ColorInfo::REC709, par: (1, 1), pts: Default::default() })
}

/// A Y'CbCr layer with an alpha plane (ProRes 4444) is drawn with its alpha: the GPU result matches
/// the CPU compositor over an opaque background. (It used to draw such a layer opaque.)
#[test]
fn gpu_draws_yuv_alpha_plane_like_the_cpu() {
    let Some((dev, q)) = device() else {
        eprintln!("no GPU adapter; skipping");
        return;
    };
    let (w, h) = (320usize, 180usize);
    let pixels = filmcraft_media::generators::render(
        &filmcraft_media::Generator::Demo(filmcraft_media::DemoScene::OceanSunset),
        320,
        180,
        1.0,
        24,
        filmcraft_time::FrameRate::FPS_24,
    )
    .unwrap();
    let background = Arc::new(VideoFrame::rgba8(320, 180, pixels));
    for bits in [8, 10, 12] {
        let mut c = GpuCompositor::new(&dev, &q);
        let plan = FramePlan::Layers {
            width: w,
            height: h,
            layers: vec![
                PlanLayer { frame: background.clone(), matrix: Affine::IDENTITY, opacity: 1.0, blend: Blend::Normal, fx: None, input: None },
                PlanLayer { frame: yuv_alpha_frame(320, 180, bits), matrix: Affine::IDENTITY, opacity: 0.9, blend: Blend::Normal, fx: None, input: None },
            ],
        };
        let cpu = execute_cpu(&plan).over_black_rgba8();
        c.composite(&plan);
        let (_, _, gpu) = c.read_output().expect("readback");
        let mut diffs: Vec<u32> =
            cpu.chunks(4).zip(gpu.chunks(4)).map(|(a, b)| (0..3).map(|k| (a[k] as i32 - b[k] as i32).unsigned_abs()).max().unwrap_or(0)).collect();
        diffs.sort_unstable();
        let (p99, max) = (diffs[diffs.len() * 99 / 100], diffs[diffs.len() - 1]);
        assert!(p99 <= 3 && max <= 8, "{bits}-bit: p99 {p99}, max {max}");
        // the transparent left edge shows the background, the opaque right edge the layer
        let at = |img: &[u8], x: usize| (img[(90 * w + x) * 4], img[(90 * w + x) * 4 + 1], img[(90 * w + x) * 4 + 2]);
        let bg = execute_cpu(&FramePlan::Layers {
            width: w,
            height: h,
            layers: vec![PlanLayer { frame: background.clone(), matrix: Affine::IDENTITY, opacity: 1.0, blend: Blend::Normal, fx: None, input: None }],
        })
        .over_black_rgba8();
        let (l, r, b0) = (at(&gpu, 1), at(&gpu, w - 2), at(&bg, 1));
        assert!((l.0 as i32 - b0.0 as i32).abs() <= 6 && (l.1 as i32 - b0.1 as i32).abs() <= 6, "{bits}-bit: left edge {l:?} vs background {b0:?}");
        assert!(r != at(&bg, w - 2), "{bits}-bit: right edge {r:?} should differ from the background");
    }
}

#[test]
fn fast_half_float_conversion_matches_the_reference_for_every_value() {
    for h in 0..=u16::MAX {
        let (fast, slow) = (f16_to_f32_fast(h), f16_to_f32(h));
        assert!(fast.to_bits() == slow.to_bits() || (fast.is_nan() && slow.is_nan()), "{h:#06x}: {fast} vs {slow}");
    }
}

/// HDR and wide-gamut Y'CbCr on a Rec. 709 sequence (#965): the shader's colour management
/// (curve, HLG OOTF, BT.2020 → 709 matrix, BT.2390 tone mapping, desaturation) matches the CPU
/// running the same `InputTransform`.
#[test]
fn hdr_input_gpu_matches_cpu() {
    use filmcraft_color::{ColorInfo, ColorPipeline, ColorSpace, InputTransform, Matrix, Primaries, Transfer};
    let Some((dev, q)) = device() else {
        eprintln!("no GPU adapter; skipping");
        return;
    };
    let mut c = GpuCompositor::new(&dev, &q);
    // a 10-bit luma ramp (to the top code) under neutral, saturated and extreme chroma, 4:4:4
    let (w, h) = (256u32, 6u32);
    let chroma = [(512, 512), (300, 760), (760, 300), (64, 512), (960, 960), (512, 64)];
    let y: Vec<u16> = (0..h).flat_map(|_| (0..w).map(|x| (64 + x * 876 / 255) as u16)).collect();
    let cb: Vec<u16> = (0..h).flat_map(|r| std::iter::repeat_n(chroma[r as usize].0, w as usize)).collect();
    let cr: Vec<u16> = (0..h).flat_map(|r| std::iter::repeat_n(chroma[r as usize].1, w as usize)).collect();
    let data = PixelData::Yuv16 { planes: [Arc::new(y), Arc::new(cb), Arc::new(cr)], chroma: Chroma::C444, bits: 10, alpha: None };
    let hdr = |transfer| ColorInfo { matrix: Matrix::Bt2020Ncl, transfer, primaries: Primaries::Bt2020, range: Range::Limited };
    let cases = [
        ("HLG", hdr(Transfer::Hlg), None),
        ("PQ", hdr(Transfer::Pq), None),
        ("PQ 4000 cd/m²", hdr(Transfer::Pq), Some(4000.0)),
        ("BT.2020 SDR", ColorInfo { primaries: Primaries::Bt2020, ..ColorInfo::REC709 }, None),
    ];
    for (name, color, peak) in cases {
        let frame = Arc::new(VideoFrame { width: w, height: h, data: data.clone(), color, par: (1, 1), pts: Default::default() });
        let cs = ColorSpace::from_info(&color);
        let t = Arc::new(InputTransform::new(cs, color.range, &ColorPipeline::REC709, peak));
        assert!(t.gpu_params().is_some(), "{name}");
        let layer = PlanLayer { input: Some(t), ..PlanLayer::new(frame.clone(), Affine::IDENTITY, 1.0, Blend::Normal) };
        let plan = FramePlan::Layers { width: w as usize, height: h as usize, layers: vec![layer] };
        let cpu = execute_cpu(&plan).over_black_rgba8();
        c.composite(&plan);
        let (_, _, gpu) = c.read_output().expect("readback");
        let error = cpu.iter().zip(&gpu).map(|(a, b)| a.abs_diff(*b)).max().unwrap_or(0);
        // and the stage does something: the default decode of the same frame is far off
        let plain = FramePlan::Layers { width: w as usize, height: h as usize, layers: vec![PlanLayer::new(frame, Affine::IDENTITY, 1.0, Blend::Normal)] };
        c.composite(&plain);
        let (_, _, unmanaged) = c.read_output().expect("readback");
        let apart = cpu.iter().zip(&unmanaged).map(|(a, b)| a.abs_diff(*b)).max().unwrap_or(0);
        eprintln!("{name}: max error {error}, unmanaged differs by up to {apart}");
        assert!(error <= 2, "{name}: GPU differs from the CPU by up to {error} codes");
        assert!(apart > 20, "{name}: colour management changed nothing ({apart})");
    }
}

/// Regression for #3418: several threads creating instances and devices, using them and dropping
/// them at once deadlocked inside the NVIDIA driver (Vulkan loader critical section in
/// `vkDestroyDevice` against other devices' `create_texture`/`submit`, plus WGL threads started by
/// GL-enabled instances). Each thread makes its own instance and device, as concurrent windows and exports do.
#[test]
fn many_threads_create_use_and_drop_devices_at_once() {
    use std::sync::mpsc;
    use std::time::{Duration, Instant};
    const THREADS: usize = 16;
    const ROUNDS: usize = 4;
    let (tx, rx) = mpsc::channel();
    for _ in 0..THREADS {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let mut used = 0usize;
            for _ in 0..ROUNDS {
                let Some((device, queue)) = device() else { break };
                let tex = device.create_texture(&wgpu::TextureDescriptor {
                    label: None,
                    size: wgpu::Extent3d { width: 64, height: 64, depth_or_array_layers: 1 },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: wgpu::TextureFormat::Rgba8Unorm,
                    usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
                    view_formats: &[],
                });
                let view = tex.create_view(&wgpu::TextureViewDescriptor::default());
                let mut enc = device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
                drop(enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &view,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::RED), store: wgpu::StoreOp::Store },
                    })],
                    ..Default::default()
                }));
                queue.submit([enc.finish()]);
                let _ = device.poll(wgpu::PollType::wait_indefinitely());
                used += 1;
                drop((view, tex, queue, device));
            }
            let _ = tx.send(used);
        });
    }
    drop(tx);
    let deadline = Instant::now() + Duration::from_secs(120);
    for i in 0..THREADS {
        let left = deadline.saturating_duration_since(Instant::now());
        assert!(rx.recv_timeout(left).is_ok(), "only {i} of {THREADS} threads finished within 120 s: device setup deadlocked");
    }
}

/// Require an adapter for the explicit lifecycle regressions; absence is not a passing skip.
fn retained_readback_compositor() -> GpuCompositor {
    let (dev, q) = device().expect("GPU adapter required for explicit readback regression");
    let mut compositor = GpuCompositor::new(&dev, &q);
    compositor.accum_readback_buf = Some((
        dev.create_buffer(&wgpu::BufferDescriptor {
            label: Some("readback-cleanup-regression"),
            size: 256,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        }),
        256,
    ));
    compositor
}

/// Failed buffers must leave the cache; a new copy must recreate usable staging storage.
fn assert_readback_retired_and_rendering_recovers(compositor: &mut GpuCompositor) {
    assert!(compositor.accum_readback_buf.is_none(), "failed staging buffer remained cached");
    let plan = FramePlan::Layers { width: 1, height: 1, layers: vec![] };
    let (width, height, rgba) = compositor.render_export_prepared(&plan, None).expect("readback recovers with new staging storage");
    assert_eq!((width, height, rgba.len()), (1, 1, 4));
    assert!(compositor.accum_readback_buf.is_some(), "successful readback must retain its cache");
    let again = compositor.read_accumulator().expect("successful staging storage remains reusable");
    assert_eq!(again, (width, height, rgba));
}

/// A wait failure after a real completed map must not retain the failed staging storage.
#[test]
#[ignore = "requires a working GPU adapter for staging-buffer lifecycle verification"]
fn accumulator_readback_retires_after_failed_wait() {
    let mut compositor = retained_readback_compositor();
    assert!(
        compositor
            .map_accumulator_with_wait((1, 1, 256), |device, rx| {
                assert!(wait_mapped(device, rx).is_some());
                None
            })
            .is_none()
    );
    assert_readback_retired_and_rendering_recovers(&mut compositor);
}

/// Reject hostile conversion dimensions without retaining a successfully mapped buffer.
#[test]
#[ignore = "requires a working GPU adapter for staging-buffer lifecycle verification"]
fn accumulator_readback_retires_after_rejected_conversion() {
    let mut compositor = retained_readback_compositor();
    assert!(compositor.map_accumulator((u32::MAX, u32::MAX, 256)).is_none());
    assert_readback_retired_and_rendering_recovers(&mut compositor);
}

/// Retire a still-pending map before the next copy/readback attempt uses the cache.
#[test]
#[ignore = "requires a working GPU adapter for staging-buffer lifecycle verification"]
fn accumulator_readback_retires_pending_map_after_failed_wait() {
    let mut compositor = retained_readback_compositor();
    assert!(compositor.map_accumulator_with_wait((1, 1, 256), |_, _| None).is_none());
    assert_readback_retired_and_rendering_recovers(&mut compositor);
}

/// Real callback rejection may already leave native storage unmapped; do not unmap it twice.
#[test]
#[ignore = "requires a working GPU adapter for staging-buffer lifecycle verification"]
fn accumulator_readback_retires_rejected_native_map() {
    let mut compositor = retained_readback_compositor();
    let errors = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let observed = errors.clone();
    compositor.device.on_uncaptured_error(Arc::new(move |error| {
        observed.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push(format!("{error}"));
    }));
    compositor.accum_readback_buf.as_ref().unwrap().0.destroy();
    assert!(
        compositor
            .map_accumulator_with_wait((1, 1, 256), |device, rx| {
                device.poll(wgpu::PollType::Wait { submission_index: None, timeout: Some(MAP_TIMEOUT) }).unwrap();
                assert!(matches!(rx.recv_timeout(MAP_TIMEOUT), Ok(Err(_))), "expected an actual rejected map callback");
                None
            })
            .is_none()
    );
    assert_readback_retired_and_rendering_recovers(&mut compositor);
    assert!(
        !errors.lock().unwrap_or_else(std::sync::PoisonError::into_inner).iter().any(|error| error.contains("Buffer is not mapped")),
        "cleanup attempted to unmap an already rejected mapping"
    );
}

pub(super) fn camera_circle() -> filmcraft_project::Mask {
    let mut mask =
        filmcraft_project::Mask::new("camera", filmcraft_project::MaskPath::ellipse(Vec2::new(960.0, 560.0), Vec2::new(427.118644068, 427.118644068)));
    mask.feather.value = filmcraft_project::ParamValue::Float(1.0);
    mask
}

pub(super) struct CameraSource {
    pub generator: filmcraft_media::generators::GeneratorSource,
    pub frame: Arc<VideoFrame>,
    pub turns: u8,
}

impl filmcraft_media::MediaSource for CameraSource {
    fn info(&self) -> &filmcraft_media::MediaInfo {
        filmcraft_media::MediaSource::info(&self.generator)
    }
    fn video_frame(&self, _: filmcraft_media::FrameRequest) -> filmcraft_media::Result<Arc<VideoFrame>> {
        Ok(Arc::new(self.frame.rotated(self.turns)))
    }
    fn video_frame_unturned(&self, _: filmcraft_media::FrameRequest) -> filmcraft_media::Result<(Arc<VideoFrame>, u8)> {
        Ok((self.frame.clone(), self.turns))
    }
    fn audio(&self, start: i64, frames: usize, rate: u32) -> filmcraft_media::Result<filmcraft_frame::AudioBuffer> {
        filmcraft_media::MediaSource::audio(&self.generator, start, frames, rate)
    }
}

pub(super) fn camera_source(frame: Arc<VideoFrame>, turns: u8) -> Arc<CameraSource> {
    Arc::new(CameraSource {
        generator: filmcraft_media::generators::GeneratorSource::new(
            filmcraft_media::Generator::ColorMatte { color: [0.2, 0.4, 0.6, 1.0] },
            1920,
            1080,
            filmcraft_time::FrameRate::FPS_60,
            filmcraft_time::Tick(filmcraft_time::TICKS_PER_SECOND),
        ),
        frame,
        turns,
    })
}

pub(super) fn camera_frame() -> Arc<VideoFrame> {
    let mut f = (*yuv_frame(1920, 1080)).clone();
    // Adjacent chroma blocks vary strongly: bilinear chroma reconstruction or an extra
    // Motion prefilter must not accidentally pass on a smooth/constant test picture.
    if let PixelData::Yuv8 { planes, .. } = &mut f.data {
        planes[1] = Arc::new((0..960 * 540).map(|i| [64, 192, 96, 160][(i + i / 960) % 4]).collect());
        planes[2] = Arc::new((0..960 * 540).map(|i| [192, 64, 160, 96][(i * 3 + i / 960) % 4]).collect());
    }
    Arc::new(f)
}

pub(super) fn camera_project() -> (filmcraft_project::Project, filmcraft_project::ItemId, filmcraft_project::ItemId, filmcraft_render::SourceMap) {
    use filmcraft_media::MediaSource;
    use filmcraft_project::{ItemKind, Label, MediaClip, MediaRef, ParamValue, Project, SequenceSettings, TrackKind};
    use filmcraft_time::{FrameRate, Tick, TimeRange};
    let mut p = Project::new("masked camera");
    let source = camera_source(camera_frame(), 0);
    let media = p.add_item(
        "camera",
        Label::Iris,
        ItemKind::Media(Box::new(MediaClip {
            media: MediaRef::Generator(source.generator.generator.clone()),
            info: source.info().clone(),
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
    let seq = p.new_sequence("camera", SequenceSettings { width: 1920, height: 1080, frame_rate: FrameRate::FPS_60, ..Default::default() }, 1, 0, None);
    let mut clip =
        p.make_track_item(media, TrackKind::Video, Tick::ZERO, TimeRange::new(Tick::ZERO, FrameRate::FPS_60.tick_of(30)), FrameRate::FPS_60).unwrap();
    let motion = clip.effect_mut("motion").unwrap();
    for (id, value) in [
        ("anchor", ParamValue::Vec2(Vec2::new(960.0, 560.0))),
        ("position", ParamValue::Vec2(Vec2::new(1705.0, 856.0))),
        ("scale", ParamValue::Float(51.5079365079)),
        ("scale_width", ParamValue::Float(100.0)),
        ("uniform_scale", ParamValue::Bool(true)),
        ("rotation", ParamValue::Float(0.0)),
    ] {
        motion.param_mut(id).unwrap().value = value;
    }
    clip.effect_mut("opacity").unwrap().masks.push(camera_circle());
    p.sequence_mut(seq).unwrap().video_tracks[0].items.push(clip);
    let mut sources = filmcraft_render::SourceMap::default();
    sources.0.insert(media, source);
    (p, seq, media, sources)
}

#[test]
fn masked_camera_coverage_and_texture_multiply() {
    let (dev, q) = device().expect("real GPU for masked camera gate");
    let mut c = GpuCompositor::new(&dev, &q);
    let masks = filmcraft_render::mask::prepare(&[camera_circle()], filmcraft_time::Tick::ZERO, 1.0);
    let cpu_cov = filmcraft_render::mask::coverage(&masks, 1920, 1080).unwrap();
    let gpu_cov = GpuMask::new(&dev, &q).coverage(&masks, 1920, 1080).unwrap();
    let coverage_error = cpu_cov.iter().zip(&gpu_cov).map(|(a, b)| (a - b).abs()).fold(0.0f32, f32::max);
    let pixels: Vec<f32> = (0..1920 * 1080).flat_map(|i| [0.25, 0.5, if i % 2 == 0 { 0.125 } else { 0.75 }, 1.0]).collect();
    let frame = VideoFrame::rgba_f32(1920, 1080, pixels);
    let fx = filmcraft_render::plan::LayerFx { size: (1920, 1080), decimation: 1, turns: 0, ops: vec![], opacity_masks: masks };
    let reference = filmcraft_render::plan::effect_image(&frame, &fx);
    let (_, _, gpu) = c.effect_image(&frame, &fx).expect("recorded coverage and multiply");
    let multiply_error = reference.px.iter().zip(&gpu).map(|(a, b)| (a - b).abs()).fold(0.0f32, f32::max);
    eprintln!("masked camera: coverage max={coverage_error:e}, texture multiply max={multiply_error:e}");
    assert!(coverage_error < 2e-4 && multiply_error < 2e-4);
}

#[test]
fn masked_yuv8_isolated_conversion_matches_cpu() {
    let (dev, q) = device().expect("real GPU for masked Yuv8 gate");
    let mut c = GpuCompositor::new(&dev, &q);
    let mut mask = camera_circle();
    mask.path.value = filmcraft_project::ParamValue::Path(filmcraft_project::MaskPath::ellipse(Vec2::new(128.0, 16.0), Vec2::new(1000.0, 1000.0)));
    let fx = filmcraft_render::plan::LayerFx {
        size: (256, 32),
        decimation: 1,
        turns: 0,
        ops: vec![],
        opacity_masks: filmcraft_render::mask::prepare(&[mask], filmcraft_time::Tick::ZERO, 1.0),
    };
    for range in [Range::Limited, Range::Full] {
        for matrix in [Matrix::Bt709, Matrix::Bt601, Matrix::Bt2020Ncl] {
            let mut frame = (*yuv_frame(256, 32)).clone();
            frame.color.range = range;
            frame.color.matrix = matrix;
            if let PixelData::Yuv8 { planes, .. } = &mut frame.data {
                planes[0] = Arc::new((0..256 * 32).map(|i| (i % 256) as u8).collect());
                planes[1] = Arc::new((0..128 * 16).map(|i| ((i * 73) % 256) as u8).collect());
                planes[2] = Arc::new((0..128 * 16).map(|i| ((i * 157) % 256) as u8).collect());
            }
            let cpu = filmcraft_render::plan::effect_image(&frame, &fx).over_black_rgba8();
            let (_, _, px) = c.effect_image(&frame, &fx).unwrap();
            let gpu = filmcraft_render::Image { w: 256, h: 32, px }.over_black_rgba8();
            let max = cpu.iter().zip(&gpu).map(|(a, b)| a.abs_diff(*b)).max().unwrap();
            eprintln!("masked Yuv8 isolated {range:?} {matrix:?}: max code difference={max}");
            assert!(max <= 1);
            let key = c.upload(&frame, None);
            let src = c.src_info(&frame, key).unwrap();
            assert_eq!(GpuCompositor::uniforms(&src, &Affine::IDENTITY, 1.0, Blend::Normal, fx.size)[52], 0.0);
            let mut unmasked = fx.clone();
            unmasked.opacity_masks.clear();
            assert_eq!(GpuCompositor::fx_source_uniforms(&src, &frame, &unmasked)[52], 0.0);
            assert_eq!(GpuCompositor::fx_source_uniforms(&src, &frame, &fx)[52], 1.0);
        }
    }
}

#[test]
fn masked_camera_plan_and_device_fallback_match_render_sequence() {
    use filmcraft_render::{RenderOptions, plan::plan_frame, render_sequence};
    let (dev, q) = device().expect("real GPU for masked camera parity");
    let mut c = GpuCompositor::new(&dev, &q);
    let (p, seq, _, sources) = camera_project();
    for effects in [true, false] {
        let opts = RenderOptions { effects, ..Default::default() };
        let plan = plan_frame(&p, seq, filmcraft_time::Tick::ZERO, opts, &sources);
        let FramePlan::Layers { layers, .. } = &plan else { panic!("direct camera layers") };
        assert_eq!(layers.len(), 1);
        assert!(matches!(layers[0].frame.data, PixelData::Yuv8 { .. }));
        let fx = layers[0].fx.as_ref().unwrap();
        assert!(fx.ops.is_empty());
        assert_eq!(fx.opacity_masks.len(), 1);
        assert_eq!((fx.size, fx.decimation, fx.turns), ((1920, 1080), 1, 0));
        let prep = prepare(&plan);
        assert!(prep.layers.iter().all(Option::is_none), "Yuv8 needs no float packing");
        let cpu = render_sequence(&p, seq, filmcraft_time::Tick::ZERO, opts, &sources);
        assert_eq!(execute_cpu(&plan).px, cpu.px, "plan CPU mask order");
        for fallback in [false, true] {
            let stage = if fallback { c.fx.take() } else { None };
            c.composite_prepared(&plan, Some(&prep));
            let (_, _, px) = c.read_output().unwrap();
            let reference = cpu.over_black_rgba8();
            let mut d: Vec<u32> = reference
                .as_chunks::<4>()
                .0
                .iter()
                .zip(px.as_chunks::<4>().0)
                .map(|(a, b)| (0..3).map(|k| u32::from(a[k].abs_diff(b[k]))).max().unwrap())
                .collect();
            d.sort_unstable();
            let p99 = d[d.len() * 99 / 100];
            let mean = d.iter().sum::<u32>() as f64 / d.len() as f64;
            eprintln!("masked camera effects={effects} device fallback={fallback}: p99={p99}, mean={mean:.6}");
            assert!(p99 <= 6 && mean < 1.5);
            if fallback {
                c.fx = stage;
            }
        }
    }
}

#[test]
fn masked_camera_ineligible_cases_keep_cpu_layer() {
    use filmcraft_project::{MaskMode, MaskPath, ParamValue, TimeInterpolation};
    use filmcraft_render::{RenderOptions, plan::plan_frame};
    use filmcraft_time::Tick;
    let (base, seq, media, sources) = camera_project();
    for case in [
        "ellipse",
        "polygon",
        "open",
        "handle",
        "inverted",
        "mode",
        "feather",
        "expansion",
        "opacity",
        "second mask",
        "path key",
        "feather key",
        "expansion key",
        "opacity key",
        "standard effect",
        "rotation",
        "negative",
        "minification",
        "output scale",
        "chroma",
        "alpha",
        "rgba",
        "size",
        "turns",
        "managed",
        "interpolation",
    ] {
        let mut p = base.clone();
        let mut map = filmcraft_render::SourceMap::default();
        map.0.clone_from(&sources.0);
        let clip = &mut p.sequence_mut(seq).unwrap().video_tracks[0].items[0];
        let mask = &mut clip.effect_mut("opacity").unwrap().masks[0];
        let mut opts = RenderOptions::default();
        let mut frame = (*sources.0[&media].video_frame(filmcraft_media::FrameRequest { time: Tick::ZERO, scale: 1.0 }).unwrap()).clone();
        let mut turns = 0;
        match case {
            "ellipse" => mask.path.value = ParamValue::Path(MaskPath::ellipse(Vec2::new(960.0, 560.0), Vec2::new(427.0, 400.0))),
            "polygon" => mask.path.value = ParamValue::Path(MaskPath::rect(500.0, 100.0, 1300.0, 900.0)),
            "open" => {
                if let ParamValue::Path(path) = &mut mask.path.value {
                    path.closed = false;
                }
            }
            "handle" => {
                if let ParamValue::Path(path) = &mut mask.path.value {
                    path.vertices[0].t_out.x += 1.0;
                }
            }
            "inverted" => mask.inverted = true,
            "mode" => mask.mode = MaskMode::Subtract,
            "feather" => mask.feather.value = ParamValue::Float(2.0),
            "expansion" => mask.expansion.value = ParamValue::Float(1.0),
            "opacity" => mask.opacity.value = ParamValue::Float(50.0),
            "second mask" => clip.effect_mut("opacity").unwrap().masks.push(camera_circle()),
            "path key" => mask.path.toggle_animation(Tick::ZERO),
            "feather key" => mask.feather.toggle_animation(Tick::ZERO),
            "expansion key" => mask.expansion.toggle_animation(Tick::ZERO),
            "opacity key" => mask.opacity.toggle_animation(Tick::ZERO),
            "standard effect" => {
                clip.effects.push(filmcraft_project::find_effect("black_white").unwrap().instance());
                opts.effects = false;
            }
            "rotation" => clip.effect_mut("motion").unwrap().param_mut("rotation").unwrap().value = ParamValue::Float(5.0),
            "negative" => clip.effect_mut("motion").unwrap().param_mut("scale").unwrap().value = ParamValue::Float(-60.0),
            "minification" => clip.effect_mut("motion").unwrap().param_mut("scale").unwrap().value = ParamValue::Float(50.0),
            "output scale" => opts.scale = 0.5,
            "chroma" => {
                if let PixelData::Yuv8 { chroma, .. } = &mut frame.data {
                    *chroma = Chroma::C444;
                }
            }
            "alpha" => {
                if let PixelData::Yuv8 { alpha, .. } = &mut frame.data {
                    *alpha = Some(Arc::new(vec![255; 1920 * 1080]));
                }
            }
            "rgba" => frame = VideoFrame::rgba8(1920, 1080, vec![255; 1920 * 1080 * 4]),
            "size" => frame = (*yuv_frame(960, 540)).clone(),
            "turns" => turns = 2,
            "managed" => {
                frame.color.transfer = Transfer::Pq;
                frame.color.primaries = filmcraft_color::Primaries::Bt2020;
            }
            "interpolation" => {
                clip.speed = 0.7;
                clip.time_interpolation = TimeInterpolation::FrameBlending;
            }
            _ => unreachable!(),
        }
        if case == "chroma" {
            // Valid 4:4:4 planes, still deliberately ineligible.
            if let PixelData::Yuv8 { planes, .. } = &mut frame.data {
                planes[1] = Arc::new(vec![90; 1920 * 1080]);
                planes[2] = Arc::new(vec![160; 1920 * 1080]);
            }
        }
        map.0.insert(media, camera_source(Arc::new(frame), turns));
        let plan = plan_frame(&p, seq, filmcraft_time::FrameRate::FPS_60.tick_of(1), opts, &map);
        let FramePlan::Layers { layers, .. } = plan else { panic!("{case}: layer fallback") };
        assert_eq!(layers.len(), 1, "{case}");
        assert!(layers[0].fx.is_none() && matches!(layers[0].frame.data, PixelData::RgbaF32(_)), "{case}: must retain CPU layer fallback");
    }
}
