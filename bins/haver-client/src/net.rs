//! The client's network and decode worker: connect (TCP or ssh), run the
//! protocol, decode the Dual420 streams, and hand decoded RGBA frames to the
//! GTK thread. Input commands flow the other way.
//!
//! cros-codecs' decoder is `!Send`, so it lives entirely on this worker thread;
//! only plain byte buffers cross to the GTK thread.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Sender as StdSender, SyncSender};
use std::sync::Arc;

use haver_codec::color::yuv444_to_bgra;
use haver_codec::{vaapi, DecodedPlanes, Decoder};
use haver_proto::{ChromaMode, ClientCaps, ClientMsg, Codec, ServerMsg, PROTOCOL_VERSION};
use haver_transport::{spawn_ssh, Framed, SshTarget};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};

/// A decoded frame ready to display: either GPU dmabuf planes (fast path) or
/// CPU-recombined BGRA (fallback).
pub enum DecodedFrame {
    Rgba {
        width: usize,
        height: usize,
        bgra: Vec<u8>,
    },
    Planes(DecodedPlanes),
}

/// Status/telemetry the worker reports to the UI.
pub enum Status {
    Connected {
        width: u32,
        height: u32,
    },
    Stats {
        fps: f32,
        mbit: f32,
        decode_ms: f32,
    },
    /// The remote cursor image, for the client to set as its widget cursor.
    Cursor {
        width: u32,
        height: u32,
        hot_x: i32,
        hot_y: i32,
        argb: Vec<u8>,
    },
    /// The remote text selection, for the client to put on its local clipboard.
    Clipboard(String),
    Error(String),
    Closed,
}

/// How to reach the server.
#[derive(Clone)]
pub enum Endpoint {
    Tcp(String),
    Ssh(SshTarget),
}

pub struct Worker {
    pub endpoint: Endpoint,
    /// Shared with the UI: true = send dmabuf planes (GPU), false = BGRA (CPU).
    /// The GLArea render callback clears it if dmabuf rendering fails at run time.
    pub gpu: Arc<AtomicBool>,
    /// Bounded so a stalled UI thread cannot make the decoder buffer frames
    /// without limit; when full, the newest frame is dropped (latest-wins).
    pub frames: SyncSender<DecodedFrame>,
    pub status: StdSender<Status>,
    pub input: UnboundedReceiver<(ClientMsg, Vec<u8>)>,
}

impl Worker {
    /// Run to completion on the calling thread (spawn it yourself).
    pub fn run(self) {
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                let _ = self.status.send(Status::Error(e.to_string()));
                return;
            }
        };
        let local = tokio::task::LocalSet::new();
        let status = self.status.clone();
        let result: anyhow::Result<()> = local.block_on(&rt, async move {
            match self.endpoint {
                Endpoint::Tcp(ref addr) => {
                    let stream = tokio::net::TcpStream::connect(addr).await?;
                    stream.set_nodelay(true)?;
                    let (rd, wr) = tokio::io::split(stream);
                    session(
                        rd,
                        wr,
                        self.frames,
                        self.status,
                        self.input,
                        self.gpu.clone(),
                    )
                    .await
                }
                Endpoint::Ssh(ref target) => {
                    let ssh = spawn_ssh(target)?;
                    session(
                        ssh.stdout,
                        ssh.stdin,
                        self.frames,
                        self.status,
                        self.input,
                        self.gpu.clone(),
                    )
                    .await
                }
            }
        });
        if let Err(e) = result {
            let _ = status.send(Status::Error(e.to_string()));
        } else {
            let _ = status.send(Status::Closed);
        }
    }
}

