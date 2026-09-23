//! The local keymap to ship to the server, so every key maps the same on
//! both ends. It follows the compositor: a change of keyboard or layout
//! mid-session is sent on.

use std::time::Duration;

use tokio::sync::watch;

/// The current local keymap (xkb text, format v1); empty until the first
/// one arrives, or for good when the compositor cannot be reached.
pub type Keymap = watch::Receiver<String>;

/// How long a new session waits for the first keymap before it lets the
/// server fall back to its default.
pub const FIRST_KEYMAP_WAIT: Duration = Duration::from_secs(1);

pub fn watch() -> Keymap {
    let (tx, rx) = watch::channel(String::new());
    let sink = Box::new(move |text: String| {
        tracing::debug!(bytes = text.len(), "local keymap changed");
        tx.send_replace(text);
    });
    if let Err(e) = hypr_input::watch_keymap(hypr_input::Target::default(), sink) {
        tracing::warn!(error = %e, "cannot follow the local keymap; server will default to us");
    }
    rx
}
