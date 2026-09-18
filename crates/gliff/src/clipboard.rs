//! The network worker's half of the clipboard: joins the transfer engine to
//! the GTK thread, which owns `gdk::Clipboard`. Bytes cross between the two
//! threads through bounded channels, so the ack window applies end to end.
//!
//! Server to local: the server's Offer goes to the UI as a status, which puts
//! a proxy provider on the local clipboard. When an application pastes, the
//! provider sends a [`ToWorker::Fetch`] with a channel; the item is fetched
//! from the server into that channel. A file paste spools every file first,
//! once per offer, and answers with a URI list into the spool.
//!
//! Local to server: the UI reports a new local selection as
//! [`ToWorker::LocalOffer`]. When the server requests a mime type the UI is
//! asked to read it ([`Status::ClipboardRead`]) into a channel that is served
//! to the engine; a file is served from disk here.

use std::cell::RefCell;
use std::io;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::mpsc::Sender as StdSender;

use bytes::Bytes;
use tokio::sync::{mpsc, oneshot, OnceCell};

use gliff_proto::clipboard::{file_list_body, is_file_mime, MAX_ITEM};
use gliff_proto::{ClientMsg, ClipboardFile, ClipboardItem, ClipboardMsg};
use gliff_transport::clipboard::files::{fetch_files, open_source, write_body, LocalFiles, Spool};
use gliff_transport::clipboard::{Event, Transfers};

use crate::net::Status;

/// What the UI thread sends the worker.
pub enum ToWorker {
    Send(ClientMsg),
    /// The local clipboard changed to these forwardable mime types and files.
    LocalOffer {
        mime_types: Vec<String>,
        files: LocalFiles,
    },
    /// An application pastes `mime_type` from the server's offer: stream it
    /// into `sink`, then report the outcome.
    Fetch {
        mime_type: String,
        sink: mpsc::Sender<Bytes>,
        result: oneshot::Sender<Result<(), String>>,
    },
}

#[derive(Default)]
struct RemoteOffer {
    files: Vec<ClipboardFile>,
    spooled: Rc<OnceCell<Vec<PathBuf>>>,
    spool: Option<Rc<Spool>>,
}

pub struct Bridge {
    transfers: Transfers,
    status: StdSender<Status>,
    local_files: RefCell<LocalFiles>,
    remote: RefCell<RemoteOffer>,
}

impl Bridge {
    pub fn new(transfers: Transfers, status: StdSender<Status>) -> Self {
        Self {
            transfers,
            status,
            local_files: RefCell::default(),
            remote: RefCell::default(),
        }
    }

    pub fn on_peer_msg(&self, msg: ClipboardMsg, payload: Bytes) {
        match self.transfers.on_msg(msg, payload) {
            Some(Event::Offer { mime_types, files }) => {
                *self.remote.borrow_mut() = RemoteOffer {
                    files: files.clone(),
                    ..RemoteOffer::default()
                };
                let _ = self
                    .status
                    .send(Status::ClipboardOffer { mime_types, files });
            }
            Some(Event::Request { id, item }) => self.on_request(id, item),
            None => {}
        }
    }

    fn on_request(&self, id: u32, item: ClipboardItem) {
        match item {
            ClipboardItem::Mime(mime_type) => {
                let (reply, rx) = mpsc::channel::<io::Result<Bytes>>(2);
                if self
                    .status
                    .send(Status::ClipboardRead { mime_type, reply })
                    .is_err()
                {
                    self.transfers.refuse(id);
                    return;
                }
                self.transfers.serve(id, rx);
            }
            ClipboardItem::File(_) => {
                let Some(path) = self.local_files.borrow().path_for(&item).map(PathBuf::from)
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

    /// A clipboard command from the UI; `Send` is handled by the caller.
    pub fn on_ui(&self, cmd: ToWorker) {
        match cmd {
            ToWorker::Send(_) => {}
            ToWorker::LocalOffer { mime_types, files } => {
                let entries = files.entries.clone();
                *self.local_files.borrow_mut() = files;
                let transfers = self.transfers.clone();
                tokio::task::spawn_local(async move {
                    let _ = transfers
                        .send(ClipboardMsg::Offer {
                            mime_types,
                            files: entries,
                        })
                        .await;
                });
            }
            ToWorker::Fetch {
                mime_type,
                sink,
                result,
            } => self.fetch(mime_type, sink, result),
        }
    }

    fn fetch(
        &self,
        mime_type: String,
        sink: mpsc::Sender<Bytes>,
        result: oneshot::Sender<Result<(), String>>,
    ) {
        let transfers = self.transfers.clone();
        if !is_file_mime(&mime_type) {
            tokio::task::spawn_local(async move {
                let r = transfers
                    .fetch(ClipboardItem::Mime(mime_type), sink, Some(MAX_ITEM))
                    .await;
                let _ = result.send(r.map(|_| ()).map_err(|e| e.to_string()));
            });
            return;
        }
        let (files, spooled, spool) = {
            let mut remote = self.remote.borrow_mut();
            if remote.files.is_empty() {
                let _ = result.send(Err("no files offered".into()));
                return;
            }
            if remote.spool.is_none() {
                match Spool::create() {
                    Ok(s) => remote.spool = Some(Rc::new(s)),
                    Err(e) => {
                        let _ = result.send(Err(format!("clipboard spool dir: {e}")));
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
            let r = match spooled
                .get_or_try_init(|| fetch_files(&transfers, &files, &spool))
                .await
            {
                Ok(paths) => {
                    let body = file_list_body(&mime_type, paths).unwrap_or_default();
                    write_body(sink, body).await.map_err(|e| e.to_string())
                }
                Err(e) => Err(e.to_string()),
            };
            let _ = result.send(r);
        });
    }
}
