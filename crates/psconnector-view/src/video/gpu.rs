use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use bytemuck::{Pod, Zeroable};
use eframe::egui;
use eframe::egui_wgpu::{CallbackResources, CallbackTrait, RenderState, ScreenDescriptor};
use eframe::wgpu;
use eframe::wgpu::util::DeviceExt;
use psconnector::Frame;

use crate::video::picture::Lut;

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Params {
    width: u32,
    height: u32,
    target_is_srgb: u32,
    /// From the buffer: 0 = packed RGB, 1 = BGRA, 2 = BGRA from an HDR stream (SDR curve applied here).
    /// From textures (zero-copy): 3 = NV12, 4 = BGRA, 5 = BGRA from an HDR stream.
    format: u32,
    red: i32,
    blue: i32,
    /// NV12 only: sample range and matrix for YUV→RGB
    full_range: u32,
    bt709: u32,
    /// 1 = smooth compression blocks in dark areas before the adjustment (it lifts them into view)
    deband: u32,
    _pad: [u32; 3],
}

struct Pending {
    frame: Arc<Frame>,
    table: [[i32; 4]; 256],
    multipliers: [i32; 2],
    picture_version: u64,
}

struct Shared {
    pending: Mutex<Option<Pending>>,
    target_is_srgb: bool,
    prepare_count: AtomicU32,
    prepare_nanos: AtomicU64,
}

#[derive(Clone)]
pub(crate) struct GpuVideo {
    shared: Arc<Shared>,
}

impl GpuVideo {
    pub(crate) fn new(render_state: &RenderState) -> Self {
        let resources = Resources::new(&render_state.device, render_state.target_format);
        render_state.renderer.write().callback_resources.insert(resources);
        log::info!("video renderer: GPU picture shader ({:?})", render_state.target_format);
        Self {
            shared: Arc::new(Shared {
                pending: Mutex::new(None),
                target_is_srgb: render_state.target_format.is_srgb(),
                prepare_count: AtomicU32::new(0),
                prepare_nanos: AtomicU64::new(0),
            }),
        }
    }

    pub(crate) fn submit(&self, frame: Arc<Frame>, lut: &Lut, picture_version: u64) {
        let (table, multipliers) = lut.gpu_table();
        *self.shared.pending.lock().unwrap() = Some(Pending { frame, table, multipliers, picture_version });
    }

    pub(crate) fn paint_callback(&self, rect: egui::Rect) -> egui::PaintCallback {
        eframe::egui_wgpu::Callback::new_paint_callback(rect, VideoCallback { shared: self.shared.clone() })
    }

    pub(crate) fn take_prepare_stats(&self) -> (u32, f64) {
        let count = self.shared.prepare_count.swap(0, Ordering::Relaxed);
        let nanos = self.shared.prepare_nanos.swap(0, Ordering::Relaxed);
        let ms = if count == 0 { 0.0 } else { nanos as f64 / 1_000_000.0 / f64::from(count) };
        (count, ms)
    }
}

struct VideoCallback {
    shared: Arc<Shared>,
}

impl CallbackTrait for VideoCallback {
    fn prepare(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        _screen_descriptor: &ScreenDescriptor,
        egui_encoder: &mut wgpu::CommandEncoder,
        callback_resources: &mut CallbackResources,
    ) -> Vec<wgpu::CommandBuffer> {
        let Some(pending) = self.shared.pending.lock().unwrap().take() else {
            return Vec::new();
        };
        let started = Instant::now();
        let resources = callback_resources.get_mut::<Resources>().expect("GPU video resources");
        resources.upload(device, queue, &pending, self.shared.target_is_srgb);
        resources.correct(device, egui_encoder, pending.frame.width as u32, pending.frame.height as u32);
        self.shared.prepare_count.fetch_add(1, Ordering::Relaxed);
        self.shared.prepare_nanos.fetch_add(started.elapsed().as_nanos() as u64, Ordering::Relaxed);
        Vec::new()
    }

