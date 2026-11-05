//! GPU conversion of the export accumulator to tight BT.709 limited-range YUV420 planes.

use crate::{GpuCompositor, PreparedPlan, export_renderer::timing, wait_mapped};
use filmcraft_render::plan::FramePlan;
use wgpu::util::DeviceExt;

#[derive(Clone, Copy)]
struct Layout {
    w: u32,
    h: u32,
    y_stride: u32,
    c_stride: u32,
    bytes: u32,
    groups: (u32, u32),
}

impl Layout {
    fn new(device: &wgpu::Device, w: u32, h: u32) -> Option<Self> {
        let limits = device.limits();
        if w == 0 || h == 0 || w > limits.max_texture_dimension_2d || h > limits.max_texture_dimension_2d {
            return None;
        }
        let y_stride = w.div_ceil(4).checked_mul(4)?;
        let c_stride = w.div_ceil(2).div_ceil(4).checked_mul(4)?;
        let bytes = y_stride.checked_mul(h)?.checked_add(c_stride.checked_mul(h.div_ceil(2))?.checked_mul(2)?)?;
        let groups = ((y_stride / 4).div_ceil(2).max(c_stride / 4).div_ceil(16), h.div_ceil(2).div_ceil(16));
        if u64::from(bytes) > limits.max_buffer_size
            || u64::from(bytes) > limits.max_storage_buffer_binding_size
            || groups.0 > limits.max_compute_workgroups_per_dimension
            || groups.1 > limits.max_compute_workgroups_per_dimension
        {
            return None;
        }
        Some(Self { w, h, y_stride, c_stride, bytes, groups })
    }
}

struct Buffers {
    layout: Layout,
    output: wgpu::Buffer,
    staging: wgpu::Buffer,
}

pub(crate) struct YuvStage {
    pipeline: wgpu::ComputePipeline,
    bgl: wgpu::BindGroupLayout,
    tables: wgpu::Buffer,
    buffers: Option<Buffers>,
}

