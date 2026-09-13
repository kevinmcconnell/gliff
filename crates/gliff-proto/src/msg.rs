//! Client and server messages. Keep variants additive; never renumber.

use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u16 = 2;

/// Chunk cap for clipboard payloads on the wire.
pub const CLIPBOARD_CHUNK: usize = 256 * 1024;
/// Total clipboard transfer cap.
pub const CLIPBOARD_MAX: u64 = 32 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Codec {
    /// Uncompressed BGRA, phase 1 / localhost only.
    RawBgra,
    H264,
    H265,
    Av1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChromaMode {
    /// Single stream, encoder produced 4:4:4.
    Native444,
    /// AVC444-style: main 4:2:0 stream plus an auxiliary 4:2:0 stream carrying
    /// the chroma the main stream dropped.
    Dual420,
    /// Low-bandwidth fallback only.
    Single420,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Axis {
    Vertical,
    Horizontal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientCaps {
    pub codecs: Vec<Codec>,
    pub max_width: u32,
    pub max_height: u32,
    /// Chroma modes the client can decode, preferred first.
    pub chroma: Vec<ChromaMode>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputInfo {
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub scale_milli: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionInfo {
    pub headless: bool,
    pub output: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ClientMsg {
    Hello {
        version: u16,
        keymap: String,
        caps: ClientCaps,
    },
    Resize {
        width: u32,
        height: u32,
        scale: f32,
    },
    Key {
        keycode: u32,
        pressed: bool,
    },
    PointerMotion {
        x: f64,
        y: f64,
    },
    PointerButton {
        button: u32,
        pressed: bool,
    },
    PointerAxis {
        axis: Axis,
        value: f64,
        discrete: Option<i32>,
        stop: bool,
    },
    FrameAck {
        frame_id: u64,
        decoded_at_ms: u64,
    },
    RequestKeyframe,
    ClipboardOffer {
        mime_types: Vec<String>,
    },
    ClipboardRequest {
        mime_type: String,
    },
    ClipboardData {
        mime_type: String,
        offset: u64,
        total: u64,
        data_len: u32,
    },
    Ping {
        t: u64,
    },
    Bye,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ServerMsg {
    HelloAck {
        version: u16,
        session: SessionInfo,
        outputs: Vec<OutputInfo>,
    },
    StreamConfig {
        codec: Codec,
        chroma: ChromaMode,
        /// Stream size in physical pixels.
        width: u32,
        height: u32,
        /// Output scale x1000: the remote's logical size is `width / scale`,
        /// and pointer coordinates are sent in that logical space.
        scale_milli: u32,
        extradata: Vec<u8>,
        aux_extradata: Option<Vec<u8>>,
    },
    VideoFrame {
        frame_id: u64,
        pts_us: u64,
        keyframe: bool,
        damage: Vec<Rect>,
        data_len: u32,
        /// `aux_len > 0` iff `chroma == Dual420`; bytes follow the header.
        aux_len: u32,
    },
    CursorShape {
        id: u32,
        width: u32,
        height: u32,
        hot_x: i32,
        hot_y: i32,
        argb_len: u32,
    },
    CursorPos {
        x: f64,
        y: f64,
        shape_id: u32,
        visible: bool,
    },
    ClipboardOffer {
        mime_types: Vec<String>,
    },
    ClipboardRequest {
        mime_type: String,
    },
    ClipboardData {
        mime_type: String,
        offset: u64,
        total: u64,
        data_len: u32,
    },
    Pong {
        t: u64,
        server_now_ms: u64,
    },
    Error {
        code: u16,
        message: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_messages() {
        let msgs = vec![
            ClientMsg::Hello {
                version: 1,
                keymap: "xkb".into(),
                caps: ClientCaps {
                    codecs: vec![Codec::H264],
                    max_width: 3840,
                    max_height: 2160,
                    chroma: vec![ChromaMode::Dual420, ChromaMode::Single420],
                },
            },
            ClientMsg::Key {
                keycode: 30,
                pressed: true,
            },
            ClientMsg::PointerAxis {
                axis: Axis::Vertical,
                value: 1.5,
                discrete: Some(1),
                stop: false,
            },
            ClientMsg::FrameAck {
                frame_id: 42,
                decoded_at_ms: 1000,
            },
        ];
        for m in msgs {
            let bytes = postcard::to_stdvec(&m).unwrap();
            let back: ClientMsg = postcard::from_bytes(&bytes).unwrap();
            assert_eq!(m, back);
        }
        let s = ServerMsg::VideoFrame {
            frame_id: 1,
            pts_us: 2,
            keyframe: true,
            damage: vec![Rect {
                x: 0,
                y: 0,
                width: 10,
                height: 10,
            }],
            data_len: 100,
            aux_len: 50,
        };
        let bytes = postcard::to_stdvec(&s).unwrap();
        assert_eq!(postcard::from_bytes::<ServerMsg>(&bytes).unwrap(), s);
    }
}
