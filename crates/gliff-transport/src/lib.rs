//! Length-prefixed framing over any tokio `AsyncRead + AsyncWrite`, plus ssh
//! process spawning for the client.
//!
//! Each frame is a postcard-encoded message (`ClientMsg`/`ServerMsg`) preceded
//! by a `u32` little-endian length, optionally followed by raw payload bytes
//! the message declares (`data_len`, `aux_len`, `argb_len`). Payloads never go
//! through postcard; the writer sends them with `write_vectored` and the reader
//! returns them as a `Bytes` slice for the decoder.

mod ssh;

use bytes::{Bytes, BytesMut};
use gliff_proto::frame::MAX_FRAME_BODY;
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::io::IoSlice;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub use ssh::{spawn_ssh, SshTarget};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("postcard: {0}")]
    Postcard(#[from] postcard::Error),
    #[error("frame body of {0} bytes exceeds the limit")]
    BodyTooLarge(u32),
    #[error("payload of {0} bytes exceeds the limit")]
    PayloadTooLarge(u32),
    #[error("connection closed")]
    Closed,
}

pub type Result<T> = std::result::Result<T, Error>;

/// Largest single payload (a keyframe) we will read.
pub const MAX_PAYLOAD: u32 = 64 * 1024 * 1024;

/// Framed reader/writer over a split or duplex stream.
pub struct Framed<S> {
    stream: S,
    read_buf: BytesMut,
}

impl<S> Framed<S> {
    pub fn new(stream: S) -> Self {
        Self {
            stream,
            read_buf: BytesMut::with_capacity(64 * 1024),
        }
    }

    pub fn into_inner(self) -> S {
        self.stream
    }
}

impl<S: AsyncWrite + Unpin> Framed<S> {
    /// Write a message with no trailing payload.
    pub async fn write_msg<M: Serialize>(&mut self, msg: &M) -> Result<()> {
        self.write_msg_with_payloads(msg, &[]).await
    }

    /// Write a message then the given payload slices, all in order. Uses
    /// `write_vectored` so a large encoder payload is not copied into a
    /// staging buffer first.
    pub async fn write_msg_with_payloads<M: Serialize>(
        &mut self,
        msg: &M,
        payloads: &[&[u8]],
    ) -> Result<()> {
        let body = postcard::to_stdvec(msg)?;
        let len = (body.len() as u32).to_le_bytes();
        let mut parts: Vec<&[u8]> = Vec::with_capacity(2 + payloads.len());
        parts.push(&len);
        parts.push(&body);
        parts.extend_from_slice(payloads);
        write_all_vectored(&mut self.stream, &parts).await?;
        self.stream.flush().await?;
        Ok(())
    }
}

impl<S: AsyncRead + Unpin> Framed<S> {
    /// Read one message body. Payloads (if any) must then be read with
    /// [`read_payload`] according to the message's declared lengths.
    pub async fn read_msg<M: DeserializeOwned>(&mut self) -> Result<M> {
        let len = self.read_u32().await?;
        if len as usize > MAX_FRAME_BODY {
            return Err(Error::BodyTooLarge(len));
        }
        let body = self.read_exact_bytes(len as usize).await?;
        Ok(postcard::from_bytes(&body)?)
    }

    /// Read exactly `len` payload bytes that follow a message.
    pub async fn read_payload(&mut self, len: u32) -> Result<Bytes> {
        if len > MAX_PAYLOAD {
            return Err(Error::PayloadTooLarge(len));
        }
        Ok(self.read_exact_bytes(len as usize).await?.freeze())
    }

    async fn read_u32(&mut self) -> Result<u32> {
        let b = self.read_exact_bytes(4).await?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    async fn read_exact_bytes(&mut self, n: usize) -> Result<BytesMut> {
        while self.read_buf.len() < n {
            self.read_buf
                .reserve((n - self.read_buf.len()).max(32 * 1024));
            if self.stream.read_buf(&mut self.read_buf).await? == 0 {
                return Err(Error::Closed);
            }
        }
        Ok(self.read_buf.split_to(n))
    }
}

/// Write every byte of `parts` in order using vectored writes, handling
/// partial writes by tracking a global offset and rebuilding the `IoSlice`
/// list for the unwritten remainder each iteration.
async fn write_all_vectored<S: AsyncWrite + Unpin>(stream: &mut S, parts: &[&[u8]]) -> Result<()> {
    let total: usize = parts.iter().map(|p| p.len()).sum();
    let mut done = 0usize;
    while done < total {
        // Build IoSlices for the remainder starting at global offset `done`.
        let mut slices: Vec<IoSlice> = Vec::with_capacity(parts.len());
        let mut skip = done;
        for p in parts {
            if skip >= p.len() {
                skip -= p.len();
                continue;
            }
            slices.push(IoSlice::new(&p[skip..]));
            skip = 0;
        }
        let n = stream.write_vectored(&slices).await?;
        if n == 0 {
            return Err(Error::Closed);
        }
        done += n;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use gliff_proto::{ChromaMode, ClientCaps, ClientMsg, Codec};

    #[tokio::test]
    async fn round_trips_message_and_payload() {
        let (a, b) = tokio::io::duplex(4096);
        let mut wr = Framed::new(a);
        let mut rd = Framed::new(b);
        let msg = ClientMsg::Hello {
            version: 1,
            keymap: "k".into(),
            caps: ClientCaps {
                codecs: vec![Codec::H264],
                max_width: 1920,
                max_height: 1080,
                chroma: vec![ChromaMode::Dual420],
            },
        };
        let payload = vec![7u8; 5000];
        let p2 = vec![9u8; 100];
        let m2 = msg.clone();
        let payload2 = payload.clone();
        let p2b = p2.clone();
        let task = tokio::spawn(async move {
            wr.write_msg_with_payloads(&m2, &[&payload2, &p2b])
                .await
                .unwrap();
        });
        let got: ClientMsg = rd.read_msg().await.unwrap();
        assert_eq!(got, msg);
        let gp = rd.read_payload(5000).await.unwrap();
        let gp2 = rd.read_payload(100).await.unwrap();
        assert_eq!(&gp[..], &payload[..]);
        assert_eq!(&gp2[..], &p2[..]);
        task.await.unwrap();
    }
}