impl YuvStage {
    pub(crate) fn new(device: &wgpu::Device) -> Option<Self> {
        let limits = device.limits();
        const TABLE_BYTES: u32 = (4096 + 4 * 256) * 4;
        if limits.max_compute_invocations_per_workgroup < 256
            || limits.max_compute_workgroup_size_x < 16
            || limits.max_compute_workgroup_size_y < 16
            || limits.max_storage_buffers_per_shader_stage < 2
            || limits.max_storage_buffer_binding_size < u64::from(TABLE_BYTES)
            || limits.max_buffer_size < u64::from(TABLE_BYTES)
            || limits.max_compute_workgroups_per_dimension == 0
        {
            return None;
        }
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("filmcraft-yuv"),
            source: wgpu::ShaderSource::Wgsl(include_str!("yuv.wgsl").into()),
        });
        let buffer_entry = |binding, ty| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::COMPUTE,
            ty: wgpu::BindingType::Buffer { ty, has_dynamic_offset: false, min_binding_size: None },
            count: None,
        };
        let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("yuv"),
            entries: &[
                buffer_entry(0, wgpu::BufferBindingType::Uniform),
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: false },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                buffer_entry(2, wgpu::BufferBindingType::Storage { read_only: true }),
                buffer_entry(3, wgpu::BufferBindingType::Storage { read_only: false }),
            ],
        });
        let pipeline_layout =
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor { label: Some("yuv"), bind_group_layouts: &[Some(&bgl)], immediate_size: 0 });
        let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("yuv"),
            layout: Some(&pipeline_layout),
            module: &shader,
            entry_point: Some("yuv420"),
            compilation_options: Default::default(),
            cache: None,
        });
        let mut bytes = Vec::with_capacity(TABLE_BYTES as usize);
        for &v in filmcraft_color::linear_to_srgb_u8_table() {
            bytes.extend_from_slice(&u32::from(v).to_le_bytes());
        }
        let unit: [f32; 256] = std::array::from_fn(|c| c as f32 / 255.0);
        let kr: [f32; 256] = std::array::from_fn(|c| 0.2126 * unit[c]);
        let kg: [f32; 256] = std::array::from_fn(|c| 0.7152 * unit[c]);
        let kb: [f32; 256] = std::array::from_fn(|c| 0.0722 * unit[c]);
        for table in [&unit, &kr, &kg, &kb] {
            for v in table {
                bytes.extend_from_slice(&v.to_le_bytes());
            }
        }
        let tables =
            device.create_buffer_init(&wgpu::util::BufferInitDescriptor { label: Some("yuv-tables"), contents: &bytes, usage: wgpu::BufferUsages::STORAGE });
        Some(Self { pipeline, bgl, tables, buffers: None })
    }

    /// Record the conversion and its readback copy; also used directly by the parity test.
    pub(crate) fn record_copy(&mut self, device: &wgpu::Device, enc: &mut wgpu::CommandEncoder, accum: &wgpu::TextureView, w: u32, h: u32) -> Option<()> {
        let layout = Layout::new(device, w, h)?;
        if self.buffers.as_ref().is_none_or(|b| (b.layout.w, b.layout.h) != (w, h)) {
            let buffer = |label, usage| {
                device.create_buffer(&wgpu::BufferDescriptor { label: Some(label), size: u64::from(layout.bytes), usage, mapped_at_creation: false })
            };
            self.buffers = Some(Buffers {
                layout,
                output: buffer("yuv-output", wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC),
                staging: buffer("yuv-readback", wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ),
            });
        }
        let buffers = self.buffers.as_ref()?;
        let mut params = Vec::with_capacity(16);
        for v in [w, h, layout.y_stride, layout.c_stride] {
            params.extend_from_slice(&v.to_le_bytes());
        }
        let uniform =
            device.create_buffer_init(&wgpu::util::BufferInitDescriptor { label: Some("yuv-params"), contents: &params, usage: wgpu::BufferUsages::UNIFORM });
        let bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("yuv"),
            layout: &self.bgl,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: uniform.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(accum) },
                wgpu::BindGroupEntry { binding: 2, resource: self.tables.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 3, resource: buffers.output.as_entire_binding() },
            ],
        });
        {
            let mut pass = enc.begin_compute_pass(&wgpu::ComputePassDescriptor { label: Some("yuv"), timestamp_writes: None });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &bg, &[]);
            pass.dispatch_workgroups(layout.groups.0, layout.groups.1, 1);
        }
        enc.copy_buffer_to_buffer(&buffers.output, 0, &buffers.staging, 0, u64::from(layout.bytes));
        Some(())
    }

    pub(crate) fn readback(&mut self, device: &wgpu::Device) -> Option<(u32, u32, Vec<u8>, Vec<u8>, Vec<u8>)> {
        let started = std::time::Instant::now();
        // Cache only after success, as in map_accumulator_with_wait: failed mappings retire storage.
        let buffers = self.buffers.take()?;
        let slice = buffers.staging.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        wait_mapped(device, &rx)?;
        timing::add_map_wait(started.elapsed());
        let started = std::time::Instant::now();
        let data = slice.get_mapped_range().ok()?;
        let l = buffers.layout;
        let (w, h, ys, cs) = (usize::try_from(l.w).ok()?, usize::try_from(l.h).ok()?, usize::try_from(l.y_stride).ok()?, usize::try_from(l.c_stride).ok()?);
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        let y_bytes = ys.checked_mul(h)?;
        let c_bytes = cs.checked_mul(ch)?;
        let plane = |offset: usize, width: usize, height: usize, stride: usize| -> Option<Vec<u8>> {
            let mut out = Vec::new();
            out.try_reserve_exact(width.checked_mul(height)?).ok()?;
            for row in 0..height {
                let start = offset.checked_add(row.checked_mul(stride)?)?;
                out.extend_from_slice(data.get(start..start.checked_add(width)?)?);
            }
            Some(out)
        };
        let y = plane(0, w, h, ys)?;
        let u = plane(y_bytes, cw, ch, cs)?;
        let v = plane(y_bytes.checked_add(c_bytes)?, cw, ch, cs)?;
        drop(data);
        buffers.staging.unmap();
        self.buffers = Some(buffers);
        timing::add_convert(started.elapsed());
        Some((l.w, l.h, y, u, v))
    }
}

