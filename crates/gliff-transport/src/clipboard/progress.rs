//! Progress of a paste as the user sees it: one job per paste, whether a
//! single item or a whole file set, with the bytes done of a known or unknown
//! total. A job that lasts longer than [`QUIET`] is reported at a steady
//! cadence so the owner can show a bar or a notification, and can be
//! cancelled, which aborts its fetch and tells the peer.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::future::Future;
use std::io;
use std::rc::Rc;
use std::time::Duration;

use bytes::Bytes;
use gliff_proto::clipboard::{is_text_mime, top_level};
use gliff_proto::ClipboardFile;
use tokio::task::AbortHandle;

use super::{ChunkSink, TransferError};

/// A job shorter than this is never reported, so a small paste shows nothing.
pub const QUIET: Duration = Duration::from_secs(1);
/// How often a running job is reported after that.
pub const CADENCE: Duration = Duration::from_millis(250);

#[derive(Clone, Debug, PartialEq)]
pub struct Progress {
    /// What is being pasted: the kind of item or a file summary.
    pub label: String,
    pub done: u64,
    pub total: Option<u64>,
    /// Bytes per second over the last few reports.
    pub rate: f64,
    pub state: State,
}

#[derive(Clone, Debug, PartialEq)]
pub enum State {
    Running,
    Done,
    Cancelled,
    Failed(String),
}

impl Progress {
    pub fn is_final(&self) -> bool {
        self.state != State::Running
    }

    /// Completion in percent, when the total is known.
    pub fn percent(&self) -> Option<u32> {
        let total = self.total.filter(|&t| t > 0)?;
        Some((self.done.saturating_mul(100) / total).min(100) as u32)
    }
}

/// Bytes done so far, shared between a job and the sinks it writes through.
#[derive(Clone, Default)]
pub struct Meter(Rc<Cell<u64>>);

impl Meter {
    pub fn add(&self, n: u64) {
        self.0.set(self.0.get() + n);
    }

    pub fn get(&self) -> u64 {
        self.0.get()
    }

    /// Count every chunk written through `sink`.
    pub fn wrap<S: ChunkSink>(&self, sink: S) -> Metered<S> {
        Metered {
            sink,
            meter: self.clone(),
        }
    }
}

pub struct Metered<S> {
    sink: S,
    meter: Meter,
}

impl<S: ChunkSink> ChunkSink for Metered<S> {
    async fn write(&mut self, chunk: Bytes) -> io::Result<()> {
        let n = chunk.len() as u64;
        self.sink.write(chunk).await?;
        self.meter.add(n);
        Ok(())
    }

    async fn finish(&mut self) -> io::Result<()> {
        self.sink.finish().await
    }
}

/// The jobs of one bridge. `report` is called on the owner's thread with
/// every change the user should see.
#[derive(Clone)]
pub struct Jobs {
    inner: Rc<Inner>,
}

struct Inner {
    next_id: Cell<u32>,
    running: RefCell<HashMap<u32, Running>>,
    report: Box<dyn Fn(u32, Progress)>,
}

struct Running {
    abort: AbortHandle,
    label: String,
    total: Option<u64>,
    meter: Meter,
    /// Whether a report went out, so a final state is only sent for a job
    /// the user has seen (a failure is always sent).
    shown: bool,
}

impl Jobs {
    pub fn new(report: impl Fn(u32, Progress) + 'static) -> Self {
        Self {
            inner: Rc::new(Inner {
                next_id: Cell::new(1),
                running: RefCell::default(),
                report: Box::new(report),
            }),
        }
    }

    /// Run `work` as a job on the current `LocalSet`, reporting its progress
    /// through the meter it is given. Returns the job's id.
    pub fn run<F, Fut>(&self, label: String, total: Option<u64>, work: F) -> u32
    where
        F: FnOnce(Meter) -> Fut,
        Fut: Future<Output = Result<(), TransferError>> + 'static,
    {
        let id = self.inner.next_id.get();
        self.inner.next_id.set(id.wrapping_add(1).max(1));
        let meter = Meter::default();
        let fut = work(meter.clone());
        let jobs = self.clone();
        let handle = tokio::task::spawn_local(async move {
            let outcome = jobs.watch(id, fut).await;
            jobs.finish(id, outcome);
        });
        self.inner.running.borrow_mut().insert(
            id,
            Running {
                abort: handle.abort_handle(),
                label,
                total,
                meter,
                shown: false,
            },
        );
        id
    }

