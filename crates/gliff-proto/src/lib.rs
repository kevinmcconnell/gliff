//! gliff wire protocol: message types, length-prefixed framing with an
//! out-of-band payload, the clipboard transfer rules, and the CPU reference for the colour conversion and
//! the 4:4:4 chroma split/recombine that the GPU shaders must match.
//!
//! No I/O here. `gliff-transport` moves bytes; this crate defines their shape.

#![forbid(unsafe_code)]

pub mod cbor;
pub mod chroma;
pub mod clipboard;
pub mod color;
pub mod frame;
pub mod msg;

pub use chroma::{recombine_yuv444, split_yuv444, Yuv444};
pub use frame::MAX_FRAME_BODY;
pub use msg::*;
