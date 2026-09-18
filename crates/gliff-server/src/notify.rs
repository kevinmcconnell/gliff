//! Desktop notifications for pastes from the client that take a while, over
//! `org.freedesktop.Notifications` on the session bus. The remote side has no
//! gliff window, so a notification is where progress and cancel live. Daemons
//! such as mako, dunst and swaync draw the `value` hint as a bar.

use std::collections::HashMap;

use futures_util::StreamExt;
use tokio::sync::mpsc;
use zbus::zvariant::Value;

use gliff_transport::clipboard::progress::{describe, human_bytes, Jobs, Progress, State};

const APP: &str = "gliff";
const ICON: &str = "edit-paste-symbolic";
const CANCEL: &str = "cancel";

#[zbus::proxy(
    interface = "org.freedesktop.Notifications",
    default_service = "org.freedesktop.Notifications",
    default_path = "/org/freedesktop/Notifications"
)]
trait Notifications {
    #[allow(clippy::too_many_arguments)]
    fn notify(
        &self,
        app_name: &str,
        replaces_id: u32,
        app_icon: &str,
        summary: &str,
        body: &str,
        actions: &[&str],
        hints: HashMap<&str, Value<'_>>,
        expire_timeout: i32,
    ) -> zbus::Result<u32>;

    #[zbus(signal)]
    fn action_invoked(&self, id: u32, action_key: &str) -> zbus::Result<()>;

    #[zbus(signal)]
    fn notification_closed(&self, id: u32, reason: u32) -> zbus::Result<()>;
}

/// Start the notification task on the current `LocalSet`. Every report from
/// `reports` becomes, or updates, one notification per job; its Cancel
/// action cancels the job in `jobs`.
pub fn start(reports: mpsc::UnboundedReceiver<(u32, Progress)>, jobs: Jobs) {
    tokio::task::spawn_local(async move {
        if let Err(e) = run(reports, jobs).await {
            tracing::warn!(error = %e, "clipboard notifications unavailable");
        }
    });
}

async fn run(mut rx: mpsc::UnboundedReceiver<(u32, Progress)>, jobs: Jobs) -> zbus::Result<()> {
    let conn = zbus::Connection::session().await?;
    let proxy = NotificationsProxy::new(&conn).await?;
    let mut actions = proxy.receive_action_invoked().await?;
    let mut closed = proxy.receive_notification_closed().await?;
    let mut notes = Notes::default();
    loop {
        tokio::select! {
            report = rx.recv() => {
                let Some((job, p)) = report else { return Ok(()) };
                let Some(replaces) = notes.slot_for(job, &p) else { continue };
                let note = render(&p);
                match proxy
                    .notify(APP, replaces, ICON, &note.summary, &note.body, &note.actions, note.hints, note.timeout)
                    .await
                {
                    Ok(id) => notes.shown(job, id, p.is_final()),
                    Err(e) => tracing::debug!(error = %e, "clipboard notification failed"),
                }
            }
            Some(sig) = actions.next() => {
                if let Ok(a) = sig.args() {
                    if a.action_key == CANCEL {
                        if let Some(job) = notes.job_of(a.id) {
                            jobs.cancel(job);
                        }
                    }
                }
            }
            Some(sig) = closed.next() => {
                if let Ok(c) = sig.args() {
                    notes.dismissed(c.id);
                }
            }
        }
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
    hints: HashMap<&'static str, Value<'static>>,
    /// Milliseconds; 0 never expires, -1 is the daemon's default.
    timeout: i32,
}

fn render(p: &Progress) -> Note {
    let mut hints = HashMap::new();
    match &p.state {
        State::Running => {
            if let Some(pct) = p.percent() {
                hints.insert("value", Value::I32(pct as i32));
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
            hints.insert("urgency", Value::U8(2));
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

    #[test]
    fn a_running_paste_has_a_bar_and_a_cancel_action() {
        let n = render(&running(Some(100)));
        assert_eq!(n.summary, "Pasting photos");
        assert_eq!(n.actions, vec!["cancel", "Cancel"]);
        assert_eq!(n.hints.get("value"), Some(&Value::I32(25)));
        assert_eq!(n.timeout, 0);
        let unknown = render(&running(None));
        assert!(!unknown.hints.contains_key("value"));
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
