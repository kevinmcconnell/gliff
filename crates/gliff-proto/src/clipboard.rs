//! Clipboard transfer rules shared by both peers.
//!
//! A selection is never pushed. The side whose clipboard changed sends an
//! [`ClipboardMsg::Offer`] naming the mime types (and files) it can serve; the
//! other side advertises the same to its own compositor and only when an
//! application there pastes does it send a [`ClipboardMsg::Request`]. The data
//! then flows as [`ClipboardMsg::Data`] chunks of at most [`CHUNK`] bytes,
//! with at most [`WINDOW`] chunks in flight before an [`ClipboardMsg::Ack`],
//! so a large payload neither stalls the video stream nor buffers without
//! bound on either peer. Either side may [`ClipboardMsg::Abort`] a transfer.
//!
//! Files (`text/uri-list` on the source clipboard) are not forwarded as URIs,
//! which would be meaningless on the other machine. The offer lists them by
//! relative path and size; the pasting side streams each one into a spool
//! directory and hands its applications a URI list pointing there.
//!
//! No I/O here; the state machines and string helpers are pure.

use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Largest `data_len` of one `Data` message.
pub const CHUNK: usize = 256 * 1024;
/// Chunks a sender may have outstanding before it waits for an `Ack`.
pub const WINDOW: u32 = 4;
/// Cap on a single in-memory item (anything that is not a file).
pub const MAX_ITEM: u64 = 32 * 1024 * 1024;
/// Cap on the summed size of the files in one offer.
pub const MAX_FILES_TOTAL: u64 = 4 * 1024 * 1024 * 1024;
/// Cap on the number of entries (files and directories) in one offer.
pub const MAX_FILE_ENTRIES: usize = 10_000;

/// Mime type the pasting side uses for text it received as any text flavour.
pub const TEXT_MIME: &str = "text/plain;charset=utf-8";
/// Text flavours, best first; they are interchangeable for our purposes.
pub const TEXT_MIMES: &[&str] = &[
    "text/plain;charset=utf-8",
    "text/plain",
    "UTF8_STRING",
    "STRING",
    "TEXT",
];
/// Mime types that carry a list of local files. They are consumed at offer
/// time and regenerated on the pasting side.
pub const URI_LIST_MIME: &str = "text/uri-list";
pub const GNOME_FILES_MIME: &str = "x-special/gnome-copied-files";
pub const FILE_MIMES: &[&str] = &[URI_LIST_MIME, GNOME_FILES_MIME];

/// X11 selection atoms that name no data; some apps still advertise them.
const NON_DATA_TARGETS: &[&str] = &[
    "TARGETS",
    "TIMESTAMP",
    "MULTIPLE",
    "SAVE_TARGETS",
    "DELETE",
    "INSERT_PROPERTY",
    "INSERT_SELECTION",
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClipboardFile {
    /// Path relative to the copied item, `/`-separated; a top-level item is
    /// its own name.
    pub path: String,
    pub size: u64,
    pub dir: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClipboardItem {
    Mime(String),
    /// Index into the offer's `files`.
    File(u32),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClipboardMsg {
    /// The sender's selection changed to something it can serve. Empty lists
    /// mean the selection was cleared or holds nothing forwardable.
    Offer {
        mime_types: Vec<String>,
        files: Vec<ClipboardFile>,
    },
    /// Stream `item` from the current offer. `id` is chosen by the requester.
    Request { id: u32, item: ClipboardItem },
    /// One chunk; the payload of `data_len` bytes follows on the wire.
    Data {
        id: u32,
        offset: u64,
        data_len: u32,
        done: bool,
    },
    /// The requester has consumed `received` bytes so far.
    Ack { id: u32, received: u64 },
    /// Either side gives up on the transfer.
    Abort { id: u32 },
}

/// The mime types worth advertising to the peer: real data types, minus the
/// file-list types (those become `files`), with duplicates removed and the
/// text flavours collapsed to what the pasting side regenerates anyway.
pub fn forwardable_mimes(offered: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut had_text = false;
    for m in offered {
        let m = m.trim();
        if m.is_empty()
            || NON_DATA_TARGETS.contains(&m)
            || FILE_MIMES.contains(&m)
            || m.contains(char::is_whitespace)
        {
            continue;
        }
        if TEXT_MIMES.contains(&m) {
            had_text = true;
            continue;
        }
        if !out.iter().any(|o| o == m) {
            out.push(m.to_string());
        }
    }
    if had_text {
        out.splice(0..0, TEXT_MIMES.iter().map(|m| m.to_string()));
    }
    out
}

pub fn is_text_mime(mime: &str) -> bool {
    TEXT_MIMES.contains(&mime)
}

pub fn is_file_mime(mime: &str) -> bool {
    FILE_MIMES.contains(&mime)
}

/// Whether `offered` includes a file list.
pub fn offers_files(offered: &[String]) -> bool {
    offered.iter().any(|m| is_file_mime(m))
}

/// The mime type to actually read from a local clipboard for a peer's
/// request: the exact type when present, otherwise any text flavour for a
/// text request.
pub fn resolve_mime<'a>(requested: &str, offered: &'a [String]) -> Option<&'a str> {
    if let Some(m) = offered.iter().find(|o| o.as_str() == requested) {
        return Some(m);
    }
    if is_text_mime(requested) {
        return TEXT_MIMES
            .iter()
            .find_map(|t| offered.iter().find(|o| o.as_str() == *t))
            .map(String::as_str);
    }
    None
}

/// What to advertise to the local compositor for a peer's offer: the peer's
/// mime types plus the file-list types when the offer carries files.
pub fn local_mimes_for_offer(mime_types: &[String], has_files: bool) -> Vec<String> {
    let mut out: Vec<String> = mime_types
        .iter()
        .filter(|m| !is_file_mime(m))
        .cloned()
        .collect();
    if has_files {
        out.extend(FILE_MIMES.iter().map(|m| m.to_string()));
    }
    out
}

/// Local paths from a `text/uri-list` or `x-special/gnome-copied-files` body.
/// Non-`file:` URIs and malformed lines are skipped.
pub fn parse_uri_list(body: &str) -> Vec<PathBuf> {
    body.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(file_uri_to_path)
        .collect()
}

fn file_uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    // An authority (`file://host/path`) is only accepted when it is empty or
    // localhost; the path always starts at the first `/`.
    let slash = rest.find('/')?;
    let host = &rest[..slash];
    if !(host.is_empty() || host == "localhost") {
        return None;
    }
    let decoded = percent_decode(&rest[slash..])?;
    if decoded.contains('\0') {
        return None;
    }
    Some(PathBuf::from(decoded))
}

