//! Joins the compositor's clipboard (`hypr_input::Clipboard`) to the peer
//! (`gliff_transport::clipboard::Transfers`).
//!
//! Compositor to client: a new selection becomes an Offer of its mime types
//! (a `text/uri-list` is read and turned into a file list); when the client
//! asks for an item, the compositor's pipe for it is streamed out.
//!
//! Client to compositor: the client's Offer is advertised as our selection;
//! when an application pastes, the item is fetched from the client straight
//! into the application's pipe. A file paste first spools every file, once
//! per offer, then hands the application a URI list pointing at the spool.

use std::cell::{Cell, RefCell};
use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use bytes::Bytes;
use tokio::io::AsyncReadExt;
use tokio::net::unix::pipe;
use tokio::sync::OnceCell;

use gliff_proto::clipboard::{
    file_list_body, forwardable_mimes, is_file_mime, local_mimes_for_offer, offers_files,
    parse_uri_list, resolve_mime, MAX_ITEM, URI_LIST_MIME,
};
use gliff_proto::{ClipboardFile, ClipboardItem, ClipboardMsg};
use gliff_transport::clipboard::files::{
    fetch_files, list_files, open_source, write_body, LocalFiles, Spool,
};
use gliff_transport::clipboard::{Event, ReadSource, Transfers, WriteSink};
use hypr_input::{Clipboard, ClipboardEvent};

/// Longest `text/uri-list` we read to build an offer.
const URI_LIST_MAX: usize = 1024 * 1024;

#[derive(Default)]
struct LocalOffer {
    /// Mime types the compositor's current selection advertises.
    mime_types: Vec<String>,
    files: LocalFiles,
}

#[derive(Default)]
struct RemoteOffer {
    files: Vec<ClipboardFile>,
    /// The files once spooled for a paste; filled at most once per offer.
    spooled: Rc<OnceCell<Vec<PathBuf>>>,
    spool: Option<Rc<Spool>>,
}

pub struct Bridge {
    transfers: Transfers,
    compositor: Option<Clipboard>,
    local: Rc<RefCell<LocalOffer>>,
    remote: Rc<RefCell<RemoteOffer>>,
    /// Bumped per selection so a slow offer build for a stale one is dropped.
    selection_gen: Rc<Cell<u64>>,
}

impl Bridge {
    pub fn new(transfers: Transfers, compositor: Option<Clipboard>) -> Self {
        Self {
            transfers,
            compositor,
            local: Rc::default(),
            remote: Rc::default(),
            selection_gen: Rc::new(Cell::new(0)),
        }
    }

    /// A message from the client.
    pub fn on_peer_msg(&self, msg: ClipboardMsg, payload: Bytes) {
        match self.transfers.on_msg(msg, payload) {
            Some(Event::Offer { mime_types, files }) => self.on_remote_offer(mime_types, files),
            Some(Event::Request { id, item }) => self.on_request(id, item),
            None => {}
        }
    }

    /// An event from the compositor's clipboard thread.
    pub fn on_compositor_event(&self, ev: ClipboardEvent) {
        match ev {
            ClipboardEvent::Selection { mime_types } => self.on_selection(mime_types),
            ClipboardEvent::Paste { mime_type, fd } => self.on_paste(mime_type, fd),
        }
    }

    fn on_remote_offer(&self, mime_types: Vec<String>, files: Vec<ClipboardFile>) {
        let advertise = local_mimes_for_offer(&mime_types, !files.is_empty());
        let mut remote = self.remote.borrow_mut();
        // Dropping the previous spool removes its files.
        *remote = RemoteOffer {
            files,
            ..RemoteOffer::default()
        };
        if let Some(c) = &self.compositor {
            c.offer(advertise);
        }
    }

    fn on_request(&self, id: u32, item: ClipboardItem) {
        let Some(compositor) = &self.compositor else {
            self.transfers.refuse(id);
            return;
        };
        match item {
            ClipboardItem::Mime(requested) => {
                let local = self.local.borrow();
                let Some(actual) = resolve_mime(&requested, &local.mime_types) else {
                    tracing::debug!(id, %requested, "clipboard request for an item not offered");
                    self.transfers.refuse(id);
                    return;
                };
                match compositor
                    .receive(actual.to_string())
                    .map_err(|e| e.to_string())
                    .and_then(|fd| pipe::Receiver::from_owned_fd(fd).map_err(|e| e.to_string()))
                {
                    Ok(rx) => self.transfers.serve(id, ReadSource(rx)),
                    Err(e) => {
                        tracing::warn!(id, error = %e, "clipboard receive failed");
                        self.transfers.refuse(id);
                    }
                }
            }
            ClipboardItem::File(_) => {
                let Some(path) = self.local.borrow().files.path_for(&item).map(PathBuf::from)
                else {
                    self.transfers.refuse(id);
                    return;
                };
                let transfers = self.transfers.clone();
                tokio::task::spawn_local(async move {
                    match open_source(&path).await {
                        Ok(src) => transfers.serve(id, src),
                        Err(e) => {
                            tracing::warn!(id, path = %path.display(), error = %e, "clipboard file open failed");
                            transfers.refuse(id);
                        }
                    }
                });
            }
        }
    }

