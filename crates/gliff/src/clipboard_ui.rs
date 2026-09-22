//! The GTK thread's half of the clipboard: watches the local `gdk::Clipboard`
//! for changes to offer to the server, reads a local item when the server
//! requests one, and proxies the server's offer as a lazy content provider
//! whose bytes are fetched only when an application pastes.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use bytes::Bytes;
use gtk::gdk;
use gtk::gio;
use gtk::gio::prelude::*;
use gtk::glib;
use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk4 as gtk;
use tokio::sync::mpsc::{Sender, UnboundedSender};
use tokio::sync::oneshot;

use gliff_proto::clipboard::{
    forwardable_mimes, local_mimes_for_offer, offers_files, parse_uri_list, resolve_mime, CHUNK,
    URI_LIST_MIME,
};
use gliff_proto::ClipboardFile;
use gliff_transport::clipboard::files::{list_files, LocalFiles};
use gliff_transport::clipboard::progress::{describe, Progress, State};

use crate::clipboard::ToWorker;

/// Longest `text/uri-list` we read to build an offer.
const URI_LIST_MAX: usize = 1024 * 1024;

fn clipboard() -> Option<gdk::Clipboard> {
    gdk::Display::default().map(|d| d.clipboard())
}

/// Report every change of the local clipboard to the worker, except changes
/// made by our own proxy provider.
pub fn watch_local(sender: impl Fn() -> Option<UnboundedSender<ToWorker>> + 'static) {
    let Some(cb) = clipboard() else {
        return;
    };
    let sender = Rc::new(sender);
    // Bumped per change so a slow file listing for a stale one is dropped.
    let selection_gen = Rc::new(Cell::new(0u64));
    cb.connect_changed(move |cb| {
        if cb.is_local() {
            return;
        }
        let Some(tx) = sender() else {
            return;
        };
        let gen = selection_gen.get() + 1;
        selection_gen.set(gen);
        let selection_gen = selection_gen.clone();
        let mimes: Vec<String> = cb
            .formats()
            .mime_types()
            .iter()
            .map(|m| m.to_string())
            .collect();
        let cb = cb.clone();
        glib::MainContext::default().spawn_local(async move {
            let files = if offers_files(&mimes) {
                local_files(&cb).await
            } else {
                LocalFiles::default()
            };
            if selection_gen.get() != gen {
                return;
            }
            let _ = tx.send(ToWorker::LocalOffer {
                mime_types: forwardable_mimes(&mimes),
                files,
            });
        });
    });
}

/// List the files behind the local clipboard's URI list, if readable.
async fn local_files(cb: &gdk::Clipboard) -> LocalFiles {
    let Ok((stream, _)) = cb
        .read_future(&[URI_LIST_MIME], glib::Priority::DEFAULT)
        .await
    else {
        return LocalFiles::default();
    };
    let mut body = Vec::new();
    while body.len() <= URI_LIST_MAX {
        match stream
            .read_bytes_future(64 * 1024, glib::Priority::DEFAULT)
            .await
        {
            Ok(b) if b.is_empty() => break,
            Ok(b) => body.extend_from_slice(&b),
            Err(_) => return LocalFiles::default(),
        }
    }
    if body.len() > URI_LIST_MAX {
        return LocalFiles::default();
    }
    let paths = parse_uri_list(&String::from_utf8_lossy(&body));
    if paths.is_empty() {
        return LocalFiles::default();
    }
    match list_files(&paths) {
        Ok(files) => files,
        Err(e) => {
            tracing::warn!(error = %e, "clipboard files not offered");
            LocalFiles::default()
        }
    }
}