fn percent_decode(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = bytes.get(i + 1..i + 3)?;
            let v = u8::from_str_radix(std::str::from_utf8(hex).ok()?, 16).ok()?;
            out.push(v);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn percent_encode_path(path: &Path) -> String {
    let mut out = String::new();
    for b in path.to_string_lossy().bytes() {
        let keep = b.is_ascii_alphanumeric() || b"/-._~!$&'()*+,;=:@".contains(&b);
        if keep {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// A `text/uri-list` body for local absolute paths (CRLF-separated, as the
/// format requires).
pub fn uri_list(paths: &[PathBuf]) -> String {
    let mut s = String::new();
    for p in paths {
        s.push_str("file://");
        s.push_str(&percent_encode_path(p));
        s.push_str("\r\n");
    }
    s
}

/// An `x-special/gnome-copied-files` body: a `copy` verb then one URI per line.
pub fn gnome_copied_files(paths: &[PathBuf]) -> String {
    let mut s = String::from("copy");
    for p in paths {
        s.push('\n');
        s.push_str("file://");
        s.push_str(&percent_encode_path(p));
    }
    s
}

/// The body for a file-list mime type pointing at local copies.
pub fn file_list_body(mime: &str, paths: &[PathBuf]) -> Option<String> {
    match mime {
        URI_LIST_MIME => Some(uri_list(paths)),
        GNOME_FILES_MIME => Some(gnome_copied_files(paths)),
        _ => None,
    }
}

/// A peer-supplied relative path that is safe to create under a spool
/// directory: non-empty, relative, and without `.`/`..` components or NULs.
pub fn safe_relative_path(path: &str) -> Option<PathBuf> {
    if path.is_empty() || path.contains('\0') {
        return None;
    }
    let p = Path::new(path);
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::Normal(n) if !n.is_empty() => out.push(n),
            _ => return None,
        }
    }
    if out.as_os_str().is_empty() {
        None
    } else {
        Some(out)
    }
}

/// Whether the entries of an offer are acceptable: every path safe and
/// distinct, and the counts and sizes within the caps.
pub fn validate_files(files: &[ClipboardFile]) -> Result<(), FilesError> {
    if files.len() > MAX_FILE_ENTRIES {
        return Err(FilesError::TooMany(files.len()));
    }
    let mut total: u64 = 0;
    let mut seen = std::collections::HashSet::new();
    for f in files {
        let p = safe_relative_path(&f.path).ok_or_else(|| FilesError::BadPath(f.path.clone()))?;
        if !seen.insert(p) {
            return Err(FilesError::BadPath(f.path.clone()));
        }
        if f.dir && f.size != 0 {
            return Err(FilesError::BadPath(f.path.clone()));
        }
        total = total.saturating_add(f.size);
    }
    if total > MAX_FILES_TOTAL {
        return Err(FilesError::TooLarge(total));
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FilesError {
    #[error("{0} entries exceed the limit of {MAX_FILE_ENTRIES}")]
    TooMany(usize),
    #[error("files total {0} bytes, over the limit of {MAX_FILES_TOTAL}")]
    TooLarge(u64),
    #[error("unsafe or duplicate path {0:?}")]
    BadPath(String),
}

/// The top-level entries of a file list: those without a `/` in their path.
/// They are what the pasting side puts in its URI list.
pub fn top_level(files: &[ClipboardFile]) -> impl Iterator<Item = &ClipboardFile> {
    files.iter().filter(|f| !f.path.contains('/'))
}

/// Sender-side flow control: bytes sent but not yet acked must stay under
/// [`WINDOW`] chunks' worth.
#[derive(Debug, Clone, Default)]
pub struct SendWindow {
    pub offset: u64,
    acked: u64,
}

impl SendWindow {
    pub fn may_send(&self) -> bool {
        self.offset - self.acked < WINDOW as u64 * CHUNK as u64
    }

    pub fn on_sent(&mut self, len: usize) {
        self.offset += len as u64;
    }

    pub fn on_ack(&mut self, received: u64) -> Result<(), ChunkError> {
        if received > self.offset {
            return Err(ChunkError::BadAck { received });
        }
        self.acked = self.acked.max(received);
        Ok(())
    }
}

/// Receiver-side check of chunk order and size, with an optional total cap.
#[derive(Debug, Clone)]
pub struct Assembler {
    pub received: u64,
    pub chunks: u32,
    cap: Option<u64>,
    pub done: bool,
}

impl Assembler {
    pub fn new(cap: Option<u64>) -> Self {
        Self {
            received: 0,
            chunks: 0,
            cap,
            done: false,
        }
    }

    /// Validate a `Data` header before its payload is used.
    pub fn accept(&mut self, offset: u64, data_len: u32, done: bool) -> Result<(), ChunkError> {
        if self.done {
            return Err(ChunkError::AfterDone);
        }
        if offset != self.received {
            return Err(ChunkError::BadOffset {
                expected: self.received,
                got: offset,
            });
        }
        if data_len as usize > CHUNK {
            return Err(ChunkError::ChunkTooLarge(data_len));
        }
        let next = self.received + data_len as u64;
        if let Some(cap) = self.cap {
            if next > cap {
                return Err(ChunkError::OverCap(cap));
            }
        }
        self.received = next;
        self.chunks += 1;
        self.done = done;
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ChunkError {
    #[error("chunk at offset {got}, expected {expected}")]
    BadOffset { expected: u64, got: u64 },
    #[error("chunk of {0} bytes exceeds {CHUNK}")]
    ChunkTooLarge(u32),
    #[error("transfer exceeds the {0} byte limit")]
    OverCap(u64),
    #[error("chunk after the final one")]
    AfterDone,
    #[error("ack for {received} bytes that were not sent")]
    BadAck { received: u64 },
    #[error("more than {WINDOW} chunks in flight")]
    WindowExceeded,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strs(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn forwardable_drops_atoms_and_file_lists_and_collapses_text() {
        let offered = strs(&[
            "TARGETS",
            "text/plain",
            "image/png",
            "text/uri-list",
            "image/png",
            "UTF8_STRING",
            "x-special/gnome-copied-files",
            "text/html",
        ]);
        let got = forwardable_mimes(&offered);
        let mut expected = strs(TEXT_MIMES);
        expected.extend(strs(&["image/png", "text/html"]));
        assert_eq!(got, expected);
        assert!(forwardable_mimes(&strs(&["TIMESTAMP", "text/uri-list"])).is_empty());
    }

    #[test]
    fn resolve_prefers_exact_then_any_text() {
        let offered = strs(&["image/png", "STRING"]);
        assert_eq!(resolve_mime("image/png", &offered), Some("image/png"));
        assert_eq!(resolve_mime("text/plain", &offered), Some("STRING"));
        assert_eq!(resolve_mime("image/jpeg", &offered), None);
        assert_eq!(resolve_mime("text/plain", &strs(&["image/png"])), None);
    }

    #[test]
    fn local_mimes_add_file_lists_only_with_files() {
        let mimes = strs(&["text/plain", "text/uri-list"]);
        assert_eq!(local_mimes_for_offer(&mimes, false), strs(&["text/plain"]));
        assert_eq!(
            local_mimes_for_offer(&mimes, true),
            strs(&["text/plain", URI_LIST_MIME, GNOME_FILES_MIME])
        );
    }

    #[test]
    fn uri_list_round_trips_with_escapes() {
        let paths = vec![
            PathBuf::from("/home/k/My Docs/a#1.txt"),
            PathBuf::from("/tmp/ü.png"),
        ];
        let body = uri_list(&paths);
        assert_eq!(
            body,
            "file:///home/k/My%20Docs/a%231.txt\r\nfile:///tmp/%C3%BC.png\r\n"
        );
        assert_eq!(parse_uri_list(&body), paths);
        let gnome = gnome_copied_files(&paths);
        assert!(gnome.starts_with("copy\nfile:///home/k/My%20Docs/a%231.txt\n"));
        assert_eq!(parse_uri_list(&gnome), paths);
    }

    #[test]
    fn parse_uri_list_skips_foreign_and_broken_lines() {
        let body = "# comment\r\nhttps://example.com/x\r\nfile://otherhost/a\r\nfile://localhost/b\r\nfile:///bad%zz\r\n\r\nfile:///ok\r\n";
        assert_eq!(
            parse_uri_list(body),
            vec![PathBuf::from("/b"), PathBuf::from("/ok")]
        );
    }

    #[test]
    fn safe_relative_path_rejects_escapes() {
        assert_eq!(
            safe_relative_path("a/b.txt"),
            Some(PathBuf::from("a/b.txt"))
        );
        for bad in ["", "/etc/passwd", "../x", "a/../b", "./a", "a\0b"] {
            assert_eq!(safe_relative_path(bad), None, "{bad:?}");
        }
        assert_eq!(safe_relative_path("a/./b"), Some(PathBuf::from("a/b")));
    }

    #[test]
    fn validate_files_enforces_caps_and_duplicates() {
        let f = |p: &str, size: u64, dir: bool| ClipboardFile {
            path: p.into(),
            size,
            dir,
        };
        assert_eq!(
            validate_files(&[f("d", 0, true), f("d/x", 5, false)]),
            Ok(())
        );
        assert!(matches!(
            validate_files(&[f("x", 1, false), f("x", 1, false)]),
            Err(FilesError::BadPath(_))
        ));
        assert!(matches!(
            validate_files(&[f("../x", 1, false)]),
            Err(FilesError::BadPath(_))
        ));
        assert!(matches!(
            validate_files(&[f("d", 1, true)]),
            Err(FilesError::BadPath(_))
        ));
        assert!(matches!(
            validate_files(&[f("x", MAX_FILES_TOTAL + 1, false)]),
            Err(FilesError::TooLarge(_))
        ));
        let many: Vec<_> = (0..=MAX_FILE_ENTRIES)
            .map(|i| f(&format!("f{i}"), 0, false))
            .collect();
        assert!(matches!(validate_files(&many), Err(FilesError::TooMany(_))));
        let tops: Vec<_> = top_level(&[f("d", 0, true), f("d/x", 5, false), f("y", 1, false)])
            .map(|f| f.path.clone())
            .collect();
        assert_eq!(tops, vec!["d", "y"]);
    }

    #[test]
    fn send_window_blocks_after_window_chunks_until_acked() {
        let mut w = SendWindow::default();
        for _ in 0..WINDOW {
            assert!(w.may_send());
            w.on_sent(CHUNK);
        }
        assert!(!w.may_send());
        assert_eq!(w.offset, (CHUNK as u64) * WINDOW as u64);
        w.on_ack(CHUNK as u64).unwrap();
        assert!(w.may_send());
        w.on_sent(CHUNK);
        assert!(!w.may_send());
        assert_eq!(
            w.on_ack(w.offset + 1),
            Err(ChunkError::BadAck {
                received: w.offset + 1
            })
        );
        w.on_ack(w.offset).unwrap();
        w.on_ack(1).unwrap();
        assert!(w.may_send());
    }

    #[test]
    fn assembler_checks_order_size_cap_and_end() {
        let mut a = Assembler::new(Some(10));
        assert_eq!(a.accept(0, 4, false), Ok(()));
        assert_eq!(
            a.accept(3, 1, false),
            Err(ChunkError::BadOffset {
                expected: 4,
                got: 3
            })
        );
        assert_eq!(a.accept(4, 7, false), Err(ChunkError::OverCap(10)));
        assert_eq!(
            a.accept(4, CHUNK as u32 + 1, false),
            Err(ChunkError::ChunkTooLarge(CHUNK as u32 + 1))
        );
        assert_eq!(a.accept(4, 6, true), Ok(()));
        assert!(a.done);
        assert_eq!(a.accept(10, 0, true), Err(ChunkError::AfterDone));
        let mut unbounded = Assembler::new(None);
        assert_eq!(unbounded.accept(0, CHUNK as u32, false), Ok(()));
        assert_eq!(unbounded.chunks, 1);
    }
}
