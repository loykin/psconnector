//! Showing the video: drawing frames on the GPU, the picture adjustment and its saved presets.

pub(crate) mod gpu;
#[cfg(target_os = "macos")]
pub(crate) mod metal;
pub(crate) mod native;
pub(crate) mod picture;
pub(crate) mod presets;