/// Stream the local clipboard's `mime_type` into `reply` for the server;
/// dropping `reply` marks the end. An error is sent as such.
pub fn read_local(mime_type: String, reply: Sender<std::io::Result<Bytes>>) {
    glib::MainContext::default().spawn_local(async move {
        let Some(cb) = clipboard() else {
            return;
        };
        // A text request may name a flavour the local source does not
        // advertise; any text flavour it does have will do.
        let available: Vec<String> = cb
            .formats()
            .mime_types()
            .iter()
            .map(|m| m.to_string())
            .collect();
        let Some(actual) = resolve_mime(&mime_type, &available) else {
            let error = std::io::Error::other(format!("clipboard has no {mime_type}"));
            let _ = reply.send(Err(error)).await;
            return;
        };
        let stream = match cb.read_future(&[actual], glib::Priority::DEFAULT).await {
            Ok((stream, _)) => stream,
            Err(e) => {
                let _ = reply.send(Err(std::io::Error::other(e.to_string()))).await;
                return;
            }
        };
        loop {
            match stream
                .read_bytes_future(CHUNK, glib::Priority::DEFAULT)
                .await
            {
                Ok(b) if b.is_empty() => break,
                Ok(b) => {
                    if reply.send(Ok(Bytes::copy_from_slice(&b))).await.is_err() {
                        break;
                    }
                }
                Err(e) => {
                    let _ = reply.send(Err(std::io::Error::other(e.to_string()))).await;
                    break;
                }
            }
        }
        let _ = stream.close_future(glib::Priority::DEFAULT).await;
    });
}

/// Make the server's offer the local selection, or clear ours for an empty
/// offer.
pub fn set_remote_offer(
    tx: UnboundedSender<ToWorker>,
    serial: u32,
    mime_types: Vec<String>,
    files: Vec<ClipboardFile>,
) {
    let Some(cb) = clipboard() else {
        return;
    };
    let mimes = local_mimes_for_offer(&mime_types, !files.is_empty());
    if mimes.is_empty() {
        if cb.is_local() {
            let _ = cb.set_content(gdk::ContentProvider::NONE);
        }
        return;
    }
    let provider = RemoteProvider::new(tx, serial, mimes);
    if let Err(e) = cb.set_content(Some(&provider)) {
        tracing::warn!(error = %e, "clipboard proxy not set");
    }
}

glib::wrapper! {
    /// A content provider for the server's offer; each paste fetches the
    /// item from the server through the worker.
    pub struct RemoteProvider(ObjectSubclass<imp::RemoteProvider>)
        @extends gdk::ContentProvider;
}

impl RemoteProvider {
    fn new(tx: UnboundedSender<ToWorker>, serial: u32, mime_types: Vec<String>) -> Self {
        let obj: Self = glib::Object::new();
        let imp = obj.imp();
        *imp.tx.borrow_mut() = Some(tx);
        imp.serial.set(serial);
        *imp.mime_types.borrow_mut() = mime_types;
        obj
    }
}

mod imp {
    use super::*;
    use std::cell::RefCell;
    use std::future::Future;
    use std::pin::Pin;

    #[derive(Default)]
    pub struct RemoteProvider {
        pub tx: RefCell<Option<UnboundedSender<ToWorker>>>,
        /// Serial of the offer this provider proxies. A paste echoes it, so
        /// one racing a newer offer is refused instead of served the wrong
        /// item.
        pub serial: Cell<u32>,
        pub mime_types: RefCell<Vec<String>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for RemoteProvider {
        const NAME: &'static str = "GliffRemoteClipboard";
        type Type = super::RemoteProvider;
        type ParentType = gdk::ContentProvider;
    }

    impl ObjectImpl for RemoteProvider {}

    impl ContentProviderImpl for RemoteProvider {
        fn formats(&self) -> gdk::ContentFormats {
            let mimes = self.mime_types.borrow();
            let refs: Vec<&str> = mimes.iter().map(String::as_str).collect();
            gdk::ContentFormats::new(&refs)
        }

