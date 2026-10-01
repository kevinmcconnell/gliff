//! Vulkan media pipeline: dmabuf import, the AVC444 split and recombine as
//! compute shaders, and the hand-off of NV12 surfaces to and from the
//! VA-API codec in `gliff-va`.
//!
//! Vulkan calls use `unsafe` through `ash`. Callers see plain Rust types.

pub use gliff_va::h264;
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("vulkan: {0}")]
    Vk(#[from] ash::vk::Result),
    #[error("vulkan loader: {0}")]
    Load(#[from] ash::LoadingError),
    #[error("no suitable GPU: {0}")]
    NoDevice(&'static str),
    #[error("unsupported: {0}")]
    Unsupported(String),
    #[error("va-api: {0}")]
    Va(#[from] gliff_va::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, Error>;
pub mod compute;
pub mod device;
pub mod image;

pub mod pipeline;

pub use device::Gpu;
pub use gliff_va::EncoderSettings;
pub use image::{DmabufPlane, ExportedDmabuf};
pub use pipeline::{split_into_surface, Decoder, DisplayFrame, EncodedFrame, Encoder, SurfacePath};
