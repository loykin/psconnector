//! Drawing the decoder's own buffers without copying them, behind one interface so the renderer has no
//! platform code.
//!
//! macOS: the IOSurface-backed buffers are wrapped as Metal textures (`metal_video.rs`). Other systems have
//! no native frames yet ([`psconnector::Frame::native_buffer`] is always `None` there): [`NativeFrames::new`]
//! returns `None` and frames are uploaded from memory. A new platform implements the same two calls.

#[cfg(target_os = "macos")]
pub(crate) use crate::metal_video::MetalFrames as NativeFrames;

#[cfg(target_os = "macos")]
/// How frames reach the GPU, for the log.
pub(crate) const KIND: &str = "zero-copy (decoder buffers drawn as Metal textures)";

#[cfg(not(target_os = "macos"))]
pub(crate) use none::NativeFrames;

#[cfg(not(target_os = "macos"))]
pub(crate) const KIND: &str = "copied (no native frames on this platform)";

#[cfg(not(target_os = "macos"))]
mod none {
    use std::sync::Arc;

    use eframe::wgpu;
    use psconnector::Frame;

    /// No native frames: this type has no values, so `Option<NativeFrames>` is always `None`.
    pub(crate) enum NativeFrames {}

    impl NativeFrames {
        pub(crate) fn new(_device: &wgpu::Device) -> Option<Self> {
            None
        }

        pub(crate) fn textures(&mut self, _device: &wgpu::Device, _frame: &Arc<Frame>) -> Option<Vec<wgpu::Texture>> {
            match *self {}
        }
    }
}