    fn paint(
        &self,
        _info: egui::PaintCallbackInfo,
        render_pass: &mut wgpu::RenderPass<'static>,
        callback_resources: &CallbackResources,
    ) {
        let resources = callback_resources.get::<Resources>().expect("GPU video resources");
        let Some(corrected) = &resources.corrected else { return };
        render_pass.set_pipeline(&resources.present_pipeline);
        render_pass.set_bind_group(0, &corrected.bind_group, &[]);
        render_pass.draw(0..3, 0..1);
    }
}

/// Whether the adjustment brightens dark tones noticeably (then compression blocks there become visible).
fn lifts_shadows(table: &[[i32; 4]; 256]) -> bool {
    // [2] is the target luminance for each input luminance
    table[24][2] * 4 >= 24 * 5
}

/// A decoded plane read by the shader with textureLoad (zero-copy frames).
fn plane_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Float { filterable: false },
            view_dimension: wgpu::TextureViewDimension::D2,
            multisampled: false,
        },
        count: None,
    }
}

fn tiny_texture(device: &wgpu::Device, format: wgpu::TextureFormat) -> wgpu::TextureView {
    device
        .create_texture(&wgpu::TextureDescriptor {
            label: Some("psconnector unused plane"),
            size: wgpu::Extent3d { width: 1, height: 1, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        })
        .create_view(&Default::default())
}

struct Resources {
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    rgb: wgpu::Buffer,
    lut: wgpu::Buffer,
    params: wgpu::Buffer,
    hdr_curve: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
    rgb_capacity: u64,
    picture_version: Option<u64>,
    /// bound when the frame comes from the buffer
    unused_planes: [wgpu::TextureView; 2],
    /// whether the bind group points at a frame's textures (rebuilt for the buffer when not)
    bound_textures: bool,
    /// draws the decoder's own buffers without a copy, where the platform allows
    native: Option<crate::video::native::NativeFrames>,
    present_pipeline: wgpu::RenderPipeline,
    present_layout: wgpu::BindGroupLayout,
    present_params: wgpu::Buffer,
    sampler: wgpu::Sampler,
    /// the adjusted frame at the stream's size, scaled to the window by the present pass
    corrected: Option<Corrected>,
}

/// Adjusted frame: 8-bit sRGB so the window's scaling filter blends in linear light.
const CORRECTED_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;

struct Corrected {
    view: wgpu::TextureView,
    bind_group: wgpu::BindGroup,
    size: (u32, u32),
}

fn present_pipeline(
    device: &wgpu::Device,
    target_format: wgpu::TextureFormat,
) -> (wgpu::RenderPipeline, wgpu::BindGroupLayout) {
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("psconnector present shader"),
        source: wgpu::ShaderSource::Wgsl(include_str!("gpu_present.wgsl").into()),
    });
    let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("psconnector present layout"),
        entries: &[
            wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 1,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 2,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            },
        ],
    });
    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("psconnector present pipeline layout"),
        bind_group_layouts: &[Some(&layout)],
        immediate_size: 0,
    });
    let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("psconnector present pipeline"),
        layout: Some(&pipeline_layout),
        vertex: wgpu::VertexState {
            module: &shader,
            entry_point: Some("vs_main"),
            compilation_options: Default::default(),
            buffers: &[],
        },
        fragment: Some(wgpu::FragmentState {
            module: &shader,
            entry_point: Some("fs_main"),
            compilation_options: Default::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format: target_format,
                blend: None,
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        multiview_mask: None,
        cache: None,
    });
    (pipeline, layout)
}

