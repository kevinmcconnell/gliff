//! haver wire protocol: message types, length-prefixed framing with an
//! out-of-band payload, and the 4:4:4 chroma split/recombine reference.
//!
//! No I/O here. `haver-transport` moves bytes; this crate defines their shape.

pub mod chroma;
pub mod frame;
pub mod msg;

pub use chroma::{recombine_yuv444, split_yuv444, Yuv444};
pub use frame::{FrameHeader, PayloadLens, MAX_FRAME_BODY};
pub use msg::*;