    /// Stop a job: its fetch is dropped, which tells the peer to stop.
    pub fn cancel(&self, id: u32) {
        let removed = self.inner.running.borrow_mut().remove(&id);
        if let Some(job) = removed {
            job.abort.abort();
            if job.shown {
                (self.inner.report)(id, job.progress(State::Cancelled, 0.0));
            }
        }
    }

    async fn watch<Fut>(&self, id: u32, fut: Fut) -> Result<(), TransferError>
    where
        Fut: Future<Output = Result<(), TransferError>>,
    {
        tokio::pin!(fut);
        let mut next = tokio::time::Instant::now() + QUIET;
        let mut last = (tokio::time::Instant::now(), 0u64);
        let mut rate = 0.0;
        loop {
            tokio::select! {
                r = &mut fut => return r,
                _ = tokio::time::sleep_until(next) => {
                    next += CADENCE;
                    let mut running = self.inner.running.borrow_mut();
                    let Some(job) = running.get_mut(&id) else { return Ok(()) };
                    let now = tokio::time::Instant::now();
                    let done = job.meter.get();
                    let dt = now.duration_since(last.0).as_secs_f64();
                    if dt > 0.0 {
                        let instant = (done - last.1) as f64 / dt;
                        rate = if job.shown { 0.7 * rate + 0.3 * instant } else { instant };
                    }
                    last = (now, done);
                    job.shown = true;
                    let p = job.progress(State::Running, rate);
                    drop(running);
                    (self.inner.report)(id, p);
                }
            }
        }
    }

    fn finish(&self, id: u32, outcome: Result<(), TransferError>) {
        let removed = self.inner.running.borrow_mut().remove(&id);
        let Some(job) = removed else { return };
        let state = match outcome {
            Ok(()) => State::Done,
            Err(e) => State::Failed(e.to_string()),
        };
        if job.shown || matches!(state, State::Failed(_)) {
            (self.inner.report)(id, job.progress(state, 0.0));
        }
    }
}

impl Running {
    fn progress(&self, state: State, rate: f64) -> Progress {
        Progress {
            label: self.label.clone(),
            done: self.meter.get(),
            total: self.total,
            rate,
            state,
        }
    }
}

/// The label and total of a job that pastes a file set: the single
/// top-level name, or a count, and the sum of the file sizes.
pub fn describe_files(files: &[ClipboardFile]) -> (String, Option<u64>) {
    let tops: Vec<&str> = top_level(files).map(|f| f.path.as_str()).collect();
    let label = match tops.as_slice() {
        [one] => one.to_string(),
        many => format!("{} items", many.len()),
    };
    let total = files.iter().filter(|f| !f.dir).map(|f| f.size).sum();
    (label, Some(total))
}

/// The label of a job that pastes one clipboard item: what the user would
/// call it, not its mime type, when there is a common name for it.
pub fn describe_mime(mime: &str) -> String {
    if is_text_mime(mime) {
        return "text".into();
    }
    let base = mime
        .split(';')
        .next()
        .unwrap_or(mime)
        .trim()
        .to_ascii_lowercase();
    let Some((kind, sub)) = base.split_once('/') else {
        return mime.into();
    };
    let format = sub.split('+').next().unwrap_or(sub);
    match (kind, sub) {
        (_, "") => mime.into(),
        ("text", "html") => "formatted text".into(),
        ("text", _) => "text".into(),
        ("image" | "audio" | "video", _) if !format.is_empty() => {
            format!("{} {kind}", format.to_uppercase())
        }
        ("application", "octet-stream") => "binary data".into(),
        ("application", "pdf") => "PDF".into(),
        _ => mime.into(),
    }
}

/// Bytes as a short human figure: `1.5 MiB`.
pub fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[unit])
    }
}

