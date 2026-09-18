//! Desktop notifications for pastes from the client that take a while, over
//! `org.freedesktop.Notifications` on the session bus. The remote side has no
//! gliff window, so a notification is where progress and cancel live. Daemons
//! such as mako, dunst and swaync draw the `value` hint as a bar.
//!
//! libdbus is blocking, so one thread owns the connection: it posts each
//! report as it arrives and dispatches the daemon's signals in between.

use std::collections::HashMap;
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use dbus::arg::{RefArg, Variant};
use dbus::blocking::Connection;
use dbus::message::MatchRule;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

use gliff_transport::clipboard::progress::{describe, human_bytes, Jobs, Progress, State};

const APP: &str = "gliff";
const ICON: &str = "edit-paste-symbolic";
const CANCEL: &str = "cancel";
const BUS: &str = "org.freedesktop.Notifications";
const PATH: &str = "/org/freedesktop/Notifications";
const CALL_TIMEOUT: Duration = Duration::from_secs(2);
const POLL: Duration = Duration::from_millis(100);

/// Start the notification thread. Every report sent to the first sender
/// becomes, or updates, one notification per job; a Cancel pressed on one
/// arrives as the job's id on the receiver.
pub fn start() -> (Sender<(u32, Progress)>, UnboundedReceiver<u32>) {
    let (report_tx, report_rx) = std::sync::mpsc::channel();
    let (cancel_tx, cancel_rx) = unbounded_channel();
    let spawned = std::thread::Builder::new()
        .name("gliff-notify".into())
        .spawn(move || {
            if let Err(e) = run(report_rx, cancel_tx) {
                tracing::warn!(error = %e, "clipboard notifications unavailable");
            }
        });
    if let Err(e) = spawned {
        tracing::warn!(error = %e, "clipboard notification thread");
    }
    (report_tx, cancel_rx)
}

/// Cancel, on the current `LocalSet`, every job whose id arrives.
pub fn forward_cancels(mut cancels: UnboundedReceiver<u32>, jobs: Jobs) {
    tokio::task::spawn_local(async move {
        while let Some(job) = cancels.recv().await {
            jobs.cancel(job);
        }
    });
}

fn run(reports: Receiver<(u32, Progress)>, cancels: UnboundedSender<u32>) -> dbus::Result<()> {
    let conn = Connection::new_session()?;
    let notes = Arc::new(Mutex::new(Notes::default()));
    let on_action = notes.clone();
    conn.add_match(
        MatchRule::new_signal(BUS, "ActionInvoked"),
        move |(id, key): (u32, String), _, _| {
            if key == CANCEL {
                if let Some(job) = on_action.lock().unwrap().job_of(id) {
                    let _ = cancels.send(job);
                }
            }
            true
        },
    )?;
    let on_close = notes.clone();
    conn.add_match(
        MatchRule::new_signal(BUS, "NotificationClosed"),
        move |(id, _reason): (u32, u32), _, _| {
            on_close.lock().unwrap().dismissed(id);
            true
        },
    )?;
    let proxy = conn.with_proxy(BUS, PATH, CALL_TIMEOUT);
    loop {
        loop {
            let (job, p) = match reports.try_recv() {
                Ok(r) => r,
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return Ok(()),
            };
            let Some(replaces) = notes.lock().unwrap().slot_for(job, &p) else {
                continue;
            };
            let note = render(&p);
            let hints: HashMap<&str, Variant<Box<dyn RefArg>>> = note
                .hints
                .into_iter()
                .map(|(k, v)| (k, Variant(v)))
                .collect();
            let sent: dbus::Result<(u32,)> = proxy.method_call(
                BUS,
                "Notify",
                (
                    APP,
                    replaces,
                    ICON,
                    note.summary.as_str(),
                    note.body.as_str(),
                    note.actions,
                    hints,
                    note.timeout,
                ),
            );
            match sent {
                Ok((id,)) => notes.lock().unwrap().shown(job, id, p.is_final()),
                Err(e) => tracing::debug!(error = %e, "clipboard notification failed"),
            }
        }
        conn.process(POLL)?;
    }
}

