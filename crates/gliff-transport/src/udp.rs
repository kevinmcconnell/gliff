//! The video path over UDP: sealed datagrams, a frame split into parts, and
//! the receiver that puts frames back together, asks for the parts it
//! misses, and gives a frame up when they do not come.
//!
//! Every datagram is `seq (u64 LE) || ChaCha20-Poly1305(plaintext)` with the
//! sequence as nonce (together with the direction) and as associated data.
//! The key is a per-session secret the server hands over inside the SSH
//! session, so a datagram that does not authenticate is dropped without
//! further work. The plaintext is a postcard `Packet`, followed by the
//! part's bytes for a `Part`.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Nonce};
use serde::{Deserialize, Serialize};

pub const KEY_LEN: usize = 32;
/// The largest datagram sent: fits an IPv6 packet on a 1280-byte MTU
/// (a Tailscale tunnel, for one) without fragmentation.
pub const MAX_DATAGRAM: usize = 1200;
const SEQ_LEN: usize = 8;
const TAG_LEN: usize = 16;
pub const DIR_CLIENT: u8 = 0;
pub const DIR_SERVER: u8 = 1;

/// What one datagram carries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Packet {
    /// Client to server: is the path open, and what is its RTT. Also the
    /// keepalive that holds a NAT mapping. `t` only rises.
    Probe { t: u64 },
    ProbeAck { t: u64 },
    /// One part of a frame; the part's bytes follow. The header repeats
    /// in every part so no single loss can hide the frame.
    Part {
        frame_id: u64,
        index: u16,
        count: u16,
        keyframe: bool,
        reference: Option<u64>,
        pts_us: u64,
        main_len: u32,
        total_len: u32,
    },
    /// Client to server: send these parts of this frame again.
    Nack { frame_id: u64, missing: Vec<u16> },
    /// Client to server: every frame up to this one is decoded or dropped.
    Ack { frame_id: u64, decoded_at_ms: u64 },
    /// Client to server: as `ClientMsg::Recover`.
    Recover { last_good: Option<u64> },
}

fn nonce(dir: u8, seq: u64) -> Nonce {
    let mut n = [0u8; 12];
    n[0] = dir;
    n[4..].copy_from_slice(&seq.to_le_bytes());
    Nonce::from(n)
}

/// Seals outgoing datagrams with a fresh sequence number each.
pub struct Sealer {
    cipher: ChaCha20Poly1305,
    dir: u8,
    next: u64,
}

impl Sealer {
    pub fn new(key: &[u8; KEY_LEN], dir: u8) -> Self {
        Self {
            cipher: ChaCha20Poly1305::new(key.into()),
            dir,
            next: 1,
        }
    }

    pub fn seal(&mut self, plaintext: &[u8]) -> Vec<u8> {
        let seq = self.next;
        self.next += 1;
        let mut out = Vec::with_capacity(SEQ_LEN + plaintext.len() + TAG_LEN);
        out.extend_from_slice(&seq.to_le_bytes());
        let sealed = self
            .cipher
            .encrypt(
                &nonce(self.dir, seq),
                Payload {
                    msg: plaintext,
                    aad: &out[..SEQ_LEN],
                },
            )
            .expect("ChaCha20-Poly1305 encrypt cannot fail");
        out.extend_from_slice(&sealed);
        out
    }

    pub fn seal_packet(&mut self, packet: &Packet) -> Vec<u8> {
        self.seal(&postcard::to_stdvec(packet).expect("packet serialises"))
    }
}

/// Opens incoming datagrams from one direction and rejects any sequence
/// seen before, or older than the window.
pub struct Opener {
    cipher: ChaCha20Poly1305,
    dir: u8,
    window: ReplayWindow,
}

impl Opener {
    pub fn new(key: &[u8; KEY_LEN], dir: u8) -> Self {
        Self {
            cipher: ChaCha20Poly1305::new(key.into()),
            dir,
            window: ReplayWindow::default(),
        }
    }

    /// The packet and, for a `Part`, its bytes. `None` for anything that
    /// does not authenticate, replays, or does not parse.
    pub fn open(&mut self, datagram: &[u8]) -> Option<(Packet, Vec<u8>)> {
        if datagram.len() < SEQ_LEN + TAG_LEN {
            return None;
        }
        let seq = u64::from_le_bytes(datagram[..SEQ_LEN].try_into().ok()?);
        if !self.window.is_fresh(seq) {
            return None;
        }
        let plain = self
            .cipher
            .decrypt(
                &nonce(self.dir, seq),
                Payload {
                    msg: &datagram[SEQ_LEN..],
                    aad: &datagram[..SEQ_LEN],
                },
            )
            .ok()?;
        self.window.mark(seq);
        let (packet, rest) = postcard::take_from_bytes::<Packet>(&plain).ok()?;
        Some((packet, rest.to_vec()))
    }
}

