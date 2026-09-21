use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;
use std::time::{Duration, Instant};

use anyhow::Result;
use gliff_proto::ServerMsg;
use gliff_transport::Framed;
use tokio::io::AsyncWrite;
use tokio::sync::{mpsc, Notify};
use tokio::task::JoinHandle;

struct Packet {
    message: ServerMsg,
    payloads: Vec<Vec<u8>>,
}

#[derive(Default)]
struct Queue {
    packets: VecDeque<Packet>,
}

impl Queue {
    fn video_ready(&self) -> bool {
        !self
            .packets
            .iter()
            .any(|p| matches!(p.message, ServerMsg::VideoFrame { .. }))
    }

    fn push(&mut self, packet: Packet) {
        match &packet.message {
            ServerMsg::VideoFrame { .. } => assert!(self.video_ready()),
            ServerMsg::StreamConfig { .. } => {
                let after_video = self
                    .packets
                    .iter()
                    .rposition(|p| matches!(p.message, ServerMsg::VideoFrame { .. }))
                    .map_or(0, |i| i + 1);
                let mut index = 0;
                self.packets.retain(|p| {
                    let keep =
                        index < after_video || !matches!(p.message, ServerMsg::StreamConfig { .. });
                    index += 1;
                    keep
                });
            }
            ServerMsg::CursorShape { .. } => self.packets.retain(|p| {
                !matches!(
                    p.message,
                    ServerMsg::CursorShape { .. } | ServerMsg::CursorPos { .. }
                )
            }),
            ServerMsg::CursorPos { .. }
            | ServerMsg::ClipboardData { .. }
            | ServerMsg::Pong { .. } => {
                self.packets.retain(|p| {
                    std::mem::discriminant(&p.message) != std::mem::discriminant(&packet.message)
                });
            }
            _ => unreachable!("unsupported queued server message"),
        }
        self.packets.push_back(packet);
        debug_assert!(self.packets.len() <= 7);
    }
}

pub struct Output {
    queue: Rc<RefCell<Queue>>,
    notify: Rc<Notify>,
    started: Rc<Cell<Option<Instant>>>,
    task: JoinHandle<()>,
}

impl Output {
    pub fn spawn<W: AsyncWrite + Unpin + 'static>(
        mut writer: Framed<W>,
    ) -> (Self, mpsc::Receiver<Result<Duration>>) {
        let queue = Rc::new(RefCell::new(Queue::default()));
        let notify = Rc::new(Notify::new());
        let started = Rc::new(Cell::new(None));
        let (tx, rx) = mpsc::channel(1);
        let task = {
            let queue = queue.clone();
            let notify = notify.clone();
            let started = started.clone();
            tokio::task::spawn_local(async move {
                loop {
                    let packet = queue.borrow_mut().packets.pop_front();
                    let Some(packet) = packet else {
                        notify.notified().await;
                        continue;
                    };
                    let begin = Instant::now();
                    started.set(Some(begin));
                    let payloads: Vec<_> = packet.payloads.iter().map(Vec::as_slice).collect();
                    let result = writer
                        .write_msg_with_payloads(&packet.message, &payloads)
                        .await;
                    let elapsed = begin.elapsed();
                    started.set(None);
                    tracing::debug!(
                        write_ms = elapsed.as_secs_f64() * 1000.0,
                        "output write completed"
                    );
                    let failed = result.is_err();
                    if tx
                        .send(result.map(|_| elapsed).map_err(Into::into))
                        .await
                        .is_err()
                        || failed
                    {
                        break;
                    }
                }
            })
        };
        (
            Self {
                queue,
                notify,
                started,
                task,
            },
            rx,
        )
    }

    pub fn video_ready(&self) -> bool {
        self.queue.borrow().video_ready()
    }

    pub fn blocked_for(&self) -> Duration {
        self.started.get().map_or(Duration::ZERO, |t| t.elapsed())
    }

    pub fn send(&self, message: ServerMsg, payloads: Vec<Vec<u8>>) {
        self.queue.borrow_mut().push(Packet { message, payloads });
        self.notify.notify_one();
    }
}

