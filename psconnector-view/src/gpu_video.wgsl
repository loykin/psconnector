struct Params {
    width: u32,
    height: u32,
    target_is_srgb: u32,
    // buffer: 0 = packed RGB, 1 = BGRA, 2 = BGRA from an HDR stream
    // textures (zero-copy): 3 = NV12, 4 = BGRA, 5 = BGRA from an HDR stream
    format: u32,
    red: i32,
    blue: i32,
    full_range: u32,
    bt709: u32,
    // 1 when the picture lifts dark tones: smooth compression blocks there first (see deband_delta)
    deband: u32,
    _pad0: u32,
    _pad1: u32,
    _pad2: u32,
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
@group(0) @binding(3) var<storage, read> hdr_curve: array<u32>;
// Y (NV12) or BGRA, and CbCr (NV12)
@group(0) @binding(4) var plane0: texture_2d<f32>;
@group(0) @binding(5) var plane1: texture_2d<f32>;

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

// Same as the library's hdr_bgra_to_sdr_rgb: SDR curve, then a gentle chroma restore that fades out
// towards white.
fn hdr_to_sdr(bgr: vec3<u32>) -> vec3<i32> {
    var c = vec3<f32>(f32(hdr_curve[bgr.z]), f32(hdr_curve[bgr.y]), f32(hdr_curve[bgr.x]));
    let luma = 0.2126 * c.r + 0.7152 * c.g + 0.0722 * c.b;
    let saturation = 1.0 + 0.20 * (1.0 - luma / 255.0);
    c = vec3<f32>(luma) + (c - vec3<f32>(luma)) * saturation;
    return vec3<i32>(round(clamp(c, vec3<f32>(0.0), vec3<f32>(255.0))));
}

fn to_byte(v: f32) -> i32 {
    return i32(round(v * 255.0));
}

// Same 8-bit integer YUV→RGB as the library's software path (write_rgb8 / nv12_to_rgb).
fn yuv_to_rgb(xy: vec2<i32>) -> vec3<i32> {
    var k = vec4<i32>(403, -48, -120, 475);
    var ym = 256;
    var y_offset = 0;
    if params.full_range == 0u {
        ym = 298;
        y_offset = 16;
        if params.bt709 != 0u { k = vec4<i32>(459, -55, -136, 541); } else { k = vec4<i32>(409, -100, -208, 516); }
    } else if params.bt709 == 0u {
        k = vec4<i32>(359, -88, -183, 454);
    }
    let c = to_byte(textureLoad(plane0, xy, 0).r) - y_offset;
    let uv = textureLoad(plane1, xy / 2, 0);
    let d = to_byte(uv.r) - 128;
    let e = to_byte(uv.g) - 128;
    let base = ym * c;
    let rgb = vec3<i32>(base + k.x * e + 128, base + k.y * d + k.z * e + 128, base + k.w * d + 128);
    return clamp(shift8(rgb), vec3<i32>(0), vec3<i32>(255));
}

fn source_colour(x: i32, y_pos: i32) -> vec3<i32> {
    let sx = u32(clamp(x, 0, i32(params.width) - 1));
    let sy = u32(clamp(y_pos, 0, i32(params.height) - 1));
    if params.format >= 3u {
        let xy = vec2<i32>(i32(sx), i32(sy));
        if params.format == 3u {
            return yuv_to_rgb(xy);
        }
        let t = textureLoad(plane0, xy, 0);
        if params.format == 5u {
            return hdr_to_sdr(vec3<u32>(u32(to_byte(t.b)), u32(to_byte(t.g)), u32(to_byte(t.r))));
        }
        return vec3<i32>(to_byte(t.r), to_byte(t.g), to_byte(t.b));
    }
    let pixel = sy * params.width + sx;
    if params.format == 0u {
        let base = pixel * 3u;
        return vec3<i32>(byte_at(base), byte_at(base + 1u), byte_at(base + 2u));
    }
    let word = packed_rgb[pixel];
    let bgr = vec3<u32>(word & 255u, (word >> 8u) & 255u, (word >> 16u) & 255u);
    if params.format == 2u {
        return hdr_to_sdr(bgr);
    }
    return vec3<i32>(i32(bgr.z), i32(bgr.y), i32(bgr.x));
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

fn hash(x: u32) -> u32 {
    // PCG
    let state = x * 747796405u + 2891336453u;
    let word = ((state >> ((state >> 28u) + 4u)) ^ state) * 277803737u;
    return (word >> 22u) ^ word;
}

// 0..1, fixed per source pixel and salt, so the pattern does not crawl from frame to frame
fn rand(p: vec2<i32>, salt: u32) -> f32 {
    return f32(hash(u32(p.x) ^ hash(u32(p.y) ^ hash(salt)))) / 4294967296.0;
}

// Compression leaves dark areas as flat blocks a level or two apart; lifting shadows makes those steps
// visible. Like mpv's deband: compare the pixel with the average of four pixels at a random distance,
// and take the average when it is within a few levels (a step, not an edge). A level of grain then
// hides what is left. Returns the change to add to the source colour, so it costs one pass per
// window pixel rather than one per bilinear tap.
fn deband_delta(p: vec2<i32>) -> vec3<f32> {
    let c = source_colour(p.x, p.y);
    if (54 * c.r + 183 * c.g + 19 * c.b) >> 8 > 80 {
        return vec3<f32>(0.0);
    }
    var col = vec3<f32>(c);
    for (var it = 1; it <= 2; it++) {
        // a random offset up to 8 * it pixels in each direction (no trig needed)
        let d = vec2<f32>(rand(p, u32(it) * 2u), rand(p, u32(it) * 2u + 1u)) * 2.0 - vec2<f32>(1.0);
        let o = vec2<i32>(round(d * 8.0 * f32(it)));
        let sum = source_colour(p.x + o.x, p.y + o.y) + source_colour(p.x - o.x, p.y - o.y)
            + source_colour(p.x - o.y, p.y + o.x) + source_colour(p.x + o.y, p.y - o.x);
        let avg = vec3<f32>(sum) / 4.0;
        if all(abs(avg - col) < vec3<f32>(3.0 / f32(it))) {
            col = avg;
        }
    }
    let grain = rand(p, 7u) + rand(p, 8u) - 1.0;
    return col + vec3<f32>(grain) - vec3<f32>(c);
}

fn shifted(c: vec3<i32>, delta: vec3<f32>) -> vec3<i32> {
    return clamp(vec3<i32>(round(vec3<f32>(c) + delta)), vec3<i32>(0), vec3<i32>(255));
}

// First pass, once per stream pixel (the render target is the frame texture, at the stream's size):
// colour from the decoder, dark-area smoothing, then the picture adjustment. Written as linear light into
// an sRGB texture, which stores the adjusted 8-bit value; gpu_present.wgsl scales it to the window.
@fragment
fn fs_main(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    let p = vec2<i32>(floor(position.xy));
    var c = source_colour(p.x, p.y);
    if params.deband != 0u {
        c = shifted(c, deband_delta(p));
    }
    return vec4<f32>(decode_srgb(corrected_encoded(c)), 1.0);
}
