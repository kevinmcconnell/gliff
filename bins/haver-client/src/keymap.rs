//! Build the local xkb keymap to ship to the server, so every key maps the
//! same on both ends. RMLVO names come from the local Hyprland; on any failure
//! we fall back to a plain us layout and let the server default.

use hypr_input::{keymap_from_names, KeymapNames};

/// Full xkb keymap text (format v1) for the local layout, or empty to let the
/// server use its own default.
pub fn local_keymap() -> String {
    match build() {
        Ok(text) => text,
        Err(e) => {
            tracing::warn!(error = %e, "could not read local keymap; server will default to us");
            String::new()
        }
    }
}

fn build() -> anyhow::Result<String> {
    let inst = hypr_ipc::Instance::discover(None)?;
    // Hyprland reports an unset string option as the literal "[[EMPTY]]".
    let opt = |name: &str| {
        inst.get_option(name)
            .ok()
            .and_then(|o| o.string)
            .filter(|v| v != "[[EMPTY]]")
            .unwrap_or_default()
    };
    let names = KeymapNames {
        rules: String::new(),
        model: opt("input:kb_model"),
        layout: {
            let l = opt("input:kb_layout");
            if l.is_empty() {
                "us".to_string()
            } else {
                l
            }
        },
        variant: opt("input:kb_variant"),
        options: {
            let o = opt("input:kb_options");
            if o.is_empty() {
                None
            } else {
                Some(o)
            }
        },
    };
    Ok(keymap_from_names(&names)?)
}