/// A one-line description of a running job: `12.0 MiB of 40.0 MiB, 3.1 MiB/s`.
pub fn describe(p: &Progress) -> String {
    let mut s = human_bytes(p.done);
    if let Some(total) = p.total {
        s.push_str(&format!(" of {}", human_bytes(total)));
    }
    if p.rate > 0.0 {
        s.push_str(&format!(", {}/s", human_bytes(p.rate as u64)));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;
    use tokio::task::LocalSet;

    fn run<F: Future>(f: F) -> F::Output {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        LocalSet::new().block_on(&rt, f)
    }

    fn jobs() -> (Jobs, mpsc::UnboundedReceiver<(u32, Progress)>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            Jobs::new(move |id, p| {
                let _ = tx.send((id, p));
            }),
            rx,
        )
    }

    #[test]
    fn a_short_job_is_never_reported() {
        run(async {
            tokio::time::pause();
            let (jobs, mut rx) = jobs();
            jobs.run("x".into(), Some(10), |m| async move {
                m.add(10);
                Ok(())
            });
            tokio::time::sleep(QUIET * 2).await;
            assert!(rx.try_recv().is_err());
        });
    }

    #[test]
    fn a_long_job_is_reported_then_finished() {
        run(async {
            tokio::time::pause();
            let (jobs, mut rx) = jobs();
            let id = jobs.run("image/png".into(), Some(100), |m| async move {
                for _ in 0..4 {
                    tokio::time::sleep(QUIET).await;
                    m.add(25);
                }
                Ok(())
            });
            let (rid, first) = rx.recv().await.unwrap();
            assert_eq!(rid, id);
            assert_eq!(first.state, State::Running);
            assert_eq!(first.label, "image/png");
            let mut last = first;
            while !last.is_final() {
                last = rx.recv().await.unwrap().1;
            }
            assert_eq!(last.state, State::Done);
            assert_eq!(last.done, 100);
            assert_eq!(last.percent(), Some(100));
        });
    }

    #[test]
    fn a_failure_is_always_reported() {
        run(async {
            tokio::time::pause();
            let (jobs, mut rx) = jobs();
            jobs.run("x".into(), None, |_| async { Err(TransferError::Aborted) });
            let (_, p) = rx.recv().await.unwrap();
            assert!(matches!(p.state, State::Failed(_)), "{p:?}");
            assert_eq!(p.percent(), None);
        });
    }

    #[test]
    fn cancel_aborts_the_work_and_reports_it() {
        run(async {
            tokio::time::pause();
            let (jobs, mut rx) = jobs();
            let finished = Rc::new(Cell::new(false));
            let f = finished.clone();
            let id = jobs.run("x".into(), Some(10), |_| async move {
                tokio::time::sleep(QUIET * 10).await;
                f.set(true);
                Ok(())
            });
            let (_, p) = rx.recv().await.unwrap();
            assert_eq!(p.state, State::Running);
            jobs.cancel(id);
            let (_, p) = rx.recv().await.unwrap();
            assert_eq!(p.state, State::Cancelled);
            tokio::time::sleep(QUIET * 20).await;
            assert!(!finished.get());
            assert!(rx.try_recv().is_err());
        });
    }

    #[test]
    fn metered_sink_counts_bytes() {
        run(async {
            let meter = Meter::default();
            let mut sink = meter.wrap(super::super::MemorySink::default());
            sink.write(Bytes::from_static(b"hello")).await.unwrap();
            sink.write(Bytes::from_static(b"!")).await.unwrap();
            assert_eq!(meter.get(), 6);
            assert_eq!(sink.sink.0, b"hello!");
        });
    }

    #[test]
    fn a_file_set_is_named_by_its_top_level() {
        let f = |p: &str, size, dir| ClipboardFile {
            path: p.into(),
            size,
            dir,
        };
        let one = [f("photos", 0, true), f("photos/a", 5, false)];
        assert_eq!(describe_files(&one), ("photos".into(), Some(5)));
        let two = [f("a", 1, false), f("b", 2, false)];
        assert_eq!(describe_files(&two), ("2 items".into(), Some(3)));
    }

    #[test]
    fn an_item_is_named_by_its_kind() {
        assert_eq!(describe_mime("text/plain;charset=utf-8"), "text");
        assert_eq!(describe_mime("UTF8_STRING"), "text");
        assert_eq!(describe_mime("text/html"), "formatted text");
        assert_eq!(describe_mime("image/png"), "PNG image");
        assert_eq!(describe_mime("image/svg+xml"), "SVG image");
        assert_eq!(describe_mime("video/mp4"), "MP4 video");
        assert_eq!(describe_mime("application/octet-stream"), "binary data");
        assert_eq!(describe_mime("application/pdf"), "PDF");
        assert_eq!(describe_mime("application/x-foo"), "application/x-foo");
        assert_eq!(describe_mime("IMAGE/PNG"), "PNG image");
        assert_eq!(describe_mime("Text/HTML"), "formatted text");
        assert_eq!(describe_mime("text/"), "text/");
        assert_eq!(describe_mime("image/+xml"), "image/+xml");
    }

    #[test]
    fn figures_are_short() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1536), "1.5 KiB");
        assert_eq!(human_bytes(40 << 20), "40.0 MiB");
        let p = Progress {
            label: String::new(),
            done: 12 << 20,
            total: Some(40 << 20),
            rate: 3.1 * 1048576.0,
            state: State::Running,
        };
        assert_eq!(describe(&p), "12.0 MiB of 40.0 MiB, 3.1 MiB/s");
    }
}
