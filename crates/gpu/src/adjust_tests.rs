//! Adjustment canvases use the composited-effect parity bounds: p99 <= 6, mean < 1.5.
use super::blend_tests::{device, interior, ramp_layer, read_accum, stats8};
use super::*;
use filmcraft_geom::{Affine, Vec2};
use filmcraft_project::{EffectInstance, ParamValue, find_effect};
use filmcraft_render::plan::{AdjustWeight, execute_cpu};
use filmcraft_time::{FrameRate, Tick};

const W: usize = 320;
const H: usize = 180;
const RATE: FrameRate = FrameRate::FPS_24;

fn effect(id: &str, values: &[(&str, ParamValue)]) -> EffectInstance {
    let mut e = find_effect(id).expect("effect definition").instance();
    for (key, value) in values {
        e.params.get_mut(*key).expect("effect parameter").value = value.clone();
    }
    e
}

fn fl(v: f64) -> ParamValue {
    ParamValue::Float(v)
}

fn pt(x: f64, y: f64) -> ParamValue {
    ParamValue::Vec2(Vec2::new(x, y))
}

fn weight() -> AdjustWeight {
    AdjustWeight { opacity: 1.0, frame_size: (W, H), matrix: Affine::IDENTITY, px_scale: 1.0 }
}

fn adjust(effects: Vec<EffectInstance>, time: Tick, weight: AdjustWeight, blend: Blend) -> PlanStep {
    PlanStep::Adjust { effects, time, weight, blend }
}

fn plan(adjustments: Vec<PlanStep>, scale: f32) -> FramePlan {
    let (w, h) = ((W as f32 * scale) as usize, (H as f32 * scale) as usize);
    let mut steps = vec![PlanStep::Layers(vec![PlanLayer::new(ramp_layer(w as u32, h as u32, 2), Affine::IDENTITY, 1.0, Blend::Normal)])];
    steps.extend(adjustments);
    // A later layer must draw over the adjusted canvas and retain its blend order.
    steps.push(PlanStep::Layers(vec![PlanLayer::new(
        ramp_layer(40, 24, 1),
        Affine::translate(260.0 * scale as f64, 130.0 * scale as f64),
        0.7,
        Blend::Screen,
    )]));
    FramePlan::Composite { width: w, height: h, steps }
}

fn compare(c: &mut GpuCompositor, plan: &FramePlan) {
    let FramePlan::Composite { width, height, steps } = plan else { panic!("composite plan") };
    assert!(steps.iter().any(|s| matches!(s, PlanStep::Adjust { .. })), "test must exercise Adjust");
    let cpu = execute_cpu(plan).over_black_rgba8();
    let before = c.fx.as_ref().expect("GPU FX stage").adjustments;
    c.composite_prepared(plan, Some(&prepare(plan)));
    let expected = steps.iter().filter(|s| matches!(s, PlanStep::Adjust { .. })).count();
    assert_eq!(c.fx.as_ref().expect("GPU FX stage").adjustments - before, expected, "adjustments must execute on the GPU");
    let (_, _, gpu) = c.read_output().expect("GPU readback");
    let mut outlines = Vec::new();
    for step in steps {
        match step {
            PlanStep::Layers(layers) => outlines.extend(layers.iter().cloned()),
            PlanStep::Adjust { weight, .. } => {
                outlines.push(PlanLayer::new(ramp_layer(weight.frame_size.0 as u32, weight.frame_size.1 as u32, 0), weight.matrix, 1.0, Blend::Normal));
            }
            _ => {}
        }
    }
    let keep = interior(&FramePlan::Layers { width: *width, height: *height, layers: outlines });
    let (p99, mean, max) = stats8(&cpu, &gpu, &keep);
    eprintln!("GPU adjustment executed: p99 {p99}, mean {mean:.3}, max {max}");
    assert!(p99 <= 6 && mean < 1.5, "p99 {p99}, mean {mean:.3}, max {max}");
}

#[test]
fn keyframed_zoom_pan_transform() {
    let Some((dev, q)) = device() else {
        eprintln!("no GPU adapter; skipping");
        return;
    };
    let mut c = GpuCompositor::new(&dev, &q);
    let mut tf = effect("transform", &[]);
    for (id, from, to) in [("position", pt(160.0, 90.0), pt(190.0, 76.0)), ("scale_height", fl(105.0), fl(145.0))] {
        let p = tf.params.get_mut(id).unwrap();
        p.put_keyframe(Tick::ZERO, from);
        p.put_keyframe(RATE.tick_of(24), to);
        for k in &mut p.keyframes {
            k.interp = filmcraft_project::Interpolation::Bezier;
        }
    }
    let mut previous = None;
    for frame in [0, 6, 12, 18, 24] {
        let plan = plan(vec![adjust(vec![tf.clone()], RATE.tick_of(frame), weight(), Blend::Normal)], 1.0);
        compare(&mut c, &plan);
        let image = read_accum(&c);
        assert_ne!(previous.as_ref(), Some(&image), "keyframes must change the output");
        previous = Some(image);
    }
}