        fn write_mime_type_future(
            &self,
            mime_type: &str,
            stream: &gio::OutputStream,
            io_priority: glib::Priority,
        ) -> Pin<Box<dyn Future<Output = Result<(), glib::Error>> + 'static>> {
            let tx = self.tx.borrow().clone();
            let serial = self.serial.get();
            let mime_type = mime_type.to_string();
            let stream = stream.clone();
            Box::pin(async move {
                let failed = |m: String| glib::Error::new(gio::IOErrorEnum::Failed, &m);
                let tx = tx.ok_or_else(|| failed("not connected".into()))?;
                let (sink, mut rx) = tokio::sync::mpsc::channel::<Bytes>(2);
                let (result_tx, result_rx) = oneshot::channel();
                tx.send(ToWorker::Fetch {
                    mime_type,
                    serial,
                    sink,
                    result: result_tx,
                })
                .map_err(|_| failed("worker gone".into()))?;
                while let Some(chunk) = rx.recv().await {
                    stream
                        .write_all_future(chunk, io_priority)
                        .await
                        .map_err(|(_, e)| e)?;
                }
                match result_rx.await {
                    Ok(Ok(())) => Ok(()),
                    Ok(Err(e)) => Err(failed(e)),
                    Err(_) => Err(failed("transfer dropped".into())),
                }
            })
        }
    }
}

/// The transfer bars shown over the video while a paste from the server
/// takes a while: one row per job, with a cancel button. Empty rows are
/// removed, so the box is only visible while something is in flight.
pub struct TransferBars {
    list: gtk::Box,
    rows: RefCell<HashMap<u32, Row>>,
}

struct Row {
    root: gtk::Box,
    text: gtk::Label,
    bar: gtk::ProgressBar,
}

/// How long a failure stays on screen.
const FAILURE_LINGER: Duration = Duration::from_secs(6);

impl TransferBars {
    pub fn new() -> Self {
        let list = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(4)
            .halign(gtk::Align::Fill)
            .valign(gtk::Align::End)
            .visible(false)
            .css_classes(["transfers"])
            .build();
        Self {
            list,
            rows: RefCell::default(),
        }
    }

    pub fn widget(&self) -> &gtk::Box {
        &self.list
    }

    /// Show the state of job `id`; `cancel` is called with the id when the
    /// user presses the row's button.
    pub fn update(self: &Rc<Self>, id: u32, p: &Progress, cancel: impl Fn(u32) + 'static) {
        match &p.state {
            State::Running => {
                let mut rows = self.rows.borrow_mut();
                let row = rows.entry(id).or_insert_with(|| self.add_row(id, cancel));
                row.text
                    .set_text(&format!("Pasting {}: {}", p.label, describe(p)));
                match p.percent() {
                    Some(pct) => row.bar.set_fraction(pct as f64 / 100.0),
                    None => row.bar.pulse(),
                }
            }
            State::Done | State::Cancelled => self.remove(id),
            State::Failed(e) => {
                let mut rows = self.rows.borrow_mut();
                let row = rows.entry(id).or_insert_with(|| self.add_row(id, cancel));
                row.text
                    .set_text(&format!("Paste of {} failed: {e}", p.label));
                row.bar.set_visible(false);
                drop(rows);
                let this = self.clone();
                glib::timeout_add_local_once(FAILURE_LINGER, move || this.remove(id));
            }
        }
    }

    fn add_row(&self, id: u32, cancel: impl Fn(u32) + 'static) -> Row {
        let root = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(8)
            .css_classes(["transfer"])
            .build();
        let column = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(2)
            .hexpand(true)
            .build();
        let text = gtk::Label::builder()
            .halign(gtk::Align::Start)
            .ellipsize(gtk::pango::EllipsizeMode::Middle)
            .build();
        let bar = gtk::ProgressBar::new();
        column.append(&text);
        column.append(&bar);
        let button = gtk::Button::builder()
            .icon_name("process-stop-symbolic")
            .tooltip_text("Cancel")
            .valign(gtk::Align::Center)
            .build();
        button.connect_clicked(move |_| cancel(id));
        root.append(&column);
        root.append(&button);
        self.list.append(&root);
        self.list.set_visible(true);
        Row { root, text, bar }
    }

    fn remove(&self, id: u32) {
        let mut rows = self.rows.borrow_mut();
        if let Some(row) = rows.remove(&id) {
            self.list.remove(&row.root);
        }
        self.list.set_visible(!rows.is_empty());
    }
}
