//! Vulkan media pipeline: dmabuf import, the AVC444 split and recombine as
//! compute shaders, and H.264 encode/decode through Vulkan Video.
//!
//! Vulkan calls use `unsafe` through `ash`. Callers see plain Rust types.

use ash::vk::native as std_video;

pub mod h264;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("vulkan: {0}")]
    Vk(#[from] ash::vk::Result),
    #[error("vulkan loader: {0}")]
    Load(#[from] ash::LoadingError),
    #[error("no suitable GPU: {0}")]
    NoDevice(&'static str),
    #[error("bitstream: {0}")]
    Bitstream(&'static str),
    #[error("unsupported: {0}")]
    Unsupported(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, Error>;
pub mod compute;
pub mod device;
pub mod encoder;
pub mod image;

trait Zeroable: Copy {}

impl Zeroable for std_video::StdVideoEncodeH264ReferenceInfoFlags {}
impl Zeroable for std_video::StdVideoEncodeH264ReferenceListsInfoFlags {}
impl Zeroable for std_video::StdVideoEncodeH264PictureInfoFlags {}
impl Zeroable for std_video::StdVideoEncodeH264SliceHeaderFlags {}
impl Zeroable for std_video::StdVideoDecodeH264ReferenceInfoFlags {}
impl Zeroable for std_video::StdVideoDecodeH264PictureInfoFlags {}
impl Zeroable for std_video::StdVideoH264SpsFlags {}
impl Zeroable for std_video::StdVideoH264PpsFlags {}

pub(crate) fn zeroed<T: Zeroable>() -> T {
    // SAFETY: every Zeroable type above is a C bitfield struct whose fields
    // accept an all-zero representation.
    unsafe { std::mem::zeroed() }
}

pub mod decoder;
pub mod pipeline;

pub use device::Gpu;
pub use encoder::EncoderSettings;
pub use image::{DmabufPlane, ExportedDmabuf};
pub use pipeline::{Decoder, DisplayFrame, EncodedFrame, Encoder};