async fn session<R, W>(
    rd: R,
    wr: W,
    frames: SyncSender<DecodedFrame>,
    status: StdSender<Status>,
    mut input: UnboundedReceiver<(ClientMsg, Vec<u8>)>,
    gpu: Arc<AtomicBool>,
) -> anyhow::Result<()>
where
    R: AsyncRead + Unpin + 'static,
    W: AsyncWrite + Unpin + 'static,
{
    let mut reader = Framed::new(rd);
    let mut writer = Framed::new(wr);

    let caps = ClientCaps {
        codecs: vec![Codec::H264],
        max_width: 3840,
        max_height: 2160,
        chroma: vec![ChromaMode::Dual420, ChromaMode::Single420],
    };
    let keymap = crate::keymap::local_keymap();
    writer
        .write_msg(&ClientMsg::Hello {
            version: PROTOCOL_VERSION,
            keymap,
            caps,
        })
        .await?;

    let ack = reader.read_msg::<ServerMsg>().await?;
    let ServerMsg::HelloAck { .. } = ack else {
        anyhow::bail!("expected HelloAck, got {ack:?}");
    };
    let cfg = reader.read_msg::<ServerMsg>().await?;
    let (width, height, chroma) = match cfg {
        ServerMsg::StreamConfig {
            width,
            height,
            chroma,
            ..
        } => (width, height, chroma),
        other => anyhow::bail!("expected StreamConfig, got {other:?}"),
    };
    let display = vaapi::open_display(&vaapi::render_node(None))?;
    let mut decoder = Decoder::new(display.clone(), chroma, width as usize, height as usize)?;
    let _ = status.send(Status::Connected { width, height });
    tracing::info!(width, height, "connected");
    let mut logged_first = false;

    // Writes run on their own task, fed by `out_tx`, so reads (draining video)
    // never block on a write and the two peers cannot deadlock. Both the reader
    // loop (acks, keyframe requests) and the UI thread (input) feed `out_tx`.
    let (out_tx, mut out_rx) = unbounded_channel::<(ClientMsg, Vec<u8>)>();
    tokio::task::spawn_local(async move {
        while let Some((m, payload)) = out_rx.recv().await {
            let r = if payload.is_empty() {
                writer.write_msg(&m).await
            } else {
                writer.write_msg_with_payloads(&m, &[&payload]).await
            };
            if r.is_err() {
                break;
            }
        }
    });
    // Forward UI input into the same write channel.
    {
        let out_tx = out_tx.clone();
        tokio::task::spawn_local(async move {
            while let Some(mp) = input.recv().await {
                if out_tx.send(mp).is_err() {
                    break;
                }
            }
        });
    }

    let mut frames_since = 0u32;
    let mut bytes_since = 0u64;
    let mut decode_ms_acc = 0f32;
    let mut last_report = std::time::Instant::now();

    loop {
        let msg = match reader.read_msg::<ServerMsg>().await {
            Ok(m) => m,
            Err(haver_transport::Error::Closed) => break,
            Err(e) => return Err(e.into()),
        };
        match msg {
            ServerMsg::VideoFrame {
                frame_id,
                keyframe: _,
                data_len,
                aux_len,
                ..
            } => {
                let main = reader.read_payload(data_len).await?;
                let aux = if aux_len > 0 {
                    reader.read_payload(aux_len).await?.to_vec()
                } else {
                    Vec::new()
                };
                bytes_since += (data_len + aux_len) as u64;
                let t0 = std::time::Instant::now();
                let use_gpu = gpu.load(Ordering::Relaxed);
                let decoded = if use_gpu {
                    decoder
                        .decode_planes(frame_id, &main, &aux)
                        .map(|p| p.map(DecodedFrame::Planes))
                } else {
                    decoder.decode(frame_id, &main, &aux).map(|y| {
                        y.map(|y| DecodedFrame::Rgba {
                            width: y.width,
                            height: y.height,
                            bgra: yuv444_to_bgra(&y),
                        })
                    })
                };
                let dec_ms = t0.elapsed().as_secs_f32() * 1000.0;
                // Ack immediately so the server keeps pacing.
                let _ = out_tx.send((
                    ClientMsg::FrameAck {
                        frame_id,
                        decoded_at_ms: now_ms(),
                    },
                    Vec::new(),
                ));
                match decoded {
                    Ok(Some(frame)) => {
                        if !logged_first {
                            tracing::info!(use_gpu, "first frame decoded and displayed");
                            logged_first = true;
                        }
                        // Latest-wins: drop this frame if the UI hasn't drained.
                        let _ = frames.try_send(frame);
                        frames_since += 1;
                        decode_ms_acc += dec_ms;
                    }
                    Ok(None) => {}
                    Err(e) => {
                        tracing::warn!(error = %e, "decode error; requesting keyframe");
                        let _ = out_tx.send((ClientMsg::RequestKeyframe, Vec::new()));
                    }
                }
            }
            ServerMsg::StreamConfig {
                width,
                height,
                chroma,
                ..
            } => {
                decoder = Decoder::new(display.clone(), chroma, width as usize, height as usize)?;
                let _ = status.send(Status::Connected { width, height });
            }
            ServerMsg::CursorShape {
                width,
                height,
                hot_x,
                hot_y,
                argb_len,
                ..
            } => {
                let argb = reader.read_payload(argb_len).await?.to_vec();
                let _ = status.send(Status::Cursor {
                    width,
                    height,
                    hot_x,
                    hot_y,
                    argb,
                });
            }
            ServerMsg::ClipboardData { data_len, .. } => {
                let bytes = reader.read_payload(data_len).await?;
                if let Ok(text) = String::from_utf8(bytes.to_vec()) {
                    let _ = status.send(Status::Clipboard(text));
                }
            }
            ServerMsg::CursorPos { .. } | ServerMsg::Pong { .. } => {}
            ServerMsg::Error { code, message } => anyhow::bail!("server error {code}: {message}"),
            ServerMsg::HelloAck { .. }
            | ServerMsg::ClipboardOffer { .. }
            | ServerMsg::ClipboardRequest { .. } => {}
        }
        if last_report.elapsed().as_secs_f32() >= 1.0 {
            let secs = last_report.elapsed().as_secs_f32();
            let _ = status.send(Status::Stats {
                fps: frames_since as f32 / secs,
                mbit: bytes_since as f32 * 8.0 / 1_000_000.0 / secs,
                decode_ms: if frames_since > 0 {
                    decode_ms_acc / frames_since as f32
                } else {
                    0.0
                },
            });
            frames_since = 0;
            bytes_since = 0;
            decode_ms_acc = 0.0;
            last_report = std::time::Instant::now();
        }
    }
    Ok(())
}

fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
