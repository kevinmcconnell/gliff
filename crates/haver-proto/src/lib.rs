//! haver wire protocol: message types, length-prefixed framing with an
//! out-of-band payload, and the CPU reference for the colour conversion and
//! the 4:4:4 chroma split/recombine that the GPU shaders must match.
//!
//! No I/O here. `haver-transport` moves bytes; this crate defines their shape.

pub mod chroma;
pub mod color;
pub mod frame;
pub mod msg;

pub use chroma::{recombine_yuv444, split_yuv444, Yuv444};
pub use frame::MAX_FRAME_BODY;
pub use msg::*;
