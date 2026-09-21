use super::*;
use gliff_proto::{Codec, SessionInfo};
use std::sync::mpsc as std_mpsc;

/// Records what the session did; `fail` makes those decode calls (by index)
/// fail.
#[derive(Default)]
struct Recorder {
    configured: Vec<(u32, u32, ChromaMode)>,
    decoded: Vec<(usize, usize)>,
    frames: usize,
    statuses: Vec<String>,
    fail: Vec<usize>,
}

impl Sink for Recorder {
    type Frame = ();

    fn configure(&mut self, width: u32, height: u32, chroma: ChromaMode) -> anyhow::Result<()> {
        self.configured.push((width, height, chroma));
        Ok(())
    }

    fn decode(&mut self, main: &[u8], aux: &[u8]) -> anyhow::Result<Option<()>> {
        let n = self.decoded.len();
        self.decoded.push((main.len(), aux.len()));
        if self.fail.contains(&n) {
            anyhow::bail!("bad frame");
        }
        Ok(Some(()))
    }

    fn frame(&mut self, _: ()) {
        self.frames += 1;
    }

    fn status(&mut self, status: Status) {
        self.statuses.push(format!("{status:?}"));
    }
}

fn config() -> Config {
    Config {
        endpoint: Endpoint::Tcp(String::new()),
        keymap: "km".into(),
        caps: ClientCaps {
            codecs: vec![Codec::H264],
            max_width: 5120,
            max_height: 2880,
            chroma: vec![ChromaMode::Dual420],
        },
    }
}

fn stream_config(width: u32, height: u32) -> ServerMsg {
    ServerMsg::StreamConfig {
        codec: Codec::H264,
        chroma: ChromaMode::Dual420,
        width,
        height,
        scale_milli: 2000,
        extradata: Vec::new(),
        aux_extradata: None,
    }
}

async fn send_frame<W: AsyncWrite + Unpin>(server: &mut Framed<W>, frame_id: u64) {
    let (main, aux) = (vec![1u8; 100], vec![2u8; 50]);
    server
        .write_msg_with_payloads(
            &ServerMsg::VideoFrame {
                frame_id,
                pts_us: 0,
                keyframe: frame_id == 0,
                damage: Vec::new(),
                data_len: main.len() as u32,
                aux_len: aux.len() as u32,
            },
            &[&main, &aux],
        )
        .await
        .unwrap();
}

/// Run a session against a scripted server. Returns what the client sent.
async fn scripted(recorder: &mut Recorder) -> (anyhow::Result<()>, Vec<ClientMsg>) {
    let (client_side, server_side) = tokio::io::duplex(1 << 16);
    let (crd, cwr) = tokio::io::split(client_side);
    let (srd, swr) = tokio::io::split(server_side);
    let (_input_tx, input_rx) = mpsc::unbounded_channel();
    let (_log_tx, log_rx) = mpsc::unbounded_channel();
    let cfg = config();

    let server = async move {
        let mut rd = Framed::new(srd);
        let mut wr = Framed::new(swr);
        let hello: ClientMsg = rd.read_msg().await.unwrap();
        let mut got = vec![hello];
        wr.write_msg(&ServerMsg::HelloAck {
            version: PROTOCOL_VERSION,
            session: SessionInfo {
                headless: true,
                output: "HEADLESS-1".into(),
            },
            outputs: Vec::new(),
        })
        .await
        .unwrap();
        wr.write_msg(&stream_config(640, 360)).await.unwrap();
        for id in 0..3 {
            send_frame(&mut wr, id).await;
        }
        wr.write_msg(&stream_config(1280, 720)).await.unwrap();
        send_frame(&mut wr, 3).await;
        // Four acks and one keyframe request, then hang up.
        while got.len() < 6 {
            got.push(rd.read_msg().await.unwrap());
        }
        drop(wr);
        got
    };
    let client = session(crd, cwr, &cfg, recorder, input_rx, log_rx);
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async move {
            let (result, got) = tokio::join!(client, server);
            (result, got)
        })
        .await
}

#[tokio::test]
async fn acks_every_frame_even_when_decode_fails() {
    let mut recorder = Recorder {
        fail: vec![1],
        ..Default::default()
    };
    let (result, sent) = scripted(&mut recorder).await;
    result.unwrap();

    assert!(matches!(&sent[0], ClientMsg::Hello { keymap, .. } if keymap == "km"));
    let acks: Vec<u64> = sent
        .iter()
        .filter_map(|m| match m {
            ClientMsg::FrameAck { frame_id, .. } => Some(*frame_id),
            _ => None,
        })
        .collect();
    assert_eq!(acks, vec![0, 1, 2, 3]);
    let keyframe_requests = sent
        .iter()
        .filter(|m| matches!(m, ClientMsg::RequestKeyframe))
        .count();
    assert_eq!(keyframe_requests, 1);

    assert_eq!(recorder.decoded, vec![(100, 50); 4]);
    assert_eq!(recorder.frames, 3);
    assert_eq!(
        recorder.configured,
        vec![
            (640, 360, ChromaMode::Dual420),
            (1280, 720, ChromaMode::Dual420)
        ]
    );
    assert!(recorder.statuses[0].starts_with("Connected { width: 640"));
}

#[test]
fn stop_ends_a_session_from_another_thread() {
    // A server that accepts and then says nothing.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let hold = std::thread::spawn(move || listener.accept().map(|(s, _)| s));

    let (handle, signal) = stopper();
    let (done_tx, done_rx) = std_mpsc::channel();
    let worker = std::thread::spawn(move || {
        let mut recorder = Recorder::default();
        let (_tx, rx) = mpsc::unbounded_channel();
        let cfg = Config {
            endpoint: Endpoint::Tcp(addr),
            ..config()
        };
        run(cfg, &mut recorder, rx, signal);
        done_tx.send(recorder.statuses).unwrap();
    });
    std::thread::sleep(std::time::Duration::from_millis(200));
    handle.stop();
    let statuses = done_rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("session stopped");
    worker.join().unwrap();
    drop(hold.join());
    assert!(
        statuses.is_empty(),
        "a stopped session reports nothing: {statuses:?}"
    );
}

#[test]
fn closed_before_handshake_is_an_error() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let hang_up = std::thread::spawn(move || drop(listener.accept()));
    let mut recorder = Recorder::default();
    let (_tx, rx) = mpsc::unbounded_channel();
    let (_handle, signal) = stopper();
    run(
        Config {
            endpoint: Endpoint::Tcp(addr),
            ..config()
        },
        &mut recorder,
        rx,
        signal,
    );
    hang_up.join().unwrap();
    // Depending on timing the hang-up shows as a closed read or a reset
    // write; either way it is one error, not a clean close.
    assert_eq!(recorder.statuses.len(), 1);
    assert!(
        recorder.statuses[0].starts_with("Error("),
        "{:?}",
        recorder.statuses
    );
}
