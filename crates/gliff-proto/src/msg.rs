//! Client and server messages. Keep variants additive; never renumber.

use serde::{Deserialize, Serialize};

pub use crate::clipboard::{ClipboardFile, ClipboardItem, ClipboardMsg};

pub const PROTOCOL_VERSION: u16 = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Codec {
    /// Uncompressed BGRA, phase 1 / localhost only.
    RawBgra,
    H264,
    H265,
    Av1,
}

/// Which pipeline encodes or decodes on one end, reported for the stats
/// display.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum VideoPipeline {
    Gpu,
    Cpu,
}

impl VideoPipeline {
    pub fn label(self) -> &'static str {
        match self {
            Self::Gpu => "gpu",
            Self::Cpu => "cpu",
        }
    }
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
    /// The local selection changed; see [`ClipboardMsg::Offer`].
    ClipboardOffer {
        serial: u32,
        mime_types: Vec<String>,
        files: Vec<ClipboardFile>,
    },
    ClipboardRequest {
        id: u32,
        serial: u32,
        item: ClipboardItem,
    },
    /// `data_len` payload bytes follow the header.
    ClipboardData {
        id: u32,
        offset: u64,
        data_len: u32,
        done: bool,
    },
    Ping {
        t: u64,
    },
    Bye,
    ClipboardAck {
        id: u32,
        received: u64,
    },
    ClipboardAbort {
        id: u32,
    },
    /// The local keymap changed since `Hello`; same format as its `keymap`.
    Keymap {
        keymap: String,
    },
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
        /// Which pipeline encodes on the server, for the client's stats.
        pipeline: VideoPipeline,
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
        serial: u32,
        mime_types: Vec<String>,
        files: Vec<ClipboardFile>,
    },
    ClipboardRequest {
        id: u32,
        serial: u32,
        item: ClipboardItem,
    },
    ClipboardData {
        id: u32,
        offset: u64,
        data_len: u32,
        done: bool,
    },
    Pong {
        t: u64,
        server_now_ms: u64,
    },
    Error {
        code: u16,
        message: String,
    },
    ClipboardAck {
        id: u32,
        received: u64,
    },
    ClipboardAbort {
        id: u32,
    },
}

/// The clipboard variants have the same shape in both directions so the
/// transfer logic can be shared; these convert to and from [`ClipboardMsg`].
macro_rules! clipboard_bridge {
    ($msg:ident) => {
        impl From<ClipboardMsg> for $msg {
            fn from(m: ClipboardMsg) -> Self {
                match m {
                    ClipboardMsg::Offer {
                        serial,
                        mime_types,
                        files,
                    } => $msg::ClipboardOffer {
                        serial,
                        mime_types,
                        files,
                    },
                    ClipboardMsg::Request { id, serial, item } => {
                        $msg::ClipboardRequest { id, serial, item }
                    }
                    ClipboardMsg::Data {
                        id,
                        offset,
                        data_len,
                        done,
                    } => $msg::ClipboardData {
                        id,
                        offset,
                        data_len,
                        done,
                    },
                    ClipboardMsg::Ack { id, received } => $msg::ClipboardAck { id, received },
                    ClipboardMsg::Abort { id } => $msg::ClipboardAbort { id },
                }
            }
        }

        impl $msg {
            /// Split off a clipboard message; any other message is handed back.
            pub fn into_clipboard(self) -> Result<ClipboardMsg, Self> {
                Ok(match self {
                    $msg::ClipboardOffer {
                        serial,
                        mime_types,
                        files,
                    } => ClipboardMsg::Offer {
                        serial,
                        mime_types,
                        files,
                    },
                    $msg::ClipboardRequest { id, serial, item } => {
                        ClipboardMsg::Request { id, serial, item }
                    }
                    $msg::ClipboardData {
                        id,
                        offset,
                        data_len,
                        done,
                    } => ClipboardMsg::Data {
                        id,
                        offset,
                        data_len,
                        done,
                    },
                    $msg::ClipboardAck { id, received } => ClipboardMsg::Ack { id, received },
                    $msg::ClipboardAbort { id } => ClipboardMsg::Abort { id },
                    other => return Err(other),
                })
            }
        }
    };
}

clipboard_bridge!(ClientMsg);
clipboard_bridge!(ServerMsg);

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
            ClientMsg::Keymap {
                keymap: "xkb".into(),
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

    #[test]
    fn clipboard_messages_bridge_both_directions() {
        let msgs = vec![
            ClipboardMsg::Offer {
                serial: 4,
                mime_types: vec!["image/png".into()],
                files: vec![ClipboardFile {
                    path: "a/b.txt".into(),
                    size: 3,
                    dir: false,
                }],
            },
            ClipboardMsg::Request {
                id: 7,
                serial: 4,
                item: ClipboardItem::File(0),
            },
            ClipboardMsg::Data {
                id: 7,
                offset: 10,
                data_len: 3,
                done: true,
            },
            ClipboardMsg::Ack {
                id: 7,
                received: 13,
            },
            ClipboardMsg::Abort { id: 7 },
        ];
        for m in msgs {
            let c = ClientMsg::from(m.clone());
            let bytes = postcard::to_stdvec(&c).unwrap();
            let back: ClientMsg = postcard::from_bytes(&bytes).unwrap();
            assert_eq!(back.into_clipboard(), Ok(m.clone()));
            let s = ServerMsg::from(m.clone());
            let bytes = postcard::to_stdvec(&s).unwrap();
            let back: ServerMsg = postcard::from_bytes(&bytes).unwrap();
            assert_eq!(back.into_clipboard(), Ok(m));
        }
        assert_eq!(ClientMsg::Bye.into_clipboard(), Err(ClientMsg::Bye));
    }
}