/// Anti-replay window over sequence numbers: a bitmap of the last
/// `BITS` sequences below the highest seen.
struct ReplayWindow {
    highest: u64,
    bits: [u64; Self::WORDS],
}

impl Default for ReplayWindow {
    fn default() -> Self {
        Self {
            highest: 0,
            bits: [0; Self::WORDS],
        }
    }
}

impl ReplayWindow {
    const WORDS: usize = 64;
    const BITS: u64 = (Self::WORDS * 64) as u64;

    fn is_fresh(&self, seq: u64) -> bool {
        if seq == 0 {
            return false;
        }
        if seq > self.highest {
            return true;
        }
        if self.highest - seq >= Self::BITS {
            return false;
        }
        !self.test(seq)
    }

    fn test(&self, seq: u64) -> bool {
        let bit = seq % Self::BITS;
        self.bits[(bit / 64) as usize] & (1 << (bit % 64)) != 0
    }

    fn set(&mut self, seq: u64, on: bool) {
        let bit = seq % Self::BITS;
        let word = &mut self.bits[(bit / 64) as usize];
        if on {
            *word |= 1 << (bit % 64);
        } else {
            *word &= !(1 << (bit % 64));
        }
    }

    fn mark(&mut self, seq: u64) {
        if seq > self.highest {
            // Clear the bits the window slides over.
            let advance = (seq - self.highest).min(Self::BITS);
            for s in 1..=advance {
                self.set(self.highest + s, false);
            }
            self.highest = seq;
        }
        self.set(seq, true);
    }
}

/// The header every part of a frame repeats.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameInfo {
    pub frame_id: u64,
    pub keyframe: bool,
    pub reference: Option<u64>,
    pub pts_us: u64,
}

/// Bytes of frame payload per part: the datagram less the sequence, the
/// tag and the largest possible `Part` header.
pub fn part_payload_len() -> usize {
    let header = postcard::to_stdvec(&Packet::Part {
        frame_id: u64::MAX,
        index: u16::MAX,
        count: u16::MAX,
        keyframe: true,
        reference: Some(u64::MAX),
        pts_us: u64::MAX,
        main_len: u32::MAX,
        total_len: u32::MAX,
    })
    .expect("packet serialises")
    .len();
    MAX_DATAGRAM - SEQ_LEN - TAG_LEN - header
}

/// Split a frame into part plaintexts (header then bytes), ready to seal.
pub fn split_frame(info: &FrameInfo, main: &[u8], aux: &[u8]) -> Vec<Vec<u8>> {
    let chunk = part_payload_len();
    let total = main.len() + aux.len();
    let count = total.div_ceil(chunk).max(1);
    assert!(count <= u16::MAX as usize, "frame too large for the part index");
    let body: Vec<u8> = [main, aux].concat();
    (0..count)
        .map(|i| {
            let start = i * chunk;
            let end = (start + chunk).min(total);
            let header = Packet::Part {
                frame_id: info.frame_id,
                index: i as u16,
                count: count as u16,
                keyframe: info.keyframe,
                reference: info.reference,
                pts_us: info.pts_us,
                main_len: main.len() as u32,
                total_len: total as u32,
            };
            let mut out = postcard::to_stdvec(&header).expect("packet serialises");
            out.extend_from_slice(&body[start..end]);
            out
        })
        .collect()
}

/// A frame put back together.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssembledFrame {
    pub info: FrameInfo,
    pub main: Vec<u8>,
    pub aux: Vec<u8>,
}

/// What the assembler hands up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// A complete frame, in frame order.
    Frame(AssembledFrame),
    /// A frame given up on: its parts did not come. It has been acked.
    Lost(u64),
}

struct Pending {
    info: FrameInfo,
    count: u16,
    main_len: usize,
    total_len: usize,
    parts: Vec<Option<Vec<u8>>>,
    have: u16,
    first_seen: Instant,
    last_nack: Option<Instant>,
    nacks: u32,
}

impl Pending {
    fn complete(&self) -> bool {
        self.have == self.count
    }

