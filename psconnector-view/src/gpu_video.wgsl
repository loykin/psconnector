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

fn linear_to_srgb(v: f32) -> f32 {
    if v <= 0.0031308 {
        return v * 12.92;
    }
    return 1.055 * pow(v, 1.0 / 2.4) - 0.055;
}

fn decode_srgb(v: vec3<f32>) -> vec3<f32> {
    return vec3<f32>(srgb_to_linear(v.r), srgb_to_linear(v.g), srgb_to_linear(v.b));
}

fn encode_srgb(v: vec3<f32>) -> vec3<f32> {
    return vec3<f32>(linear_to_srgb(v.r), linear_to_srgb(v.g), linear_to_srgb(v.b));
}

fn shift8(v: vec3<i32>) -> vec3<i32> {
    // Match Rust's signed `>> 8` exactly, including negative chroma deltas.
    return vec3<i32>(v.r >> 8u, v.g >> 8u, v.b >> 8u);
}

fn source_colour(x: i32, y_pos: i32) -> vec3<i32> {
    let sx = u32(clamp(x, 0, i32(params.width) - 1));
    let sy = u32(clamp(y_pos, 0, i32(params.height) - 1));
    let base = (sy * params.width + sx) * 3u;
    return vec3<i32>(byte_at(base), byte_at(base + 1u), byte_at(base + 2u));
}

fn corrected_encoded(input_colour: vec3<i32>) -> vec3<f32> {
    var colour = input_colour;
    let y = u32((54 * colour.r + 183 * colour.g + 19 * colour.b) >> 8);
    let entry = lut[y];
    colour = shift8(colour * entry.gain) + vec3<i32>(entry.offset);
    if entry.chroma != 256 {
        colour = vec3<i32>(entry.target_luma)
            + shift8((colour - vec3<i32>(entry.target_luma)) * entry.chroma);
    }
    colour.r = (colour.r * params.red) >> 8u;
    colour.b = (colour.b * params.blue) >> 8u;
    return vec3<f32>(clamp(colour, vec3<i32>(0), vec3<i32>(255))) / 255.0;
}

@fragment
fn fs_main(in: VertexOut) -> @location(0) vec4<f32> {
    // Match a linearly filtered sRGB texture. Sampling the packed source at one
    // integer coordinate made 1080p visibly jagged when the window was larger.
    let pixel = in.uv * vec2<f32>(f32(params.width), f32(params.height)) - vec2<f32>(0.5);
    let origin = vec2<i32>(floor(pixel));
    let fraction = fract(pixel);
    let c00 = decode_srgb(corrected_encoded(source_colour(origin.x, origin.y)));
    let c10 = decode_srgb(corrected_encoded(source_colour(origin.x + 1, origin.y)));
    let c01 = decode_srgb(corrected_encoded(source_colour(origin.x, origin.y + 1)));
    let c11 = decode_srgb(corrected_encoded(source_colour(origin.x + 1, origin.y + 1)));
    let linear = mix(mix(c00, c10, fraction.x), mix(c01, c11, fraction.x), fraction.y);
    if params.target_is_srgb != 0u {
        return vec4<f32>(linear, 1.0);
    }
    return vec4<f32>(encode_srgb(linear), 1.0);
}
