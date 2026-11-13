use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use bytemuck::{Pod, Zeroable};
use eframe::egui;
use eframe::egui_wgpu::{CallbackResources, CallbackTrait, RenderState, ScreenDescriptor};
use eframe::wgpu;
use eframe::wgpu::util::DeviceExt;
use psconnector::Frame;

use crate::picture::Lut;

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Params {
    width: u32,
    height: u32,
    target_is_srgb: u32,
    _pad0: u32,
    red: i32,
    blue: i32,
    _pad1: [i32; 2],
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
        _egui_encoder: &mut wgpu::CommandEncoder,
        callback_resources: &mut CallbackResources,
    ) -> Vec<wgpu::CommandBuffer> {
        let Some(pending) = self.shared.pending.lock().unwrap().take() else {
            return Vec::new();
        };
        let started = Instant::now();
        let resources = callback_resources.get_mut::<Resources>().expect("GPU video resources");
        resources.upload(device, queue, &pending, self.shared.target_is_srgb);
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
        render_pass.set_pipeline(&resources.pipeline);
        render_pass.set_bind_group(0, &resources.bind_group, &[]);
        render_pass.draw(0..3, 0..1);
    }
}

struct Resources {
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    rgb: wgpu::Buffer,
    lut: wgpu::Buffer,
    params: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
    rgb_capacity: u64,
    picture_version: Option<u64>,
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
        let bind_group = Self::bind_group(device, &layout, &rgb, &lut, &params);
        Self { pipeline, layout, rgb, lut, params, bind_group, rgb_capacity, picture_version: None }
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
    ) -> wgpu::BindGroup {
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("psconnector video bind group"),
            layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: rgb.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 1, resource: lut.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 2, resource: params.as_entire_binding() },
            ],
        })
    }

    fn upload(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, pending: &Pending, target_is_srgb: bool) {
        let required = pending.frame.rgb.len() as u64;
        if required > self.rgb_capacity {
            self.rgb_capacity = required.next_power_of_two();
            self.rgb = Self::rgb_buffer(device, self.rgb_capacity);
            self.bind_group = Self::bind_group(device, &self.layout, &self.rgb, &self.lut, &self.params);
        }
        if pending.frame.rgb.len().is_multiple_of(4) {
            queue.write_buffer(&self.rgb, 0, &pending.frame.rgb);
        } else {
            // WGPU writes are four-byte aligned. Stream widths are normally aligned;
            // this only handles unusual decoder sizes without copying every frame.
            let mut padded = pending.frame.rgb.clone();
            padded.resize(padded.len().next_multiple_of(4), 0);
            queue.write_buffer(&self.rgb, 0, &padded);
        }
        if self.picture_version != Some(pending.picture_version) {
            queue.write_buffer(&self.lut, 0, bytemuck::cast_slice(&pending.table));
            self.picture_version = Some(pending.picture_version);
        }
        let params = Params {
            width: pending.frame.width as u32,
            height: pending.frame.height as u32,
            target_is_srgb: u32::from(target_is_srgb),
            _pad0: 0,
            red: pending.multipliers[0],
            blue: pending.multipliers[1],
            _pad1: [0; 2],
        };
        queue.write_buffer(&self.params, 0, bytemuck::bytes_of(&params));
    }
}
