//! Wire framing: a header, a CBOR message, then raw payload bytes whose
//! lengths the message declares.
//!
//! Layout on the wire, little-endian:
//! ```text
//! u32 body_len | u32 payload_len | body_len bytes of CBOR message | payload_len bytes
//! ```
//! The payload bytes are the video `data`/`aux`, cursor `argb`, or clipboard
//! `data` chunk. They are kept out of the CBOR body so the encoder's output
//! goes straight to the socket with `write_vectored`, and the reader hands
//! the slices to the decoder without an extra copy. The header carries the
//! payload length too, so a reader can skip a message it does not know.
//!
//! This layout never changes, whatever the protocol version.

pub const HEADER_LEN: usize = 8;

/// Largest CBOR body either end sends or accepts.
pub const MAX_FRAME_BODY: usize = 1 << 20;

/// Largest single payload a message may declare (a keyframe).
pub const MAX_PAYLOAD: u32 = 64 * 1024 * 1024;

/// Largest total payload of one frame: a keyframe's main and aux streams.
pub const MAX_FRAME_PAYLOAD: u32 = 2 * MAX_PAYLOAD;
