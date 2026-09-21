//! xkb keymap construction and the per-session key state.

use xkbcommon::xkb;

use crate::{Error, Result};

#[derive(Debug, Clone, Default)]
pub struct KeymapNames {
    pub rules: String,
    pub model: String,
    pub layout: String,
    pub variant: String,
    pub options: Option<String>,
}

/// Compile a keymap from RMLVO names and return its text (format v1).
pub fn keymap_from_names(names: &KeymapNames) -> Result<String> {
    let ctx = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
    let keymap = xkb::Keymap::new_from_names(
        &ctx,
        &names.rules,
        &names.model,
        &names.layout,
        &names.variant,
        names.options.clone(),
        xkb::KEYMAP_COMPILE_NO_FLAGS,
    )
    .ok_or_else(|| Error::Xkb(format!("cannot compile keymap {names:?}")))?;
    Ok(keymap.get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1))
}

/// Parse a client's keymap: full xkb keymap text (format v1), or RMLVO names
/// for the server to compile, written `rmlvo:layout=dk;variant=;options=...`
/// by clients that cannot produce xkb text themselves (macOS). Unnamed
/// fields are empty; an empty layout means `us`.
pub fn keymap_text(keymap: &str) -> Result<String> {
    let Some(fields) = keymap.strip_prefix("rmlvo:") else {
        return Ok(keymap.to_owned());
    };
    let mut names = KeymapNames::default();
    for field in fields.split(';') {
        let (key, value) = field.split_once('=').unwrap_or((field, ""));
        let value = value.trim().to_owned();
        match key.trim() {
            "rules" => names.rules = value,
            "model" => names.model = value,
            "layout" => names.layout = value,
            "variant" => names.variant = value,
            "options" => names.options = (!value.is_empty()).then_some(value),
            _ => {}
        }
    }
    if names.layout.is_empty() {
        names.layout = "us".into();
    }
    keymap_from_names(&names)
}

/// Tracks modifier state for the virtual keyboard from raw key events.
pub struct KeyState {
    pub keymap: xkb::Keymap,
    state: xkb::State,
    pub text: String,
    last_mods: (u32, u32, u32, u32),
}

impl KeyState {
    /// Compile a client's keymap (see [`keymap_text`]).
    pub fn from_text(keymap: &str) -> Result<Self> {
        let text = keymap_text(keymap)?;
        let text = text.as_str();
        let ctx = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let keymap = xkb::Keymap::new_from_string(
            &ctx,
            text.to_owned(),
            xkb::KEYMAP_FORMAT_TEXT_V1,
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .ok_or_else(|| Error::Xkb("cannot compile keymap text".into()))?;
        let state = xkb::State::new(&keymap);
        Ok(Self {
            keymap,
            state,
            text: text.to_owned(),
            last_mods: (0, 0, 0, 0),
        })
    }

    pub fn default_us() -> Result<Self> {
        let text = keymap_from_names(&KeymapNames {
            layout: "us".into(),
            ..Default::default()
        })?;
        Self::from_text(&text)
    }

    /// Apply a key event. Returns the new `(depressed, latched, locked, group)`
    /// when it changed.
    pub fn update(&mut self, evdev_code: u32, pressed: bool) -> Option<(u32, u32, u32, u32)> {
        let dir = if pressed {
            xkb::KeyDirection::Down
        } else {
            xkb::KeyDirection::Up
        };
        self.state
            .update_key(xkb::Keycode::new(evdev_code.saturating_add(8)), dir);
        let mods = (
            self.state.serialize_mods(xkb::STATE_MODS_DEPRESSED),
            self.state.serialize_mods(xkb::STATE_MODS_LATCHED),
            self.state.serialize_mods(xkb::STATE_MODS_LOCKED),
            self.state.serialize_layout(xkb::STATE_LAYOUT_EFFECTIVE),
        );
        if mods != self.last_mods {
            self.last_mods = mods;
            Some(mods)
        } else {
            None
        }
    }

    pub fn reset(&mut self) {
        self.state = xkb::State::new(&self.keymap);
        self.last_mods = (0, 0, 0, 0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_us_keymap_text() {
        let text = keymap_from_names(&KeymapNames {
            layout: "us".into(),
            ..Default::default()
        })
        .unwrap();
        assert!(text.contains("xkb_keymap"));
        assert!(text.contains("xkb_symbols"));
    }

    #[test]
    fn compiles_rmlvo_names() {
        let text = keymap_text("rmlvo:layout=dk;variant=").unwrap();
        assert!(text.contains("xkb_keymap"));
        assert!(text.contains("dk"), "the Danish symbols are included");
        let ks =
            KeyState::from_text("rmlvo:layout=de;variant=nodeadkeys;options=ctrl:nocaps").unwrap();
        assert!(ks.text.starts_with("xkb_keymap"));
        // Plain xkb text passes through unchanged.
        assert_eq!(keymap_text(&text).unwrap(), text);
        assert!(keymap_text("rmlvo:layout=no-such-layout").is_err());
    }

    #[test]
    fn shift_changes_modifiers() {
        let mut ks = KeyState::default_us().unwrap();
        const KEY_LEFTSHIFT: u32 = 42;
        let mods = ks
            .update(KEY_LEFTSHIFT, true)
            .expect("shift press changes mods");
        assert_ne!(mods.0, 0);
        assert!(ks.update(35, true).is_none());
        assert!(ks.update(35, false).is_none());
        let mods = ks
            .update(KEY_LEFTSHIFT, false)
            .expect("shift release changes mods");
        assert_eq!(mods.0, 0);
    }
}