impl GpuCompositor {
    pub(crate) fn render_export_yuv_prepared(
        &mut self,
        plan: &FramePlan,
        prep: Option<&PreparedPlan>,
        yuv: &mut YuvStage,
    ) -> Option<(u32, u32, Vec<u8>, Vec<u8>, Vec<u8>)> {
        let started = std::time::Instant::now();
        self.render_plan(plan, prep, true, false);
        let (_, view, (w, h)) = self.accum.as_ref()?;
        let mut enc = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("filmcraft-export-yuv") });
        yuv.record_copy(&self.device, &mut enc, view, *w, *h)?;
        self.queue.submit([enc.finish()]);
        timing::add_submit(started.elapsed());
        yuv.readback(&self.device)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ACCUM_FORMAT, blend_tests::device, f16_to_f32, f32_to_f16};
    use filmcraft_render::Image;

    #[test]
    fn gpu_yuv_matches_cpu_encoder_input() {
        let Some((device, queue)) = device() else {
            eprintln!("no GPU adapter; skipping");
            return;
        };
        let mut stage = YuvStage::new(&device).expect("YUV compute stage");
        for (w, h) in [(64u32, 32u32), (37, 21)] {
            let mut image = Image { w: w as usize, h: h as usize, px: Vec::new() };
            let mut bytes = Vec::new();
            for y in 0..h {
                for x in 0..w {
                    let fx = x as f32 / (w - 1) as f32;
                    let fy = y as f32 / (h - 1) as f32;
                    let rgb = match y * 4 / h {
                        0 => {
                            let vertical = y as f32 / (h.div_ceil(4) - 1) as f32;
                            [fx * 1.3 - 0.1, vertical * 1.3 - 0.1, (fx + vertical) * 0.65 - 0.1]
                        }
                        1 => [[0.0; 3], [1.0; 3], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0], [0.5; 3]][(x * 6 / w) as usize],
                        2 => [fx, fy, 1.0 - fx],
                        _ => {
                            if (x + y) % 2 == 0 {
                                [1.0, 0.0, 0.5]
                            } else {
                                [0.0, 1.0, 0.0]
                            }
                        }
                    };
                    let alpha = if y * 4 / h == 2 { if x < w / 3 { 0.0 } else { 0.375 } } else { 1.0 };
                    for v in [rgb[0] * alpha, rgb[1] * alpha, rgb[2] * alpha, alpha] {
                        let half = f32_to_f16(v);
                        bytes.extend_from_slice(&half.to_le_bytes());
                        image.px.push(f16_to_f32(half));
                    }
                }
            }
            let texture = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("yuv-test-accum"),
                size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: ACCUM_FORMAT,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });
            queue.write_texture(
                wgpu::TexelCopyTextureInfo { texture: &texture, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
                &bytes,
                wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(w * 8), rows_per_image: Some(h) },
                wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
            );
            let view = texture.create_view(&Default::default());
            let mut enc = device.create_command_encoder(&Default::default());
            stage.record_copy(&device, &mut enc, &view, w, h).expect("record YUV");
            queue.submit([enc.finish()]);
            let (gw, gh, gy, gu, gv) = stage.readback(&device).expect("read YUV");
            assert_eq!((gw, gh), (w, h));
            let (mut cy, mut cu, mut cv) = (Vec::new(), Vec::new(), Vec::new());
            filmcraft_export::rgba_to_yuv420_8(&image.over_black_rgba8(), image.w, image.h, &mut cy, &mut cu, &mut cv);
            for (name, gpu, cpu, size) in [("Y", gy, cy, w * h), ("U", gu, cu, w.div_ceil(2) * h.div_ceil(2)), ("V", gv, cv, w.div_ceil(2) * h.div_ceil(2))] {
                assert_eq!(gpu.len(), size as usize);
                assert_eq!(gpu.len(), cpu.len());
                let max = gpu.iter().zip(&cpu).map(|(a, b)| a.abs_diff(*b)).max().unwrap_or(0);
                let mismatches = gpu.iter().zip(&cpu).filter(|(a, b)| a != b).count();
                eprintln!("{w}x{h} {name}: max difference {max}, non-identical samples {mismatches}/{}", gpu.len());
                assert!(max <= 1, "{w}x{h} {name}: max difference {max}");
            }
        }
    }
}
