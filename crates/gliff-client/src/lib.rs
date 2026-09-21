//! The platform-neutral half of a gliff client: connect (TCP or ssh), run
//! the protocol, pace the server with acks, and hand video payloads to a
//! platform decoder. The GTK client plugs in Vulkan; the macOS client plugs
//! in VideoToolbox and Metal through `gliff-ffi`.
//!
//! Everything runs on the thread that calls [`run`]; the [`Sink`] is only
//! ever called from there. Input flows in through a channel, and a
//! [`StopHandle`] ends the session from any thread.

mod view;

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use bytes::Bytes;
use gliff_proto::{ChromaMode, ClientCaps, ClientMsg, ServerMsg, PROTOCOL_VERSION};
use gliff_transport::{spawn_ssh, Framed};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, BufReader};
use tokio::sync::{mpsc, watch};

pub use gliff_transport::SshTarget;
pub use view::{has_visible_shape, FrameRect};

/// How to reach the server.
#[derive(Debug, Clone)]
pub enum Endpoint {
    /// Dev only: a `gliff-server --listen` address.
    Tcp(String),
    Ssh(SshTarget),
}

pub struct Config {
    pub endpoint: Endpoint,
    /// xkb keymap text for the server, or empty for its default (us).
    pub keymap: String,
    pub caps: ClientCaps,
}

/// What the session reports to the platform side, in order.
#[derive(Debug)]
pub enum Status {
    /// A stream (re)started at this size; the remote's logical size is
    /// `width / scale`, the space pointer coordinates are sent in.
    Connected {
        width: u32,
        height: u32,
        scale_milli: u32,
    },
    Stats {
        fps: f32,
        mbit: f32,
        decode_ms: f32,
    },
    /// The remote cursor image, BGRA (little-endian ARGB), unpremultiplied.
    Cursor {
        width: u32,
        height: u32,
        hot_x: i32,
        hot_y: i32,
        argb: Vec<u8>,
    },
    /// The remote text selection changed.
    Clipboard(String),
    /// A line ssh or the server wrote to stderr (only when captured).
    Log(String),
    /// The session failed. Always the last status.
    Error(String),
    /// The server closed the session. Always the last status.
    Closed,
}

/// The platform side of a session.
pub trait Sink {
    type Frame;
    /// A new stream: size in physical pixels and chroma layout. Called before
    /// the first frame and again whenever the server restarts the stream.
    fn configure(&mut self, width: u32, height: u32, chroma: ChromaMode) -> anyhow::Result<()>;
    /// Decode one frame's main and auxiliary access units (`aux` is empty
    /// for `Single420`). The frame is acked once this returns, whatever the
    /// result, so the time spent here is what the server paces against.
    fn decode(&mut self, main: &[u8], aux: &[u8]) -> anyhow::Result<Option<Self::Frame>>;
    /// A decoded frame, ready to show.
    fn frame(&mut self, frame: Self::Frame);
    fn status(&mut self, status: Status);
}

/// A borrowed sink works too, so the caller can inspect it after [`run`].
impl<T: Sink + ?Sized> Sink for &mut T {
    type Frame = T::Frame;

    fn configure(&mut self, width: u32, height: u32, chroma: ChromaMode) -> anyhow::Result<()> {
        (**self).configure(width, height, chroma)
    }

    fn decode(&mut self, main: &[u8], aux: &[u8]) -> anyhow::Result<Option<Self::Frame>> {
        (**self).decode(main, aux)
    }

    fn frame(&mut self, frame: Self::Frame) {
        (**self).frame(frame)
    }

    fn status(&mut self, status: Status) {
        (**self).status(status)
    }
}

/// A message plus its trailing payload (clipboard text), from the platform
/// side to the server.
pub type Input = (ClientMsg, Vec<u8>);

/// Ends a session from any thread. Cheap to clone.
#[derive(Clone)]
pub struct StopHandle(Arc<watch::Sender<bool>>);

impl StopHandle {
    pub fn stop(&self) {
        let _ = self.0.send(true);
    }
}

pub struct StopSignal(watch::Receiver<bool>);

impl StopSignal {
    async fn wait(mut self) {
        let _ = self.0.wait_for(|stopped| *stopped).await;
    }
}

