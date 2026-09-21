//! The client's network and decode worker: `gliff-client` runs the session,
//! and this sink decodes the streams on the GPU and hands finished display
//! frames (dmabufs) to the GTK thread. Input commands flow the other way.
//!
//! The Vulkan objects live entirely on this worker thread; only dmabuf fds
//! and plain values cross to the GTK thread.

use std::sync::mpsc::{Sender as StdSender, SyncSender};
use std::sync::Arc;

use gliff_client::{Config, Input, Sink};
use gliff_proto::{ChromaMode, ClientCaps, Codec};
use gliff_vk::{Decoder, DisplayFrame, Gpu};
use tokio::sync::mpsc::UnboundedReceiver;

pub use gliff_client::{Endpoint, Status};

pub struct Worker {
    pub endpoint: Endpoint,
    /// Bounded so a stalled UI thread cannot make the decoder buffer frames
    /// without limit; when full, the newest frame is dropped (latest-wins).
    pub frames: SyncSender<DisplayFrame>,
    pub status: StdSender<Status>,
    pub input: UnboundedReceiver<Input>,
}

impl Worker {
    /// Run to completion on the calling thread (spawn it yourself).
    pub fn run(self) {
        let config = Config {
            endpoint: self.endpoint,
            keymap: crate::keymap::local_keymap(),
            caps: ClientCaps {
                codecs: vec![Codec::H264],
                max_width: 3840,
                max_height: 2160,
                chroma: vec![ChromaMode::Dual420, ChromaMode::Single420],
            },
        };
        let sink = VulkanSink {
            gpu: None,
            decoder: None,
            frames: self.frames,
            status: self.status,
        };
        // The GTK client never stops a worker early: a new session replaces
        // one that has already ended.
        let (_stop, signal) = gliff_client::stopper();
        gliff_client::run(config, sink, self.input, signal);
    }
}

struct VulkanSink {
    gpu: Option<Arc<Gpu>>,
    decoder: Option<Decoder>,
    frames: SyncSender<DisplayFrame>,
    status: StdSender<Status>,
}

impl Sink for VulkanSink {
    type Frame = DisplayFrame;

    fn configure(&mut self, width: u32, height: u32, chroma: ChromaMode) -> anyhow::Result<()> {
        let gpu = match &self.gpu {
            Some(gpu) => gpu.clone(),
            None => {
                let gpu = Gpu::open(Some(&hypr_capture::render_node(None)))?;
                tracing::info!(gpu = %gpu.name, "decoding on");
                self.gpu.insert(gpu).clone()
            }
        };
        self.decoder = None;
        self.decoder = Some(Decoder::new(
            &gpu,
            chroma != ChromaMode::Single420,
            width,
            height,
        )?);
        Ok(())
    }

    fn decode(&mut self, main: &[u8], aux: &[u8]) -> anyhow::Result<Option<DisplayFrame>> {
        let decoder = self
            .decoder
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("no decoder"))?;
        Ok(decoder.decode(main, aux)?)
    }

    fn frame(&mut self, frame: DisplayFrame) {
        // Latest-wins: drop this frame if the UI hasn't drained.
        let _ = self.frames.try_send(frame);
    }

    fn status(&mut self, status: Status) {
        let _ = self.status.send(status);
    }
}
