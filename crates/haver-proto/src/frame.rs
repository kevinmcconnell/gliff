//! Wire framing: a length-prefixed postcard message, optionally followed by raw
//! payload bytes whose lengths the message declares.
//!
//! Layout on the wire, little-endian:
//! ```text
//! u32 body_len | body_len bytes of postcard message | payload bytes (concatenated)
//! ```
//! The payload bytes are the video `data`/`aux`, cursor `argb`, or clipboard
//! `data` chunk. They are kept out of the postcard body so the encoder's
//! output goes straight to the socket with `write_vectored`, and the reader
//! hands the slices to the decoder without an extra copy.

use serde::{Deserialize, Serialize};

/// Largest postcard body we will read; guards against a bad length prefix.
pub const MAX_FRAME_BODY: usize = 1 << 20;

/// The parsed length prefix plus how many payload bytes follow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHeader {
    pub body_len: u32,
}

/// Payload lengths a message declares, in the order they appear on the wire.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PayloadLens {
    pub lens: Vec<u32>,
}

impl PayloadLens {
    pub fn total(&self) -> usize {
        self.lens.iter().map(|&l| l as usize).sum()
    }
}