pub fn stopper() -> (StopHandle, StopSignal) {
    let (tx, rx) = watch::channel(false);
    (StopHandle(Arc::new(tx)), StopSignal(rx))
}

/// Run one session to its end on the calling thread. The sink hears
/// [`Status::Error`] or [`Status::Closed`] last, unless the session was
/// stopped, in which case it hears nothing more once this returns.
pub fn run<S: Sink>(
    config: Config,
    mut sink: S,
    input: mpsc::UnboundedReceiver<Input>,
    stop: StopSignal,
) {
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => return sink.status(Status::Error(e.to_string())),
    };
    let local = tokio::task::LocalSet::new();
    let stderr_tail = Arc::new(Mutex::new(VecDeque::new()));
    let outcome = local.block_on(&rt, async {
        tokio::select! {
            r = connect(&config, &mut sink, input, stderr_tail.clone()) => Some(r),
            _ = stop.wait() => None,
        }
    });
    match outcome {
        None => {}
        Some(Ok(())) => sink.status(Status::Closed),
        Some(Err(e)) => {
            // A failed ssh usually explains itself on stderr ("Permission
            // denied", "command not found"); a bare "connection closed" does not.
            let tail: Vec<String> = stderr_tail.lock().unwrap().iter().cloned().collect();
            let message = if tail.is_empty() {
                format!("{e:#}")
            } else {
                format!("{e:#}: {}", tail.join(" / "))
            };
            sink.status(Status::Error(message));
        }
    }
}

async fn connect<S: Sink>(
    config: &Config,
    sink: &mut S,
    input: mpsc::UnboundedReceiver<Input>,
    stderr_tail: Arc<Mutex<VecDeque<String>>>,
) -> anyhow::Result<()> {
    let (log_tx, log_rx) = mpsc::unbounded_channel();
    match &config.endpoint {
        Endpoint::Tcp(addr) => {
            drop(log_tx);
            let stream = tokio::net::TcpStream::connect(addr).await?;
            stream.set_nodelay(true)?;
            let (rd, wr) = tokio::io::split(stream);
            session(rd, wr, config, sink, input, log_rx).await
        }
        Endpoint::Ssh(target) => {
            let mut ssh = spawn_ssh(target)?;
            if let Some(stderr) = ssh.stderr.take() {
                tokio::task::spawn_local(async move {
                    let mut lines = BufReader::new(stderr).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        {
                            let mut tail = stderr_tail.lock().unwrap();
                            if tail.len() == 3 {
                                tail.pop_front();
                            }
                            tail.push_back(line.clone());
                        }
                        let _ = log_tx.send(line);
                    }
                });
            }
            // `ssh.child` lives until this returns; dropping it kills ssh.
            let result = session(ssh.stdout, ssh.stdin, config, sink, input, log_rx).await;
            drop(ssh.child);
            result
        }
    }
}

type Received = Result<(ServerMsg, Vec<Bytes>), gliff_transport::Error>;

/// Read messages and their payloads on their own task, so the session loop
/// can wait on several sources without cancelling a half-read frame. The
/// channel is small, so a slow decoder still pushes back on the socket.
fn spawn_reader<R>(mut reader: Framed<R>) -> mpsc::Receiver<Received>
where
    R: AsyncRead + Unpin + 'static,
{
    let (tx, rx) = mpsc::channel(4);
    tokio::task::spawn_local(async move {
        loop {
            let received = read_one(&mut reader).await;
            let failed = received.is_err();
            if tx.send(received).await.is_err() || failed {
                break;
            }
        }
    });
    rx
}

async fn read_one<R: AsyncRead + Unpin>(reader: &mut Framed<R>) -> Received {
    let msg = reader.read_msg::<ServerMsg>().await?;
    let lengths = match &msg {
        ServerMsg::VideoFrame {
            data_len, aux_len, ..
        } => vec![*data_len, *aux_len],
        ServerMsg::CursorShape { argb_len, .. } => vec![*argb_len],
        ServerMsg::ClipboardData { data_len, .. } => {
            if *data_len as u64 > gliff_proto::CLIPBOARD_MAX {
                return Err(gliff_transport::Error::PayloadTooLarge(*data_len));
            }
            vec![*data_len]
        }
        _ => Vec::new(),
    };
    let mut payloads = Vec::with_capacity(lengths.len());
    for len in lengths {
        payloads.push(if len == 0 {
            Bytes::new()
        } else {
            reader.read_payload(len).await?
        });
    }
    Ok((msg, payloads))
}

