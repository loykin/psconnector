struct Params {
    width: u32,
    height: u32,
    target_is_srgb: u32,
    _pad0: u32,
    red: i32,
    blue: i32,
    _pad1: vec2<i32>,
}

struct LutEntry {
    gain: i32,
    offset: i32,
    target_luma: i32,
    chroma: i32,
}

@group(0) @binding(0) var<storage, read> packed_rgb: array<u32>;
@group(0) @binding(1) var<storage, read> lut: array<LutEntry>;
@group(0) @binding(2) var<uniform> params: Params;

struct VertexOut {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
}

@vertex
fn vs_main(@builtin(vertex_index) vertex: u32) -> VertexOut {
    var out: VertexOut;
    if vertex == 0u {
        out.position = vec4<f32>(-1.0, 1.0, 0.0, 1.0);
        out.uv = vec2<f32>(0.0, 0.0);
    } else if vertex == 1u {
        out.position = vec4<f32>(3.0, 1.0, 0.0, 1.0);
        out.uv = vec2<f32>(2.0, 0.0);
    } else {
        out.position = vec4<f32>(-1.0, -3.0, 0.0, 1.0);
        out.uv = vec2<f32>(0.0, 2.0);
    }
    return out;
}

fn byte_at(index: u32) -> i32 {
    let word = packed_rgb[index >> 2u];
    let shift = (index & 3u) * 8u;
    return i32((word >> shift) & 255u);
}

fn srgb_to_linear(v: f32) -> f32 {
    if v <= 0.04045 {
        return v / 12.92;
    }
    return pow((v + 0.055) / 1.055, 2.4);
}

fn shift8(v: vec3<i32>) -> vec3<i32> {
    // Match Rust's signed `>> 8` exactly, including negative chroma deltas.
    return vec3<i32>(v.r >> 8u, v.g >> 8u, v.b >> 8u);
}

@fragment
fn fs_main(in: VertexOut) -> @location(0) vec4<f32> {
    let x = min(u32(in.uv.x * f32(params.width)), params.width - 1u);
    let y_pos = min(u32(in.uv.y * f32(params.height)), params.height - 1u);
    let base = (y_pos * params.width + x) * 3u;
    var colour = vec3<i32>(byte_at(base), byte_at(base + 1u), byte_at(base + 2u));
    let y = u32((54 * colour.r + 183 * colour.g + 19 * colour.b) >> 8);
    let entry = lut[y];
    colour = shift8(colour * entry.gain) + vec3<i32>(entry.offset);
    if entry.chroma != 256 {
        colour = vec3<i32>(entry.target_luma)
            + shift8((colour - vec3<i32>(entry.target_luma)) * entry.chroma);
    }
    colour.r = (colour.r * params.red) >> 8u;
    colour.b = (colour.b * params.blue) >> 8u;
    let encoded = vec3<f32>(clamp(colour, vec3<i32>(0), vec3<i32>(255))) / 255.0;
    if params.target_is_srgb != 0u {
        return vec4<f32>(
            srgb_to_linear(encoded.r),
            srgb_to_linear(encoded.g),
            srgb_to_linear(encoded.b),
            1.0,
        );
    }
    return vec4<f32>(encoded, 1.0);
}