impl Drop for Output {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gliff_proto::{ChromaMode, Codec};

    fn video(id: u64) -> Packet {
        Packet {
            message: ServerMsg::VideoFrame {
                frame_id: id,
                pts_us: 0,
                keyframe: id == 1,
                damage: Vec::new(),
                data_len: 64,
                aux_len: 0,
            },
            payloads: vec![vec![42; 64]],
        }
    }

    fn config(width: u32) -> Packet {
        Packet {
            message: ServerMsg::StreamConfig {
                codec: Codec::H264,
                chroma: ChromaMode::Single420,
                width,
                height: 480,
                scale_milli: 1000,
                extradata: Vec::new(),
                aux_extradata: None,
            },
            payloads: Vec::new(),
        }
    }

    #[test]
    fn updates_stay_bounded_and_preserve_video_configuration() {
        let mut queue = Queue::default();
        queue.push(config(640));
        queue.push(video(1));
        for id in 1..1000 {
            queue.push(config(800 + id));
            queue.push(Packet {
                message: ServerMsg::CursorShape {
                    id,
                    width: 1,
                    height: 1,
                    hot_x: 0,
                    hot_y: 0,
                    argb_len: 4,
                },
                payloads: vec![vec![0; 4]],
            });
            queue.push(Packet {
                message: ServerMsg::CursorPos {
                    x: 0.0,
                    y: 0.0,
                    shape_id: id,
                    visible: true,
                },
                payloads: Vec::new(),
            });
            queue.push(Packet {
                message: ServerMsg::ClipboardData {
                    mime_type: "text/plain".into(),
                    offset: 0,
                    total: 1,
                    data_len: 1,
                },
                payloads: vec![vec![0]],
            });
            queue.push(Packet {
                message: ServerMsg::Pong {
                    t: id as u64,
                    server_now_ms: 0,
                },
                payloads: Vec::new(),
            });
            assert_eq!(queue.packets.len(), 7);
        }
        assert!(matches!(
            queue.packets[0].message,
            ServerMsg::StreamConfig { width: 640, .. }
        ));
        assert!(matches!(
            queue.packets[1].message,
            ServerMsg::VideoFrame { frame_id: 1, .. }
        ));
        assert!(matches!(
            queue.packets[2].message,
            ServerMsg::StreamConfig { width: 1799, .. }
        ));
        assert!(!queue.video_ready());
    }

    #[tokio::test]
    async fn blocked_write_leaves_the_session_free_and_preserves_frames() {
        tokio::task::LocalSet::new().run_until(async {
            let (socket, peer) = tokio::io::duplex(1);
            let (output, mut reports) = Output::spawn(Framed::new(socket));
            let first = video(1);
            output.send(first.message, first.payloads);
            tokio::task::yield_now().await;
            assert!(output.started.get().is_some());
            assert!(output.video_ready());
            let second = video(2);
            output.send(second.message, second.payloads);
            assert!(!output.video_ready());
            let mut reader = Framed::new(peer);
            tokio::time::timeout(Duration::from_secs(2), async {
                for expected in [1, 2] {
                    let message: ServerMsg = reader.read_msg().await.unwrap();
                    assert!(matches!(message, ServerMsg::VideoFrame { frame_id, .. } if frame_id == expected));
                    assert_eq!(reader.read_payload(64).await.unwrap().as_ref(), &[42; 64]);
                    reports.recv().await.unwrap().unwrap();
                }
            }).await.unwrap();
            assert!(output.video_ready());
        }).await;
    }

    #[tokio::test]
    async fn write_failure_is_reported() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (socket, peer) = tokio::io::duplex(1);
                let (output, mut reports) = Output::spawn(Framed::new(socket));
                drop(peer);
                let packet = video(1);
                output.send(packet.message, packet.payloads);
                assert!(tokio::time::timeout(Duration::from_secs(1), reports.recv())
                    .await
                    .unwrap()
                    .unwrap()
                    .is_err());
            })
            .await;
    }
}