    fn on_selection(&self, mime_types: Vec<String>) {
        let gen = self.selection_gen.get() + 1;
        self.selection_gen.set(gen);
        {
            let mut local = self.local.borrow_mut();
            local.mime_types = mime_types.clone();
            local.files = LocalFiles::default();
        }
        let offered = forwardable_mimes(&mime_types);
        let wants_files = offers_files(&mime_types);
        let uri_list = wants_files
            .then_some(self.compositor.as_ref())
            .flatten()
            .and_then(|c| match c.receive(URI_LIST_MIME.into()) {
                Ok(fd) => Some(fd),
                Err(e) => {
                    tracing::warn!(error = %e, "clipboard uri-list receive failed");
                    None
                }
            });
        let transfers = self.transfers.clone();
        let local = self.local.clone();
        let selection_gen = self.selection_gen.clone();
        tokio::task::spawn_local(async move {
            let files = match uri_list {
                Some(fd) => local_files_from_pipe(fd).await,
                None => LocalFiles::default(),
            };
            if selection_gen.get() != gen {
                return; // superseded while we were listing
            }
            let entries = files.entries.clone();
            local.borrow_mut().files = files;
            let _ = transfers
                .send(ClipboardMsg::Offer {
                    mime_types: offered,
                    files: entries,
                })
                .await;
        });
    }

    fn on_paste(&self, mime_type: String, fd: OwnedFd) {
        let target = match pipe::Sender::from_owned_fd(fd) {
            Ok(tx) => tx,
            Err(e) => {
                tracing::warn!(error = %e, "paste target is not a pipe");
                return;
            }
        };
        let transfers = self.transfers.clone();
        if is_file_mime(&mime_type) {
            let (files, spooled, spool) = {
                let mut remote = self.remote.borrow_mut();
                if remote.files.is_empty() {
                    return;
                }
                if remote.spool.is_none() {
                    match Spool::create() {
                        Ok(s) => remote.spool = Some(Rc::new(s)),
                        Err(e) => {
                            tracing::warn!(error = %e, "clipboard spool dir");
                            return;
                        }
                    }
                }
                (
                    remote.files.clone(),
                    remote.spooled.clone(),
                    remote.spool.clone().expect("just created"),
                )
            };
            tokio::task::spawn_local(async move {
                let paths = spooled
                    .get_or_try_init(|| fetch_files(&transfers, &files, &spool))
                    .await;
                match paths {
                    Ok(paths) => {
                        let body = file_list_body(&mime_type, paths).unwrap_or_default();
                        if let Err(e) = write_body(WriteSink(target), body).await {
                            tracing::debug!(error = %e, "paste target closed early");
                        }
                    }
                    Err(e) => tracing::warn!(error = %e, "clipboard file paste failed"),
                }
            });
        } else {
            tokio::task::spawn_local(async move {
                if let Err(e) = transfers
                    .fetch(
                        ClipboardItem::Mime(mime_type),
                        WriteSink(target),
                        Some(MAX_ITEM),
                    )
                    .await
                {
                    tracing::warn!(error = %e, "clipboard paste failed");
                }
            });
        }
    }
}

/// Read the compositor's `text/uri-list` and list the files it names.
async fn local_files_from_pipe(fd: OwnedFd) -> LocalFiles {
    let Ok(mut rx) = pipe::Receiver::from_owned_fd(fd) else {
        return LocalFiles::default();
    };
    let mut body = Vec::new();
    let read = tokio::time::timeout(
        Duration::from_secs(5),
        (&mut rx)
            .take(URI_LIST_MAX as u64 + 1)
            .read_to_end(&mut body),
    )
    .await;
    if !matches!(read, Ok(Ok(_))) || body.len() > URI_LIST_MAX {
        tracing::warn!("clipboard uri-list unreadable or too large");
        return LocalFiles::default();
    }
    let paths = parse_uri_list(&String::from_utf8_lossy(&body));
    if paths.is_empty() {
        return LocalFiles::default();
    }
    match tokio::task::spawn_blocking(move || list_files(&paths)).await {
        Ok(Ok(files)) => files,
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "clipboard files not offered");
            LocalFiles::default()
        }
        Err(_) => LocalFiles::default(),
    }
}
