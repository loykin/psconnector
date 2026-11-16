//! Zero-copy video on macOS: the hardware decoder's IOSurface-backed pixel buffers are wrapped as Metal
//! textures (CVMetalTextureCache) and handed to wgpu, so a frame never passes through the CPU on its way
//! to the screen.

use std::sync::Arc;

use cidre::{arc, cv, mtl};
use eframe::wgpu;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_metal::{MTLTexture, MTLTextureType};
use psconnector::{Frame, PixelFormat};

/// What wgpu needs to be told about each plane of a pixel buffer.
struct Plane {
    index: usize,
    mtl: mtl::PixelFormat,
    wgpu: wgpu::TextureFormat,
    width: usize,
    height: usize,
}

/// Keeps a frame's pixel buffer (and its Metal texture) alive until wgpu has finished with the texture,
/// so the decoder cannot reuse the buffer while the GPU still reads it.
struct Keep {
    _texture: arc::R<cv::MetalTexture>,
    _frame: Arc<Frame>,
}

// SAFETY: only moved to whichever thread drops the wgpu texture; nothing is read through it.
unsafe impl Send for Keep {}
unsafe impl Sync for Keep {}

pub(crate) struct MetalFrames {
    cache: arc::R<cv::MetalTextureCache>,
}

// SAFETY: used only from the render callbacks, one at a time (egui-wgpu holds the resources lock).
unsafe impl Send for MetalFrames {}
unsafe impl Sync for MetalFrames {}

impl MetalFrames {
    pub(crate) fn new(device: &wgpu::Device) -> Option<Self> {
        // SAFETY: the raw device is only used to create the texture cache.
        let hal = unsafe { device.as_hal::<wgpu::hal::api::Metal>() }?;
        let raw = hal.raw_device();
        // SAFETY: both are the same Objective-C MTLDevice object; cidre's type is a transparent wrapper.
        let device = unsafe { &*(Retained::as_ptr(raw) as *const mtl::Device) };
        match cv::MetalTextureCache::create(None, device, None) {
            Ok(cache) => Some(Self { cache }),
            Err(e) => {
                log::warn!("Metal texture cache unavailable ({e:?}); video frames are copied");
                None
            }
        }
    }

    /// The frame's planes as wgpu textures (Y and CbCr for NV12, one BGRA texture otherwise), or None for
    /// a frame without a native buffer.
    pub(crate) fn textures(&mut self, device: &wgpu::Device, frame: &Arc<Frame>) -> Option<Vec<wgpu::Texture>> {
        let pb = frame.cv_pixel_buffer()?;
        // SAFETY: the frame owns the pixel buffer and outlives this borrow.
        let pb: &cv::PixelBuf = unsafe { &*(pb as *const cv::PixelBuf) };
        let (w, h) = (pb.width(), pb.height());
        let planes = match frame.format {
            PixelFormat::Nv12 { .. } => vec![
                Plane {
                    index: 0,
                    mtl: mtl::PixelFormat::R8UNorm,
                    wgpu: wgpu::TextureFormat::R8Unorm,
                    width: w,
                    height: h,
                },
                Plane {
                    index: 1,
                    mtl: mtl::PixelFormat::Rg8UNorm,
                    wgpu: wgpu::TextureFormat::Rg8Unorm,
                    width: w.div_ceil(2),
                    height: h.div_ceil(2),
                },
            ],
            PixelFormat::Bgra8 | PixelFormat::BgraHdr8 => vec![Plane {
                index: 0,
                mtl: mtl::PixelFormat::Bgra8UNorm,
                wgpu: wgpu::TextureFormat::Bgra8Unorm,
                width: w,
                height: h,
            }],
            PixelFormat::Rgb8 => return None,
        };
        let mut out = Vec::with_capacity(planes.len());
        for p in planes {
            let texture = match self.cache.texture(pb, None, p.mtl, p.width, p.height, p.index) {
                Ok(t) => t,
                Err(e) => {
                    log::debug!("cannot wrap plane {} as a Metal texture: {e:?}", p.index);
                    return None;
                }
            };
            let raw = texture.texture()?;
            // SAFETY: the same Objective-C MTLTexture; retained here, released when wgpu drops it.
            let raw: Retained<ProtocolObject<dyn MTLTexture>> =
                unsafe { Retained::retain(raw as *const mtl::Texture as *mut ProtocolObject<dyn MTLTexture>) }?;
            let keep = Keep { _texture: texture, _frame: frame.clone() };
            let (width, height) = (p.width as u32, p.height as u32);
            // SAFETY: a 2D, single-mip texture of exactly this format and size, created on wgpu's device.
            let hal = unsafe {
                wgpu::hal::metal::Device::texture_from_raw(
                    raw,
                    p.wgpu,
                    MTLTextureType::Type2D,
                    1,
                    1,
                    wgpu::hal::CopyExtent { width, height, depth: 1 },
                    Some(Box::new(move || drop(keep))),
                )
            };
            let desc = wgpu::TextureDescriptor {
                label: Some("psconnector decoded plane"),
                size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: p.wgpu,
                usage: wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            };
            // SAFETY: as above; the decoder finished writing the buffer before handing it over.
            out.push(unsafe {
                device.create_texture_from_hal::<wgpu::hal::api::Metal>(hal, &desc, wgpu::TextureUses::RESOURCE)
            });
        }
        // lets the cache recycle textures of frames that are gone
        self.cache.flush();
        Some(out)
    }
}
