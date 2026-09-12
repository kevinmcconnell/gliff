//! VA-API encode and decode through `cros-codecs`, with dmabuf-backed frames.
//!
//! Nothing here touches raw pointers: buffers are allocated by `gbm`, wrapped
//! as [`frame::Nv12Frame`], and imported into VA-API by cros-codecs.

pub mod annexb;
pub mod frame;
pub mod h264;
pub mod vaapi;

pub use cros_codecs::libva;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("gbm: {0}")]
    Gbm(String),
    #[error("va-api: {0}")]
    Va(String),
    #[error("encoder: {0}")]
    Encode(String),
    #[error("decoder: {0}")]
    Decode(String),
    #[error("frame pool is empty")]
    PoolEmpty,
}

pub type Result<T> = std::result::Result<T, Error>;