    fn missing(&self) -> Vec<u16> {
        self.parts
            .iter()
            .enumerate()
            .filter(|(_, p)| p.is_none())
            .map(|(i, _)| i as u16)
            .collect()
    }
}

/// Puts parts back into frames and delivers them in order. A frame with
/// parts missing is asked for again after a gap shows (a later part or
/// frame arrived) or after a wait; after `MAX_NACKS` unanswered requests
/// it is dropped and the stream goes on from the next frame.
pub struct Assembler {
    frames: BTreeMap<u64, Pending>,
    /// Every frame below this is delivered or dropped.
    next: u64,
    rtt: Duration,
    /// Packets to send to the server (NACKs, acks for dropped frames).
    pub outgoing: Vec<Packet>,
    pub stats: Stats,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Stats {
    pub frames: u64,
    pub lost: u64,
    pub nacks: u64,
    /// Frames completed only after a NACK.
    pub repaired: u64,
}

impl Assembler {
    const MAX_NACKS: u32 = 2;
    const MIN_NACK_INTERVAL: Duration = Duration::from_millis(8);
    const MAX_NACK_INTERVAL: Duration = Duration::from_millis(250);
    /// Frames kept ahead of the one being waited for; beyond this the
    /// oldest is given up so a lost frame cannot stall the stream for long.
    const MAX_AHEAD: usize = 6;

    pub fn new() -> Self {
        Self {
            frames: BTreeMap::new(),
            next: 0,
            rtt: Duration::from_millis(30),
            outgoing: Vec::new(),
            stats: Stats::default(),
        }
    }

    pub fn set_rtt(&mut self, rtt: Duration) {
        self.rtt = rtt;
    }

    /// How long to wait for a retransmit before asking again: a round trip
    /// and a little, bounded.
    fn nack_interval(&self) -> Duration {
        (self.rtt + self.rtt / 4).clamp(Self::MIN_NACK_INTERVAL, Self::MAX_NACK_INTERVAL)
    }

    /// Feed one opened packet. Only `Part`s matter here.
    pub fn on_packet(&mut self, packet: Packet, bytes: Vec<u8>, now: Instant) -> Vec<Event> {
        let Packet::Part {
            frame_id,
            index,
            count,
            keyframe,
            reference,
            pts_us,
            main_len,
            total_len,
        } = packet
        else {
            return Vec::new();
        };
        if frame_id < self.next || count == 0 || index >= count {
            return Vec::new();
        }
        let pending = self.frames.entry(frame_id).or_insert_with(|| Pending {
            info: FrameInfo {
                frame_id,
                keyframe,
                reference,
                pts_us,
            },
            count,
            main_len: main_len as usize,
            total_len: total_len as usize,
            parts: vec![None; count as usize],
            have: 0,
            first_seen: now,
            last_nack: None,
            nacks: 0,
        });
        if pending.count != count {
            return Vec::new();
        }
        if pending.parts[index as usize].is_none() {
            pending.parts[index as usize] = Some(bytes);
            pending.have += 1;
        }
        self.advance(now)
    }

    /// Run the timers: call every few milliseconds while frames are pending.
    pub fn tick(&mut self, now: Instant) -> Vec<Event> {
        self.advance(now)
    }

    pub fn pending(&self) -> usize {
        self.frames.len()
    }

