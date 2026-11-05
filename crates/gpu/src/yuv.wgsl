struct Params {
    w: u32,
    h: u32,
    y_stride: u32,
    c_stride: u32,
}

struct Tables {
    srgb: array<u32, 4096>,
    unit: array<f32, 256>,
    kr: array<f32, 256>,
    kg: array<f32, 256>,
    kb: array<f32, 256>,
}

@group(0) @binding(0) var<uniform> p: Params;
@group(0) @binding(1) var accum: texture_2d<f32>;
@group(0) @binding(2) var<storage, read> tables: Tables;
@group(0) @binding(3) var<storage, read_write> output: array<u32>;

fn srgb(v: f32) -> u32 {
    if v != v {
        return tables.srgb[0];
    }
    let idx = i32(clamp(v, 0.0, 1.0) * 4095.0 + 0.5);
    return tables.srgb[u32(idx) & 4095u];
}

fn code8(v: f32) -> u32 {
    if v != v {
        return 0u;
    }
    return u32(clamp(i32(v + 0.5), 1, 254));
}

@compute @workgroup_size(16, 16)
fn yuv420(@builtin(global_invocation_id) gid: vec3<u32>) {
    let yw = p.y_stride / 4u;
    let cw = p.c_stride / 4u;
    let ch = (p.h + 1u) / 2u;
    if gid.y >= ch || gid.x >= max((yw + 1u) / 2u, cw) {
        return;
    }
    var luma: array<u32, 4>;
    var u_word = 0u;
    var v_word = 0u;
    for (var sample = 0u; sample < 4u; sample += 1u) {
        var cb = 0.0;
        var cr = 0.0;
        var count = 0.0;
        // Match the CPU's additions: top left, top right, bottom left, bottom right.
        for (var dy = 0u; dy < 2u; dy += 1u) {
            for (var dx = 0u; dx < 2u; dx += 1u) {
                let local_x = 2u * sample + dx;
                let x = 8u * gid.x + local_x;
                let y = 2u * gid.y + dy;
                if x < p.w && y < p.h {
                    let pixel = textureLoad(accum, vec2<i32>(i32(x), i32(y)), 0);
                    let r = srgb(pixel.r);
                    let g = srgb(pixel.g);
                    let b = srgb(pixel.b);
                    let yy = (tables.kr[r] + tables.kg[g]) + tables.kb[b];
                    let word = 2u * dy + local_x / 4u;
                    luma[word] |= code8(16.0 + 219.0 * yy) << (8u * (local_x % 4u));
                    cb += (tables.unit[b] - yy) / 1.8556;
                    cr += (tables.unit[r] - yy) / 1.5748;
                    count += 1.0;
                }
            }
        }
        if count > 0.0 {
            u_word |= code8(128.0 + 224.0 * cb / count) << (8u * sample);
            v_word |= code8(128.0 + 224.0 * cr / count) << (8u * sample);
        }
    }
    for (var dy = 0u; dy < 2u; dy += 1u) {
        let y = 2u * gid.y + dy;
        for (var dx = 0u; dx < 2u; dx += 1u) {
            let x_word = 2u * gid.x + dx;
            if y < p.h && x_word < yw {
                output[y * yw + x_word] = luma[2u * dy + dx];
            }
        }
    }
    if gid.x < cw {
        let u_start = yw * p.h;
        let v_start = u_start + cw * ch;
        output[u_start + gid.y * cw + gid.x] = u_word;
        output[v_start + gid.y * cw + gid.x] = v_word;
    }
}
