// Second pass: scales the corrected frame (made at the stream's resolution by gpu_video.wgsl) to the
// window. The frame texture is sRGB, so the hardware filter blends in linear light.

struct Present {
    target_is_srgb: u32,
    _pad0: u32,
    _pad1: u32,
    _pad2: u32,
}

@group(0) @binding(0) var frame: texture_2d<f32>;
@group(0) @binding(1) var frame_sampler: sampler;
@group(0) @binding(2) var<uniform> present: Present;

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

fn linear_to_srgb(v: f32) -> f32 {
    if v <= 0.0031308 {
        return v * 12.92;
    }
    return 1.055 * pow(v, 1.0 / 2.4) - 0.055;
}

@fragment
fn fs_main(in: VertexOut) -> @location(0) vec4<f32> {
    let linear = textureSample(frame, frame_sampler, in.uv).rgb;
    if present.target_is_srgb != 0u {
        return vec4<f32>(linear, 1.0);
    }
    return vec4<f32>(linear_to_srgb(linear.r), linear_to_srgb(linear.g), linear_to_srgb(linear.b), 1.0);
}