    fn advance(&mut self, now: Instant) -> Vec<Event> {
        let mut events = Vec::new();
        loop {
            let Some((&id, _)) = self.frames.iter().next() else {
                break;
            };
            let complete = self.frames[&id].complete();
            if complete {
                let p = self.frames.remove(&id).expect("present");
                if p.nacks > 0 {
                    self.stats.repaired += 1;
                }
                self.stats.frames += 1;
                self.next = id + 1;
                let body: Vec<u8> = p.parts.into_iter().flatten().flatten().collect();
                if body.len() != p.total_len || p.main_len > body.len() {
                    // Parts disagree with the header: treat as lost.
                    self.stats.lost += 1;
                    events.push(Event::Lost(id));
                    continue;
                }
                let aux = body[p.main_len..].to_vec();
                let mut main = body;
                main.truncate(p.main_len);
                events.push(Event::Frame(AssembledFrame {
                    info: p.info,
                    main,
                    aux,
                }));
                continue;
            }
            // The oldest frame is incomplete: ask for its parts, or give up.
            let interval = self.nack_interval();
            let ahead = self.frames.len();
            let later_seen = ahead > 1;
            let p = self.frames.get_mut(&id).expect("present");
            let gap_shown = later_seen
                || p.parts.iter().rposition(Option::is_some).unwrap_or(0) + 1 > p.have as usize;
            let due = match p.last_nack {
                None => gap_shown || now.duration_since(p.first_seen) >= interval / 2,
                Some(t) => now.duration_since(t) >= interval,
            };
            if !due {
                break;
            }
            let give_up = p.nacks >= Self::MAX_NACKS || ahead > Self::MAX_AHEAD;
            if give_up {
                self.frames.remove(&id);
                self.next = id + 1;
                self.stats.lost += 1;
                self.outgoing.push(Packet::Ack {
                    frame_id: id,
                    decoded_at_ms: 0,
                });
                events.push(Event::Lost(id));
                continue;
            }
            let missing = p.missing();
            p.last_nack = Some(now);
            p.nacks += 1;
            self.stats.nacks += 1;
            self.outgoing.push(Packet::Nack {
                frame_id: id,
                missing,
            });
            break;
        }
        events
    }
}

impl Default for Assembler {
    fn default() -> Self {
        Self::new()
    }
}

/// The server's side: parts of recent frames, kept for retransmission.
pub struct PartCache {
    frames: std::collections::VecDeque<(u64, Vec<Vec<u8>>)>,
    bytes: usize,
}

impl PartCache {
    /// Retransmit cache: enough frames for a NACK to arrive at any RTT the
    /// ack window allows.
    const MAX_FRAMES: usize = 16;
    const MAX_BYTES: usize = 8 << 20;

    pub fn new() -> Self {
        Self {
            frames: std::collections::VecDeque::new(),
            bytes: 0,
        }
    }

    pub fn insert(&mut self, frame_id: u64, parts: Vec<Vec<u8>>) {
        self.bytes += parts.iter().map(Vec::len).sum::<usize>();
        self.frames.push_back((frame_id, parts));
        while self.frames.len() > Self::MAX_FRAMES || self.bytes > Self::MAX_BYTES {
            if let Some((_, old)) = self.frames.pop_front() {
                self.bytes -= old.iter().map(Vec::len).sum::<usize>();
            }
        }
    }

    pub fn part(&self, frame_id: u64, index: u16) -> Option<&[u8]> {
        self.frames
            .iter()
            .find(|(id, _)| *id == frame_id)
            .and_then(|(_, parts)| parts.get(index as usize))
            .map(Vec::as_slice)
    }
}

impl Default for PartCache {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> [u8; KEY_LEN] {
        [7u8; KEY_LEN]
    }

    fn info(id: u64) -> FrameInfo {
        FrameInfo {
            frame_id: id,
            keyframe: id == 0,
            reference: (id > 0).then(|| id - 1),
            pts_us: id * 16_667,
        }
    }

    #[test]
    fn seal_and_open_round_trip_and_reject_tampering() {
        let mut sealer = Sealer::new(&key(), DIR_SERVER);
        let mut opener = Opener::new(&key(), DIR_SERVER);
        let d = sealer.seal_packet(&Packet::Probe { t: 5 });
        assert!(d.len() <= MAX_DATAGRAM);
        assert_eq!(opener.open(&d), Some((Packet::Probe { t: 5 }, Vec::new())));
        // A replay is dropped.
        assert_eq!(opener.open(&d), None);
        let mut bad = sealer.seal_packet(&Packet::Probe { t: 6 });
        bad[12] ^= 1;
        assert_eq!(opener.open(&bad), None);
        // The other direction's key stream does not open it.
        let mut wrong_dir = Opener::new(&key(), DIR_CLIENT);
        assert_eq!(wrong_dir.open(&sealer.seal_packet(&Packet::Probe { t: 7 })), None);
    }

    #[test]
    fn replay_window_tracks_out_of_order_sequences() {
        let mut w = ReplayWindow::default();
        assert!(!w.is_fresh(0));
        w.mark(10);
        assert!(w.is_fresh(9));
        w.mark(9);
        assert!(!w.is_fresh(9));
        assert!(!w.is_fresh(10));
        assert!(w.is_fresh(11));
        w.mark(10 + ReplayWindow::BITS);
        assert!(!w.is_fresh(10));
        assert!(w.is_fresh(11 + ReplayWindow::BITS));
    }