#[test]
fn offset() {
    let Some((dev, q)) = device() else {
        eprintln!("no GPU adapter; skipping");
        return;
    };
    let mut c = GpuCompositor::new(&dev, &q);
    for scale in [1.0, 0.5] {
        let w = AdjustWeight { frame_size: ((W as f32 * scale) as usize, (H as f32 * scale) as usize), px_scale: scale, ..weight() };
        let fx = effect("offset", &[("shift", pt(201.0, 72.0))]);
        compare(&mut c, &plan(vec![adjust(vec![fx], Tick::ZERO, w, Blend::Normal)], scale));
    }
}

#[test]
fn directional_blur() {
    let Some((dev, q)) = device() else {
        eprintln!("no GPU adapter; skipping");
        return;
    };
    let mut c = GpuCompositor::new(&dev, &q);
    let fx = effect("directional_blur", &[("length", fl(17.0)), ("direction", fl(38.0))]);
    compare(&mut c, &plan(vec![adjust(vec![fx], Tick::ZERO, weight(), Blend::Normal)], 1.0));
}

#[test]
fn opacity_below_one_hundred() {
    let Some((dev, q)) = device() else {
        eprintln!("no GPU adapter; skipping");
        return;
    };
    let mut c = GpuCompositor::new(&dev, &q);
    let fx = effect("transform", &[("scale_height", fl(75.0)), ("position", pt(173.0, 83.0))]);
    compare(&mut c, &plan(vec![adjust(vec![fx], Tick::ZERO, AdjustWeight { opacity: 0.37, ..weight() }, Blend::Normal)], 1.0));
}

#[test]
fn motion_scaled_adjustment_frame() {
    let Some((dev, q)) = device() else {
        eprintln!("no GPU adapter; skipping");
        return;
    };
    let mut c = GpuCompositor::new(&dev, &q);
    let motion = Affine::motion(Vec2::new(154.0, 88.0), Vec2::new(0.63, 0.72), 9.0, Vec2::new(160.0, 90.0));
    let fx = effect("offset", &[("shift", pt(195.0, 70.0))]);
    compare(&mut c, &plan(vec![adjust(vec![fx], Tick::ZERO, AdjustWeight { matrix: motion, opacity: 0.8, ..weight() }, Blend::Normal)], 1.0));
}

#[test]
fn non_normal_blend() {
    let Some((dev, q)) = device() else {
        eprintln!("no GPU adapter; skipping");
        return;
    };
    let mut c = GpuCompositor::new(&dev, &q);
    let fx = effect("offset", &[("shift", pt(195.0, 70.0))]);
    compare(&mut c, &plan(vec![adjust(vec![fx], Tick::ZERO, AdjustWeight { opacity: 0.63, ..weight() }, Blend::Multiply)], 1.0));
}

#[test]
fn two_stacked_adjustment_layers() {
    let Some((dev, q)) = device() else {
        eprintln!("no GPU adapter; skipping");
        return;
    };
    let mut c = GpuCompositor::new(&dev, &q);
    let tf = effect("transform", &[("scale_height", fl(115.0)), ("position", pt(171.0, 84.0))]);
    let blur = effect("directional_blur", &[("length", fl(11.0)), ("direction", fl(73.0))]);
    let offset = effect("offset", &[("shift", pt(185.0, 100.0))]);
    compare(
        &mut c,
        &plan(
            vec![
                adjust(vec![tf], Tick::ZERO, weight(), Blend::Normal),
                adjust(vec![blur, offset], Tick::ZERO, AdjustWeight { opacity: 0.72, ..weight() }, Blend::Screen),
            ],
            1.0,
        ),
    );
}

#[test]
fn transform_empty_areas_replace_below_1524() {
    let Some((dev, q)) = device() else {
        eprintln!("no GPU adapter; skipping");
        return;
    };
    let mut c = GpuCompositor::new(&dev, &q);
    let fx = effect("transform", &[("scale_height", fl(65.0))]);
    let p = plan(vec![adjust(vec![fx], Tick::ZERO, weight(), Blend::Normal)], 1.0);
    compare(&mut c, &p);
    let cpu = execute_cpu(&p);
    let gpu = read_accum(&c);
    let i = (20 * W + 160) * 4;
    assert_eq!(cpu.px[i + 3], 0.0, "CPU transform leaves an empty area");
    assert_eq!(gpu[i + 3], 0.0, "Normal Adjust must erase the original alpha");
    assert!(gpu[i..i + 4].iter().all(|v| *v == 0.0));
}