/// Which notification shows which job. A notification the user closed stays
/// closed: its job is not shown again unless it fails.
#[derive(Default)]
struct Notes {
    by_job: HashMap<u32, u32>,
    by_note: HashMap<u32, u32>,
    silenced: Vec<u32>,
}

impl Notes {
    /// The notification id to replace for this report, or `None` to skip it.
    fn slot_for(&mut self, job: u32, p: &Progress) -> Option<u32> {
        if self.silenced.contains(&job) {
            if matches!(p.state, State::Failed(_)) {
                self.silenced.retain(|&j| j != job);
                return Some(0);
            }
            return None;
        }
        Some(self.by_job.get(&job).copied().unwrap_or(0))
    }

    fn shown(&mut self, job: u32, note: u32, done: bool) {
        if done {
            if let Some(old) = self.by_job.remove(&job) {
                self.by_note.remove(&old);
            }
        } else {
            self.by_job.insert(job, note);
            self.by_note.insert(note, job);
        }
    }

    fn job_of(&self, note: u32) -> Option<u32> {
        self.by_note.get(&note).copied()
    }

    fn dismissed(&mut self, note: u32) {
        if let Some(job) = self.by_note.remove(&note) {
            self.by_job.remove(&job);
            self.silenced.push(job);
        }
    }
}

struct Note {
    summary: String,
    body: String,
    actions: Vec<&'static str>,
    hints: Vec<(&'static str, Box<dyn RefArg>)>,
    /// Milliseconds; 0 never expires, -1 is the daemon's default.
    timeout: i32,
}

fn render(p: &Progress) -> Note {
    let mut hints: Vec<(&'static str, Box<dyn RefArg>)> = Vec::new();
    match &p.state {
        State::Running => {
            if let Some(pct) = p.percent() {
                hints.push(("value", Box::new(pct as i32)));
            }
            Note {
                summary: format!("Pasting {}", p.label),
                body: describe(p),
                actions: vec![CANCEL, "Cancel"],
                hints,
                timeout: 0,
            }
        }
        State::Done => Note {
            summary: format!("Pasted {}", p.label),
            body: human_bytes(p.done),
            actions: vec![],
            hints,
            timeout: -1,
        },
        State::Cancelled => Note {
            summary: format!("Paste of {} cancelled", p.label),
            body: String::new(),
            actions: vec![],
            hints,
            timeout: -1,
        },
        State::Failed(e) => {
            hints.push(("urgency", Box::new(2u8)));
            Note {
                summary: format!("Paste of {} failed", p.label),
                body: e.clone(),
                actions: vec![],
                hints,
                timeout: -1,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn running(pct_of: Option<u64>) -> Progress {
        Progress {
            label: "photos".into(),
            done: 25,
            total: pct_of,
            rate: 0.0,
            state: State::Running,
        }
    }

    fn hint_i32(n: &Note, key: &str) -> Option<i32> {
        n.hints
            .iter()
            .find(|(k, _)| *k == key)
            .and_then(|(_, v)| v.as_i64())
            .map(|v| v as i32)
    }

    #[test]
    fn a_running_paste_has_a_bar_and_a_cancel_action() {
        let n = render(&running(Some(100)));
        assert_eq!(n.summary, "Pasting photos");
        assert_eq!(n.actions, vec!["cancel", "Cancel"]);
        assert_eq!(hint_i32(&n, "value"), Some(25));
        assert_eq!(n.timeout, 0);
        let unknown = render(&running(None));
        assert_eq!(hint_i32(&unknown, "value"), None);
    }

    #[test]
    fn a_dismissed_job_is_only_shown_again_when_it_fails() {
        let mut notes = Notes::default();
        assert_eq!(notes.slot_for(7, &running(None)), Some(0));
        notes.shown(7, 42, false);
        assert_eq!(notes.slot_for(7, &running(None)), Some(42));
        assert_eq!(notes.job_of(42), Some(7));
        notes.dismissed(42);
        assert_eq!(notes.slot_for(7, &running(None)), None);
        let mut failed = running(None);
        failed.state = State::Failed("gone".into());
        assert_eq!(notes.slot_for(7, &failed), Some(0));
    }
}