async fn session<R, W, S>(
    rd: R,
    wr: W,
    config: &Config,
    sink: &mut S,
    mut input: mpsc::UnboundedReceiver<Input>,
    mut log_rx: mpsc::UnboundedReceiver<String>,
) -> anyhow::Result<()>
where
    R: AsyncRead + Unpin + 'static,
    W: AsyncWrite + Unpin + 'static,
    S: Sink,
{
    let mut writer = Framed::new(wr);
    writer
        .write_msg(&ClientMsg::Hello {
            version: PROTOCOL_VERSION,
            keymap: config.keymap.clone(),
            caps: config.caps.clone(),
        })
        .await?;
    let mut incoming = spawn_reader(Framed::new(rd));

    // Writes run on their own task, fed by `out_tx`, so reads (draining
    // video) never block on a write and the two peers cannot deadlock. The
    // session loop (acks, keyframe requests) and the platform side (input)
    // both feed it.
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Input>();
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
    {
        let out_tx = out_tx.clone();
        tokio::task::spawn_local(async move {
            while let Some(m) = input.recv().await {
                if out_tx.send(m).is_err() {
                    break;
                }
            }
        });
    }

    let mut handshaken = false;
    let mut configured = false;
    let mut logged_first = false;
    let mut frames_since = 0u32;
    let mut bytes_since = 0u64;
    let mut decode_ms_acc = 0f32;
    let mut last_report = Instant::now();

    loop {
        let received = tokio::select! {
            Some(line) = log_rx.recv() => {
                sink.status(Status::Log(line));
                continue;
            }
            received = incoming.recv() => received,
        };
        let (msg, payloads) = match received {
            None | Some(Err(gliff_transport::Error::Closed)) if handshaken => break,
            None | Some(Err(gliff_transport::Error::Closed)) => {
                anyhow::bail!("connection closed before the server answered")
            }
            Some(Err(e)) => return Err(e.into()),
            Some(Ok(m)) => m,
        };
        match msg {
            ServerMsg::HelloAck { .. } => handshaken = true,
            ServerMsg::StreamConfig {
                width,
                height,
                chroma,
                scale_milli,
                ..
            } => {
                sink.configure(width, height, chroma)?;
                configured = true;
                tracing::info!(width, height, scale_milli, ?chroma, "stream configured");
                sink.status(Status::Connected {
                    width,
                    height,
                    scale_milli,
                });
            }
            ServerMsg::VideoFrame { frame_id, .. } => {
                let (main, aux) = (&payloads[0], &payloads[1]);
                bytes_since += (main.len() + aux.len()) as u64;
                let t0 = Instant::now();
                let decoded = if configured {
                    sink.decode(main, aux)
                } else {
                    Err(anyhow::anyhow!("video before StreamConfig"))
                };
                let dec_ms = t0.elapsed().as_secs_f32() * 1000.0;
                // Every frame is acked, decoded or not: the server frees a
                // slot in its window only on an ack.
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
                            tracing::info!("first frame decoded");
                            logged_first = true;
                        }
                        sink.frame(frame);
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
            ServerMsg::CursorShape {
                width,
                height,
                hot_x,
                hot_y,
                ..
            } => sink.status(Status::Cursor {
                width,
                height,
                hot_x,
                hot_y,
                argb: payloads[0].to_vec(),
            }),
            ServerMsg::ClipboardData { .. } => {
                if let Ok(text) = String::from_utf8(payloads[0].to_vec()) {
                    sink.status(Status::Clipboard(text));
                }
            }
            ServerMsg::Error { code, message } => anyhow::bail!("server error {code}: {message}"),
            ServerMsg::CursorPos { .. }
            | ServerMsg::Pong { .. }
            | ServerMsg::ClipboardOffer { .. }
            | ServerMsg::ClipboardRequest { .. } => {}
        }
        if last_report.elapsed().as_secs_f32() >= 1.0 {
            let secs = last_report.elapsed().as_secs_f32();
            sink.status(Status::Stats {
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
            last_report = Instant::now();
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

#[cfg(test)]
mod tests;