    #[test]
    fn split_parts_fit_the_datagram_and_reassemble() {
        let main = vec![1u8; 5000];
        let aux = vec![2u8; 3000];
        let parts = split_frame(&info(3), &main, &aux);
        let mut sealer = Sealer::new(&key(), DIR_SERVER);
        let mut opener = Opener::new(&key(), DIR_SERVER);
        let mut asm = Assembler::new();
        asm.next = 3;
        let now = Instant::now();
        let mut events = Vec::new();
        for p in &parts {
            let d = sealer.seal(p);
            assert!(d.len() <= MAX_DATAGRAM, "{}", d.len());
            let (pkt, bytes) = opener.open(&d).unwrap();
            events.extend(asm.on_packet(pkt, bytes, now));
        }
        assert_eq!(
            events,
            vec![Event::Frame(AssembledFrame {
                info: info(3),
                main,
                aux
            })]
        );
        assert!(asm.outgoing.is_empty());
    }

    fn feed(asm: &mut Assembler, parts: &[Vec<u8>], skip: &[usize], now: Instant) -> Vec<Event> {
        let mut events = Vec::new();
        for (i, p) in parts.iter().enumerate() {
            if skip.contains(&i) {
                continue;
            }
            let (pkt, bytes) = postcard::take_from_bytes::<Packet>(p).unwrap();
            events.extend(asm.on_packet(pkt, bytes.to_vec(), now));
        }
        events
    }

    #[test]
    fn a_missing_part_is_nacked_then_delivered_in_order() {
        let mut asm = Assembler::new();
        let t0 = Instant::now();
        let f0 = split_frame(&info(0), &vec![0u8; 3000], &[]);
        let f1 = split_frame(&info(1), &vec![1u8; 3000], &[]);
        assert_eq!(feed(&mut asm, &f0, &[], t0).len(), 1);
        // Frame 1 loses its middle part; the gap shows when part 2 lands.
        let ev = feed(&mut asm, &f1, &[1], t0);
        assert!(ev.is_empty());
        assert_eq!(
            asm.outgoing,
            vec![Packet::Nack {
                frame_id: 1,
                missing: vec![1]
            }]
        );
        // Frame 2 completes but waits behind frame 1.
        let f2 = split_frame(&info(2), &vec![2u8; 100], &[]);
        assert!(feed(&mut asm, &f2, &[], t0).is_empty());
        // The retransmit arrives: both frames come out, in order.
        let ev = feed(&mut asm, &f1[1..2], &[], t0 + Duration::from_millis(5));
        let ids: Vec<u64> = ev
            .iter()
            .map(|e| match e {
                Event::Frame(f) => f.info.frame_id,
                Event::Lost(id) => *id,
            })
            .collect();
        assert_eq!(ids, vec![1, 2]);
        assert_eq!(asm.stats.repaired, 1);
    }

    #[test]
    fn a_frame_is_given_up_after_unanswered_nacks() {
        let mut asm = Assembler::new();
        asm.set_rtt(Duration::from_millis(20));
        let t0 = Instant::now();
        let f0 = split_frame(&info(0), &vec![0u8; 3000], &[]);
        assert!(feed(&mut asm, &f0, &[2], t0).is_empty());
        // Tail loss: nothing later arrives, so the timer asks.
        assert!(asm.tick(t0 + Duration::from_millis(13)).is_empty());
        assert_eq!(asm.outgoing.len(), 1);
        assert!(asm.tick(t0 + Duration::from_millis(40)).is_empty());
        assert_eq!(asm.outgoing.len(), 2);
        let ev = asm.tick(t0 + Duration::from_millis(70));
        assert_eq!(ev, vec![Event::Lost(0)]);
        assert_eq!(
            asm.outgoing.last(),
            Some(&Packet::Ack {
                frame_id: 0,
                decoded_at_ms: 0
            })
        );
        assert_eq!(asm.stats.lost, 1);
        // A late part of the dropped frame is ignored.
        assert!(feed(&mut asm, &f0[2..3], &[], t0 + Duration::from_millis(80)).is_empty());
        assert_eq!(asm.pending(), 0);
    }

    #[test]
    fn part_cache_serves_recent_parts_only() {
        let mut cache = PartCache::new();
        for id in 0..20u64 {
            cache.insert(id, vec![vec![id as u8; 10], vec![id as u8 + 100; 10]]);
        }
        assert_eq!(cache.part(19, 1), Some(&[119u8; 10][..]));
        assert_eq!(cache.part(19, 2), None);
        assert_eq!(cache.part(1, 0), None);
    }
}