impl Resources {
    fn new(device: &wgpu::Device, target_format: wgpu::TextureFormat) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("psconnector video picture shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("gpu_video.wgsl").into()),
        });
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("psconnector video bind group layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage { read_only: true },
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage { read_only: true },
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage { read_only: true },
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                plane_entry(4),
                plane_entry(5),
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("psconnector video pipeline layout"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("psconnector video pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: CORRECTED_FORMAT,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });
        let (present_pipeline, present_layout) = present_pipeline(device, target_format);
        let present_params = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("psconnector present params"),
            contents: bytemuck::cast_slice(&[u32::from(target_format.is_srgb()), 0, 0, 0]),
            usage: wgpu::BufferUsages::UNIFORM,
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("psconnector frame sampler"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let rgb_capacity = 4;
        let rgb = Self::rgb_buffer(device, rgb_capacity);
        let lut = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("psconnector video LUT"),
            size: (256 * std::mem::size_of::<[i32; 4]>()) as u64,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let params = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("psconnector video params"),
            contents: bytemuck::bytes_of(&Params::zeroed()),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });
        let curve: Vec<u32> = psconnector::hdr_sdr_curve().iter().map(|&v| u32::from(v)).collect();
        let hdr_curve = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("psconnector HDR to SDR curve"),
            contents: bytemuck::cast_slice(&curve),
            usage: wgpu::BufferUsages::STORAGE,
        });
        let unused_planes =
            [tiny_texture(device, wgpu::TextureFormat::R8Unorm), tiny_texture(device, wgpu::TextureFormat::Rg8Unorm)];
        let planes = [&unused_planes[0], &unused_planes[1]];
        let bind_group = Self::bind_group(device, &layout, &rgb, &lut, &params, &hdr_curve, planes);
        let native = crate::video::native::NativeFrames::new(device);
        if native.is_some() {
            log::info!("video frames: {}", crate::video::native::KIND);
        }
        Self {
            pipeline,
            layout,
            rgb,
            lut,
            params,
            hdr_curve,
            bind_group,
            rgb_capacity,
            picture_version: None,
            unused_planes,
            bound_textures: false,
            native,
            present_pipeline,
            present_layout,
            present_params,
            sampler,
            corrected: None,
        }
    }

    /// Runs the first pass into the frame texture (made or resized to the stream's size).
    fn correct(&mut self, device: &wgpu::Device, encoder: &mut wgpu::CommandEncoder, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        if self.corrected.as_ref().is_none_or(|c| c.size != (width, height)) {
            let texture = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("psconnector corrected frame"),
                size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: CORRECTED_FORMAT,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            });
            let view = texture.create_view(&Default::default());
            let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("psconnector present bind group"),
                layout: &self.present_layout,
                entries: &[
                    wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&view) },
                    wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(&self.sampler) },
                    wgpu::BindGroupEntry { binding: 2, resource: self.present_params.as_entire_binding() },
                ],
            });
            self.corrected = Some(Corrected { view, bind_group, size: (width, height) });
        }
        let corrected = self.corrected.as_ref().expect("frame texture");
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("psconnector picture pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &corrected.view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::BLACK), store: wgpu::StoreOp::Store },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &self.bind_group, &[]);
        pass.draw(0..3, 0..1);
    }

    fn rgb_buffer(device: &wgpu::Device, size: u64) -> wgpu::Buffer {
        device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("psconnector packed RGB frame"),
            size: size.max(4).next_multiple_of(4),
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        })
    }

    fn bind_group(
        device: &wgpu::Device,
        layout: &wgpu::BindGroupLayout,
        rgb: &wgpu::Buffer,
        lut: &wgpu::Buffer,
        params: &wgpu::Buffer,
        hdr_curve: &wgpu::Buffer,
        planes: [&wgpu::TextureView; 2],
    ) -> wgpu::BindGroup {
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("psconnector video bind group"),
            layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: rgb.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 1, resource: lut.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 2, resource: params.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 3, resource: hdr_curve.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 4, resource: wgpu::BindingResource::TextureView(planes[0]) },
                wgpu::BindGroupEntry { binding: 5, resource: wgpu::BindingResource::TextureView(planes[1]) },
            ],
        })
    }

    fn rebind(&mut self, device: &wgpu::Device, planes: Option<[&wgpu::TextureView; 2]>) {
        self.bound_textures = planes.is_some();
        let planes = planes.unwrap_or([&self.unused_planes[0], &self.unused_planes[1]]);
        self.bind_group =
            Self::bind_group(device, &self.layout, &self.rgb, &self.lut, &self.params, &self.hdr_curve, planes);
    }

    /// Binds a native frame's planes directly. False when the frame has none or they cannot be wrapped.
    fn bind_native(&mut self, device: &wgpu::Device, frame: &Arc<Frame>) -> bool {
        let Some(textures) = self.native.as_mut().and_then(|m| m.textures(device, frame)) else { return false };
        let views: Vec<wgpu::TextureView> = textures.iter().map(|t| t.create_view(&Default::default())).collect();
        let second = views.get(1).unwrap_or(&self.unused_planes[1]).clone();
        let first = views[0].clone();
        self.rebind(device, Some([&first, &second]));
        true
    }

    fn upload(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, pending: &Pending, target_is_srgb: bool) {
        let frame = &pending.frame;
        let native = self.bind_native(device, frame);
        let mut format = match frame.format {
            psconnector::PixelFormat::Rgb8 => 0,
            psconnector::PixelFormat::Bgra8 => 1,
            psconnector::PixelFormat::BgraHdr8 => 2,
            psconnector::PixelFormat::Nv12 { .. } => 3,
        };
        if native {
            format = match format {
                1 => 4,
                2 => 5,
                f => f,
            };
        } else {
            if self.bound_textures {
                self.rebind(device, None);
            }
            // the decoder's own pixels go up unchanged (RGB from software decoding, BGRA from a decoder
            // that copied them out); a native frame that could not be wrapped goes up as RGB
            let pixels: &[u8] = if frame.data.is_empty() {
                format = 0;
                frame.rgb()
            } else {
                &frame.data
            };
            self.write_pixels(device, queue, pixels);
        }
        if self.picture_version != Some(pending.picture_version) {
            queue.write_buffer(&self.lut, 0, bytemuck::cast_slice(&pending.table));
            self.picture_version = Some(pending.picture_version);
        }
        let (full_range, bt709) = match frame.format {
            psconnector::PixelFormat::Nv12 { full_range, bt709 } => (full_range, bt709),
            _ => (false, false),
        };
        let params = Params {
            width: frame.width as u32,
            height: frame.height as u32,
            target_is_srgb: u32::from(target_is_srgb),
            format,
            red: pending.multipliers[0],
            blue: pending.multipliers[1],
            full_range: u32::from(full_range),
            bt709: u32::from(bt709),
            deband: u32::from(lifts_shadows(&pending.table)),
            _pad: [0; 3],
        };
        queue.write_buffer(&self.params, 0, bytemuck::bytes_of(&params));
    }

    fn write_pixels(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, pixels: &[u8]) {
        let required = pixels.len() as u64;
        if required > self.rgb_capacity {
            self.rgb_capacity = required.next_power_of_two();
            self.rgb = Self::rgb_buffer(device, self.rgb_capacity);
            self.rebind(device, None);
        }
        if pixels.len().is_multiple_of(4) {
            queue.write_buffer(&self.rgb, 0, pixels);
        } else {
            // WGPU writes are four-byte aligned. Stream widths are normally aligned;
            // this only handles unusual decoder sizes.
            let mut padded = pixels.to_vec();
            padded.resize(padded.len().next_multiple_of(4), 0);
            queue.write_buffer(&self.rgb, 0, &padded);
        }
    }
}

#[cfg(test)]
mod tests {
    use eframe::wgpu::naga;

    #[test]
    fn shaders_parse_and_validate() {
        for (name, source) in
            [("gpu_video", include_str!("gpu_video.wgsl")), ("gpu_present", include_str!("gpu_present.wgsl"))]
        {
            let module = naga::front::wgsl::parse_str(source).unwrap_or_else(|e| panic!("{name}: {e}"));
            naga::valid::Validator::new(naga::valid::ValidationFlags::all(), naga::valid::Capabilities::empty())
                .validate(&module)
                .unwrap_or_else(|e| panic!("{name}: {e:?}"));
        }
    }

    #[test]
    fn params_layout_matches_the_shader() {
        // WGSL uniform: 12 x 4-byte fields, padded to 16 bytes
        assert_eq!(std::mem::size_of::<super::Params>(), 48);
    }
}
