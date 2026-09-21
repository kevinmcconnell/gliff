//! Read the server's messages on a task of their own, each with the
//! payloads it declares, so a client loop can also listen to another
//! source (the UDP thread) without cancelling a read half-way.

use bytes::Bytes;
use gliff_proto::ServerMsg;
use tokio::io::AsyncRead;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};

use crate::Framed;

/// A message from the connection with the payloads it declared: a video
/// frame's streams, a cursor image, or clipboard bytes, in `main`.
pub struct Incoming {
    pub msg: ServerMsg,
    pub main: Bytes,
    pub aux: Bytes,
}

/// Spawn the reader on the current `LocalSet`. The channel closes when
/// the connection does.
pub fn spawn_server_reader<R>(mut reader: Framed<R>) -> UnboundedReceiver<Incoming>
where
    R: AsyncRead + Unpin + 'static,
{
    let (tx, rx) = unbounded_channel();
    tokio::task::spawn_local(async move {
        loop {
            let Ok(msg) = reader.read_msg::<ServerMsg>().await else {
                break;
            };
            let (main, aux) = match &msg {
                ServerMsg::VideoFrame {
                    data_len, aux_len, ..
                } => {
                    let Ok(main) = reader.read_payload(*data_len).await else {
                        break;
                    };
                    let aux = if *aux_len > 0 {
                        match reader.read_payload(*aux_len).await {
                            Ok(a) => a,
                            Err(_) => break,
                        }
                    } else {
                        Bytes::new()
                    };
                    (main, aux)
                }
                ServerMsg::CursorShape { argb_len, .. } => {
                    let Ok(argb) = reader.read_payload(*argb_len).await else {
                        break;
                    };
                    (argb, Bytes::new())
                }
                ServerMsg::ClipboardData { data_len, .. } => {
                    if *data_len as u64 > gliff_proto::CLIPBOARD_MAX {
                        break;
                    }
                    let Ok(data) = reader.read_payload(*data_len).await else {
                        break;
                    };
                    (data, Bytes::new())
                }
                _ => (Bytes::new(), Bytes::new()),
            };
            if tx.send(Incoming { msg, main, aux }).is_err() {
                break;
            }
        }
    });
    rx
}
